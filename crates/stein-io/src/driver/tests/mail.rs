//! Cross-thread posts and full-queue behavior.

use super::*;
use std::sync::mpsc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Epoll,
    Ring,
    /// `SINGLE_ISSUER` and `DEFER_TASKRUN`.
    Core,
}

/// Leaked so a driver can be built on its own thread.
fn storage(ops: usize, bufs: usize) -> Storage {
    Storage::new(ops, bufs)
}

fn start(how: Start, ops: usize) -> Io {
    let store = storage(ops, 16);
    let io = match how {
        Start::Epoll => Io::with_backend(store, 64, Backend::Epoll),
        Start::Ring => Io::with_backend(store, 64, Backend::Uring),
        Start::Core => Io::single_issuer(store, 64),
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

/// Spawns a driver thread, returns its mailbox, then runs `body`.
fn target<T: Send + 'static>(
    how: Start,
    ops: usize,
    body: impl FnOnce(&mut Io) -> T + Send + 'static,
) -> (Mailbox, std::thread::JoinHandle<T>) {
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        let mut io = start(how, ops);
        tx.send(io.mailbox().expect("mailbox")).expect("send");
        body(&mut io)
    });
    (rx.recv().expect("mailbox"), h)
}

/// Post and wait for the post's own completion.
fn post(io: &mut Io, to: Peer, value: u64) -> Result<u32, Errno> {
    let id = io.post(to, value).expect("post");
    let c = pump(io).expect("post completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Post);
    assert!(c.is_final());
    c.result
}

fn value_of(c: Completion) -> u64 {
    assert_eq!(c.kind, Kind::Msg, "something else completed: {c:?}");
    c.value().expect("a value")
}

fn take(io: &mut Io, n: usize) -> Vec<u64> {
    let mut got = Vec::new();
    while got.len() < n {
        let c = pump(io).expect("a posted value never arrived");
        assert_eq!(c.kind, Kind::Msg, "something else completed: {c:?}");
        assert!(c.more, "a value must not look final");
        assert_eq!(c.fd, -1);
        got.push(c.value().expect("a value"));
    }
    got
}

fn check_a_sleeping_driver_wakes_on_a_post(how: Start) {
    let (mailbox, h) = target(how, 8, |io| {
        // Only a wake ends this wait in time.
        let begin = Instant::now();
        io.tick(Some(Duration::from_secs(10))).expect("tick");
        let waited = begin.elapsed();
        let c = io.next_completion().expect("woke with nothing");
        (waited, c.kind, c.value())
    });
    let mut io = start(how, 8);
    let to = io.peer(mailbox);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(post(&mut io, to, 0xfeed_beef_0000_0001), Ok(0));
    let (waited, kind, value) = h.join().expect("target");
    assert!(
        waited >= Duration::from_millis(80),
        "woke early, {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "slept through a post, {waited:?}"
    );
    assert_eq!(kind, Kind::Msg);
    assert_eq!(value, Some(0xfeed_beef_0000_0001));
    assert!(io.close(to.fd()));
}

/// Including values that look like the target's own op ids.
fn check_values_arrive_whole_and_in_order(how: Start) {
    const N: u64 = 2000;
    let (ids_tx, ids_rx) = mpsc::channel();
    let (mailbox, h) = target(how, 16, move |io| {
        // Sender posts this live op's id as a value.
        let timer = io.timeout(Duration::from_millis(400)).expect("timeout");
        ids_tx.send(timer.to_user_data()).expect("send");
        let got = take(io, N as usize + 4);
        assert!(io.is_live(timer), "a value hit a live operation");
        let c = pump(io).expect("the timer never fired");
        assert_eq!((c.op, c.kind), (timer, Kind::Timeout));
        got
    });
    let mut io = start(how, 16);
    let to = io.peer(mailbox);
    let timer = ids_rx.recv().expect("id");
    let odd = [timer, u64::MAX, 0, u64::MAX - 1];
    for v in odd {
        assert_eq!(post(&mut io, to, v), Ok(0));
    }
    for i in 0..N {
        let r = post(&mut io, to, i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        assert_eq!(r, Ok(0));
    }
    let got = h.join().expect("target");
    assert_eq!(&got[..4], &odd);
    for (i, v) in got[4..].iter().enumerate() {
        assert_eq!(*v, (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15), "at {i}");
    }
}

/// Overflow while the target is idle. Nothing may vanish silently.
fn check_a_burst_past_the_queue_is_accounted_for(how: Start) {
    const BURST: u64 = 10_000;
    let (go_tx, go_rx) = mpsc::channel::<u64>();
    let (mailbox, h) = target(how, 8, move |io| {
        // Idle until the sender finishes.
        let sent = go_rx.recv().expect("go");
        let before = io.cq_backlog();
        // Per tick, to measure how many one kernel entry brings.
        let (mut got, mut most, mut after) = (Vec::new(), 0, false);
        let begin = Instant::now();
        while got.len() < sent as usize && begin.elapsed() < LIMIT {
            let n = io.tick(Some(Duration::from_millis(20))).expect("tick");
            most = most.max(n);
            after |= io.cq_backlog();
            got.extend(std::iter::from_fn(|| io.next_completion()).map(value_of));
        }
        io.tick(Some(Duration::from_millis(50))).expect("tick");
        let extra = io.next_completion().is_some();
        (got, (before, after), most, io.cq_overflow(), extra)
    });
    let mut io = start(how, 64);
    let to = io.peer(mailbox);
    let (mut sent, mut full, mut other) = (0u64, 0u64, Vec::new());
    for i in 0..BURST {
        match post(&mut io, to, sent) {
            Ok(0) => sent += 1,
            Err(Errno::AGAIN) => full += 1,
            r => other.push((i, r)),
        }
    }
    assert!(
        other.is_empty(),
        "posts failed in ways not accounted for: {other:?}"
    );
    go_tx.send(sent).expect("go");
    let (got, backlog, most, dropped, extra) = h.join().expect("target");
    eprintln!(
        "{how:?}: {BURST} posts into a queue for 8 operations, {sent} taken, \
         {full} refused, backlog before and while taking {backlog:?}, \
         at most {most} a turn, dropped {dropped}"
    );
    assert_eq!(sent + full, BURST);
    assert_eq!(got.len() as u64, sent, "values were lost");
    assert!(
        got.iter().enumerate().all(|(i, v)| *v == i as u64),
        "out of order"
    );
    assert!(!extra, "more arrived than was sent");
    assert_eq!(dropped, 0, "the kernel dropped completions");
    match how {
        // Pipe holds 8192, then EAGAIN.
        Start::Epoll => {
            assert_eq!(sent, 8192);
            assert_eq!(backlog, (false, false));
        }
        // Takes all, overflow held in the kernel.
        Start::Ring => {
            assert_eq!(full, 0);
            assert!(backlog.0, "ten thousand entries fitted a queue of dozens");
        }
        // Deferred task work, trickled in per kernel entry.
        Start::Core => {
            assert_eq!(full, 0);
            assert_eq!(backlog, (false, false));
            assert!(most <= 32, "{most} in one turn overflowed the queue");
        }
    }
}

/// Drop with a mailbox out cancels in-flight work without hanging.
fn check_a_shared_driver_drops_cleanly(how: Start) {
    let (l, addr) = listener();
    let mut io = start(how, 16);
    let mailbox = io.mailbox().expect("mailbox");
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect").fd;
    let _peer = l.accept().expect("accept").0;
    io.recv(fd, true).expect("recv");
    io.recv(fd, false).ok();
    io.timeout(Duration::from_secs(60)).expect("timeout");
    io.flush().expect("flush");
    let begin = Instant::now();
    drop(io);
    assert!(begin.elapsed() < Duration::from_secs(1), "the drop waited");
    // Posting into an orphaned mailbox is harmless.
    let mut other = start(how, 8);
    let to = other.peer(mailbox);
    let r = post(&mut other, to, 1);
    assert!(matches!(r, Ok(0) | Err(Errno::PIPE)), "{r:?}");
}

#[test]
fn a_readiness_driver_cannot_post_to_a_ring() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    let mut ring = start(Start::Ring, 8);
    let mut ep = start(Start::Epoll, 8);
    let to = ep.peer(ring.mailbox().expect("mailbox"));
    assert!(to.is_ring());
    assert_eq!(post(&mut ep, to, 7), Err(Errno::OPNOTSUPP));
    // Reverse direction uses the pipe.
    let to = ring.peer(ep.mailbox().expect("mailbox"));
    assert!(!to.is_ring());
    assert_eq!(post(&mut ring, to, 7), Ok(0));
    assert_eq!(take(&mut ep, 1), [7]);
}

fn check_a_driver_posts_to_itself(how: Start) {
    let mut io = start(how, 8);
    let m = io.mailbox().expect("mailbox");
    let to = io.peer(m);
    let id = io.post(to, 41).expect("post");
    let mut seen = Vec::new();
    for _ in 0..2 {
        let c = pump(&mut io).expect("completion");
        seen.push((c.kind, c.value(), c.result));
        assert!(c.kind == Kind::Msg || c.op == id);
    }
    seen.sort_by_key(|s| s.1);
    assert_eq!(
        seen,
        [(Kind::Post, None, Ok(0)), (Kind::Msg, Some(41), Ok(0))]
    );
}

macro_rules! each {
    ($($name:ident => $check:ident,)*) => {$(
        #[test]
        fn $name() {
            for s in starts() {
                $check(s);
            }
        }
    )*};
}

each! {
    a_sleeping_driver_wakes_on_a_post => check_a_sleeping_driver_wakes_on_a_post,
    values_arrive_whole_and_in_order => check_values_arrive_whole_and_in_order,
    a_burst_past_the_queue_is_accounted_for => check_a_burst_past_the_queue_is_accounted_for,
    a_shared_driver_drops_cleanly => check_a_shared_driver_drops_cleanly,
    a_driver_posts_to_itself => check_a_driver_posts_to_itself,
}
