//! Regressions for the driver findings of the sixth review. Chains end where their member
//! never came, chained stream sends keep their order, a splice dies with its source, and
//! `pending` means a completion can come.

use super::*;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use std::sync::Arc;

fn pair(kind: SocketType) -> (OwnedFd, OwnedFd) {
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    socketpair(AddressFamily::UNIX, kind, flags, None).expect("socketpair")
}

fn shared(n: usize, byte: u8) -> Shared {
    Arc::new(vec![byte; n])
}

/// Ticks until `id` has a final completion or `limit` passes. Others are dropped.
fn wait_for(io: &mut Io, id: OpId, limit: Duration) -> Option<Completion> {
    let start = Instant::now();
    while start.elapsed() < limit {
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = io.next_completion() {
            if let Some(b) = c.buf {
                io.release(b);
                continue;
            }
            if c.op == id && c.is_final() {
                return Some(c);
            }
        }
    }
    None
}

/// A send in a chain failed for want of a slot and left the chain open, so an unrelated
/// timeout pushed next joined it and was cancelled when the chain failed.
fn check_a_failed_chain_member_drags_no_op_into_the_chain(back: Backend) {
    driver!(io, back, ops = 4, bufs = 16, size = 4096);
    let (a, b) = pair(SocketType::SEQPACKET);
    drop(b);
    let a = io.own(a);
    let t1 = io.timeout(Duration::from_secs(10)).expect("timeout");
    let _t2 = io.timeout(Duration::from_secs(10)).expect("timeout");
    let _t3 = io.timeout(Duration::from_secs(10)).expect("timeout");
    io.chain();
    io.send_shared(a, shared(8, 1), 0, 8).expect("first send");
    io.chain();
    let e = io.send_shared(a, shared(8, 2), 0, 8).expect_err("no slot");
    assert!(matches!(e, IoError::OpsFull), "{back:?} {e:?}");
    io.cancel(t1);
    let y = io.timeout(Duration::from_millis(1)).expect("timeout");
    let c = wait_for(&mut io, y, LIMIT).expect("the timeout never completed");
    assert_eq!(
        c.result,
        Ok(0),
        "{back:?} the timeout failed with the chain"
    );
}

#[test]
fn a_failed_chain_member_drags_no_op_into_the_chain() {
    for b in backends() {
        check_a_failed_chain_member_drags_no_op_into_the_chain(b);
    }
}

/// The link of a refused call survived submit and tick, so the first op of a later turn
/// linked to the op after it.
fn check_a_link_left_by_a_refused_call_dies_at_the_tick(back: Backend) {
    driver!(io, back, ops = 4, bufs = 16, size = 4096);
    let ts: Vec<_> = (0..4)
        .map(|_| io.timeout(Duration::from_secs(10)).expect("timeout"))
        .collect();
    let (a, _b) = pair(SocketType::SEQPACKET);
    let a = io.own(a);
    io.chain();
    let e = io.send_shared(a, shared(8, 1), 0, 8);
    assert!(matches!(e, Err(IoError::OpsFull)), "{back:?} {e:?}");
    for t in &ts[..3] {
        io.cancel(*t);
    }
    io.tick(Some(Duration::ZERO)).expect("tick");
    io.flush().expect("flush");
    let (r, _w) = io.pipe().expect("pipe");
    let _idle = io.ready(r, false).expect("poll");
    let y = io.timeout(Duration::from_millis(1)).expect("timeout");
    let c = wait_for(&mut io, y, Duration::from_millis(500));
    assert!(
        c.is_some(),
        "{back:?} the timeout waited behind an idle poll"
    );
}

#[test]
fn a_link_left_by_a_refused_call_dies_at_the_tick() {
    for b in backends() {
        check_a_link_left_by_a_refused_call_dies_at_the_tick(b);
    }
}

/// A call refused before it took a slot, a send too long for a buffer here, left its
/// pending link and the open chain, so the next op joined a poll that never ends.
fn check_a_call_refused_before_its_op_ends_the_chain(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let (r, _w) = io.pipe().expect("pipe");
    let (a, _b) = pair(SocketType::STREAM);
    let a = io.own(a);
    io.chain();
    let _idle = io.ready(r, false).expect("poll");
    io.chain();
    let buf = io.alloc().expect("buffer");
    let e = io.send(a, buf, 65);
    assert!(matches!(e, Err(IoError::TooLong { .. })), "{back:?} {e:?}");
    let y = io.timeout(Duration::from_millis(1)).expect("timeout");
    let c = wait_for(&mut io, y, Duration::from_millis(500));
    assert!(c.is_some(), "{back:?} the timeout joined the chain");
}

