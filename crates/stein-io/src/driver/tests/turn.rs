//! `stat_opened`, batched close, and `pending`.

use super::*;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Epoll,
    /// Plain ring, closes synchronously.
    Ring,
    /// `SINGLE_ISSUER` and `DEFER_TASKRUN`.
    Core,
}

const BUFS: usize = 16;

fn start(how: Start, ops: usize) -> Io {
    let store = Storage::new(ops, BUFS);
    let io = match how {
        Start::Epoll => Io::with_backend(store, 512, Backend::Epoll),
        Start::Ring => Io::with_backend(store, 512, Backend::Uring),
        Start::Core => Io::single_issuer(store, 512),
    }
    .expect("driver");
    if how == Start::Core {
        assert_eq!(io.ring_flags(), RingFlags::Deferred, "not a deferred ring");
    }
    io
}

fn starts() -> Vec<Start> {
    let mut v = vec![Start::Epoll];
    if backends().contains(&Backend::Uring) {
        v.extend([Start::Ring, Start::Core]);
    }
    v
}

fn scratch(name: &str, how: Start, content: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("stein-io-{}-{name}-{how:?}", std::process::id()));
    std::fs::write(&p, content).expect("write scratch");
    p
}

fn free(io: &Io) -> usize {
    io.available()
}

/// Pipe with a driver-owned write end. Nonblocking read end detects its close.
struct Tell {
    w: RawFd,
    r: OwnedFd,
}

impl Tell {
    fn new(io: &mut Io) -> Tell {
        let flags = rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK;
        let (r, w) = rustix::pipe::pipe_with(flags).expect("pipe");
        Tell { w: io.own(w), r }
    }

    fn closed(&self) -> bool {
        let mut b = [0u8; 1];
        match rustix::io::read(&self.r, &mut b) {
            Ok(0) => true,
            Err(Errno::AGAIN) => false,
            other => panic!("the pipe read {other:?}"),
        }
    }
}

fn turn(io: &mut Io) {
    io.tick(Some(Duration::ZERO)).expect("tick");
}

