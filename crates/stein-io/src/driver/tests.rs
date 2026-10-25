#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The driver macro always binds mutably.
#![allow(unused_mut)]

use super::*;
use std::time::Instant;

fn span(d: Duration) -> rustix::time::Timespec {
    rustix::time::Timespec {
        tv_sec: d.as_secs() as i64,
        tv_nsec: d.subsec_nanos() as i64,
    }
}
use std::io::{Read as _, Write as _};
use std::net::TcpListener;

const LIMIT: Duration = Duration::from_secs(5);

/// Declares a driver and its storage in the caller's scope.
macro_rules! driver {
    ($name:ident, $back:expr, ops = $ops:expr, bufs = $n:expr, size = $size:expr) => {
        let mut $name =
            Io::with_backend(Storage::new($ops, $n), $size as u32, $back).expect("driver");
    };
}

fn pump(io: &mut Io) -> Option<Completion> {
    let start = Instant::now();
    loop {
        if let Some(c) = io.next_completion() {
            return Some(c);
        }
        if start.elapsed() > LIMIT {
            return None;
        }
        let b = io.backend();
        if let Err(e) = io.tick(Some(Duration::from_millis(20))) {
            panic!("tick on {b:?}: {e:?}");
        }
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let a = l.local_addr().expect("addr");
    (l, a)
}

/// Bound then dropped, nothing listens.
fn dead_addr() -> SocketAddr {
    let (l, a) = listener();
    drop(l);
    a
}

fn check_a_driver_starts_with_one_descriptor(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    assert_eq!(io.backend(), back);
    assert!(io.as_fd().as_raw_fd() >= 0);
    assert_eq!(io.in_flight(), 0);
}

fn check_a_connection_completes(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let id = io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("no completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Connect);
    assert!(c.result.is_ok(), "connect failed: {:?}", c.result);
    assert!(c.is_final());
    assert!(!io.is_live(id));
    assert_eq!(io.in_flight(), 0);
    drop(l);
}

/// Refused sockets go writable too, so `SO_ERROR` must be checked.
fn check_a_refused_connection_reports_the_error(back: Backend) {
    let addr = dead_addr();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let id = io.connect(addr).expect("submit");
    let c = pump(&mut io).expect("no completion");
    assert_eq!(c.op, id);
    assert_eq!(c.result, Err(Errno::CONNREFUSED), "got {:?}", c.result);
}

fn check_bytes_written_by_a_peer_arrive(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;
    peer.write_all(b"hello there").expect("write");

    let id = io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Recv);
    assert_eq!(c.result, Ok(11));
    let buf = c.buf.expect("a receive must deliver a buffer");
    assert_eq!(io.filled(&buf), b"hello there");
    assert!(io.release(buf));
}

fn check_bytes_reach_the_peer(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;

    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..5].copy_from_slice(b"ping!");
    let id = io.send(fd, b, 5).expect("send");
    let c = pump(&mut io).expect("send completion");
    assert_eq!(c.op, id);
    assert_eq!(c.result, Ok(5));

    let mut got = [0u8; 5];
    peer.read_exact(&mut got).expect("read");
    assert_eq!(&got, b"ping!");
    // Driver released the send buffer itself.
    assert_eq!(io.bufs().available(), 4);
}

/// EOF is `Ok(0)`, not an error.
fn check_a_closed_peer_reads_as_end_of_stream(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let peer = l.accept().expect("accept").0;
    drop(peer);

    io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    assert_eq!(c.result, Ok(0));
    assert!(c.is_eof());
    if let Some(b) = c.buf {
        io.release(b);
    }
}

fn check_a_repeating_receive_delivers_more_than_once(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;

    let id = io.recv(fd, true).expect("recv");
    let mut seen = Vec::new();
    for n in 0..3 {
        peer.write_all(format!("msg{n}").as_bytes()).expect("write");
        let c = pump(&mut io).expect("recv completion");
        assert_eq!(c.op, id, "the operation id changed between deliveries");
        assert!(c.more, "a repeating receive reported itself finished");
        assert!(io.is_live(id), "the slot was freed while still armed");
        let b = c.buf.expect("buffer");
        seen.push(String::from_utf8_lossy(io.filled(&b)).to_string());
        io.release(b);
    }
    assert_eq!(seen, ["msg0", "msg1", "msg2"]);

    // EOF still ends it, or the loop spins on a readable fd.
    drop(peer);
    let c = pump(&mut io).expect("eof completion");
    assert!(c.is_eof());
    assert!(!c.more, "a finished receive still claimed to be armed");
    assert!(!io.is_live(id));
    if let Some(b) = c.buf {
        io.release(b);
    }
}

fn check_another_thread_reaches_the_loop_through_an_eventfd(back: Backend) {
    use rustix::event::{EventfdFlags, eventfd};
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let efd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd");
    let raw = io.own(efd);
    let id = io.read_stream(raw).expect("read_stream");
    // Armed with nothing written, so no completion.
    io.tick(Some(Duration::from_millis(30))).expect("tick");
    assert!(io.next_completion().is_none(), "nothing to read yet");
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        // SAFETY: the driver keeps the fd open until drop, ordered after the join.
        let fd = unsafe { BorrowedFd::borrow_raw(raw) };
        rustix::io::write(fd, &3u64.to_ne_bytes()).expect("write")
    });
    let c = pump(&mut io).expect("no completion");
    writer.join().expect("writer");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Read);
    assert_eq!(c.result, Ok(8));
    let b = c.buf.expect("a buffer");
    assert_eq!(
        &io.filled(&b)[..8],
        &3u64.to_ne_bytes(),
        "the count, then reset"
    );
    io.release(b);
    let id2 = io.read_stream(raw).expect("read_stream again");
    let fd = unsafe { BorrowedFd::borrow_raw(raw) };
    rustix::io::write(fd, &1u64.to_ne_bytes()).expect("write");
    let c = pump(&mut io).expect("no second completion");
    assert_eq!(c.op, id2);
    assert_eq!(c.result, Ok(8));
    if let Some(b) = c.buf {
        io.release(b);
    }
    assert!(io.available() == 2, "both buffers came back");
}

