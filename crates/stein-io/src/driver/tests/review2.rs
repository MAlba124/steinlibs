//! Regressions for the driver findings of the second unsafe review.

use super::*;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared memory that says when it drops.
struct Flagged(Vec<u8>, Arc<AtomicBool>);

impl AsRef<[u8]> for Flagged {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Flagged {
    fn drop(&mut self) {
        self.1.store(true, Ordering::SeqCst);
    }
}

/// Large enough that the send is still in flight after a tick.
const HUGE: usize = 32 << 20;

/// Finding 1, a cancel of a stale or forged id dropped the memory of the live shared
/// send that had reused its slot.
fn check_a_stale_cancel_keeps_a_live_send_memory(back: Backend, forged: bool) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let conn = io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connected");
    assert_eq!(c.op, conn);
    let mut peer = l.accept().expect("accept").0;
    peer.set_read_timeout(Some(Duration::from_secs(1)))
        .expect("timeout");
    let dropped = Arc::new(AtomicBool::new(false));
    let data: Shared = Arc::new(Flagged(vec![7; HUGE], dropped.clone()));
    let send = io.send_shared(c.fd, data, 0, HUGE).expect("send");
    io.tick(Some(Duration::from_millis(20))).expect("tick");
    assert!(io.is_live(send), "{back:?} the send finished early");
    assert_eq!(send.slot(), conn.slot(), "the slot was not reused");
    let stale = if forged {
        OpId::from_user_data(u64::from(send.slot()) << 32)
    } else {
        conn
    };
    assert!(!io.cancel(stale), "{back:?} a stale cancel took");
    assert!(
        !dropped.load(Ordering::SeqCst),
        "{back:?} forged {forged}, a stale cancel dropped a live send's memory"
    );
    let reader = std::thread::spawn(move || {
        let mut got = vec![0u8; 1 << 16];
        while matches!(peer.read(&mut got), Ok(n) if n > 0) {}
    });
    let start = Instant::now();
    while io.is_live(send) && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(20))).expect("tick");
        while io.next_completion().is_some() {}
    }
    drop(io);
    reader.join().expect("reader");
    assert!(dropped.load(Ordering::SeqCst), "the driver kept the memory");
}

/// Tick until only `left` ops are in flight, dropping completions.
fn settle(io: &mut Io, left: usize) {
    let start = Instant::now();
    while io.in_flight() > left && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        while io.next_completion().is_some() {}
    }
    while io.next_completion().is_some() {}
}

/// Finding 2, a provided ring buffer sent on was marked free but never offered to the
/// ring again, so receives starved after as many rounds as the ring has buffers.
fn check_a_ring_buffer_sent_on_goes_back_to_the_ring(back: Backend) {
    let (mine, mut theirs) = UnixStream::pair().expect("pair");
    theirs.set_read_timeout(Some(LIMIT)).expect("timeout");
    driver!(io, back, ops = 16, bufs = 16, size = 64);
    let ring = io.provided_buffers();
    assert!(ring > 0, "{back:?} no provided ring");
    let fd = io.own(OwnedFd::from(mine));
    let before = io.available();
    let rx = io.recv(fd, true).expect("recv");
    for round in 0..2 * ring {
        theirs.write_all(b"ping").expect("write");
        let c = loop {
            let Some(c) = pump(&mut io) else {
                panic!(
                    "{back:?} round {round} starved, ring {ring}, available {} of {before}",
                    io.available()
                );
            };
            if c.op == rx {
                break c;
            }
            assert_eq!(c.kind, Kind::Send);
        };
        let b = c.buf.expect("buffer");
        io.send(fd, b.id, b.len).expect("send");
        io.flush().expect("flush");
        let mut got = [0u8; 4];
        theirs.read_exact(&mut got).expect("echo");
    }
    settle(&mut io, 1);
    assert_eq!(io.available(), before, "{back:?} buffers lost");
}

/// Finding 2, a failed send of a ring buffer frees it through `fail_read`, which must
/// offer it to the ring again too.
fn check_a_failed_send_of_a_ring_buffer_returns_it(back: Backend) {
    let (mine, mut theirs) = UnixStream::pair().expect("pair");
    driver!(io, back, ops = 16, bufs = 16, size = 64);
    let fd = io.own(OwnedFd::from(mine));
    let before = io.available();
    let rx = io.recv(fd, true).expect("recv");
    theirs.write_all(b"ping").expect("write");
    let c = pump(&mut io).expect("received");
    assert_eq!(c.op, rx);
    let b = c.buf.expect("buffer");
    assert!(io.bufs().in_ring(&b.id), "{back:?} not a ring buffer");
    drop(theirs);
    let tx = io.send(fd, b.id, b.len).expect("send");
    let start = Instant::now();
    let mut failed = None;
    while failed.is_none() && start.elapsed() < LIMIT {
        match io.next_completion() {
            Some(c) if c.op == tx => failed = Some(c.result),
            Some(_) => {}
            None => drop(io.tick(Some(Duration::from_millis(10))).expect("tick")),
        }
    }
    assert!(
        matches!(failed, Some(Err(_))),
        "{back:?} the send to a closed peer gave {failed:?}"
    );
    settle(&mut io, 0);
    assert_eq!(
        io.available(),
        before,
        "{back:?} the failed send lost its buffer"
    );
}

