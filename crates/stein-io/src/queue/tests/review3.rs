//! Regressions for the queue findings of the third unsafe review.

use super::*;
use std::net::TcpStream;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;

fn setup(want: Want) -> Setup {
    Setup {
        want,
        ..Setup::default()
    }
}

/// Whether `fd` is an open number.
fn is_open(fd: RawFd) -> bool {
    // SAFETY: only asks whether the number is open, no file is touched.
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) >= 0
}

/// A number no file has.
fn unused_fd() -> RawFd {
    (1 << 18..).find(|fd| !is_open(*fd)).expect("a free number")
}

/// Finding 1, a cancelled close was closed again by the queue. A close is never
/// cancelled now.
fn a_close_is_never_cancelled(want: Want) {
    let mut q = queue(want);
    let f = std::fs::File::open("/dev/null").expect("file");
    let key = q.close(OwnedFd::from(f), 1).expect("close");
    assert!(!q.cancel(key), "{want:?} a close took a cancel");
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(c.res, 0, "{want:?}");
}

/// Finding 1, a ring close ending in `ECANCELED` was closed again, though a `->flush`
/// may return it after the kernel took the number out of the table, and another open may
/// have reused it. A close never joins a chain now, so it is never cancelled. Behind a
/// failed link a nop stands in for it and the close runs once the nop lands, so it
/// neither leaks nor closes twice.
#[test]
fn a_ring_close_behind_a_failed_link_runs_once() {
    let Ok(mut q) = Queue::new(Owned::new(8, 64), setup(Want::Uring)) else {
        return;
    };
    let (rx, tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    let mut buf = [0u8; 8];
    q.link();
    let read = Op::Read {
        fd: Fd::Raw(unused_fd()),
        mem: mem(&mut buf),
        at: 0,
    };
    q.push(read, 1).expect("read");
    q.close(tx, 2).expect("close");
    q.submit().expect("submit");
    let mut seen = Vec::new();
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Err(Errno::BADF));
    assert_eq!(wait(&mut q, 2, &mut seen).result(), Ok(0));
    // The write end is gone, so the read end sees its end.
    assert_eq!(rustix::io::read(&rx, &mut buf), Ok(0));
}

/// Finding 4, a tick of `Duration::MAX` made a negative second and failed on epoll.
fn a_tick_of_any_length_waits(want: Want) {
    let mut q = queue(want);
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
    let poll = Op::Poll {
        fd: Fd::Raw(r.as_raw_fd()),
        events: libc::POLLIN as u32,
        multi: false,
    };
    q.push(poll, 1).expect("poll");
    q.submit().expect("submit");
    rustix::io::write(&w, b"x").expect("write");
    let start = Instant::now();
    while q.pop().is_none() {
        assert!(start.elapsed() < LIMIT, "{want:?} the poll never fired");
        let t = q.tick(Some(Duration::MAX));
        assert!(t.is_ok(), "{want:?} tick(Duration::MAX) failed: {t:?}");
    }
}

/// Finding 4, a timeout past what the kernel reads as a positive second ended with
/// `EINVAL` on the ring, where epoll waited. Both wait now.
fn a_timeout_of_any_length_waits(want: Want) {
    let mut q = queue(want);
    let key = q
        .push(
            Op::Timeout {
                after: Duration::MAX,
            },
            1,
        )
        .expect("timeout");
    q.submit().expect("submit");
    q.tick(Some(Duration::from_millis(20))).expect("tick");
    let early = q.pop();
    assert!(early.is_none(), "{want:?} ended at once, {early:?}");
    assert!(q.cancel(key));
    let c = wait(&mut q, 1, &mut Vec::new());
    assert_eq!(c.result(), Err(Errno::CANCELED), "{want:?}");
}