fn check_a_timer_fires(back: Backend) {
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let start = Instant::now();
    let id = io.timeout(Duration::from_millis(60)).expect("timeout");
    let c = pump(&mut io).expect("no completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Timeout);
    assert!(c.result.is_ok());
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(50),
        "fired early: {waited:?}"
    );
}

fn check_timers_fire_in_time_order_not_submission_order(back: Backend) {
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let slow = io.timeout(Duration::from_millis(120)).expect("t");
    let fast = io.timeout(Duration::from_millis(20)).expect("t");
    let first = pump(&mut io).expect("first").op;
    let second = pump(&mut io).expect("second").op;
    assert_eq!(first, fast, "the later deadline fired first");
    assert_eq!(second, slow);
}

/// A zero timerfd deadline disarms instead of firing.
fn check_a_deadline_already_past_still_fires(back: Backend) {
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let id = io.timeout(Duration::from_nanos(1)).expect("t");
    let c = pump(&mut io).expect("a past deadline never fired");
    assert_eq!(c.op, id);
}

fn check_a_cancelled_timer_never_reports(back: Backend) {
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let doomed = io.timeout(Duration::from_millis(20)).expect("t");
    let keeper = io.timeout(Duration::from_millis(60)).expect("t");
    assert!(io.cancel(doomed));
    assert!(!io.is_live(doomed));
    let c = pump(&mut io).expect("the surviving timer never fired");
    assert_eq!(c.op, keeper, "a cancelled timer reported anyway");
    assert!(
        pump(&mut io).is_none(),
        "something reported after the last timer"
    );
}

fn check_cancelling_twice_is_refused_the_second_time(back: Backend) {
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    let id = io.timeout(Duration::from_secs(30)).expect("t");
    assert!(io.cancel(id));
    assert!(!io.cancel(id));
}

fn check_a_stale_id_does_not_reach_the_operation_that_took_its_slot(back: Backend) {
    driver!(io, back, ops = 2, bufs = 2, size = 64);
    let old = io.timeout(Duration::from_secs(30)).expect("t");
    assert!(io.cancel(old));
    let new = io.timeout(Duration::from_millis(20)).expect("t");
    assert_eq!(old.slot(), new.slot(), "the test needs the slot reused");
    assert_ne!(old, new, "a reused slot handed back an identical handle");
    assert!(!io.is_live(old));
    assert!(io.is_live(new));
    assert!(!io.cancel(old));
    assert!(io.is_live(new));
}

fn check_a_full_operation_table_is_reported_rather_than_overrunning(back: Backend) {
    driver!(io, back, ops = 2, bufs = 4, size = 64);
    io.timeout(Duration::from_secs(30)).expect("first");
    io.timeout(Duration::from_secs(30)).expect("second");
    assert!(matches!(
        io.timeout(Duration::from_secs(30)),
        Err(IoError::OpsFull)
    ));
}

fn check_a_large_send_completes_across_several_writes(back: Backend) {
    const BIG: usize = 1 << 20;
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 2, size = BIG);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;

    let b = io.alloc().expect("buffer");
    for (i, slot) in io.buf_mut(&b).iter_mut().enumerate() {
        *slot = i as u8;
    }
    io.send(fd, b, BIG as u32).expect("send");

    // Drain on a thread or both sides stall on a full socket buffer.
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        peer.read_to_end(&mut got).expect("read");
        got
    });
    let c = pump(&mut io).expect("send completion");
    assert_eq!(c.result, Ok(BIG as u32), "the tail of the write was lost");
    io.close(fd);
    let got = reader.join().expect("reader");
    assert_eq!(got.len(), BIG);
    assert!(got.iter().enumerate().all(|(i, b)| *b == i as u8));
}

fn check_many_connections_complete_together(back: Backend) {
    const N: usize = 200;
    let (l, addr) = listener();
    driver!(io, back, ops = N + 8, bufs = N + 8, size = 64);

    // Distinct byte per connection exposes misrouted completions.
    let accepter = std::thread::spawn(move || {
        let mut held = Vec::new();
        for i in 0..N {
            let (mut s, _) = l.accept().expect("accept");
            s.write_all(&[i as u8]).expect("write");
            held.push(s);
        }
        held
    });

    let mut ids = Vec::new();
    for _ in 0..N {
        ids.push(io.connect(addr).expect("connect"));
    }
    let mut fds = Vec::new();
    for _ in 0..N {
        let c = pump(&mut io).expect("a connection never completed");
        assert!(c.result.is_ok(), "connect failed: {:?}", c.result);
        fds.push(c.fd);
    }
    assert_eq!(fds.len(), N);

    for fd in &fds {
        io.recv(*fd, false).expect("recv");
    }
    let mut bytes = 0usize;
    for _ in 0..N {
        let c = pump(&mut io).expect("a receive never completed");
        assert_eq!(c.result, Ok(1));
        let b = c.buf.expect("buffer");
        bytes += io.filled(&b).len();
        io.release(b);
    }
    assert_eq!(bytes, N);
    assert_eq!(io.in_flight(), 0, "operations were left behind");
    let _ = accepter.join();
}

fn check_closing_cancels_what_was_outstanding(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let _peer = l.accept().expect("accept").0;

    let r = io.recv(fd, true).expect("recv");
    assert!(io.is_live(r));
    assert!(io.close(fd));
    assert!(!io.is_live(r), "an operation outlived its descriptor");
    assert_eq!(io.in_flight(), 0);
    assert!(!io.close(fd), "closing twice reported success");
}

fn check_the_arena_returns_to_full_after_a_round_trip(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;
    peer.write_all(b"x").expect("write");

    assert_eq!(io.bufs().available(), 4);
    io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    let b = c.buf.expect("buffer");
    assert_eq!(io.bufs().available(), 3, "the delivered buffer is on loan");
    assert!(io.release(b));
    assert_eq!(io.bufs().available(), 4);
}

fn check_the_single_descriptor_signals_when_a_timer_is_due(back: Backend) {
    use rustix::event::{PollFd, PollFlags, poll};
    driver!(io, back, ops = 4, bufs = 2, size = 64);
    io.timeout(Duration::from_millis(30)).expect("t");
    // Entry stays in userspace until flushed.
    io.flush().expect("flush");
    {
        let fd = io.as_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let n = poll(&mut fds, Some(&span(Duration::from_secs(2)))).expect("poll");
        assert_eq!(n, 1, "the driver descriptor never became readable");
    }
    assert!(io.tick(Some(Duration::ZERO)).expect("tick") > 0);
}

