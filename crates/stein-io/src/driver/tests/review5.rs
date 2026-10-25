//! Regressions for the driver findings of the fifth review. The driver's own entries
//! never join a caller's chain, and a buffer id names its own arena only.

use super::*;

/// A ring driver over `bufs` buffers, or none without a ring.
fn ring(bufs: usize) -> Option<Io> {
    let io = Io::with_backend(Storage::new(8, bufs), 64, Backend::Uring).ok()?;
    (io.backend() == Backend::Uring).then_some(io)
}

/// A number nothing has.
fn bad() -> RawFd {
    // SAFETY: `F_GETFD` only asks whether the number is open.
    (1 << 18..)
        .find(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } < 0)
        .expect("a free number")
}

/// Tick until a completion matches `f`. Others go to `seen`.
fn until(io: &mut Io, seen: &mut Vec<Completion>, f: impl Fn(&Completion) -> bool) -> Completion {
    let start = Instant::now();
    loop {
        if let Some(i) = seen.iter().position(&f) {
            return seen.swap_remove(i);
        }
        assert!(start.elapsed() < LIMIT, "it never came, saw {seen:?}");
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        while let Some(c) = io.next_completion() {
            seen.push(c);
        }
    }
}

/// The waker's poll joined an open chain and failed with it, which dropped the waker.
#[test]
fn a_wake_poll_does_not_fail_with_a_chain() {
    let Some(mut io) = ring(8) else {
        return;
    };
    io.chain();
    let sync = io.sync(bad(), true).expect("sync");
    let waker = io.waker().expect("waker");
    let after = io.timeout(Duration::ZERO).expect("timeout");
    let mut seen = Vec::new();
    let c = until(&mut io, &mut seen, |c| c.op == sync);
    assert_eq!(c.result, Err(Errno::BADF));
    let c = until(&mut io, &mut seen, |c| c.op == after);
    assert_eq!(c.result, Err(Errno::CANCELED), "the timeout left the chain");
    waker.wake();
    let c = until(&mut io, &mut seen, |c| c.kind == Kind::Wake);
    assert_eq!(c.result, Ok(0), "the wake poll failed with the chain");
}

/// The waker's poll joined an open chain and waited behind it.
#[test]
fn a_wake_poll_does_not_wait_behind_a_chain() {
    let Some(mut io) = ring(8) else {
        return;
    };
    let (gate_r, gate_w) = io.pipe().expect("gate");
    let (_r, w) = io.pipe().expect("pipe");
    io.chain();
    let head = io.ready(gate_r, false).expect("ready");
    let waker = io.waker().expect("waker");
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..4].copy_from_slice(b"late");
    let write = io.write(w, b, 4, APPEND).expect("write");
    waker.wake();
    let mut seen = Vec::new();
    let c = until(&mut io, &mut seen, |c| c.kind == Kind::Wake);
    assert_eq!(c.result, Ok(0));
    assert!(
        io.is_live(head) && io.is_live(write),
        "the write lost its link to the head"
    );
    let gate = io.fd(gate_w).expect("gate");
    rustix::io::write(gate, b"go").expect("open the gate");
    assert!(until(&mut io, &mut seen, |c| c.op == head).result.is_ok());
    assert_eq!(until(&mut io, &mut seen, |c| c.op == write).result, Ok(4));
}

/// A receive re-armed once a buffer freed joined an open chain and waited behind it.
#[test]
fn a_receive_rearm_does_not_wait_behind_a_chain() {
    // Under eight buffers, so no ring and the receive waits for a free one.
    let Some(mut io) = ring(4) else {
        return;
    };
    assert_eq!(io.provided_buffers(), 0);
    let sock = io.bind_udp("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = io.local_addr(sock).expect("addr");
    let mut held: Vec<BufId> = (0..4).map(|_| io.alloc().expect("buffer")).collect();
    let recv = io.recv(sock, true).expect("recv");
    let (gate_r, _gate_w) = io.pipe().expect("gate");
    io.chain();
    let head = io.ready(gate_r, false).expect("ready");
    assert!(io.release(held.pop().expect("one")));
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").expect("peer");
    peer.send_to(b"ping", addr).expect("send");
    let mut seen = Vec::new();
    let c = until(&mut io, &mut seen, |c| c.op == recv);
    assert_eq!(c.result, Ok(4));
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"ping");
    assert!(io.release(b));
    assert!(io.is_live(head));
    for b in held {
        assert!(io.release(b));
    }
}

/// An id named a buffer by number alone, so one of another driver reached the buffer of
/// the same number here.
fn check_an_id_of_another_driver_touches_neither_arena(back: Backend) {
    driver!(a, back, ops = 4, bufs = 4, size = 64);
    driver!(b, back, ops = 4, bufs = 4, size = 64);
    let theirs = a.alloc().expect("buffer");
    let mine = b.alloc().expect("buffer");
    assert_eq!(theirs.index(), mine.index(), "the same number in both");
    a.buf_mut(&theirs).fill(b'a');
    b.buf_mut(&mine).fill(b'b');
    assert!(b.buf_mut(&theirs).is_empty(), "{back:?} reached a buffer");
    assert!(b.bufs().bytes(&theirs).is_empty());
    assert_eq!(b.copy_range(&mine, 0, &theirs, 0, 8), 0);
    assert_eq!(b.copy_range(&theirs, 0, &mine, 0, 8), 0);
    let (in_a, in_b) = (a.available(), b.available());
    let r = b.send(0, theirs, 8);
    assert!(matches!(r, Err(IoError::NotHeld(_))), "{back:?} {r:?}");
    assert_eq!(b.in_flight(), 0, "{back:?} the send went out");
    let other = a.alloc().expect("buffer");
    assert!(!b.release(other), "{back:?} released a foreign id");
    assert_eq!(
        (a.available(), b.available()),
        (in_a - 1, in_b),
        "{back:?} an arena changed"
    );
    assert!(b.bufs().bytes(&mine).iter().all(|&x| x == b'b'));
    assert!(b.release(mine));
}

#[test]
fn an_id_of_another_driver_touches_neither_arena() {
    for b in backends() {
        check_an_id_of_another_driver_touches_neither_arena(b);
    }
}