/// Reap a completion from the queue, then take every free slot with a long timeout
/// before the driver handles it, so a re-arm finds the queue full, the reaped op's own
/// slot included.
fn land_full(io: &mut Io) {
    let start = Instant::now();
    let mut got = [Cqe::EMPTY; 8];
    let n = loop {
        let n = io.q.reap(&mut got);
        if n > 0 {
            break n;
        }
        assert!(start.elapsed() < LIMIT, "nothing landed");
        io.q.tick(Some(Duration::from_millis(10))).expect("tick");
    };
    let wait = queue::Op::Timeout {
        after: Duration::from_secs(3600),
    };
    while io.q.push(wait, CLOSING + 1).is_ok() {}
    for c in &got[..n] {
        io.complete(*c);
    }
}

/// Every completion queued now.
fn drain(io: &mut Io) -> Vec<Completion> {
    std::iter::from_fn(|| io.next_completion()).collect()
}

/// Finding 6, a repeating accept whose re-arm failed to submit dropped the op without
/// a final completion.
fn check_a_failed_accept_rearm_ends_with_an_error(back: Backend) {
    let (l, addr) = listener();
    l.set_nonblocking(true).expect("nonblocking");
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let lfd = io.own(OwnedFd::from(l));
    let id = io.accept(lfd, true).expect("accept");
    io.flush().expect("flush");
    let _peer = std::net::TcpStream::connect(addr).expect("connect");
    land_full(&mut io);
    let got = drain(&mut io);
    assert!(
        got.iter().any(|c| c.op == id && c.more && c.result.is_ok()),
        "{back:?} no connection: {got:?}"
    );
    assert!(
        got.iter()
            .any(|c| c.op == id && !c.more && c.result.is_err()),
        "{back:?} a failed re-arm ended without a final completion: {got:?}"
    );
    assert!(!io.is_live(id));
}

/// Finding 6, as above for a repeating receive on a buffer of its own.
fn check_a_failed_receive_rearm_ends_with_an_error(back: Backend) {
    let (mut a, b) = UnixStream::pair().expect("pair");
    // Under eight buffers, so no ring and each delivery re-arms with a fresh buffer.
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    assert_eq!(io.provided_buffers(), 0);
    let fd = io.own(OwnedFd::from(b));
    let id = io.recv(fd, true).expect("recv");
    io.flush().expect("flush");
    a.write_all(b"x").expect("write");
    land_full(&mut io);
    let mut got = drain(&mut io);
    let first = got.iter().position(|c| c.op == id && c.more);
    let Some(buf) = first.and_then(|i| got[i].buf.take()) else {
        panic!("{back:?} no delivery: {got:?}");
    };
    assert!(
        got.iter()
            .any(|c| c.op == id && !c.more && c.result.is_err()),
        "{back:?} a failed re-arm ended without a final completion: {got:?}"
    );
    assert!(io.release(buf));
    assert_eq!(io.available(), 4, "{back:?} the re-arm buffer leaked");
}

/// Finding 4, a ring peer closed on a batching ring went to the ring, which refuses an
/// io_uring descriptor. It closes at once instead.
#[test]
fn a_ring_peer_closes_at_once_on_a_batching_ring() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    let mut target = Io::with_backend(Storage::new(4, 4), 64, Backend::Uring).expect("target");
    let mut io = Io::single_issuer(Storage::new(8, 8), 64).expect("driver");
    if !io.ring_close {
        return;
    }
    let p = io.peer(target.mailbox().expect("mailbox"));
    assert!(p.is_ring());
    assert!(io.close(p.fd()));
    // SAFETY: only asks whether the number is open, no file is touched.
    let open = unsafe { libc::fcntl(p.fd(), libc::F_GETFD) } >= 0;
    assert!(!open, "a ring peer waited for the batch");
}

#[test]
fn a_stale_cancel_keeps_a_live_send_memory() {
    for b in backends() {
        check_a_stale_cancel_keeps_a_live_send_memory(b, false);
    }
}

#[test]
fn a_forged_cancel_keeps_a_live_send_memory() {
    for b in backends() {
        check_a_stale_cancel_keeps_a_live_send_memory(b, true);
    }
}

#[test]
fn a_ring_buffer_sent_on_goes_back_to_the_ring() {
    for b in backends() {
        check_a_ring_buffer_sent_on_goes_back_to_the_ring(b);
    }
}

#[test]
fn a_failed_send_of_a_ring_buffer_returns_it() {
    for b in backends() {
        check_a_failed_send_of_a_ring_buffer_returns_it(b);
    }
}

#[test]
fn a_failed_accept_rearm_ends_with_an_error() {
    for b in backends() {
        check_a_failed_accept_rearm_ends_with_an_error(b);
    }
}

#[test]
fn a_failed_receive_rearm_ends_with_an_error() {
    for b in backends() {
        check_a_failed_receive_rearm_ends_with_an_error(b);
    }
}