#[test]
fn a_call_refused_before_its_op_ends_the_chain() {
    for b in backends() {
        check_a_call_refused_before_its_op_ends_the_chain(b);
    }
}

/// A receive with no buffer pushes nothing until one frees, so the next op took its
/// place in the chain, behind a poll that never ends.
fn check_a_receive_waiting_for_a_buffer_ends_the_chain(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let (r, _w) = io.pipe().expect("pipe");
    let (a, _b) = pair(SocketType::STREAM);
    let a = io.own(a);
    let mut held = Vec::new();
    while let Ok(b) = io.alloc() {
        held.push(b);
    }
    io.chain();
    let _idle = io.ready(r, false).expect("poll");
    io.chain();
    let _rx = io.recv(a, false).expect("recv waits for a buffer");
    let y = io.timeout(Duration::from_millis(1)).expect("timeout");
    let c = wait_for(&mut io, y, Duration::from_millis(500));
    assert!(c.is_some(), "{back:?} the timeout took the receive's place");
}

#[test]
fn a_receive_waiting_for_a_buffer_ends_the_chain() {
    for b in backends() {
        check_a_receive_waiting_for_a_buffer_ends_the_chain(b);
    }
}

/// Two chained 1 MiB sends over a stream with a small send buffer. A short first send
/// counted as done, so the second started before the rest of the first.
fn check_chained_stream_sends_keep_their_order(back: Backend) {
    const N: usize = 1 << 20;
    driver!(io, back, ops = 16, bufs = 16, size = 4096);
    let (a, b) = pair(SocketType::STREAM);
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).expect("sndbuf");
    let a = io.own(a);
    let reader = std::thread::spawn(move || {
        let mut got = Vec::with_capacity(2 * N);
        let mut buf = vec![0u8; 65536];
        let start = Instant::now();
        while got.len() < 2 * N && start.elapsed() < LIMIT {
            match rustix::io::read(&b, &mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => panic!("read {e}"),
            }
        }
        got
    });
    io.chain();
    let s1 = io.send_shared(a, shared(N, b'A'), 0, N).expect("send A");
    let s2 = io.send_shared(a, shared(N, b'B'), 0, N).expect("send B");
    let mut results = Vec::new();
    let start = Instant::now();
    while results.len() < 2 && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = io.next_completion() {
            if (c.op == s1 || c.op == s2) && c.is_final() {
                results.push((c.op == s1, c.result));
            }
        }
    }
    let got = reader.join().expect("reader");
    assert_eq!(got.len(), 2 * N, "{back:?} bytes went missing");
    let first_b = got.iter().position(|&x| x == b'B').unwrap_or(got.len());
    let last_a = got.iter().rposition(|&x| x == b'A').unwrap_or(0);
    assert!(last_a < first_b, "{back:?} the chained sends interleaved");
    results.sort_by_key(|(a, _)| !*a);
    assert_eq!(
        results,
        [(true, Ok(N as u32)), (false, Ok(N as u32))],
        "{back:?}"
    );
}

#[test]
fn chained_stream_sends_keep_their_order() {
    for b in backends() {
        check_chained_stream_sends_keep_their_order(b);
    }
}

/// A chain of records over `SEQPACKET`, as the gateway link sends them, still lands whole
/// and in order with each send reporting its full length.
fn check_chained_seqpacket_sends_stay_whole(back: Backend) {
    driver!(io, back, ops = 16, bufs = 16, size = 4096);
    let (a, b) = pair(SocketType::SEQPACKET);
    let a = io.own(a);
    let bytes: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
    let data: Shared = Arc::new(bytes.clone());
    let cuts = [0usize, 1000, 1500, 3000];
    let mut ids = Vec::new();
    for w in cuts.windows(2) {
        if w[1] < 3000 {
            io.chain();
        }
        let id = io.send_shared(a, data.clone(), w[0], w[1] - w[0]);
        ids.push((id.expect("send"), (w[1] - w[0]) as u32));
    }
    for (id, len) in ids {
        let c = wait_for(&mut io, id, LIMIT).expect("send completes");
        assert_eq!(c.result, Ok(len), "{back:?}");
    }
    let mut buf = [0u8; 4096];
    for w in cuts.windows(2) {
        let n = rustix::io::read(&b, &mut buf).expect("record");
        assert_eq!(&buf[..n], &bytes[w[0]..w[1]], "{back:?}");
    }
}

