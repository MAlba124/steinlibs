#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::time::Instant;

use super::*;

const LIMIT: Duration = Duration::from_secs(5);

fn queue(want: Want) -> Queue {
    let setup = Setup {
        want,
        single_issuer: false,
        fixed_buffers: 4,
    };
    let q = Queue::new(Owned::new(32, 1024), setup).expect("a queue");
    assert_eq!(
        q.backend(),
        if want == Want::Epoll {
            Backend::Epoll
        } else {
            Backend::Uring
        }
    );
    q
}

/// Tick until `user` completes. Others go to `seen`.
fn wait(q: &mut Queue, user: u64, seen: &mut Vec<Cqe>) -> Cqe {
    let start = Instant::now();
    loop {
        if let Some(i) = seen.iter().position(|c| c.user == user) {
            return seen.remove(i);
        }
        while let Some(c) = q.pop() {
            seen.push(c);
        }
        if seen.iter().any(|c| c.user == user) {
            continue;
        }
        assert!(start.elapsed() < LIMIT, "{user} never completed");
        q.tick(Some(Duration::from_millis(20))).expect("tick");
    }
}

fn one(q: &mut Queue, op: Op, user: u64) -> Cqe {
    q.push(op, user).expect("push");
    wait(q, user, &mut Vec::new())
}

/// Close a raw fd through the queue, which owns it from here.
fn close(q: &mut Queue, fd: RawFd, user: u64) -> Cqe {
    // SAFETY: each test hands over an fd it owns and never uses again.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    q.close(fd, user).expect("close");
    wait(q, user, &mut Vec::new())
}

fn mem(b: &mut [u8]) -> Mem {
    // SAFETY: every test keeps its buffers alive past the completions.
    unsafe { Mem::raw(b.as_mut_ptr(), b.len()) }
}

fn path(p: &std::path::Path) -> Vec<u8> {
    let mut v = p.as_os_str().as_encoded_bytes().to_vec();
    v.push(0);
    v
}

fn statx(path: &mut [u8], out: &mut [u8], follow: bool) -> Op {
    Op::Statx {
        dir: crate::CWD,
        path: mem(path),
        out: mem(out),
        follow,
    }
}

fn tmp(name: &str, want: Want) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "stein-queue-{name}-{want:?}-{}",
        std::process::id()
    ))
}

fn nonblocking_tcp() -> RawFd {
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .expect("socket");
    fd.into_raw_fd()
}