/// Buffer returns only after the cancelled op's completion lands.
fn check_a_cancelled_receive_returns_its_buffer_eventually(back: Backend) {
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let _peer = l.accept().expect("accept").0;

    let r = io.recv(fd, false).expect("recv");
    assert!(io.cancel(r));
    assert!(!io.is_live(r), "the slot outlived the cancel");

    let start = Instant::now();
    while io.bufs().available() < 4 && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(20))).expect("tick");
        while io.next_completion().is_some() {}
    }
    assert_eq!(
        io.bufs().available(),
        4,
        "a cancelled receive on {back:?} never gave its buffer back"
    );
    // Usable, not just counted.
    let mut taken = Vec::new();
    for _ in 0..4 {
        taken.push(io.alloc().expect("the arena said it was free").index());
    }
    taken.sort();
    taken.dedup();
    assert_eq!(taken.len(), 4, "the same buffer came back twice");
}

#[test]
fn the_default_prefers_completion_where_it_is_available() {
    let has_uring = backends().contains(&Backend::Uring);
    let io = Io::new(Storage::new(8, 8), 64).expect("driver");
    let want = if has_uring {
        Backend::Uring
    } else {
        Backend::Epoll
    };
    assert_eq!(io.backend(), want, "the default chose the wrong backend");
}

/// Idle armed sockets must hold no buffers. Uring relies on the provided ring.
fn check_armed_receives_hold_no_buffers(back: Backend) {
    const N: usize = 12;
    let (l, addr) = listener();
    driver!(io, back, ops = 64, bufs = 32, size = 64);
    if back == Backend::Uring {
        assert!(
            io.provided_buffers() > 0,
            "uring started without a provided buffer ring"
        );
    }
    let free = io.available();
    let accepter = std::thread::spawn(move || {
        let mut held = Vec::new();
        for _ in 0..N {
            match l.accept() {
                Ok((s, _)) => held.push(s),
                Err(_) => break,
            }
        }
        held
    });

    let mut fds = Vec::new();
    for _ in 0..N {
        io.connect(addr).expect("connect");
    }
    while fds.len() < N {
        let c = pump(&mut io).expect("connect completion");
        assert!(c.result.is_ok());
        fds.push(c.fd);
    }
    for fd in &fds {
        io.recv(*fd, true).expect("recv");
    }
    io.flush().expect("flush");
    io.tick(Some(Duration::from_millis(30))).expect("tick");

    assert_eq!(
        io.available(),
        free,
        "{back:?}: {N} armed but silent sockets consumed buffers"
    );
    let held = accepter.join().expect("accepter");
    drop(held);
}

fn check_a_repeating_receive_uses_the_provided_ring(back: Backend) {
    if back != Backend::Uring {
        return;
    }
    let (l, addr) = listener();
    driver!(io, back, ops = 16, bufs = 32, size = 64);
    assert!(io.provided_buffers() > 0, "no ring to test");
    io.connect(addr).expect("connect");
    let c = pump(&mut io).expect("connect completion");
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;

    io.recv(fd, true).expect("recv");
    peer.write_all(b"from the ring").expect("write");
    let c = pump(&mut io).expect("recv completion");
    let b = c.buf.expect("buffer");
    assert!(
        io.bufs().in_ring(&b.id),
        "the buffer came from the send pool, not the ring"
    );
    assert_eq!(io.filled(&b), b"from the ring");
    let before = io.available();
    assert!(io.release(b));
    assert_eq!(
        io.available(),
        before + 1,
        "a ring buffer was not offered back to the kernel"
    );
}

fn check_datagrams_round_trip(back: Backend) {
    use std::net::UdpSocket;
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("addr");
    driver!(io, back, ops = 8, bufs = 32, size = 64);

    let fd = io.connect_udp(addr).expect("udp");
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..4].copy_from_slice(b"ping");
    io.send(fd, b, 4).expect("send");
    let c = pump(&mut io).expect("send completion");
    assert_eq!(c.result, Ok(4));

    let mut got = [0u8; 16];
    let (n, from) = server.recv_from(&mut got).expect("recv_from");
    assert_eq!(&got[..n], b"ping");
    server.send_to(b"pong", from).expect("reply");

    io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    assert_eq!(c.result, Ok(4));
    let buf = c.buf.expect("buffer");
    assert_eq!(io.filled(&buf), b"pong");
    io.release(buf);
}

/// Epoll, plus uring if it starts.
fn backends() -> Vec<Backend> {
    let mut v = vec![Backend::Epoll];
    let probe = Io::with_backend(Storage::new(4, 4), 64, Backend::Uring);
    if probe.is_ok() {
        v.push(Backend::Uring);
    }
    v
}

#[test]
fn a_driver_starts_with_one_descriptor() {
    for b in backends() {
        check_a_driver_starts_with_one_descriptor(b);
    }
}

#[test]
fn a_connection_completes() {
    for b in backends() {
        check_a_connection_completes(b);
    }
}

#[test]
fn a_refused_connection_reports_the_error() {
    for b in backends() {
        check_a_refused_connection_reports_the_error(b);
    }
}

#[test]
fn bytes_written_by_a_peer_arrive() {
    for b in backends() {
        check_bytes_written_by_a_peer_arrive(b);
    }
}

#[test]
fn bytes_reach_the_peer() {
    for b in backends() {
        check_bytes_reach_the_peer(b);
    }
}

#[test]
fn a_closed_peer_reads_as_end_of_stream() {
    for b in backends() {
        check_a_closed_peer_reads_as_end_of_stream(b);
    }
}

#[test]
fn a_repeating_receive_delivers_more_than_once() {
    for b in backends() {
        check_a_repeating_receive_delivers_more_than_once(b);
    }
}

#[test]
fn another_thread_reaches_the_loop_through_an_eventfd() {
    for b in backends() {
        check_another_thread_reaches_the_loop_through_an_eventfd(b);
    }
}

#[test]
fn a_timer_fires() {
    for b in backends() {
        check_a_timer_fires(b);
    }
}

#[test]
fn timers_fire_in_time_order_not_submission_order() {
    for b in backends() {
        check_timers_fire_in_time_order_not_submission_order(b);
    }
}