#[test]
fn chained_seqpacket_sends_stay_whole() {
    for b in backends() {
        check_chained_seqpacket_sends_stay_whole(b);
    }
}

/// Closing a splice's source left the splice live on epoll, where it retried the raw
/// number, so it spliced from whatever took that number next.
fn check_closing_a_splice_source_cancels_it(back: Backend) {
    driver!(io, back, ops = 16, bufs = 16, size = 4096);
    let (a, b) = pair(SocketType::STREAM);
    rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096).expect("sndbuf");
    let junk = vec![0u8; 65536];
    while rustix::io::write(&a, &junk).is_ok() {}
    let a = io.own(a);
    let (pr, pw) = io.pipe().expect("pipe");
    rustix::io::write(io.fd(pw).expect("write end"), b"OLD-SOURCE").expect("fill pipe");
    let sp = io.splice(pr, APPEND, a, 64).expect("splice");
    if back == Backend::Epoll {
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        assert!(io.is_live(sp), "the splice should wait on the full socket");
    }
    assert!(io.close(pr));
    assert!(
        !io.is_live(sp),
        "{back:?} closing the source left the splice live"
    );
    let (nr, nw) = rustix::pipe::pipe().expect("pipe");
    rustix::io::write(&nw, b"OTHER-FILE-DATA").expect("fill");
    let mut buf = vec![0u8; 65536];
    let mut got = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        while let Ok(n @ 1..) = rustix::io::read(&b, &mut buf) {
            got.extend_from_slice(&buf[..n]);
        }
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while io.next_completion().is_some() {}
    }
    let leaked = got.windows(15).any(|w| w == b"OTHER-FILE-DATA");
    assert!(!leaked, "{back:?} spliced from a reused source number");
    drop(nr);
}

#[test]
fn closing_a_splice_source_cancels_it() {
    for b in backends() {
        check_closing_a_splice_source_cancels_it(b);
    }
}

/// A single shot receive waits for a free list buffer. A free ring buffer cannot arm it,
/// yet `pending` said a tick had work, so a loop on it spun.
fn check_pending_needs_a_buffer_the_receive_can_take(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 64);
    let (a, b) = pair(SocketType::STREAM);
    let a = io.own(a);
    let mut held = Vec::new();
    while let Ok(b) = io.alloc() {
        held.push(b);
    }
    let rx = io.recv(a, false).expect("recv waits for a buffer");
    assert!(
        io.ring_free > 0,
        "{back:?} the ring should keep its buffers"
    );
    let quiet = matches!(io.ring_flags(), RingFlags::Deferred | RingFlags::Coop);
    if back == Backend::Epoll || quiet {
        assert!(!io.pending(), "{back:?} pending with nothing to do");
    }
    io.release(held.pop().expect("a held buffer"));
    assert!(io.pending(), "{back:?} the receive can arm now");
    rustix::io::write(&b, b"hi").expect("write");
    let c = wait_for_buf(&mut io, rx);
    assert_eq!(c, Ok(2), "{back:?}");
}

/// Ticks until `id` lands, releasing its buffer.
fn wait_for_buf(io: &mut Io, id: OpId) -> Result<u32, Errno> {
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < LIMIT, "{id:?} never completed");
        io.tick(Some(Duration::from_millis(5))).expect("tick");
        while let Some(c) = io.next_completion() {
            if let Some(b) = c.buf {
                io.release(b);
            }
            if c.op == id {
                return c.result;
            }
        }
    }
}

#[test]
fn pending_needs_a_buffer_the_receive_can_take() {
    for b in backends() {
        check_pending_needs_a_buffer_the_receive_can_take(b);
    }
}