macro_rules! both {
    ($($name:ident),* $(,)?) => {
        mod uring {
            $(#[test] fn $name() { super::$name(super::Want::Uring) })*
        }
        mod epoll {
            $(#[test] fn $name() { super::$name(super::Want::Epoll) })*
        }
    };
}

both!(
    a_one_shot_accept_names_its_peer,
    a_receive_of_the_whole_waits_for_all_of_it,
    a_message_receive_lays_out_name_control_and_payload,
    a_message_send_carries_its_bytes,
    a_splice_moves_bytes_through_a_pipe,
    a_file_is_reserved_and_cut,
    a_socket_option_is_set,
    a_ring_from_the_store_takes_receives,
    the_descriptor_signals_a_due_timeout,
    a_file_is_written_read_stat_renamed_and_unlinked,
    a_connection_carries_bytes_and_its_end,
    datagrams_carry_their_source,
    timeouts_fire_in_order_and_a_cancelled_one_says_so,
    a_cancelled_receive_completes_once,
    two_receives_on_one_socket_share_its_bytes,
    a_repeating_receive_takes_provided_buffers,
    a_region_is_read_into_by_range,
    a_poll_reports_a_pipe_readable,
    the_store_comes_back_and_slots_are_reused,
    a_repeating_accept_takes_each_connection,
    a_segmented_send_comes_back_coalesced,
    a_removed_group_frees_its_index,
    a_file_range_is_synced_and_advised,
    a_symlink_is_stated_itself_unless_followed,
    a_burst_on_a_repeating_receive_comes_in_one_reap,
);

fn a_file_is_written_read_stat_renamed_and_unlinked(want: Want) {
    let mut q = queue(want);
    let (a, b) = (tmp("a", want), tmp("b", want));
    let (mut pa, mut pb) = (path(&a), path(&b));
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC;
    let open = Op::Open {
        dir: crate::CWD,
        path: mem(&mut pa),
        flags,
        mode: Mode::from_raw_mode(0o644),
    };
    let fd = one(&mut q, open, 1).result().expect("open") as RawFd;
    let mut data = *b"hello queue";
    let w = Op::Write {
        fd: Fd::Raw(fd),
        mem: mem(&mut data),
        at: 0,
    };
    assert_eq!(one(&mut q, w, 2).result(), Ok(11));
    assert_eq!(
        one(
            &mut q,
            Op::Fsync {
                fd: Fd::Raw(fd),
                data_only: true
            },
            3
        )
        .result(),
        Ok(0)
    );
    assert_eq!(close(&mut q, fd, 4).result(), Ok(0));
    let mut out = [0u8; STATX_LEN];
    assert_eq!(
        one(&mut q, statx(&mut pa, &mut out, true), 5).result(),
        Ok(0)
    );
    assert_eq!(statx_of(&out).expect("statx").stx_size, 11);
    let rename = Op::Rename {
        from_dir: crate::CWD,
        from: mem(&mut pa),
        to_dir: crate::CWD,
        to: mem(&mut pb),
    };
    assert_eq!(one(&mut q, rename, 6).result(), Ok(0));

    let open = Op::Open {
        dir: crate::CWD,
        path: mem(&mut pb),
        flags: OFlags::RDONLY,
        mode: Mode::empty(),
    };
    let fd = one(&mut q, open, 7).result().expect("open") as RawFd;
    let mut back = [0u8; 32];
    let r = Op::Read {
        fd: Fd::Raw(fd),
        mem: mem(&mut back),
        at: 6,
    };
    assert_eq!(one(&mut q, r, 8).result(), Ok(5));
    assert_eq!(&back[..5], b"queue");
    let past = Op::Read {
        fd: Fd::Raw(fd),
        mem: mem(&mut back),
        at: 11,
    };
    assert_eq!(
        one(&mut q, past, 9).result(),
        Ok(0),
        "the end of the file reads nothing"
    );
    assert_eq!(close(&mut q, fd, 10).result(), Ok(0));
    let gone = Op::Unlink {
        dir: crate::CWD,
        path: mem(&mut pb),
        rmdir: false,
    };
    assert_eq!(one(&mut q, gone, 11).result(), Ok(0));
    let missing = Op::Open {
        dir: crate::CWD,
        path: mem(&mut pb),
        flags: OFlags::RDONLY,
        mode: Mode::empty(),
    };
    assert_eq!(one(&mut q, missing, 12).result(), Err(Errno::NOENT));
    let mut bare = *b"no terminator";
    let bad = Op::Open {
        dir: crate::CWD,
        path: mem(&mut bare),
        flags: OFlags::RDONLY,
        mode: Mode::empty(),
    };
    assert!(q.push(bad, 13).is_err(), "a path is a terminated string");
    assert_eq!(q.in_flight(), 0);
}

fn a_connection_carries_bytes_and_its_end(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let to = l.local_addr().expect("addr");
    let mut q = queue(want);
    let fd = nonblocking_tcp();
    assert_eq!(
        one(
            &mut q,
            Op::Connect {
                fd: Fd::Raw(fd),
                to
            },
            1
        )
        .result(),
        Ok(0)
    );
    let (mut peer, _) = l.accept().expect("accept");
    let mut out = *b"ping";
    assert_eq!(
        one(
            &mut q,
            Op::Send {
                fd: Fd::Raw(fd),
                mem: mem(&mut out)
            },
            2
        )
        .result(),
        Ok(4)
    );
    let mut got = [0u8; 4];
    peer.read_exact(&mut got).expect("read");
    assert_eq!(&got, b"ping");
    let mut inb = [0u8; 16];
    q.push(
        Op::Recv {
            fd: Fd::Raw(fd),
            mem: mem(&mut inb),
        },
        3,
    )
    .expect("recv");
    q.tick(Some(Duration::from_millis(10))).expect("tick");
    assert!(q.pop().is_none(), "nothing was sent");
    peer.write_all(b"pong").expect("write");
    assert_eq!(wait(&mut q, 3, &mut Vec::new()).result(), Ok(4));
    assert_eq!(&inb[..4], b"pong");
    drop(peer);
    assert_eq!(
        one(
            &mut q,
            Op::Recv {
                fd: Fd::Raw(fd),
                mem: mem(&mut inb)
            },
            4
        )
        .result(),
        Ok(0),
        "the peer's end"
    );
    assert_eq!(close(&mut q, fd, 5).result(), Ok(0));
}

fn datagrams_carry_their_source(want: Want) {
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let b = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    b.set_nonblocking(true).expect("nonblocking");
    let (at_a, at_b) = (a.local_addr().expect("addr"), b.local_addr().expect("addr"));
    let mut q = queue(want);
    let mut buf = [0u8; 64];
    let recv = Op::RecvFrom {
        fd: Fd::Raw(b.as_raw_fd()),
        mem: mem(&mut buf),
    };
    q.push(recv, 1).expect("recv");
    let mut out = *b"datagram";
    let send = Op::SendTo {
        fd: Fd::Raw(a.as_raw_fd()),
        mem: mem(&mut out),
        to: at_b,
    };
    let mut seen = Vec::new();
    q.push(send, 2).expect("send");
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(8));
    let got = wait(&mut q, 1, &mut seen);
    assert_eq!(got.result(), Ok(8));
    assert_eq!(&buf[..8], b"datagram");
    assert_eq!(
        got.addr,
        Some(at_a),
        "the source rides back on the completion"
    );
}

fn timeouts_fire_in_order_and_a_cancelled_one_says_so(want: Want) {
    let mut q = queue(want);
    let start = Instant::now();
    let ms = |n| Op::Timeout {
        after: Duration::from_millis(n),
    };
    q.push(ms(40), 40).expect("push");
    q.push(ms(10), 10).expect("push");
    let gone = q.push(ms(20), 20).expect("push");
    assert!(q.cancel(gone));
    assert!(
        !q.cancel(gone),
        "a second cancel of one operation is nothing"
    );
    let mut order = Vec::new();
    while order.len() < 3 {
        assert!(start.elapsed() < LIMIT);
        q.tick(Some(Duration::from_millis(50))).expect("tick");
        while let Some(c) = q.pop() {
            order.push((c.user, c.result()));
            if c.user != 20 {
                assert!(
                    start.elapsed() >= Duration::from_millis(c.user),
                    "{} fired early",
                    c.user
                );
            }
        }
    }
    assert_eq!(
        order,
        [
            (20, Err(Errno::CANCELED)),
            (10, Err(Errno::TIME)),
            (40, Err(Errno::TIME))
        ]
    );
    assert!(!q.cancel(gone), "a key past its operation names nothing");
}

fn a_cancelled_receive_completes_once(want: Want) {
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    let mut q = queue(want);
    let mut buf = [0u8; 64];
    let key = q
        .push(
            Op::Recv {
                fd: Fd::Raw(a.as_raw_fd()),
                mem: mem(&mut buf),
            },
            7,
        )
        .expect("recv");
    q.submit().expect("submit");
    assert!(q.cancel(key));
    let c = wait(&mut q, 7, &mut Vec::new());
    assert_eq!(c.result(), Err(Errno::CANCELED));
    assert!(!c.more());
    q.tick(Some(Duration::from_millis(20))).expect("tick");
    assert!(
        q.pop().is_none(),
        "one completion, and the cancel's own is the queue's"
    );
    assert_eq!(q.in_flight(), 0);
}

fn two_receives_on_one_socket_share_its_bytes(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mut q = queue(want);
    let fd = nonblocking_tcp();
    let to = l.local_addr().expect("addr");
    assert_eq!(
        one(
            &mut q,
            Op::Connect {
                fd: Fd::Raw(fd),
                to
            },
            1
        )
        .result(),
        Ok(0)
    );
    let (mut peer, _) = l.accept().expect("accept");
    let (mut x, mut y) = ([0u8; 4], [0u8; 4]);
    q.push(
        Op::Recv {
            fd: Fd::Raw(fd),
            mem: mem(&mut x),
        },
        2,
    )
    .expect("recv");
    q.push(
        Op::Recv {
            fd: Fd::Raw(fd),
            mem: mem(&mut y),
        },
        3,
    )
    .expect("recv");
    q.submit().expect("submit");
    peer.write_all(b"abcdefgh").expect("write");
    let mut seen = Vec::new();
    wait(&mut q, 2, &mut seen);
    wait(&mut q, 3, &mut seen);
    // Only epoll keeps push order.
    let both = if want == Want::Epoll {
        [x, y]
    } else {
        [x.min(y), x.max(y)]
    };
    assert_eq!(both, [*b"abcd", *b"efgh"]);
    assert_eq!(close(&mut q, fd, 4).result(), Ok(0));
}

#[repr(C, align(4096))]
struct Page([u8; 4096]);

fn a_repeating_receive_takes_provided_buffers(want: Want) {
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    let to = a.local_addr().expect("addr");
    let tx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let mut page = Box::new(Page([0; 4096]));
    let mut bufs = [[0u8; 32]; 4];
    let mut q = queue(want);
    // SAFETY: the page outlives the queue, which is dropped first below.
    let g = unsafe { q.add_group(page.0.as_mut_ptr().cast(), 4) }.expect("group");
    for (i, b) in bufs.iter_mut().enumerate() {
        q.provide(g, i as u16, mem(b)).expect("provide");
    }
    let key = q
        .push(
            Op::RecvMulti {
                fd: Fd::Raw(a.as_raw_fd()),
                group: g,
            },
            9,
        )
        .expect("recv");
    q.submit().expect("submit");
    let mut got = Vec::new();
    for word in [b"one", b"two"] {
        tx.send_to(word, to).expect("send");
        let c = wait(&mut q, 9, &mut Vec::new());
        assert!(c.more(), "still armed");
        let bid = c.buffer().expect("a buffer") as usize;
        got.push(bufs[bid][..c.res as usize].to_vec());
        q.provide(g, bid as u16, mem(&mut bufs[bid]))
            .expect("back to the ring");
    }
    assert_eq!(got, [b"one".to_vec(), b"two".to_vec()]);
    assert!(q.cancel(key));
    let last = wait(&mut q, 9, &mut Vec::new());
    assert!(!last.more(), "the cancel ends it");
    assert_eq!(q.in_flight(), 0);
    drop(q);
    drop(page);
}

fn a_region_is_read_into_by_range(want: Want) {
    let p = tmp("region", want);
    std::fs::write(&p, b"0123456789").expect("fixture");
    let file = std::fs::File::open(&p).expect("open");
    let mut slab = vec![0u8; 4096];
    let mut q = queue(want);
    // SAFETY: the slab outlives the queue and is read only after completions.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), true) }.expect("region");
    assert_eq!(
        q.is_fixed(r),
        want == Want::Uring,
        "a ring takes it as a registered buffer"
    );
    assert!(q.mem(r, 4090, 7).is_err(), "past the end");
    let part = q.mem(r, 100, 4).expect("in range");
    let read = Op::Read {
        fd: Fd::Raw(file.as_raw_fd()),
        mem: part,
        at: 3,
    };
    assert_eq!(one(&mut q, read, 1).result(), Ok(4));
    drop(q);
    assert_eq!(&slab[100..104], b"3456");
    let _ = std::fs::remove_file(&p);
}

fn a_poll_reports_a_pipe_readable(want: Want) {
    let (rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    let mut q = queue(want);
    let poll = Op::Poll {
        fd: Fd::Raw(rx.as_raw_fd()),
        events: libc::POLLIN as u32,
        multi: false,
    };
    q.push(poll, 1).expect("poll");
    q.tick(Some(Duration::from_millis(10))).expect("tick");
    assert!(q.pop().is_none(), "empty");
    rustix::io::write(&tx, b"x").expect("write");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_ne!(c.res as i16 & libc::POLLIN, 0);
    let mut b = [0u8; 4];
    let read = Op::Read {
        fd: Fd::Raw(rx.as_raw_fd()),
        mem: mem(&mut b),
        at: POSITION,
    };
    assert_eq!(one(&mut q, read, 2).result(), Ok(1));
}

fn the_store_comes_back_and_slots_are_reused(want: Want) {
    let mut q = queue(want);
    for round in 0..100 {
        assert_eq!(one(&mut q, Op::Nop, round).result(), Ok(0));
    }
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    let mut buf = [0u8; 8];
    q.push(
        Op::Recv {
            fd: Fd::Raw(a.as_raw_fd()),
            mem: mem(&mut buf),
        },
        1,
    )
    .expect("recv");
    q.submit().expect("submit");
    let store = q
        .into_store()
        .expect("what flew was cancelled and waited for");
    assert_eq!(store.ops(), 32);
}

fn a_repeating_accept_takes_each_connection(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.set_nonblocking(true).expect("nonblocking");
    let to = l.local_addr().expect("addr");
    let mut q = queue(want);
    let key = q
        .push(
            Op::Accept {
                fd: Fd::Raw(l.as_raw_fd()),
                multi: true,
            },
            5,
        )
        .expect("accept");
    q.submit().expect("submit");
    let mut keep = Vec::new();
    for _ in 0..3 {
        keep.push(std::net::TcpStream::connect(to).expect("connect"));
        let c = wait(&mut q, 5, &mut Vec::new());
        assert!(c.more());
        assert_eq!(c.addr, None, "a repeating accept names no peer");
        let fd = c.result().expect("a connection") as RawFd;
        assert_eq!(close(&mut q, fd, 6).result(), Ok(0));
    }
    assert!(q.cancel(key));
    assert!(!wait(&mut q, 5, &mut Vec::new()).more());
}

#[test]
fn a_futex_wait_ends_on_a_wake_from_another_thread() {
    let word = Box::new(core::sync::atomic::AtomicU32::new(0));
    let mut q = queue(Want::Uring);
    // SAFETY: the word outlives the queue.
    let m = unsafe { Mem::raw(word.as_ptr().cast(), 4) };
    q.push(Op::FutexWait { word: m, value: 0 }, 1)
        .expect("wait");
    q.submit().expect("submit");
    let addr = word.as_ptr() as usize;
    let waker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let w = addr as *const core::sync::atomic::AtomicU32;
        // SAFETY: the word is alive until the join below.
        unsafe { (*w).store(1, core::sync::atomic::Ordering::SeqCst) };
        // SAFETY: a futex wake on a live word, private as the wait was.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                addr,
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                1,
            )
        };
    });
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(c.result(), Ok(0));
    waker.join().expect("waker");
    // A word already moved does not wait.
    q.push(Op::FutexWait { word: m, value: 0 }, 2)
        .expect("wait");
    assert_eq!(wait(&mut q, 2, &mut Vec::new()).result(), Err(Errno::AGAIN));
    drop(q);
    drop(word);
}