#[test]
fn a_deadline_already_past_still_fires() {
    for b in backends() {
        check_a_deadline_already_past_still_fires(b);
    }
}

#[test]
fn a_cancelled_timer_never_reports() {
    for b in backends() {
        check_a_cancelled_timer_never_reports(b);
    }
}

#[test]
fn cancelling_twice_is_refused_the_second_time() {
    for b in backends() {
        check_cancelling_twice_is_refused_the_second_time(b);
    }
}

#[test]
fn a_stale_id_does_not_reach_the_operation_that_took_its_slot() {
    for b in backends() {
        check_a_stale_id_does_not_reach_the_operation_that_took_its_slot(b);
    }
}

#[test]
fn a_full_operation_table_is_reported_rather_than_overrunning() {
    for b in backends() {
        check_a_full_operation_table_is_reported_rather_than_overrunning(b);
    }
}

#[test]
fn a_large_send_completes_across_several_writes() {
    for b in backends() {
        check_a_large_send_completes_across_several_writes(b);
    }
}

#[test]
fn many_connections_complete_together() {
    for b in backends() {
        check_many_connections_complete_together(b);
    }
}

#[test]
fn closing_cancels_what_was_outstanding() {
    for b in backends() {
        check_closing_cancels_what_was_outstanding(b);
    }
}

#[test]
fn the_arena_returns_to_full_after_a_round_trip() {
    for b in backends() {
        check_the_arena_returns_to_full_after_a_round_trip(b);
    }
}

#[test]
fn the_single_descriptor_signals_when_a_timer_is_due() {
    for b in backends() {
        check_the_single_descriptor_signals_when_a_timer_is_due(b);
    }
}

#[test]
fn a_cancelled_receive_returns_its_buffer_eventually() {
    for b in backends() {
        check_a_cancelled_receive_returns_its_buffer_eventually(b);
    }
}

#[test]
fn armed_receives_hold_no_buffers() {
    for b in backends() {
        check_armed_receives_hold_no_buffers(b);
    }
}

#[test]
fn a_repeating_receive_uses_the_provided_ring() {
    for b in backends() {
        check_a_repeating_receive_uses_the_provided_ring(b);
    }
}

#[test]
fn datagrams_round_trip() {
    for b in backends() {
        check_datagrams_round_trip(b);
    }
}

fn scratch_file(name: &str, back: Backend, content: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("stein-io-{}-{name}-{back:?}", std::process::id()));
    std::fs::write(&p, content).expect("write scratch");
    p
}

fn path_bytes(p: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    p.as_os_str().as_bytes().to_vec()
}

fn check_a_file_opens_reads_and_closes(back: Backend) {
    let p = scratch_file("roundtrip", back, b"the quick brown fox");
    driver!(io, back, ops = 8, bufs = 4, size = 512);

    let id = io.open(&path_bytes(&p)).expect("open");
    let c = pump(&mut io).expect("open completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Open);
    let fd = c.fd;
    assert!(c.result.is_ok(), "open failed: {:?}", c.result);
    assert_eq!(c.result, Ok(fd as u32), "the result is the descriptor");

    io.read_at(fd, 0).expect("read");
    let c = pump(&mut io).expect("read completion");
    assert_eq!(c.kind, Kind::Read);
    assert_eq!(c.result, Ok(19));
    let b = c.buf.expect("a read delivers a buffer");
    assert_eq!(io.filled(&b), b"the quick brown fox");
    io.release(b);

    io.read_at(fd, 19).expect("read");
    let c = pump(&mut io).expect("eof completion");
    assert_eq!(c.result, Ok(0));
    assert!(c.is_eof());
    if let Some(b) = c.buf {
        io.release(b);
    }

    assert!(io.close(fd));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_file(&p);
}

/// Lands at the given range only, filled length counts the prefix.
fn check_a_file_reads_into_a_held_buffer(back: Backend) {
    let p = scratch_file("into", back, b"the quick brown fox");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open(&path_bytes(&p)).expect("open");
    let fd = pump(&mut io).expect("open completion").fd;

    let bid = io.alloc().expect("buffer");
    io.buf_mut(&bid).fill(b'.');
    io.buf_mut(&bid)[..4].copy_from_slice(b"head");
    let at = bid.index();
    io.read_at_into(fd, 4, bid, 4, 5).expect("read");
    let c = pump(&mut io).expect("read completion");
    assert_eq!(c.kind, Kind::Read);
    assert_eq!(c.result, Ok(5), "the result is the bytes read");
    let b = c.buf.expect("the buffer comes back");
    assert_eq!(b.id.index(), at);
    assert_eq!(io.filled(&b), b"headquick");
    assert_eq!(io.bufs().bytes(&b.id)[9], b'.', "read past its length");

    // Short read at EOF.
    io.read_at_into(fd, 16, b.id, 0, 60).expect("read");
    let c = pump(&mut io).expect("read completion");
    assert_eq!(c.result, Ok(3));
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"fox");

    // Failed read releases the buffer.
    assert!(io.close(fd));
    assert_eq!(io.bufs().available(), 3);
    io.read_at_into(9999, 0, b.id, 0, 8).expect("submit");
    let c = pump(&mut io).expect("failure completion");
    assert!(c.result.is_err());
    assert!(c.buf.is_none());
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_file_reads_into_a_held_buffer() {
    for b in backends() {
        check_a_file_reads_into_a_held_buffer(b);
    }
}

fn check_a_file_appends_and_writes_at_offsets(back: Backend) {
    use std::os::fd::OwnedFd;
    let p = scratch_file("append", back, b"");
    driver!(io, back, ops = 8, bufs = 4, size = 512);

    io.open_append(&path_bytes(&p)).expect("open");
    let c = pump(&mut io).expect("open completion");
    assert_eq!(c.kind, Kind::Open);
    let fd = c.fd;
    assert!(c.result.is_ok(), "open failed: {:?}", c.result);

    for word in [&b"hello "[..], &b"world\n"[..]] {
        let b = io.alloc().expect("buffer");
        io.buf_mut(&b)[..word.len()].copy_from_slice(word);
        io.write(fd, b, word.len() as u32, APPEND).expect("write");
        let c = pump(&mut io).expect("write completion");
        assert_eq!(c.kind, Kind::Write);
        assert_eq!(c.result, Ok(word.len() as u32));
        assert!(c.buf.is_none(), "a write returns no buffer");
    }
    assert_eq!(std::fs::read(&p).expect("read back"), b"hello world\n");

    // Explicit offset on an adopted fd overwrites in place.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .expect("reopen");
    let rw = io.own(OwnedFd::from(file));
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..5].copy_from_slice(b"HELLO");
    io.write(rw, b, 5, 0).expect("write");
    let c = pump(&mut io).expect("write completion");
    assert_eq!(c.result, Ok(5));
    assert_eq!(std::fs::read(&p).expect("read back"), b"HELLO world\n");

    assert!(io.close(fd));
    assert!(io.close(rw));
    assert_eq!(io.bufs().available(), 4, "a buffer leaked");
    let _ = std::fs::remove_file(&p);
}

