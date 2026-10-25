//! Regressions for the driver findings of the unsafe review.

use super::*;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Finding 2 (t3), a buffer lent to a receive was released, handed out again and
/// overwritten by the kernel under the caller. The receive spends the id now, so only
/// the handing out again is left to check.
fn check_a_lent_buffer_stays_lent(back: Backend) {
    let (mut a, b) = UnixStream::pair().expect("pair");
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let fd = io.own(OwnedFd::from(b));
    let lent = io.alloc().expect("buffer");
    let at = lent.index();
    io.recv_into(fd, lent, 0, 8).expect("recv_into");
    io.flush().expect("flush");
    let mine = io.alloc().expect("another");
    assert_ne!(mine.index(), at, "{back:?} handed a lent buffer out again");
    io.buf_mut(&mine)[..8].copy_from_slice(b"caller..");
    a.write_all(b"KERNEL!!").expect("write");
    let c = pump(&mut io).expect("receive");
    let got = c.buf.expect("buffer");
    assert_eq!(got.id.index(), at);
    assert_eq!(io.filled(&got), b"KERNEL!!");
    assert_eq!(
        &io.buf_mut(&mine)[..8],
        b"caller..",
        "{back:?} overwrote the caller"
    );
    assert!(io.release(got));
    assert!(io.release(mine));
}

/// Finding 2, region ranges an op in flight names are out of the caller's reach.
fn check_a_region_range_in_flight_is_out_of_reach(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let r = io
        .add_region(vec![0u8; 256].into_boxed_slice())
        .expect("region");
    // Non-blocking, as the readiness backend reads at submission.
    let flags = rustix::pipe::PipeFlags::NONBLOCK | rustix::pipe::PipeFlags::CLOEXEC;
    let (rx, w) = rustix::pipe::pipe_with(flags).expect("pipe");
    let rx = io.own(rx);
    let read = io.read_mem(rx, APPEND, r, 100, 50).expect("read_mem");
    io.flush().expect("flush");
    assert!(io.is_live(read), "{back:?} an empty pipe read finished");
    assert!(
        io.region_mut(r).is_empty(),
        "{back:?} the whole region while read into"
    );
    assert!(
        io.region(r).is_empty(),
        "{back:?} read while the kernel writes it"
    );
    assert!(io.region_at_mut(r, 120, 10).is_empty());
    assert!(io.region_at(r, 149, 2).is_empty());
    assert_eq!(io.region_at(r, 0, 100).len(), 100, "a range clear of it");
    io.region_at_mut(r, 150, 10).copy_from_slice(b"0123456789");
    let dir = std::env::temp_dir();
    let dir = std::fs::File::open(dir).expect("dir");
    let dfd = io.own(OwnedFd::from(dir));
    assert!(matches!(
        io.list(dfd, r, 90, 20),
        Err(IoError::RegionBusy { at: 90, len: 20 })
    ));
    rustix::io::write(&w, b"landed").expect("write");
    let c = pump(&mut io).expect("read");
    assert_eq!((c.op, c.result), (read, Ok(6)));
    assert_eq!(&io.region_mut(r)[100..106], b"landed");
    assert_eq!(&io.region(r)[150..160], b"0123456789");
}

/// Finding 1, a drain that times out leaks what the kernel may still read, here the
/// memory of a send in flight, rather than freeing it.
fn check_a_drain_that_times_out_leaks_held_memory(back: Backend) {
    const HUGE: usize = 32 << 20;
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect completion").fd;
    let _peer = l.accept().expect("accept").0;
    let data: Arc<Vec<u8>> = Arc::new(vec![7; HUGE]);
    let id = io.send_shared(fd, data.clone(), 0, HUGE).expect("send");
    io.tick(Some(Duration::from_millis(20))).expect("tick");
    assert!(io.is_live(id), "{back:?} the send finished");
    io.drain = Duration::ZERO;
    drop(io);
    assert_eq!(
        Arc::strong_count(&data),
        2,
        "{back:?} freed memory a send in flight reads"
    );
}