#[test]
fn a_ring_posts_a_value_into_another() {
    let mut a = queue(Want::Uring);
    let mut b = queue(Want::Uring);
    let to = b.as_fd().as_raw_fd();
    assert_eq!(
        one(
            &mut a,
            Op::Post {
                ring: to,
                value: 0xfeed
            },
            1
        )
        .result(),
        Ok(0)
    );
    b.tick(Some(LIMIT)).expect("tick");
    let c = b.pop().expect("the value");
    assert!(c.is_posted());
    assert_eq!(c.user, 0xfeed);
    let mut e = queue(Want::Epoll);
    assert_eq!(
        one(&mut e, Op::Post { ring: to, value: 1 }, 2).result(),
        Err(Errno::OPNOTSUPP)
    );
}

#[test]
fn a_registered_file_is_read_by_index() {
    let p = tmp("fixed", Want::Uring);
    std::fs::write(&p, b"fixed").expect("fixture");
    let file = std::fs::File::open(&p).expect("open");
    let mut q = queue(Want::Uring);
    q.register_files(4).expect("table");
    q.set_file(2, file.as_raw_fd()).expect("install");
    drop(file);
    let mut b = [0u8; 8];
    assert_eq!(
        one(
            &mut q,
            Op::Read {
                fd: Fd::Fixed(2),
                mem: mem(&mut b),
                at: 0
            },
            1
        )
        .result(),
        Ok(5)
    );
    assert_eq!(&b[..5], b"fixed");
    assert!(queue(Want::Epoll).register_files(1).is_err());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_full_queue_refuses_and_takes_again_once_one_is_reaped() {
    let mut q = Queue::new(Owned::new(2, 0), Setup::default()).expect("queue");
    q.push(Op::Nop, 1).expect("one");
    q.push(Op::Nop, 2).expect("two");
    assert!(matches!(q.push(Op::Nop, 3), Err(IoError::OpsFull)));
    assert_eq!(wait(&mut q, 1, &mut Vec::new()).result(), Ok(0));
    q.push(Op::Nop, 3).expect("room again");
}

#[test]
fn a_store_taken_back_starts_another_queue() {
    let mut store = Owned::new(4, 64);
    for want in [Want::Uring, Want::Epoll] {
        let mut q = Queue::new(
            store,
            Setup {
                want,
                ..Setup::default()
            },
        )
        .expect("queue");
        q.push(Op::Nop, 1).expect("push");
        q.tick(Some(LIMIT)).expect("tick");
        assert_eq!(q.pop().map(|c| c.user), Some(1));
        store = q.into_store().expect("nothing in flight");
    }
    assert_eq!(store.ops(), 4);
}

fn tcp_pair(q: &mut Queue) -> (RawFd, std::net::TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let fd = nonblocking_tcp();
    let to = l.local_addr().expect("addr");
    assert_eq!(
        one(
            q,
            Op::Connect {
                fd: Fd::Raw(fd),
                to
            },
            900
        )
        .result(),
        Ok(0)
    );
    (fd, l.accept().expect("accept").0)
}

fn a_one_shot_accept_names_its_peer(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.set_nonblocking(true).expect("nonblocking");
    let mut q = queue(want);
    q.push(
        Op::Accept {
            fd: Fd::Raw(l.as_raw_fd()),
            multi: false,
        },
        1,
    )
    .expect("accept");
    q.submit().expect("submit");
    let s = std::net::TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert!(c.result().is_ok());
    assert_eq!(c.addr, s.local_addr().ok(), "the accept says who connected");
    assert_eq!(close(&mut q, c.res, 2).result(), Ok(0));
}

fn a_receive_of_the_whole_waits_for_all_of_it(want: Want) {
    let mut q = queue(want);
    let (fd, mut peer) = tcp_pair(&mut q);
    let mut b = [0u8; 8];
    q.push(
        Op::RecvAll {
            fd: Fd::Raw(fd),
            mem: mem(&mut b),
        },
        1,
    )
    .expect("recv");
    q.submit().expect("submit");
    peer.write_all(b"abc").expect("write");
    q.tick(Some(Duration::from_millis(20))).expect("tick");
    assert!(q.pop().is_none(), "three of eight is not all of it");
    peer.write_all(b"defgh").expect("write");
    assert_eq!(wait(&mut q, 1, &mut Vec::new()).result(), Ok(8));
    assert_eq!(&b, b"abcdefgh");
    let mut short = [0u8; 8];
    q.push(
        Op::RecvAll {
            fd: Fd::Raw(fd),
            mem: mem(&mut short),
        },
        2,
    )
    .expect("recv");
    peer.write_all(b"xy").expect("write");
    drop(peer);
    assert_eq!(
        wait(&mut q, 2, &mut Vec::new()).result(),
        Ok(2),
        "the end cuts it short"
    );
}

fn a_message_receive_lays_out_name_control_and_payload(want: Want) {
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    let tx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let mut page = Box::new(RingPage::ZERO);
    let mut bufs = vec![[0u8; 512]; 2];
    let mut q = queue(want);
    // SAFETY: the page outlives the queue.
    let g = unsafe { q.add_group(page.0.as_mut_ptr().cast(), 2) }.expect("group");
    for (i, b) in bufs.iter_mut().enumerate() {
        q.provide(g, i as u16, mem(b)).expect("provide");
    }
    let op = Op::RecvMsgMulti {
        fd: Fd::Raw(a.as_raw_fd()),
        group: g,
        name: true,
        control: 0,
    };
    q.push(op, 1).expect("recvmsg");
    q.submit().expect("submit");
    tx.send_to(b"hello", a.local_addr().expect("addr"))
        .expect("send");
    let c = wait(&mut q, 1, &mut Vec::new());
    let b = &bufs[c.buffer().expect("a buffer") as usize];
    let word = |at: usize| u32::from_ne_bytes(b[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(word(8), 5, "the payload's length");
    let head = 16 + ADDR_LEN;
    assert_eq!(c.res as usize, head + 5);
    assert_eq!(&b[head..head + 5], b"hello");
    assert_eq!(addr_of(&b[16..16 + word(0) as usize]), tx.local_addr().ok());
    drop(q);
    drop(page);
}

fn a_message_send_carries_its_bytes(want: Want) {
    let mut q = queue(want);
    let (fd, mut peer) = tcp_pair(&mut q);
    let mut out = *b"sendmsg";
    let op = Op::SendMsg {
        fd: Fd::Raw(fd),
        mem: mem(&mut out),
        control: Mem::EMPTY,
        to: None,
    };
    assert_eq!(one(&mut q, op, 1).result(), Ok(7));
    let mut got = [0u8; 7];
    peer.read_exact(&mut got).expect("read");
    assert_eq!(&got, b"sendmsg");
}

fn a_splice_moves_bytes_through_a_pipe(want: Want) {
    let p = tmp("splice", want);
    std::fs::write(&p, b"spliced bytes").expect("fixture");
    let file = std::fs::File::open(&p).expect("open");
    let (rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    let mut q = queue(want);
    let op = Op::Splice {
        from: Fd::Raw(file.as_raw_fd()),
        at: 0,
        to: Fd::Raw(tx.as_raw_fd()),
        len: 64,
    };
    assert_eq!(one(&mut q, op, 1).result(), Ok(13));
    let mut b = [0u8; 16];
    assert_eq!(rustix::io::read(&rx, &mut b), Ok(13));
    assert_eq!(&b[..13], b"spliced bytes");
    let _ = std::fs::remove_file(&p);
}

fn a_file_is_reserved_and_cut(want: Want) {
    let p = tmp("falloc", want);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&p)
        .expect("open");
    let mut q = queue(want);
    let fd = Fd::Raw(file.as_raw_fd());
    assert_eq!(
        one(
            &mut q,
            Op::Fallocate {
                fd,
                at: 0,
                len: 8192
            },
            1
        )
        .result(),
        Ok(0)
    );
    assert_eq!(file.metadata().expect("meta").len(), 8192);
    assert_eq!(
        one(&mut q, Op::Truncate { fd, len: 100 }, 2).result(),
        Ok(0)
    );
    assert_eq!(file.metadata().expect("meta").len(), 100);
    let _ = std::fs::remove_file(&p);
}

fn a_socket_option_is_set(want: Want) {
    let mut q = queue(want);
    let (fd, _peer) = tcp_pair(&mut q);
    let mut on = 1i32.to_ne_bytes();
    let op = Op::SetSockOpt {
        fd: Fd::Raw(fd),
        level: libc::IPPROTO_TCP as u32,
        name: libc::TCP_NODELAY as u32,
        value: mem(&mut on),
    };
    assert_eq!(one(&mut q, op, 1).result(), Ok(0));
    // SAFETY: the descriptor is the test's and open.
    let sock = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    assert_eq!(rustix::net::sockopt::tcp_nodelay(sock), Ok(true));
}

fn a_ring_from_the_store_takes_receives(want: Want) {
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
    a.set_nonblocking(true).expect("nonblocking");
    let setup = Setup {
        want,
        single_issuer: false,
        fixed_buffers: 0,
    };
    let mut q = Queue::new(Owned::with_rings(8, 1024, ring_pages(4)), setup).expect("queue");
    let g = q.add_ring(4).expect("a ring in the store's pages");
    assert!(q.add_ring(4).is_err(), "the store has one page");
    let mut bufs = [[0u8; 32]; 4];
    for (i, b) in bufs.iter_mut().enumerate() {
        q.provide(g, i as u16, mem(b)).expect("provide");
    }
    q.push(
        Op::RecvMulti {
            fd: Fd::Raw(a.as_raw_fd()),
            group: g,
        },
        1,
    )
    .expect("recv");
    q.submit().expect("submit");
    UdpSocket::bind("127.0.0.1:0")
        .expect("bind")
        .send_to(b"ring", a.local_addr().expect("addr"))
        .expect("send");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(&bufs[c.buffer().expect("a buffer") as usize][..4], b"ring");
}

fn the_descriptor_signals_a_due_timeout(want: Want) {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut q = queue(want);
    q.push(
        Op::Timeout {
            after: Duration::from_millis(20),
        },
        1,
    )
    .expect("timeout");
    q.submit().expect("submit");
    {
        let fd = q.as_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let two = rustix::time::Timespec {
            tv_sec: 2,
            tv_nsec: 0,
        };
        assert_eq!(
            poll(&mut fds, Some(&two)).expect("poll"),
            1,
            "the descriptor never signalled"
        );
    }
    q.tick(Some(Duration::ZERO)).expect("tick");
    assert_eq!(q.pop().map(|c| c.result()), Some(Err(Errno::TIME)));
}

/// `SOL_UDP` options, absent from libc.
const UDP_SEGMENT: i32 = 103;
const UDP_GRO: i32 = 104;

fn a_segmented_send_comes_back_coalesced(want: Want) {
    let rx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    rx.set_nonblocking(true).expect("nonblocking");
    let tx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let mut page = Box::new(RingPage::ZERO);
    let mut bufs = vec![[0u8; 1024]; 2];
    let mut q = queue(want);
    let mut on = 1i32.to_ne_bytes();
    let gro = Op::SetSockOpt {
        fd: Fd::Raw(rx.as_raw_fd()),
        level: libc::SOL_UDP as u32,
        name: UDP_GRO as u32,
        value: mem(&mut on),
    };
    assert_eq!(one(&mut q, gro, 3).result(), Ok(0));
    // SAFETY: the page outlives the queue.
    let g = unsafe { q.add_group(page.0.as_mut_ptr().cast(), 2) }.expect("group");
    for (i, b) in bufs.iter_mut().enumerate() {
        q.provide(g, i as u16, mem(b)).expect("provide");
    }
    let control = 64;
    let op = Op::RecvMsgMulti {
        fd: Fd::Raw(rx.as_raw_fd()),
        group: g,
        name: true,
        control,
    };
    q.push(op, 1).expect("recvmsg");
    q.submit().expect("submit");
    let mut out: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
    let mut room = [0u8; 64];
    let seg = 100u16.to_ne_bytes();
    let n = crate::msg::encode(
        &mut room,
        crate::msg::Control {
            level: libc::SOL_UDP,
            kind: UDP_SEGMENT,
            data: &seg,
        },
    )
    .expect("fits");
    let send = Op::SendMsg {
        fd: Fd::Raw(tx.as_raw_fd()),
        mem: mem(&mut out),
        control: Mem::EMPTY,
        to: rx.local_addr().ok(),
    };
    q.push_control(send, &room[..n], 2).expect("send");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(250));
    let c = wait(&mut q, 1, &mut seen);
    let b = &bufs[c.buffer().expect("a buffer") as usize];
    let word = |at: usize| u32::from_ne_bytes(b[at..at + 4].try_into().expect("four bytes"));
    let head = 16 + ADDR_LEN + control as usize;
    assert_eq!(word(8), 250, "one receive holds the three segments");
    assert_eq!(&b[head..head + 250], &out[..]);
    let cmsg = crate::msg::first(&b[16 + ADDR_LEN..16 + ADDR_LEN + word(4) as usize])
        .expect("a control message");
    assert_eq!((cmsg.level, cmsg.kind), (libc::SOL_UDP, UDP_GRO));
    let size = i32::from_ne_bytes(cmsg.data[..4].try_into().expect("an int"));
    assert_eq!(size, 100, "the segment size");
    assert_eq!(addr_of(&b[16..16 + word(0) as usize]), tx.local_addr().ok());
    drop(q);
    drop(page);
}

fn a_removed_group_frees_its_index(want: Want) {
    let mut pages = [RingPage::ZERO, RingPage::ZERO];
    let mut q = queue(want);
    let [a, b] = &mut pages;
    // SAFETY: the pages outlive the queue.
    let g = unsafe { q.add_group(a.0.as_mut_ptr().cast(), 4) }.expect("group");
    q.remove_group(g).expect("removed");
    assert!(q.remove_group(g).is_err(), "removed once");
    let mut buf = [0u8; 8];
    assert!(
        q.provide(g, 0, mem(&mut buf)).is_err(),
        "nothing to offer to"
    );
    // SAFETY: as above.
    let again = unsafe { q.add_group(b.0.as_mut_ptr().cast(), 4) }.expect("group");
    assert_eq!(again, g, "the index is free again");
    for _ in 1..GROUPS {
        // SAFETY: as above, the kernel may share one ring's memory between groups here
        // only because nothing receives into them.
        unsafe { q.add_group(a.0.as_mut_ptr().cast(), 4) }.expect("room");
    }
    // SAFETY: as above.
    assert!(
        unsafe { q.add_group(a.0.as_mut_ptr().cast(), 4) }.is_err(),
        "full"
    );
    drop(q);
}

fn a_file_range_is_synced_and_advised(want: Want) {
    let p = tmp("range", want);
    std::fs::write(&p, vec![7u8; 1 << 16]).expect("write");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&p)
        .expect("open");
    let fd = Fd::Raw(f.as_raw_fd());
    let mut q = queue(want);
    let flags = libc::SYNC_FILE_RANGE_WRITE;
    let sync = Op::SyncFileRange {
        fd,
        at: 0,
        len: 1 << 16,
        flags,
    };
    assert_eq!(one(&mut q, sync, 1).result(), Ok(0));
    let drop_pages = Op::Fadvise {
        fd,
        at: 0,
        len: 0,
        advice: libc::POSIX_FADV_DONTNEED,
    };
    assert_eq!(one(&mut q, drop_pages, 2).result(), Ok(0));
    let bad = Op::Fadvise {
        fd,
        at: 0,
        len: 0,
        advice: 99,
    };
    assert_eq!(one(&mut q, bad, 3).result(), Err(Errno::INVAL));
    let closed = Op::SyncFileRange {
        fd: Fd::Raw(-1),
        at: 0,
        len: 0,
        flags,
    };
    assert_eq!(one(&mut q, closed, 4).result(), Err(Errno::BADF));
    drop(f);
    let _ = std::fs::remove_file(&p);
}

fn a_symlink_is_stated_itself_unless_followed(want: Want) {
    let (target, link) = (tmp("target", want), tmp("link", want));
    std::fs::write(&target, b"twelve bytes").expect("write");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let mut q = queue(want);
    let mut pl = path(&link);
    let mut out = [0u8; STATX_LEN];
    for (follow, kind) in [(false, libc::S_IFLNK), (true, libc::S_IFREG)] {
        assert_eq!(
            one(&mut q, statx(&mut pl, &mut out, follow), 1).result(),
            Ok(0)
        );
        let mode = statx_of(&out).expect("statx").stx_mode as u32;
        assert_eq!(mode & libc::S_IFMT, kind, "follow {follow}");
    }
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&target);
}

