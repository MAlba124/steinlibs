//! Zero steady-state allocations per op on the raw queue, both backends.
//! File I/O, datagrams, timeout and cancel, a chain, multishot recv.
//! Own binary since the counter is global. Counts this thread only, after warmup rounds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::UdpSocket;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use stein_arena::counting;
use stein_io::queue::{self, Cqe, Fd, Mem, Op, Owned, Queue, Setup, Want};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

const ROUNDS: usize = 200;

#[repr(C, align(4096))]
struct Page([u8; 4096]);

struct Rig {
    q: Queue,
    file: std::fs::File,
    a: UdpSocket,
    b: UdpSocket,
    bufs: Box<[[u8; 256]]>,
    group: queue::Group,
    multi: UdpSocket,
    _page: Box<Page>,
}

fn mem(b: &mut [u8]) -> Mem {
    // SAFETY: the rig keeps buffers alive past the queue.
    unsafe { Mem::raw(b.as_mut_ptr(), b.len()) }
}

fn wait(q: &mut Queue, user: u64) -> Cqe {
    let start = Instant::now();
    loop {
        if let Some(c) = q.pop() {
            assert_eq!(c.user, user, "another completed first");
            return c;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{user} never completed"
        );
        q.tick(Some(Duration::from_millis(20))).expect("tick");
    }
}

fn rig(want: Want) -> Rig {
    let setup = Setup {
        want,
        single_issuer: true,
        fixed_buffers: 0,
    };
    let mut q = Queue::new(Owned::new(16, 1024), setup).expect("queue");
    let path =
        std::env::temp_dir().join(format!("stein-queue-alloc-{want:?}-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("file");
    let _ = std::fs::remove_file(&path);
    let udp = || {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind");
        s.set_nonblocking(true).expect("nonblocking");
        s
    };
    let mut page = Box::new(Page([0; 4096]));
    // SAFETY: the page lives in the rig, which drops the queue first.
    let group = unsafe { q.add_group(page.0.as_mut_ptr().cast(), 4) }.expect("group");
    let mut bufs = vec![[0u8; 256]; 8].into_boxed_slice();
    for i in 4..8 {
        q.provide(group, i as u16, mem(&mut bufs[i]))
            .expect("provide");
    }
    let multi = udp();
    let fd = Fd::Raw(multi.as_raw_fd());
    q.push(Op::RecvMulti { fd, group }, 100).expect("armed");
    q.submit().expect("submit");
    Rig {
        q,
        file,
        a: udp(),
        b: udp(),
        bufs,
        group,
        multi,
        _page: page,
    }
}

fn round(r: &mut Rig, n: usize) {
    let fd = Fd::Raw(r.file.as_raw_fd());
    r.bufs[0][..8].copy_from_slice(&(n as u64).to_ne_bytes());
    let w = Op::Write {
        fd,
        mem: mem(&mut r.bufs[0][..8]),
        at: 0,
    };
    r.q.push(w, 1).expect("write");
    assert_eq!(wait(&mut r.q, 1).res, 8);
    r.q.push(
        Op::Read {
            fd,
            mem: mem(&mut r.bufs[1][..8]),
            at: 0,
        },
        2,
    )
    .expect("read");
    assert_eq!(wait(&mut r.q, 2).res, 8);
    assert_eq!(r.bufs[1][..8], (n as u64).to_ne_bytes());

    let (a, b): (RawFd, RawFd) = (r.a.as_raw_fd(), r.b.as_raw_fd());
    let to = r.b.local_addr().expect("addr");
    let recv = Op::RecvFrom {
        fd: Fd::Raw(b),
        mem: mem(&mut r.bufs[2]),
    };
    r.q.push(recv, 3).expect("recv");
    r.q.push(
        Op::SendTo {
            fd: Fd::Raw(a),
            mem: mem(&mut r.bufs[0][..8]),
            to,
        },
        4,
    )
    .expect("send");
    let mut got = [wait_any(&mut r.q), wait_any(&mut r.q)];
    got.sort_by_key(|c| c.user);
    assert_eq!((got[0].res, got[1].res), (8, 8));
    assert_eq!(got[0].addr, r.a.local_addr().ok());

    let t =
        r.q.push(
            Op::Timeout {
                after: Duration::from_secs(5),
            },
            5,
        )
        .expect("timeout");
    r.q.submit().expect("submit");
    assert!(r.q.cancel(t));
    assert_eq!(wait(&mut r.q, 5).res, -libc::ECANCELED);

    // An entry apart and a cancel wait behind a chain in their slots.
    let after = Duration::from_secs(5);
    let t = r.q.push(Op::Timeout { after }, 6).expect("timeout");
    r.q.link();
    r.q.push(Op::Nop, 7).expect("head");
    r.q.push_apart(Op::Nop, 8).expect("apart");
    assert!(r.q.cancel(t));
    r.q.push(Op::Nop, 9).expect("tail");
    let mut got = [(); 4].map(|_| wait_any(&mut r.q));
    got.sort_by_key(|c| c.user);
    let got = got.map(|c| (c.user, c.res));
    assert_eq!(got, [(6, -libc::ECANCELED), (7, 0), (8, 0), (9, 0)]);

    let target = r.multi.local_addr().expect("addr");
    r.a.send_to(b"multi", target).expect("send");
    let c = wait(&mut r.q, 100);
    assert!(c.more(), "the repeating receive stays armed");
    let bid = c.buffer().expect("a buffer") as usize;
    r.q.provide(r.group, bid as u16, mem(&mut r.bufs[bid]))
        .expect("give back");
}

fn wait_any(q: &mut Queue) -> Cqe {
    let start = Instant::now();
    loop {
        if let Some(c) = q.pop() {
            return c;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "nothing completed"
        );
        q.tick(Some(Duration::from_millis(20))).expect("tick");
    }
}

#[test]
fn the_queue_does_not_allocate() {
    let mut rigs = [rig(Want::Epoll), rig(Want::Uring)];
    for r in &mut rigs {
        round(r, 0);
        round(r, 1);
    }
    counting::arm();
    for r in &mut rigs {
        let before = counting::thread_count();
        for n in 0..ROUNDS {
            round(r, n);
        }
        let made = counting::thread_count() - before;
        assert_eq!(made, 0, "{:?} allocated {made} times", r.q.backend());
        assert_eq!(r.q.in_flight(), 1, "the repeating receive and nothing else");
    }
}