/// Finding 5, a failed wake or mailbox poll is delivered and not polled again, where
/// re-arming would complete inline again and spin.
fn check_a_failed_poll_of_its_own_is_not_rearmed(back: Backend) {
    driver!(io, back, ops = 4, bufs = 2, size = 64);
    let _w = io.waker().expect("waker");
    let flying = io.q.in_flight();
    let bad = -Errno::BADF.raw_os_error();
    io.complete(Cqe {
        user: WAKE,
        res: bad,
        flags: 0,
        addr: None,
    });
    assert_eq!(
        io.q.in_flight(),
        flying,
        "{back:?} re-armed a failed wake poll"
    );
    let c = io.next_completion().expect("the failure");
    assert_eq!((c.kind, c.result), (Kind::Wake, Err(Errno::BADF)));
    assert!(io.wake.is_none());
    assert!(io.next_completion().is_none());
    if back != Backend::Epoll {
        return;
    }
    let _m = io.mailbox().expect("mailbox");
    let flying = io.q.in_flight();
    io.complete(Cqe {
        user: MAIL,
        res: bad,
        flags: 0,
        addr: None,
    });
    assert_eq!(io.q.in_flight(), flying, "re-armed a failed mailbox poll");
    let c = io.next_completion().expect("the failure");
    assert_eq!(
        (c.kind, c.result, c.value()),
        (Kind::Msg, Err(Errno::BADF), None)
    );
    assert!(io.mail.is_none());
}

/// Finding 10, the held memory's `as_ref` is read once, so every resubmit sends the
/// bytes the send started from.
struct Fickle {
    first: Vec<u8>,
    later: Vec<u8>,
    calls: AtomicUsize,
}

impl AsRef<[u8]> for Fickle {
    fn as_ref(&self) -> &[u8] {
        match self.calls.fetch_add(1, Ordering::Relaxed) {
            0 => &self.first,
            _ => &self.later,
        }
    }
}

fn check_a_shared_send_reads_its_memory_once(back: Backend) {
    const LEN: usize = 4 << 20;
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect completion").fd;
    let mut peer = l.accept().expect("accept").0;
    let data = Arc::new(Fickle {
        first: vec![1; LEN],
        later: vec![2; LEN],
        calls: AtomicUsize::new(0),
    });
    let id = io.send_shared(fd, data.clone(), 0, LEN).expect("send");
    let reader = std::thread::spawn(move || {
        let mut got = vec![0u8; LEN];
        peer.read_exact(&mut got).expect("read");
        got
    });
    let start = Instant::now();
    let c = loop {
        assert!(start.elapsed() < LIMIT * 2, "{back:?} the send never ended");
        match pump(&mut io) {
            Some(c) if c.op == id => break c,
            _ => {}
        }
    };
    assert_eq!(c.result, Ok(LEN as u32));
    let got = reader.join().expect("reader");
    assert!(
        got.iter().all(|b| *b == 1),
        "{back:?} a resubmit sent other bytes"
    );
    assert_eq!(
        data.calls.load(Ordering::Relaxed),
        1,
        "{back:?} as_ref called again"
    );
}

#[test]
fn a_lent_buffer_stays_lent() {
    for b in backends() {
        check_a_lent_buffer_stays_lent(b);
    }
}

#[test]
fn a_region_range_in_flight_is_out_of_reach() {
    for b in backends() {
        check_a_region_range_in_flight_is_out_of_reach(b);
    }
}

#[test]
fn a_drain_that_times_out_leaks_held_memory() {
    for b in backends() {
        check_a_drain_that_times_out_leaks_held_memory(b);
    }
}

#[test]
fn a_failed_poll_of_its_own_is_not_rearmed() {
    for b in backends() {
        check_a_failed_poll_of_its_own_is_not_rearmed(b);
    }
}

