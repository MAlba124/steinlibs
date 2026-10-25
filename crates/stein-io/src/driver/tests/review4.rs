//! Regressions for the driver findings of the production review.

use super::*;

/// Whether the peer of a pipe's read end is gone.
fn hung_up(io: &Io, r: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd: r,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd on the stack, no wait.
    unsafe { libc::poll(&mut p, 1, 0) };
    io.fd(r).is_some() && p.revents & libc::POLLHUP != 0
}

/// Finding 1, a batched close behind a failed op of a chain was cancelled with it and
/// leaked its descriptor. It runs once the chain ends now.
#[test]
fn a_batched_close_behind_a_failed_chain_still_closes() {
    let Ok(mut io) = Io::single_issuer(Storage::new(8, 8), 64) else {
        return;
    };
    if io.backend() != Backend::Uring {
        return;
    }
    let (r, w) = io.pipe().expect("pipe");
    io.chain();
    // A number nothing has.
    let bad = (1 << 18..)
        .find(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } < 0)
        .expect("a free number");
    let sync = io.sync(bad, true).expect("sync");
    assert!(io.close(w));
    let c = pump(&mut io).expect("the sync ends");
    assert_eq!((c.op, c.result), (sync, Err(Errno::BADF)));
    let start = Instant::now();
    while !hung_up(&io, r) {
        assert!(start.elapsed() < LIMIT, "the write end never closed");
        io.tick(Some(Duration::from_millis(10))).expect("tick");
    }
    assert!(io.close(r));
}

/// Finding 3, a send refused for want of a slot handed the buffer back, though the docs
/// said the driver owns it, so callers that dropped it on `Err` leaked it.
fn check_a_send_without_a_slot_frees_its_buffer(back: Backend) {
    driver!(io, back, ops = 1, bufs = 4, size = 64);
    let _t = io.timeout(Duration::from_secs(60)).expect("timeout");
    let free = io.available();
    let b = io.alloc().expect("buffer");
    assert!(matches!(io.send(0, b, 4), Err(IoError::OpsFull)));
    assert_eq!(io.available(), free, "{back:?} the buffer stayed out");
}

/// Finding 3, the same for a send the queue refused after it took a slot.
fn check_a_send_the_queue_refuses_frees_its_buffer(back: Backend) {
    driver!(io, back, ops = 4, bufs = 4, size = 64);
    let hour = queue::Op::Timeout {
        after: Duration::from_secs(3600),
    };
    let mut fillers = Vec::new();
    while let Ok(k) = io.q.push(hour, CLOSING | 1) {
        fillers.push(k);
    }
    let free = io.available();
    let b = io.alloc().expect("buffer");
    let r = io.send(0, b, 4);
    assert!(matches!(r, Err(IoError::OpsFull)), "{back:?} {r:?}");
    assert_eq!(io.available(), free, "{back:?} the buffer stayed out");
    assert_eq!(io.in_flight(), 0, "{back:?} the op stayed");
    for k in fillers {
        io.q.cancel(k);
    }
}

/// Finding 3, a buffer the caller does not hold stays where it is on `NotHeld`. A lent
/// one can no longer be named, so one of another driver stands in.
fn check_a_send_of_a_lent_buffer_leaves_it_lent(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 4, bufs = 4, size = 64);
    driver!(other, back, ops = 4, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect").fd;
    let _peer = l.accept().expect("accept").0;
    let b = io.alloc().expect("buffer");
    let sent = io.send(fd, b, 4).expect("send");
    let theirs = other.alloc().expect("buffer");
    assert!(matches!(io.send(fd, theirs, 4), Err(IoError::NotHeld(_))));
    assert_eq!(other.available(), 3, "{back:?} the other arena changed");
    let c = pump(&mut io).expect("send completion");
    assert_eq!((c.op, c.result), (sent, Ok(4)));
    assert_eq!(io.available(), 4, "{back:?} the buffer went missing");
}

#[test]
fn a_send_without_a_slot_frees_its_buffer() {
    for b in backends() {
        check_a_send_without_a_slot_frees_its_buffer(b);
    }
}

#[test]
fn a_send_the_queue_refuses_frees_its_buffer() {
    for b in backends() {
        check_a_send_the_queue_refuses_frees_its_buffer(b);
    }
}

#[test]
fn a_send_of_a_lent_buffer_leaves_it_lent() {
    for b in backends() {
        check_a_send_of_a_lent_buffer_leaves_it_lent(b);
    }
}

/// A ring driver, plain or single issuer, or none without a ring.
fn ring(single: bool) -> Option<Io> {
    let store = Storage::new(8, 8);
    let io = if single {
        Io::single_issuer(store, 64).ok()?
    } else {
        Io::with_backend(store, 64, Backend::Uring).ok()?
    };
    (io.backend() == Backend::Uring).then_some(io)
}

/// Finding 4, a close ran while an op that joined a chain still named the fd. Such an op
/// may look the number up only when it starts, so it could act on a file that reused the
/// number. The close waits for that op's last completion now, cancelled or not.
#[test]
fn a_close_waits_for_a_chained_op_on_its_fd() {
    for single in [false, true] {
        let Some(mut io) = ring(single) else {
            return;
        };
        let (gate_r, gate_w) = io.pipe().expect("gate");
        let (r, w) = io.pipe().expect("pipe");
        io.chain();
        let head = io.ready(gate_r, false).expect("ready");
        let b = io.alloc().expect("buffer");
        io.buf_mut(&b)[..4].copy_from_slice(b"late");
        io.write(w, b, 4, APPEND).expect("write");
        assert!(io.close(w));
        for _ in 0..5 {
            io.tick(Some(Duration::from_millis(10))).expect("tick");
            while io.next_completion().is_some() {}
        }
        assert!(
            !hung_up(&io, r),
            "single {single}, closed under a chained op"
        );
        let gate = io.fd(gate_w).expect("gate");
        rustix::io::write(gate, b"go").expect("open the gate");
        let start = Instant::now();
        let mut ended = false;
        while !hung_up(&io, r) {
            assert!(
                start.elapsed() < LIMIT,
                "single {single}, the close never ran"
            );
            io.tick(Some(Duration::from_millis(10))).expect("tick");
            while let Some(c) = io.next_completion() {
                ended |= c.op == head;
            }
        }
        assert!(ended, "single {single}, closed before the chain went on");
    }
}

/// Finding 4, a plain ring close dropped the fd at once, while an entry naming it still
/// waited to be submitted, so it found whatever took the number next. The close goes in
/// the batch behind such entries now.
#[test]
fn a_close_never_frees_a_number_an_unsubmitted_entry_names() {
    let Some(mut io) = ring(false) else {
        return;
    };
    let (_r, w) = io.pipe().expect("pipe");
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..5].copy_from_slice(b"stray");
    io.write(w, b, 5, APPEND).expect("write");
    assert!(io.close(w));
    let p = std::env::temp_dir().join(format!("stein-io-reuse-{}", std::process::id()));
    let f = std::fs::File::create(&p).expect("file");
    for _ in 0..5 {
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        while io.next_completion().is_some() {}
    }
    let len = f.metadata().expect("meta").len();
    let _ = std::fs::remove_file(&p);
    assert_eq!(len, 0, "the write reached a file that reused the number");
    assert_ne!(
        f.as_raw_fd(),
        w,
        "the number was free before the write went out"
    );
}