fn check_a_write_to_a_pipe_reaches_the_reader(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let (r, w) = rustix::pipe::pipe().expect("pipe");
    let fd = io.own(w);
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..4].copy_from_slice(b"ping");
    io.write(fd, b, 4, APPEND).expect("write");
    let c = pump(&mut io).expect("write completion");
    assert_eq!(c.result, Ok(4));
    assert!(io.close(fd));
    let mut got = String::new();
    std::fs::File::from(r)
        .read_to_string(&mut got)
        .expect("read");
    assert_eq!(got, "ping");
}

fn check_a_read_at_an_offset_starts_there(back: Backend) {
    let p = scratch_file("offset", back, b"0123456789");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open(&path_bytes(&p)).expect("open");
    let fd = pump(&mut io).expect("open completion").fd;

    io.read_at(fd, 6).expect("read");
    let c = pump(&mut io).expect("read completion");
    assert_eq!(c.result, Ok(4));
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"6789");
    io.release(b);
    io.close(fd);
    let _ = std::fs::remove_file(&p);
}

fn check_a_large_file_reads_across_buffers(back: Backend) {
    // Small for many reads, big enough for a long `TMPDIR` path.
    const SIZE: usize = 256;
    let body: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
    let p = scratch_file("large", back, &body);
    driver!(io, back, ops = 8, bufs = 4, size = SIZE);
    io.open(&path_bytes(&p)).expect("open");
    let fd = pump(&mut io).expect("open completion").fd;

    let mut got = Vec::new();
    loop {
        io.read_at(fd, got.len() as u64).expect("read");
        let c = pump(&mut io).expect("read completion");
        let n = c.result.expect("read failed");
        if let Some(b) = c.buf {
            got.extend_from_slice(io.filled(&b));
            io.release(b);
        }
        if n == 0 {
            break;
        }
    }
    assert_eq!(got, body);
    io.close(fd);
    let _ = std::fs::remove_file(&p);
}

fn check_a_missing_path_reports_not_found(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open(b"/definitely/not/here").expect("submit");
    let c = pump(&mut io).expect("open completion");
    assert_eq!(c.result, Err(Errno::NOENT));

    io.stat(b"/definitely/not/here").expect("submit");
    let c = pump(&mut io).expect("stat completion");
    assert_eq!(c.result, Err(Errno::NOENT));
    assert_eq!(io.bufs().available(), 4, "an error path leaked a buffer");
}

fn check_stat_reports_what_a_path_is(back: Backend) {
    use std::os::unix::fs::PermissionsExt as _;
    let p = scratch_file("stat", back, b"stat me");
    let mut perm = std::fs::metadata(&p).expect("meta").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).expect("chmod");

    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.stat(&path_bytes(&p)).expect("stat");
    let c = pump(&mut io).expect("stat completion");
    assert_eq!(c.kind, Kind::Stat);
    assert!(c.result.is_ok(), "stat failed: {:?}", c.result);
    let b = c.buf.expect("a stat delivers a buffer");
    let m = crate::file::meta(io.filled(&b)).expect("meta");
    assert!(m.is_regular());
    assert!(m.is_executable());
    assert_eq!(m.size, 7);
    assert!(m.mtime_sec > 0);
    io.release(b);

    io.stat(&path_bytes(&std::env::temp_dir())).expect("stat");
    let c = pump(&mut io).expect("stat completion");
    let b = c.buf.expect("buffer");
    let m = crate::file::meta(io.filled(&b)).expect("meta");
    assert!(m.is_dir());
    io.release(b);
    let _ = std::fs::remove_file(&p);
}

fn check_a_path_too_long_for_a_buffer_is_refused(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let long = vec![b'a'; 100];
    assert!(matches!(
        io.open(&long),
        Err(IoError::PathTooLong { len: 100, .. })
    ));
    // Stat also holds the statx struct, so its limit is lower.
    assert!(matches!(io.stat(b"/tmp"), Err(IoError::PathTooLong { .. })));
    assert_eq!(io.bufs().available(), 4, "a refused path kept its buffer");
}