#[test]
fn a_shared_send_reads_its_memory_once() {
    for b in backends() {
        check_a_shared_send_reads_its_memory_once(b);
    }
}

/// Finding 8, an accept that lands after its cancel gets its fd closed, not leaked.
#[test]
fn a_cancelled_accept_that_lands_closes_its_fd() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    let (l, addr) = listener();
    driver!(io, Backend::Uring, ops = 8, bufs = 4, size = 64);
    let lfd = io.own(OwnedFd::from(l));
    let id = io.accept(lfd, false).expect("accept");
    io.flush().expect("flush");
    let mut peer = TcpStream::connect(addr).expect("connect");
    peer.set_read_timeout(Some(LIMIT)).expect("timeout");
    let start = Instant::now();
    while !io.q.ready() {
        assert!(start.elapsed() < LIMIT, "the accept never landed");
        io.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(io.cancel(id));
    for _ in 0..3 {
        io.tick(Some(Duration::from_millis(20))).expect("tick");
    }
    assert!(
        io.next_completion().is_none(),
        "a cancelled accept reported"
    );
    let mut b = [0u8; 1];
    assert_eq!(peer.read(&mut b).expect("eof"), 0, "the late fd leaked");
}

/// Finding 1 (t1), a forgotten driver keeps its arena, so a receive in flight writes
/// leaked memory and never a fresh allocation.
#[test]
fn a_forgotten_driver_keeps_its_arena() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    let (mut a, b) = UnixStream::pair().expect("pair");
    driver!(io, Backend::Uring, ops = 8, bufs = 8, size = 128);
    let fd = io.own(OwnedFd::from(b));
    io.recv(fd, false).expect("recv");
    io.flush().expect("flush");
    std::mem::forget(io);
    let fresh = vec![0xAAu8; 8 * 128];
    a.write_all(b"KERNEL-WROTE-AFTER-FREE").expect("write");
    std::thread::sleep(Duration::from_millis(100));
    let hit = fresh.windows(23).any(|w| w == b"KERNEL-WROTE-AFTER-FREE");
    assert!(!hit, "the kernel wrote into freed memory");
}

/// Finding 5 (t5), a waker whose eventfd lies past the readiness watch table spun
/// forever in harvest. The table grows instead.
#[test]
fn a_waker_on_a_high_fd_wakes_the_readiness_backend() {
    const HOLD: usize = FD_ROOM + 100;
    if !raise_nofile(HOLD as u64 + 256) {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let null = std::fs::File::open("/dev/null").expect("null");
        let hold: Vec<OwnedFd> = (0..HOLD)
            .map(|_| OwnedFd::from(null.try_clone().expect("dup")))
            .collect();
        let mut io = Io::with_backend(Storage::new(8, 8), 64, Backend::Epoll).expect("driver");
        let w = io.waker().expect("waker");
        let high = io.wake.as_ref().map_or(0, |f| f.as_raw_fd());
        let first = io
            .tick(Some(Duration::from_millis(10)))
            .map(|_| io.next_completion());
        w.wake();
        let woke = pump(&mut io).map(|c| (c.kind, c.result));
        let _ = tx.send((high, first.is_ok(), woke));
        drop(hold);
    });
    let (high, ticked, woke) = rx.recv_timeout(LIMIT).expect("harvest spun");
    assert!(
        high as usize >= FD_ROOM,
        "the eventfd {high} is not past the table"
    );
    assert!(ticked);
    assert_eq!(woke, Some((Kind::Wake, Ok(0))));
}

/// Soft `RLIMIT_NOFILE` to at least `n`, false if the hard limit is lower.
fn raise_nofile(n: u64) -> bool {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain out-parameter.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 || r.rlim_max < n {
        return false;
    }
    if r.rlim_cur >= n {
        return true;
    }
    r.rlim_cur = n;
    // SAFETY: as above, a value within the hard limit.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) == 0 }
}