/// Finding 4, lengths over `MAX_RW_COUNT` gave negative results and an overflow in
/// the epoll receive of the whole. Both backends refuse them now.
fn lengths_past_max_rw_count_are_refused(want: Want) {
    let mut q = queue(want);
    let mut buf = [0u8; 8];
    // SAFETY: refused before the queue keeps it, nothing reads or writes past `buf`.
    let huge = unsafe { Mem::raw(buf.as_mut_ptr(), MAX_RW_COUNT as usize + 1) };
    let fd = Fd::Raw(unused_fd());
    let ops = [
        Op::Read {
            fd,
            mem: huge,
            at: 0,
        },
        Op::Write {
            fd,
            mem: huge,
            at: POSITION,
        },
        Op::Recv { fd, mem: huge },
        Op::RecvAll { fd, mem: huge },
    ];
    for (i, op) in ops.into_iter().enumerate() {
        let r = q.push(op, i as u64);
        assert!(
            matches!(r, Err(IoError::TooLong { max, .. }) if max == MAX_RW_COUNT as usize),
            "{want:?} {op:?} gave {r:?}"
        );
    }
    assert_eq!(q.in_flight(), 0);
    let small = Op::Read {
        fd,
        mem: mem(&mut buf),
        at: 0,
    };
    assert_eq!(one(&mut q, small, 9).result(), Err(Errno::BADF));
}

/// Finding 4, any raw number grew the epoll watch table to cover it, gigabytes for a
/// bogus one. Only an open descriptor grows it now.
#[test]
fn a_bogus_descriptor_grows_no_watch_table() {
    let mut q = Queue::new(Owned::new(4, 16), setup(Want::Epoll)).expect("queue");
    let mut buf = [0u8; 8];
    let bogus = Op::Recv {
        fd: Fd::Raw(unused_fd()),
        mem: mem(&mut buf),
    };
    assert_eq!(one(&mut q, bogus, 1).result(), Err(Errno::BADF));
    assert_eq!(q.watches.len(), 16, "the table grew for a closed number");
}

/// Finding 4, a key generation wrapped after 2^32 uses of a slot, so a key that old
/// named the new op. The slot retires instead.
fn a_slot_retires_when_its_generation_runs_out(want: Want) {
    let mut q = Queue::new(Owned::new(1, 16), setup(want)).expect("queue");
    q.slots[0].generation = u32::MAX;
    let key = q.push(Op::Nop, 1).expect("nop");
    assert_eq!(key.generation(), u32::MAX);
    assert_eq!(wait(&mut q, 1, &mut Vec::new()).res, 0);
    assert_eq!(q.room(), 0, "{want:?} a retired slot counted as room");
    let again = q.push(Op::Nop, 2);
    assert!(
        matches!(again, Err(IoError::OpsFull)),
        "{want:?} the slot came back, {again:?}"
    );
    assert!(!q.cancel(key), "{want:?} the old key still names something");
}

/// Finding 4, `path_clear` skipped a removed group, whose op in flight may still hold
/// its buffers. They count as held until the op ends.
fn a_removed_group_keeps_its_buffers_busy(want: Want) {
    let mut q = Queue::new(Owned::with_rings(8, 64, 1), setup(want)).expect("queue");
    let slab: &'static mut [u8] = Box::leak(vec![0u8; 256].into_boxed_slice());
    // SAFETY: leaked, valid for the process.
    let r = unsafe { q.register(slab.as_mut_ptr(), slab.len(), false) }.expect("region");
    let g = q.add_ring(2).expect("ring");
    q.provide(g, 0, q.mem(r, 64, 64).expect("mem"))
        .expect("provide");
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let multi = Op::RecvMulti {
        fd: Fd::Raw(a.as_raw_fd()),
        group: g,
    };
    let key = q.push(multi, 1).expect("recv");
    q.submit().expect("submit");
    q.remove_group(g).expect("removed");
    let path = q.mem(r, 64, 8).expect("path");
    let open = Op::Open {
        dir: crate::CWD,
        path,
        flags: OFlags::RDONLY,
        mode: Mode::empty(),
    };
    let r2 = q.push(open, 2);
    assert!(
        matches!(r2, Err(IoError::RegionBusy { at: 64, .. })),
        "{want:?} a path over a removed group's buffer went, {r2:?}"
    );
    assert!(q.unregister(r).is_err(), "{want:?}");
    q.cancel(key);
    let mut seen = Vec::new();
    while wait(&mut q, 1, &mut seen).more() {}
    q.unregister(r).expect("free once the op ended");
}