fn check_a_directory_opens_and_lists(back: Backend) {
    let d = std::env::temp_dir().join(format!("stein-io-ls-{}-{back:?}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    std::fs::write(d.join("alpha"), b"").expect("write");
    std::fs::write(d.join("beta"), b"").expect("write");

    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open(&path_bytes(&d)).expect("open");
    let c = pump(&mut io).expect("open completion");
    assert!(c.result.is_ok(), "open dir failed: {:?}", c.result);
    let fd = c.fd;

    let borrowed = io.fd(fd).expect("the driver owns what it opened");
    let mut buf = [std::mem::MaybeUninit::uninit(); 2048];
    let mut names = Vec::new();
    crate::dir::list(borrowed, &mut buf, |e| names.push(e.name.to_vec())).expect("list");
    names.sort();
    assert_eq!(names, vec![b"alpha".to_vec(), b"beta".to_vec()]);

    io.close(fd);
    let _ = std::fs::remove_dir_all(&d);
}

fn check_many_stats_complete_together(back: Backend) {
    const N: usize = 40;
    let mut paths = Vec::new();
    for i in 0..N {
        paths.push(scratch_file(&format!("fan-{i}"), back, &vec![b'x'; i]));
    }
    // Doubled since the uring provided ring takes half the arena.
    driver!(io, back, ops = N + 4, bufs = 2 * N + 8, size = 512);
    let mut ids = std::collections::HashMap::new();
    for (i, p) in paths.iter().enumerate() {
        let id = io.stat(&path_bytes(p)).expect("stat");
        ids.insert(id, i);
    }
    let mut seen = 0usize;
    while seen < N {
        let c = pump(&mut io).expect("a stat never completed");
        let i = ids[&c.op];
        let b = c.buf.expect("buffer");
        let m = crate::file::meta(io.filled(&b)).expect("meta");
        assert_eq!(m.size as usize, i, "a completion landed on the wrong stat");
        io.release(b);
        seen += 1;
    }
    assert_eq!(io.in_flight(), 0);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn a_file_opens_reads_and_closes() {
    for b in backends() {
        check_a_file_opens_reads_and_closes(b);
    }
}

#[test]
fn a_file_appends_and_writes_at_offsets() {
    for b in backends() {
        check_a_file_appends_and_writes_at_offsets(b);
    }
}

#[test]
fn a_write_to_a_pipe_reaches_the_reader() {
    for b in backends() {
        check_a_write_to_a_pipe_reaches_the_reader(b);
    }
}

#[test]
fn a_read_at_an_offset_starts_there() {
    for b in backends() {
        check_a_read_at_an_offset_starts_there(b);
    }
}

#[test]
fn a_large_file_reads_across_buffers() {
    for b in backends() {
        check_a_large_file_reads_across_buffers(b);
    }
}

#[test]
fn a_missing_path_reports_not_found() {
    for b in backends() {
        check_a_missing_path_reports_not_found(b);
    }
}

#[test]
fn stat_reports_what_a_path_is() {
    for b in backends() {
        check_stat_reports_what_a_path_is(b);
    }
}

#[test]
fn a_path_too_long_for_a_buffer_is_refused() {
    for b in backends() {
        check_a_path_too_long_for_a_buffer_is_refused(b);
    }
}

#[test]
fn a_directory_opens_and_lists() {
    for b in backends() {
        check_a_directory_opens_and_lists(b);
    }
}

#[test]
fn many_stats_complete_together() {
    for b in backends() {
        check_many_stats_complete_together(b);
    }
}

/// Pump until a completion of `kind`, dropping others.
fn pump_kind(io: &mut Io, kind: Kind) -> Option<Completion> {
    loop {
        let c = pump(io)?;
        if c.kind == kind {
            return Some(c);
        }
    }
}

fn check_a_listener_accepts_a_connection(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let lfd = io.listen("127.0.0.1:0".parse().unwrap()).expect("listen");
    let addr = io.local_addr(lfd).expect("local addr");
    assert_ne!(addr.port(), 0);
    let id = io.accept(lfd, false).expect("accept");

    let mut client = std::net::TcpStream::connect(addr).expect("connect");
    let c = pump(&mut io).expect("accept completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Accept);
    assert_eq!(c.fd, lfd);
    assert_eq!(c.addr, Some(client.local_addr().unwrap()));
    assert!(c.is_final());
    let conn = c.result.expect("new descriptor") as RawFd;
    assert!(io.fd(conn).is_some());

    client.write_all(b"hello").expect("write");
    io.recv(conn, false).expect("recv");
    let c = pump_kind(&mut io, Kind::Recv).expect("recv completion");
    assert_eq!(c.result, Ok(5));
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"hello");
    io.release(b);
    assert!(io.close(conn));
    assert!(io.close(lfd));
}

fn check_a_repeating_accept_delivers_each_connection(back: Backend) {
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let lfd = io.listen("127.0.0.1:0".parse().unwrap()).expect("listen");
    let addr = io.local_addr(lfd).expect("local addr");
    let id = io.accept(lfd, true).expect("accept");
    let clients: Vec<_> = (0..3)
        .map(|_| std::net::TcpStream::connect(addr).expect("connect"))
        .collect();
    let mut seen = Vec::new();
    for _ in 0..3 {
        let c = pump(&mut io).expect("accept completion");
        assert_eq!(c.op, id);
        assert!(c.more, "a repeating accept ended early");
        seen.push(c.addr.expect("peer address"));
    }
    let mut want: Vec<_> = clients.iter().map(|c| c.local_addr().unwrap()).collect();
    want.sort();
    seen.sort();
    assert_eq!(seen, want);
    assert!(io.is_live(id));
    assert!(io.cancel(id));
    assert!(!io.is_live(id));
    assert!(io.close(lfd));
}

fn check_unconnected_datagrams_carry_their_addresses(back: Backend) {
    use std::net::UdpSocket;
    driver!(io, back, ops = 8, bufs = 32, size = 64);
    let fd = io.bind_udp("127.0.0.1:0".parse().unwrap()).expect("bind");
    let ours = io.local_addr(fd).expect("local addr");
    let a = UdpSocket::bind("127.0.0.1:0").expect("bind a");
    let b = UdpSocket::bind("127.0.0.1:0").expect("bind b");

    let id = io.recv_from(fd, true).expect("recv_from");
    a.send_to(b"from a", ours).expect("send a");
    let c = pump(&mut io).expect("first datagram");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::RecvFrom);
    assert_eq!(c.result, Ok(6));
    assert_eq!(c.addr, Some(a.local_addr().unwrap()));
    assert!(c.more);
    let buf = c.buf.expect("buffer");
    assert_eq!(io.filled(&buf), b"from a");
    io.release(buf);

    // Empty datagram is not EOF.
    b.send_to(b"", ours).expect("send b");
    let c = pump(&mut io).expect("second datagram");
    assert_eq!(c.result, Ok(0));
    assert_eq!(c.addr, Some(b.local_addr().unwrap()));
    assert!(c.more);
    assert!(!c.is_eof());
    io.release(c.buf.expect("buffer"));

    for (sock, text) in [(&a, &b"to a"[..]), (&b, b"to b")] {
        let bid = io.alloc().expect("buffer");
        io.buf_mut(&bid)[..text.len()].copy_from_slice(text);
        let to = sock.local_addr().unwrap();
        io.send_to(fd, bid, text.len() as u32, to).expect("send_to");
        let c = pump_kind(&mut io, Kind::SendTo).expect("send completion");
        assert_eq!(c.result, Ok(text.len() as u32));
        let mut got = [0u8; 16];
        let (n, from) = sock.recv_from(&mut got).expect("recv");
        assert_eq!(&got[..n], text);
        assert_eq!(from, ours);
    }
    assert!(io.cancel(id));
    assert!(io.close(fd));
    // On uring the buffer is quarantined until the cancel reports.
    let start = Instant::now();
    while io.available() < 32 && start.elapsed() < LIMIT {
        let _ = io.tick(Some(Duration::from_millis(10)));
        while io.next_completion().is_some() {}
    }
    assert_eq!(io.available(), 32);
}

fn check_fallocate_reserves_space(back: Backend) {
    let p = scratch_file("fallocate", back, b"");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open_append(&path_bytes(&p)).expect("open");
    let c = pump(&mut io).expect("open completion");
    let fd = c.result.expect("descriptor") as RawFd;
    let id = io.fallocate(fd, 0, 65536).expect("fallocate");
    let c = pump(&mut io).expect("fallocate completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::Fallocate);
    assert_eq!(c.result, Ok(0));
    assert_eq!(std::fs::metadata(&p).expect("stat").len(), 65536);
    let bad = io.fallocate(99, 0, 16).expect("submit");
    let c = pump(&mut io).expect("bad completion");
    assert_eq!(c.op, bad);
    assert!(c.result.is_err());
    assert!(io.close(fd));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_listener_accepts_a_connection() {
    for b in backends() {
        check_a_listener_accepts_a_connection(b);
    }
}

#[test]
fn a_repeating_accept_delivers_each_connection() {
    for b in backends() {
        check_a_repeating_accept_delivers_each_connection(b);
    }
}

#[test]
fn unconnected_datagrams_carry_their_addresses() {
    for b in backends() {
        check_unconnected_datagrams_carry_their_addresses(b);
    }
}

fn sockopt_int(io: &mut Io, fd: RawFd, level: i32, name: i32, v: i32) {
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..4].copy_from_slice(&v.to_ne_bytes());
    let id = io.set_sockopt(fd, level, name, b, 4).expect("set_sockopt");
    let c = pump_kind(io, Kind::SetSockOpt).expect("sockopt completion");
    assert_eq!((c.op, c.result), (id, Ok(0)));
}

fn check_named_messages_carry_sender_and_every_control(back: Backend) {
    driver!(io, back, ops = 8, bufs = 32, size = 512);
    let rx = io
        .bind_udp("127.0.0.1:0".parse().unwrap())
        .expect("bind rx");
    let tx = io
        .bind_udp("127.0.0.1:0".parse().unwrap())
        .expect("bind tx");
    let to = io.local_addr(rx).expect("rx addr");
    sockopt_int(&mut io, rx, libc::IPPROTO_IP, libc::IP_RECVTOS, 1);
    sockopt_int(&mut io, rx, libc::SOL_UDP, libc::UDP_GRO, 1);
    sockopt_int(&mut io, tx, libc::IPPROTO_IP, libc::IP_TOS, 2);
    let id = io.recv_msg_from(rx).expect("recv_msg_from");

    // Three segments of 100, 100 and 50 in one send, delivered whole to a GRO socket.
    let bid = io.alloc().expect("buffer");
    let out: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
    io.buf_mut(&bid)[..250].copy_from_slice(&out);
    let seg = 100u16.to_ne_bytes();
    let control = crate::msg::Control {
        level: libc::SOL_UDP,
        kind: libc::UDP_SEGMENT,
        data: &seg,
    };
    io.send_msg_to(tx, bid, 250, control, to)
        .expect("send_msg_to");
    let mut got = None;
    for _ in 0..2 {
        let c = pump(&mut io).expect("a completion");
        match c.kind {
            Kind::SendMsg => assert_eq!(c.result, Ok(250)),
            _ => {
                assert_eq!((c.op, c.result), (id, Ok(250)));
                got = c.buf;
            }
        }
    }
    let b = got.expect("the message buffer");
    let m = io.msg_from(&b).expect("parses");
    assert_eq!(m.payload, &out[..]);
    assert_eq!(m.from, io.local_addr(tx));
    let mut tos = None;
    let mut gro = None;
    for c in m.controls {
        match (c.level, c.kind) {
            (libc::IPPROTO_IP, libc::IP_TOS) => tos = c.data.first().copied(),
            (libc::SOL_UDP, libc::UDP_GRO) => {
                gro = Some(i32::from_ne_bytes(c.data[..4].try_into().unwrap()))
            }
            other => panic!("unexpected control {other:?}"),
        }
    }
    assert_eq!(
        (tos, gro),
        (Some(2), Some(100)),
        "both controls on {back:?}"
    );
    io.release(b);
    assert!(io.cancel(id));
    assert!(io.close(rx));
    assert!(io.close(tx));
}

#[test]
fn named_messages_carry_sender_and_every_control() {
    for b in backends() {
        check_named_messages_carry_sender_and_every_control(b);
    }
}

#[test]
fn fallocate_reserves_space() {
    for b in backends() {
        check_fallocate_reserves_space(b);
    }
}

fn check_a_file_opens_for_writing_at_offsets(back: Backend) {
    let p = scratch_file("rw", back, b"");
    driver!(io, back, ops = 8, bufs = 4, size = 512);
    io.open_rw(&path_bytes(&p)).expect("open");
    let c = pump(&mut io).expect("open completion");
    let fd = c.result.expect("descriptor") as RawFd;
    // Out of order on purpose.
    for (off, text) in [(4u64, &b"cd"[..]), (0, b"ab"), (2, b"XY")] {
        let b = io.alloc().expect("buffer");
        io.buf_mut(&b)[..2].copy_from_slice(text);
        io.write(fd, b, 2, off).expect("write");
        let c = pump_kind(&mut io, Kind::Write).expect("write completion");
        assert_eq!(c.result, Ok(2));
    }
    assert_eq!(std::fs::read(&p).expect("read back"), b"abXYcd");
    // Reopen must not truncate.
    io.open_rw(&path_bytes(&p)).expect("open");
    let c = pump(&mut io).expect("open completion");
    let fd2 = c.result.expect("descriptor") as RawFd;
    io.read_at(fd2, 2).expect("read");
    let c = pump_kind(&mut io, Kind::Read).expect("read completion");
    assert_eq!(io.filled(c.buf.as_ref().expect("buffer")), b"XYcd");
    io.release(c.buf.expect("buffer"));
    assert!(io.close(fd));
    assert!(io.close(fd2));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_file_opens_for_writing_at_offsets() {
    for b in backends() {
        check_a_file_opens_for_writing_at_offsets(b);
    }
}

fn check_a_unix_socket_carries_bytes_both_ways(back: Backend) {
    use std::os::unix::net::UnixListener;
    let dir = std::env::temp_dir();
    let p = dir.join(format!("stein-io-unix-{}-{back:?}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let l = UnixListener::bind(&p).expect("bind");
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    io.connect_unix(net::UnixAddr::Path(&path_bytes(&p)))
        .expect("connect");
    let c = pump(&mut io).expect("connect completion");
    assert_eq!(c.kind, Kind::Connect);
    assert_eq!(c.result, Ok(0));
    let fd = c.fd;
    let mut peer = l.accept().expect("accept").0;

    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..4].copy_from_slice(b"ping");
    io.send(fd, b, 4).expect("send");
    assert_eq!(pump(&mut io).expect("send completion").result, Ok(4));
    let mut got = [0u8; 4];
    peer.read_exact(&mut got).expect("read");
    assert_eq!(&got, b"ping");

    peer.write_all(b"pong").expect("write");
    io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    let buf = c.buf.expect("buffer");
    assert_eq!(io.filled(&buf), b"pong");
    io.release(buf);
    assert!(io.close(fd));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_unix_socket_carries_bytes_both_ways() {
    for b in backends() {
        check_a_unix_socket_carries_bytes_both_ways(b);
    }
}

#[test]
fn a_unix_socket_nobody_listens_on_is_refused() {
    driver!(io, Backend::Epoll, ops = 8, bufs = 4, size = 64);
    let p = std::env::temp_dir().join(format!("stein-io-nobody-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    io.connect_unix(net::UnixAddr::Path(&path_bytes(&p)))
        .expect("socket");
    let c = pump(&mut io).expect("connect completion");
    assert_eq!(c.result, Err(Errno::NOENT));
}

mod disk;
mod mail;
mod review;
mod review2;
mod review3;
mod review4;
mod review5;
mod sock;
mod turn;

fn check_a_shared_send_goes_past_a_buffer_from_where_it_lies(back: Backend) {
    const BIG: usize = 1 << 20;
    let (l, addr) = listener();
    // buffers far smaller than the send, none of them is used
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect completion").fd;
    let mut peer = l.accept().expect("accept").0;
    let data: Arc<Vec<u8>> = Arc::new((0..BIG).map(|i| i as u8).collect());
    let id = io
        .send_shared(fd, data.clone(), 10, BIG - 20)
        .expect("send_shared");
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        peer.read_to_end(&mut got).expect("read");
        got
    });
    let c = pump(&mut io).expect("send completion");
    assert_eq!(
        (c.op, c.kind, c.result),
        (id, Kind::SendShared, Ok((BIG - 20) as u32))
    );
    assert_eq!(Arc::strong_count(&data), 1, "the driver let go on {back:?}");
    assert_eq!(io.bufs().available(), 2, "no buffer was taken");
    io.close(fd);
    let got = reader.join().expect("reader");
    assert_eq!(got, data[10..BIG - 10]);
}

fn check_a_cancelled_shared_send_holds_its_memory_until_the_kernel_lets_go(back: Backend) {
    const HUGE: usize = 64 << 20;
    let (l, addr) = listener();
    driver!(io, back, ops = 8, bufs = 2, size = 64);
    io.connect(addr).expect("connect");
    let fd = pump(&mut io).expect("connect completion").fd;
    // never read, the send stays in flight on a full socket buffer
    let _peer = l.accept().expect("accept").0;
    let data: Arc<Vec<u8>> = Arc::new(vec![7; HUGE]);
    let id = io
        .send_shared(fd, data.clone(), 0, HUGE)
        .expect("send_shared");
    io.tick(Some(Duration::from_millis(50))).expect("tick");
    assert!(
        io.is_live(id),
        "a send of {HUGE} bytes to a peer that reads nothing finished"
    );
    assert!(io.cancel(id));
    if back == Backend::Uring {
        assert_eq!(Arc::strong_count(&data), 2, "the kernel may still read it");
    }
    let start = Instant::now();
    while Arc::strong_count(&data) > 1 && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(20))).expect("tick");
        while io.next_completion().is_some() {}
    }
    assert_eq!(
        Arc::strong_count(&data),
        1,
        "the memory was never let go on {back:?}"
    );
}

fn check_wakes_merge_and_end_a_wait_from_any_thread(back: Backend) {
    driver!(io, back, ops = 4, bufs = 2, size = 64);
    let w = io.waker().expect("waker");
    let again = io.waker().expect("a second waker");
    w.wake();
    w.wake();
    again.wake();
    let c = pump(&mut io).expect("a wake");
    assert_eq!((c.kind, c.fd, c.more), (Kind::Wake, -1, true));
    io.tick(Some(Duration::from_millis(50))).expect("tick");
    assert!(
        io.next_completion().is_none(),
        "three wakes before a look are one"
    );
    // a thread with no driver wakes a blocking wait
    let t = w.clone();
    let at = Instant::now();
    let th = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        t.wake();
    });
    // a tick may return with nothing, the wake comes on one of them
    let c = pump(&mut io).unwrap_or_else(|| panic!("not woken on {back:?}"));
    assert_eq!(c.kind, Kind::Wake);
    assert!(
        at.elapsed() >= Duration::from_millis(40),
        "woken before the wake on {back:?}"
    );
    assert!(
        at.elapsed() < Duration::from_secs(2),
        "the wake came late on {back:?}"
    );
    th.join().expect("waker thread");
    drop(io);
    w.wake();
}

#[test]
fn a_shared_send_goes_past_a_buffer_from_where_it_lies() {
    for b in backends() {
        check_a_shared_send_goes_past_a_buffer_from_where_it_lies(b);
    }
}

#[test]
fn a_cancelled_shared_send_holds_its_memory_until_the_kernel_lets_go() {
    for b in backends() {
        check_a_cancelled_shared_send_holds_its_memory_until_the_kernel_lets_go(b);
    }
}

#[test]
fn wakes_merge_and_end_a_wait_from_any_thread() {
    for b in backends() {
        check_wakes_merge_and_end_a_wait_from_any_thread(b);
    }
}