#[test]
fn a_stat_rides_its_open() {
    for how in starts() {
        let p = scratch("ride", how, b"seven b");
        let mut io = start(how, 8);
        let before = free(&io);
        let open = io.open(&path_bytes(&p)).expect("open");
        let stat = io.stat_opened(open).expect("stat");
        // On epoll both already landed.
        assert_eq!(io.in_flight(), 2 * usize::from(how != Start::Epoll));

        let c = pump(&mut io).expect("open completion");
        assert_eq!((c.op, c.kind), (open, Kind::Open), "{how:?}");
        let fd = c.result.expect("the open failed") as RawFd;
        // No tick between.
        let c = io.next_completion().expect("the stat came with the open");
        assert_eq!((c.op, c.kind), (stat, Kind::Stat), "{how:?}");
        assert_eq!(c.result, Ok(crate::file::META_LEN));
        let b = c.buf.expect("a stat delivers a buffer");
        let m = crate::file::meta(io.filled(&b)).expect("meta");
        assert!(m.is_regular());
        assert_eq!(m.size, 7);
        assert!(m.mtime_sec > 0);
        io.release(b);

        assert!(io.close(fd));
        turn(&mut io);
        assert_eq!(io.in_flight(), 0, "{how:?}");
        assert_eq!(free(&io), before, "{how:?} leaked a buffer");
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_stat_of_a_directory_says_so() {
    for how in starts() {
        let mut io = start(how, 8);
        let open = io.open(&path_bytes(&std::env::temp_dir())).expect("open");
        let stat = io.stat_opened(open).expect("stat");
        let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;
        let c = io.next_completion().expect("stat");
        assert_eq!(c.op, stat);
        let b = c.buf.expect("buffer");
        assert!(crate::file::meta(io.filled(&b)).expect("meta").is_dir());
        io.release(b);
        assert!(io.close(fd));
    }
}

#[test]
fn a_failed_open_fails_its_stat_the_same_way() {
    for how in starts() {
        let mut io = start(how, 8);
        let before = free(&io);
        let open = io.open(b"/definitely/not/here").expect("open");
        let stat = io.stat_opened(open).expect("stat");
        let c = pump(&mut io).expect("open completion");
        assert_eq!((c.op, c.result), (open, Err(Errno::NOENT)), "{how:?}");
        let c = io.next_completion().expect("the stat came with the open");
        assert_eq!((c.op, c.result), (stat, Err(Errno::NOENT)), "{how:?}");
        assert!(c.buf.is_none());
        assert_eq!(io.in_flight(), 0);
        assert_eq!(free(&io), before, "{how:?} leaked a buffer");
    }
}

#[test]
fn a_stat_is_of_the_file_the_open_got() {
    for how in [Start::Ring, Start::Core] {
        if !starts().contains(&how) {
            continue;
        }
        let p = scratch("swap", how, b"old");
        let next = scratch("swap-next", how, b"the new one");
        let mut io = start(how, 8);
        let open = io.open(&path_bytes(&p)).expect("open");
        io.stat_opened(open).expect("stat");
        // Swap the name before submission.
        std::fs::rename(&next, &p).expect("rename");
        let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;
        let c = io.next_completion().expect("stat");
        let b = c.buf.expect("buffer");
        assert_eq!(crate::file::meta(io.filled(&b)).expect("meta").size, 11);
        io.release(b);
        io.read_at(fd, 0).expect("read");
        let c = pump(&mut io).expect("read");
        let b = c.buf.expect("buffer");
        assert_eq!(io.filled(&b), b"the new one");
        io.release(b);
        assert!(io.close(fd));
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_stat_asked_after_the_open_landed_is_answered_at_once() {
    for how in starts() {
        let p = scratch("late", how, b"12345");
        let mut io = start(how, 8);
        // Prime the negative dentry so the next open fails inline.
        io.open(b"/definitely/not/here").expect("open");
        pump(&mut io).expect("open completion");
        let open = io.open(&path_bytes(&p)).expect("open");
        let gone = io.open(b"/definitely/not/here").expect("open");
        turn(&mut io);
        let landed = |id| io.ops.get(id).is_some_and(|o| o.done);
        assert!(landed(open) && landed(gone), "{how:?}");
        let stat = io.stat_opened(open).expect("stat");
        let none = io.stat_opened(gone).expect("stat");
        let mut got: Vec<Completion> = (0..4)
            .map(|_| io.next_completion().expect("four completions, no tick"))
            .collect();
        let mut of = |id| {
            let i = got.iter().position(|c| c.op == id).expect("its completion");
            got.swap_remove(i)
        };
        let b = of(stat).buf.expect("buffer");
        let size = crate::file::meta(io.filled(&b)).expect("meta").size;
        io.release(b);
        assert_eq!(size, 5, "{how:?}");
        assert_eq!(of(none).result, Err(Errno::NOENT), "{how:?}");
        assert!(io.close(of(open).result.expect("opens") as RawFd));
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn only_an_open_can_be_waited_for_and_only_once() {
    for how in starts() {
        let p = scratch("once", how, b"x");
        let mut io = start(how, 8);
        let before = free(&io);
        let timer = io.timeout(Duration::from_secs(60)).expect("timer");
        assert!(matches!(io.stat_opened(timer), Err(IoError::NotOpening)));
        assert!(io.cancel(timer));
        assert!(matches!(io.stat_opened(timer), Err(IoError::NotOpening)));

        let open = io.open(&path_bytes(&p)).expect("open");
        io.stat_opened(open).expect("stat");
        assert!(matches!(io.stat_opened(open), Err(IoError::NotOpening)));
        let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;
        let c = io.next_completion().expect("stat");
        io.release(c.buf.expect("buffer"));
        assert!(io.close(fd));
        // Reap the cancelled timer on a ring.
        io.tick(Some(Duration::from_millis(20))).expect("tick");
        assert_eq!(free(&io), before, "{how:?} leaked a buffer");
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_full_table_refuses_the_stat_and_keeps_no_buffer() {
    for how in starts() {
        let p = scratch("full", how, b"x");
        let mut io = start(how, 1);
        let open = io.open(&path_bytes(&p)).expect("open");
        let held = free(&io);
        assert!(matches!(io.stat_opened(open), Err(IoError::OpsFull)));
        assert_eq!(free(&io), held, "{how:?} kept the stat's buffer");
        let fd = pump(&mut io).expect("open").result.expect("opens") as RawFd;
        assert!(io.close(fd));
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_cancelled_stat_never_reports_and_frees_its_buffer_at_once() {
    for how in [Start::Ring, Start::Core] {
        if !starts().contains(&how) {
            continue;
        }
        let p = scratch("nostat", how, b"x");
        let mut io = start(how, 8);
        let open = io.open(&path_bytes(&p)).expect("open");
        let held = free(&io);
        let stat = io.stat_opened(open).expect("stat");
        assert_eq!(free(&io), held - 1);
        assert!(io.cancel(stat));
        // No entry, freed at once.
        assert_eq!(free(&io), held, "{how:?}");
        assert!(!io.cancel(stat));
        let c = pump(&mut io).expect("open completion");
        assert_eq!(c.op, open);
        let fd = c.result.expect("opens") as RawFd;
        assert!(
            io.next_completion().is_none(),
            "{how:?} a cancelled stat reported"
        );
        assert!(io.close(fd));
        turn(&mut io);
        assert_eq!(io.in_flight(), 0);
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_cancelled_open_ends_its_stat() {
    for how in [Start::Ring, Start::Core] {
        if !starts().contains(&how) {
            continue;
        }
        let p = scratch("noopen", how, b"x");
        let mut io = start(how, 8);
        let before = free(&io);
        let open = io.open(&path_bytes(&p)).expect("open");
        let stat = io.stat_opened(open).expect("stat");
        assert!(io.cancel(open));
        let c = io.next_completion().expect("the stat was told");
        assert_eq!((c.op, c.result), (stat, Err(Errno::CANCELED)), "{how:?}");
        // The open still lands and its fd is closed internally.
        for _ in 0..3 {
            io.tick(Some(Duration::from_millis(20))).expect("tick");
        }
        assert!(io.next_completion().is_none(), "{how:?}");
        assert_eq!(io.in_flight(), 0);
        assert_eq!(free(&io), before, "{how:?} leaked a buffer");
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn a_closed_descriptor_is_closed_and_nothing_comes_back() {
    for how in starts() {
        let mut io = start(how, 8);
        let tell = Tell::new(&mut io);
        assert!(!tell.closed());
        assert!(io.close(tell.w));
        assert!(!io.close(tell.w), "{how:?} closed twice");
        assert!(io.fd(tell.w).is_none(), "{how:?} still the driver's");
        // Core batches the close, the others closed synchronously.
        assert_eq!(tell.closed(), how != Start::Core, "{how:?}");
        turn(&mut io);
        assert!(tell.closed(), "{how:?} never closed");
        assert!(io.next_completion().is_none(), "{how:?} a close reported");
        assert_eq!(io.in_flight(), 0);
        assert_eq!(io.q.in_flight(), 0, "{how:?} a close still in the queue");
    }
}

#[test]
fn closing_with_operations_in_flight_cancels_them_first() {
    for how in starts() {
        let (l, addr) = listener();
        let mut io = start(how, 8);
        let before = free(&io);
        io.connect(addr).expect("connect");
        let fd = pump(&mut io).expect("connect").fd;
        let mut peer = l.accept().expect("accept").0;
        peer.set_read_timeout(Some(LIMIT)).expect("timeout");
        let recv = io.recv(fd, true).expect("recv");
        turn(&mut io);
        assert!(io.close(fd));
        assert!(!io.is_live(recv), "{how:?}");
        for _ in 0..3 {
            io.tick(Some(Duration::from_millis(20))).expect("tick");
        }
        assert!(
            io.next_completion().is_none(),
            "{how:?} a cancelled op reported"
        );
        let mut b = [0u8; 8];
        assert_eq!(peer.read(&mut b).expect("read"), 0, "{how:?}");
        assert_eq!(io.in_flight(), 0, "{how:?}");
        assert_eq!(free(&io), before, "{how:?} leaked a buffer");
    }
}

/// Closes past the queue's room fall back to a plain close.
#[test]
fn closes_past_what_is_remembered_are_made_at_once() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 4);
    let room = io.q.room();
    let tells: Vec<Tell> = (0..room + 3).map(|_| Tell::new(&mut io)).collect();
    for t in &tells {
        assert!(io.close(t.w));
    }
    let now = tells.iter().filter(|t| t.closed()).count();
    assert_eq!(now, 3, "what did not fit was not closed by the call");
    turn(&mut io);
    assert!(tells.iter().all(Tell::closed));
    assert!(io.next_completion().is_none());
    assert_eq!(io.q.in_flight(), 0);
}

/// More closes than the SQ holds.
#[test]
fn many_closes_in_one_turn_all_close() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 64);
    let tells: Vec<Tell> = (0..40).map(|_| Tell::new(&mut io)).collect();
    for round in 0..3 {
        for t in &tells[round * 10..round * 10 + 10] {
            assert!(io.close(t.w));
        }
        turn(&mut io);
    }
    for t in &tells[30..] {
        assert!(io.close(t.w));
    }
    turn(&mut io);
    assert!(tells.iter().all(Tell::closed));
    assert!(io.next_completion().is_none());
}

/// The kernel refuses to close a ring fd via a ring, so a ring peer closes in the call.
#[test]
fn a_descriptor_the_ring_refuses_is_closed_by_the_call() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut other = start(Start::Core, 8);
    let mut io = start(Start::Core, 8);
    let peer = io.peer(other.mailbox().expect("mailbox"));
    assert!(io.close(peer.fd()));
    assert_eq!(io.q.in_flight(), 0, "a ring peer went to the batch");
    turn(&mut io);
    assert!(io.next_completion().is_none());
}

#[test]
fn a_driver_that_goes_settles_its_closes() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 8);
    let never_sent = Tell::new(&mut io);
    assert!(io.close(never_sent.w));
    assert!(!never_sent.closed());
    drop(io);
    assert!(
        never_sent.closed(),
        "a queued close was lost with the driver"
    );

    let mut io = start(Start::Core, 8);
    let sent = Tell::new(&mut io);
    let queued = Tell::new(&mut io);
    assert!(io.close(sent.w));
    // Submitted, completion left unreaped.
    io.flush().expect("flush");
    assert!(sent.closed());
    assert!(io.close(queued.w));
    assert_eq!(io.q.in_flight(), 2);
    drop(io);
    assert!(queued.closed());
}

#[test]
fn a_plain_ring_and_the_readiness_backend_close_by_the_call() {
    for how in [Start::Epoll, Start::Ring] {
        if !starts().contains(&how) {
            continue;
        }
        let mut io = start(how, 8);
        let tell = Tell::new(&mut io);
        assert!(io.close(tell.w));
        assert!(tell.closed(), "{how:?}");
    }
}

#[test]
fn a_backend_that_cannot_tell_says_pending() {
    if starts().contains(&Start::Ring) {
        // Only a ring without the deferred-work flag cannot tell.
        let mut io = start(Start::Ring, 8);
        assert_eq!(io.pending(), io.ring_flags() == RingFlags::Plain);
    }
    // Epoll can only tell when nothing is in flight.
    let mut io = start(Start::Epoll, 8);
    assert!(!io.pending());
    let timer = io.timeout(Duration::from_secs(60)).expect("timer");
    assert!(io.pending());
    assert!(io.cancel(timer));
}

#[test]
fn an_idle_ring_has_nothing_pending() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 8);
    assert!(!io.pending());
    turn(&mut io);
    assert!(!io.pending());
}

#[test]
fn a_queued_submission_is_pending_until_it_is_submitted() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 8);
    let timer = io.timeout(Duration::from_secs(60)).expect("timer");
    assert!(io.pending(), "an entry waits to be submitted");
    turn(&mut io);
    assert!(!io.pending(), "an armed timer is not work");
    assert!(io.cancel(timer));
    assert!(io.pending(), "the cancel is an entry");
}

#[test]
fn a_completion_in_the_ring_or_in_hand_is_pending() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let p = scratch("ready", Start::Core, b"cached");
    let mut io = start(Start::Core, 8);
    io.open(&path_bytes(&p)).expect("open");
    // Completes inline, not reaped.
    io.flush().expect("flush");
    assert!(io.pending(), "a completion sits in the ring");
    turn(&mut io);
    assert!(io.pending(), "a completion waits to be taken");
    let fd = io.next_completion().expect("open").result.expect("opens") as RawFd;
    assert!(!io.pending());
    assert!(io.close(fd));
    let _ = std::fs::remove_file(&p);
}

/// Deferred task work shows only in the flag until an enter runs it.
#[test]
fn work_the_kernel_put_off_is_pending() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 8);
    let (r, w) = rustix::pipe::pipe().expect("pipe");
    let r = io.own(r);
    let read = io.read_stream(r).expect("read");
    turn(&mut io);
    assert!(!io.pending(), "a read that waits is not work");
    assert_eq!(rustix::io::write(&w, b"wake"), Ok(4));
    assert!(io.pending(), "the read's wake was put off and not seen");
    assert!(!io.q.ready(), "it was in the queue after all");
    let c = pump(&mut io).expect("read completion");
    assert_eq!((c.op, c.result), (read, Ok(4)));
    io.release(c.buf.expect("buffer"));
    assert!(!io.pending());
    drop(w);
}

/// `Core::doze` relies on this.
#[test]
fn a_wait_after_nothing_pending_returns_for_what_came_since() {
    if !starts().contains(&Start::Core) {
        return;
    }
    let mut io = start(Start::Core, 8);
    let (r, w) = rustix::pipe::pipe().expect("pipe");
    let r = io.own(r);
    io.read_stream(r).expect("read");
    turn(&mut io);
    for round in 0..50 {
        assert!(!io.pending(), "round {round}");
        // Arrives between `pending` and the wait.
        assert_eq!(rustix::io::write(&w, b"x"), Ok(1));
        let began = Instant::now();
        let n = io.tick(Some(LIMIT)).expect("tick");
        assert_eq!(n, 1, "round {round}");
        assert!(began.elapsed() < LIMIT / 2, "round {round} slept on it");
        let c = io.next_completion().expect("read completion");
        io.release(c.buf.expect("buffer"));
        io.read_stream(r).expect("read");
        turn(&mut io);
    }
}