fn a_burst_on_a_repeating_receive_comes_in_one_reap(want: Want) {
    let rx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    rx.set_nonblocking(true).expect("nonblocking");
    let tx = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let mut page = Box::new(RingPage::ZERO);
    let mut bufs = vec![[0u8; 64]; 8];
    let mut q = queue(want);
    // SAFETY: the page outlives the queue.
    let g = unsafe { q.add_group(page.0.as_mut_ptr().cast(), 8) }.expect("group");
    for (i, b) in bufs.iter_mut().enumerate() {
        q.provide(g, i as u16, mem(b)).expect("provide");
    }
    let op = Op::RecvMulti {
        fd: Fd::Raw(rx.as_raw_fd()),
        group: g,
    };
    q.push(op, 1).expect("recv");
    q.submit().expect("submit");
    for i in 0..5u8 {
        tx.send_to(&[i], rx.local_addr().expect("addr"))
            .expect("send");
    }
    std::thread::sleep(Duration::from_millis(20));
    q.tick(Some(Duration::ZERO)).expect("tick");
    let mut got = [Cqe::EMPTY; 16];
    let n = q.reap(&mut got);
    assert_eq!(n, 5, "the five waiting datagrams in one reap");
    assert!(got[..n].iter().all(|c| c.more() && c.buffer().is_some()));
    drop(q);
    drop(page);
}

mod review;
mod review2;
mod review3;
mod review4;
mod review5;