/// Finding 4, buffers came back by ring position, so one handed back out of ring order
/// freed the wrong range. They are held by bid now, and a held bid is refused again.
fn a_held_bid_is_not_provided_twice(want: Want) {
    let mut bufs = [[0u8; 64]; 3];
    let mut q = Queue::new(Owned::with_rings(8, 64, 1), setup(want)).expect("queue");
    let g = q.add_ring(4).expect("ring");
    q.provide(g, 5, mem(&mut bufs[0])).expect("provide");
    let twice = q.provide(g, 5, mem(&mut bufs[1]));
    assert!(
        matches!(twice, Err(IoError::Loaned(5))),
        "{want:?} {twice:?}"
    );
    q.provide(g, 6, mem(&mut bufs[2])).expect("another bid");
    drop(q);
}

/// Finding 3, an accept on a blocking listener blocked the whole epoll loop.
fn an_accept_on_a_blocking_listener_waits(want: Want) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mut q = queue(want);
    let accept = Op::Accept {
        fd: Fd::Raw(l.as_raw_fd()),
        multi: false,
    };
    q.push(accept, 1).expect("accept");
    q.submit().expect("submit");
    let _peer = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let c = wait(&mut q, 1, &mut Vec::new());
    let fd = c.result().expect("accepted") as RawFd;
    // SAFETY: fresh from the accept, owned by nobody else.
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
}

/// Finding 3, a read or write at the position on a blocking pipe blocked the whole
/// epoll loop. Both wait for the pipe now.
fn stream_io_on_a_blocking_pipe_waits(want: Want) {
    let (r, w) = rustix::pipe::pipe().expect("pipe");
    let mut q = queue(want);
    let mut got = [0u8; 8];
    let read = Op::Read {
        fd: Fd::Raw(r.as_raw_fd()),
        mem: mem(&mut got),
        at: POSITION,
    };
    q.push(read, 1).expect("read");
    q.submit().expect("submit");
    let mut seen = Vec::new();
    // Fill the pipe without blocking the test, then let the write wait for room.
    let flags = rustix::fs::fcntl_getfl(&w).expect("flags");
    rustix::fs::fcntl_setfl(&w, flags | OFlags::NONBLOCK).expect("nonblock");
    let chunk = [1u8; 4096];
    while rustix::io::write(&w, &chunk).is_ok() {}
    rustix::fs::fcntl_setfl(&w, flags).expect("blocking again");
    assert_eq!(wait(&mut q, 1, &mut seen).result(), Ok(8));
    let mut out = [2u8; 64];
    let write = Op::Write {
        fd: Fd::Raw(w.as_raw_fd()),
        mem: mem(&mut out),
        at: POSITION,
    };
    q.push(write, 2).expect("write");
    q.submit().expect("submit");
    let mut sink = vec![0u8; 1 << 20];
    let n = rustix::io::read(r.as_fd(), &mut sink).expect("drain");
    assert!(n > 0);
    let c = wait(&mut q, 2, &mut seen);
    assert!(c.res > 0, "{want:?} {c:?}");
}

both!(
    a_close_is_never_cancelled,
    a_tick_of_any_length_waits,
    a_timeout_of_any_length_waits,
    lengths_past_max_rw_count_are_refused,
    a_slot_retires_when_its_generation_runs_out,
    a_removed_group_keeps_its_buffers_busy,
    a_held_bid_is_not_provided_twice,
    an_accept_on_a_blocking_listener_waits,
    stream_io_on_a_blocking_pipe_waits,
);
