//! Sockopts, sized receives, cmsgs, pipes and splice.

use super::*;
use crate::msg::{self, Control};
use std::net::TcpStream;
use std::time::Instant;

const SOL_SOCKET: i32 = 1;
const SO_RCVBUF: i32 = 8;
const SO_SNDBUF: i32 = 7;
const IPPROTO_IP: i32 = 0;
const IP_TOS: i32 = 1;
const IP_RECVTOS: i32 = 13;
const IPPROTO_TCP: i32 = 6;
const TCP_NODELAY: i32 = 1;
const TCP_ULP: i32 = 31;

/// Driver-owned connected socket and its peer.
fn pair(io: &mut Io) -> (RawFd, TcpStream) {
    let (l, addr) = listener();
    io.connect(addr).expect("connect");
    let c = pump_kind(io, Kind::Connect).expect("connect completion");
    assert!(c.result.is_ok(), "connect failed: {:?}", c.result);
    (c.fd, l.accept().expect("accept").0)
}

fn opt(io: &mut Io, fd: RawFd, level: i32, name: i32, value: &[u8]) -> OpId {
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..value.len()].copy_from_slice(value);
    io.set_sockopt(fd, level, name, b, value.len() as u32)
        .expect("set_sockopt")
}

fn result_of(io: &mut Io, id: OpId) -> Result<u32, Errno> {
    let start = Instant::now();
    let mut parked = Vec::new();
    let found = loop {
        match pump(io) {
            Some(c) if c.op == id => break c,
            Some(c) => parked.push(c),
            None => panic!("no completion for {id:?}"),
        }
        assert!(start.elapsed() < LIMIT);
    };
    assert!(
        parked.is_empty(),
        "other completions came first: {parked:?}"
    );
    found.result
}

fn check_socket_options_are_set(back: Backend) {
    use rustix::net::sockopt;
    driver!(io, back, ops = 16, bufs = 32, size = 64);
    let free = io.available();
    let (fd, _peer) = pair(&mut io);

    assert!(!sockopt::tcp_nodelay(io.fd(fd).unwrap()).unwrap());
    let id = opt(&mut io, fd, IPPROTO_TCP, TCP_NODELAY, &1i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    assert!(sockopt::tcp_nodelay(io.fd(fd).unwrap()).unwrap());
    let id = opt(&mut io, fd, IPPROTO_TCP, TCP_NODELAY, &0i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    assert!(!sockopt::tcp_nodelay(io.fd(fd).unwrap()).unwrap());

    // Kernel doubles the value.
    let id = opt(&mut io, fd, SOL_SOCKET, SO_RCVBUF, &20_000i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    let got = sockopt::socket_recv_buffer_size(io.fd(fd).unwrap()).unwrap();
    assert_eq!(got, 40_000);

    // Byte-string value. `ENOENT` when the tls module is not loaded.
    let id = opt(&mut io, fd, IPPROTO_TCP, TCP_ULP, b"tls");
    let ulp = result_of(&mut io, id);
    assert!(matches!(ulp, Ok(0) | Err(Errno::NOENT)), "{back:?} {ulp:?}");
    let id = opt(&mut io, fd, SOL_SOCKET, 9999, &1i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Err(Errno::NOPROTOOPT), "{back:?}");
    let id = opt(&mut io, 9999, SOL_SOCKET, SO_RCVBUF, &1i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Err(Errno::BADF));

    assert!(io.close(fd));
    assert_eq!(io.available(), free, "an option's buffer leaked");
    assert_eq!(io.in_flight(), 0);
}

/// A failure cancels the rest of the chain.
fn check_linked_options_run_in_order(back: Backend) {
    use rustix::net::sockopt;
    driver!(io, back, ops = 16, bufs = 32, size = 64);
    let (fd, _peer) = pair(&mut io);

    io.chain();
    let a = opt(&mut io, fd, IPPROTO_TCP, TCP_NODELAY, &1i32.to_ne_bytes());
    io.chain();
    let b = opt(&mut io, fd, SOL_SOCKET, SO_RCVBUF, &30_000i32.to_ne_bytes());
    let c = opt(&mut io, fd, SOL_SOCKET, SO_SNDBUF, &30_000i32.to_ne_bytes());
    let mut order = Vec::new();
    for _ in 0..3 {
        let done = pump(&mut io).expect("completion");
        assert_eq!(done.result, Ok(0));
        order.push(done.op);
    }
    assert_eq!(order, [a, b, c]);
    assert!(sockopt::tcp_nodelay(io.fd(fd).unwrap()).unwrap());
    assert_eq!(
        sockopt::socket_recv_buffer_size(io.fd(fd).unwrap()).unwrap(),
        60_000
    );
    assert_eq!(
        sockopt::socket_send_buffer_size(io.fd(fd).unwrap()).unwrap(),
        60_000
    );

    io.chain();
    let a = opt(&mut io, fd, SOL_SOCKET, 9999, &1i32.to_ne_bytes());
    let b = opt(&mut io, fd, SOL_SOCKET, SO_RCVBUF, &50_000i32.to_ne_bytes());
    let first = pump(&mut io).expect("completion");
    let second = pump(&mut io).expect("completion");
    assert_eq!((first.op, second.op), (a, b));
    assert!(first.result.is_err());
    let size = sockopt::socket_recv_buffer_size(io.fd(fd).unwrap()).unwrap();
    match back {
        Backend::Uring => {
            assert_eq!(second.result, Err(Errno::CANCELED));
            assert_eq!(size, 60_000, "a cancelled option was applied");
        }
        // No links on epoll, the second still runs.
        Backend::Epoll => {
            assert_eq!(second.result, Ok(0));
            assert_eq!(size, 100_000);
        }
    }
    // Unlinked after the chain.
    let id = opt(&mut io, fd, SOL_SOCKET, SO_RCVBUF, &10_000i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    assert!(io.close(fd));
    assert_eq!(io.in_flight(), 0);
}

/// Leaves the excess in the socket.
fn check_a_sized_receive_takes_exactly_its_length(back: Backend) {
    driver!(io, back, ops = 16, bufs = 32, size = 64);
    let free = io.available();
    let (fd, mut peer) = pair(&mut io);

    let buf = io.alloc().expect("buffer");
    io.buf_mut(&buf).fill(b'.');
    io.buf_mut(&buf)[..4].copy_from_slice(b"head");
    let at = buf.index();
    let id = io.recv_into(fd, buf, 4, 10).expect("recv_into");
    let writer = std::thread::spawn(move || {
        // Three pieces, the last overshoots.
        for piece in [&b"abc"[..], b"defg", b"hijKLMNO"] {
            peer.write_all(piece).expect("write");
            peer.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(30));
        }
        peer
    });
    let c = pump(&mut io).expect("recv_into completion");
    assert_eq!(c.op, id);
    assert_eq!(c.kind, Kind::RecvInto);
    assert_eq!(c.result, Ok(10), "the receive ended before its length");
    assert!(c.is_final());
    let b = c.buf.expect("the buffer comes back");
    assert_eq!(b.id.index(), at);
    assert_eq!(io.filled(&b), b"headabcdefghij");
    assert_eq!(io.bufs().bytes(&b.id)[14], b'.', "received past its length");
    let peer = writer.join().expect("writer");

    io.recv(fd, false).expect("recv");
    let c = pump(&mut io).expect("recv completion");
    let rest = c.buf.expect("buffer");
    assert_eq!(io.filled(&rest), b"KLMNO");
    io.release(rest);

    // EOF first gives a short result.
    let id = io.recv_into(fd, b.id, 0, 8).expect("recv_into");
    (&peer).write_all(b"xyz").expect("write");
    drop(peer);
    let c = pump(&mut io).expect("short completion");
    assert_eq!(c.op, id);
    assert_eq!(c.result, Ok(3));
    let b = c.buf.expect("buffer");
    assert_eq!(io.filled(&b), b"xyz");
    // Then zero, buffer still handed back.
    io.recv_into(fd, b.id, 0, 8).expect("recv_into");
    let c = pump(&mut io).expect("end completion");
    assert_eq!(c.result, Ok(0));
    let b = c.buf.expect("buffer");
    assert_eq!(b.len, 0);

    // Failure releases the buffer.
    io.recv_into(9999, b.id, 0, 8).expect("submit");
    if back == Backend::Uring {
        let c = pump(&mut io).expect("failure completion");
        assert_eq!(c.result, Err(Errno::BADF));
        assert!(c.buf.is_none());
        assert_eq!(io.available(), free);
    }
    assert!(io.close(fd));
}

fn check_a_cancelled_sized_receive_returns_its_buffer(back: Backend) {
    driver!(io, back, ops = 16, bufs = 32, size = 64);
    let free = io.available();
    let (fd, _peer) = pair(&mut io);
    let buf = io.alloc().expect("buffer");
    let id = io.recv_into(fd, buf, 0, 16).expect("recv_into");
    io.flush().expect("flush");
    assert!(io.cancel(id));
    let start = Instant::now();
    while io.available() < free && start.elapsed() < LIMIT {
        io.tick(Some(Duration::from_millis(10))).expect("tick");
        while io.next_completion().is_some() {}
    }
    assert_eq!(io.available(), free);
    assert!(io.close(fd));
}

/// One `recv_msg` delivery, buffer already released.
struct Got {
    c: Completion,
    payload: Vec<u8>,
    /// Level, type, data.
    control: Option<(i32, i32, Vec<u8>)>,
}

fn next_msg(io: &mut Io, id: OpId) -> Got {
    let c = pump(io).expect("recv_msg completion");
    assert_eq!(c.op, id);
    read_msg(io, c)
}

fn read_msg(io: &mut Io, mut c: Completion) -> Got {
    assert_eq!(c.kind, Kind::RecvMsg);
    let buf = c.buf.take();
    let mut got = Got {
        c,
        payload: Vec::new(),
        control: None,
    };
    let Some(b) = buf else {
        return got;
    };
    let m = io.msg(&b).expect("a message");
    assert_eq!(b.len as usize, msg::HEAD + m.payload.len());
    assert_eq!(
        got.c.result,
        Ok(m.payload.len() as u32),
        "the result is the payload"
    );
    got.payload = m.payload.to_vec();
    got.control = m.control.map(|k| (k.level, k.kind, k.data.to_vec()));
    io.release(b);
    got
}

fn check_messages_arrive_behind_a_header(back: Backend) {
    driver!(io, back, ops = 16, bufs = 32, size = 256);
    let free = io.available();
    let (fd, mut peer) = pair(&mut io);

    let id = io.recv_msg(fd).expect("recv_msg");
    io.flush().expect("flush");
    io.tick(Some(Duration::from_millis(20))).expect("tick");
    assert_eq!(io.available(), free, "an armed receive held a buffer");
    for text in [&b"first"[..], b"the second one"] {
        peer.write_all(text).expect("write");
        let got = next_msg(&mut io, id);
        assert!(got.c.more);
        assert_eq!(got.payload, text);
        assert_eq!(got.control, None, "a plain stream has no control message");
    }
    // Larger than one buffer arrives in pieces.
    let big: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    peer.write_all(&big).expect("write");
    let mut all = Vec::new();
    while all.len() < big.len() {
        let got = next_msg(&mut io, id);
        assert!(got.payload.len() <= 256 - msg::HEAD);
        all.extend_from_slice(&got.payload);
    }
    assert_eq!(all, big);

    drop(peer);
    let end = next_msg(&mut io, id);
    assert_eq!(end.c.result, Ok(0));
    assert!(end.c.is_final());
    assert!(end.payload.is_empty());
    assert!(!io.is_live(id));
    assert!(io.close(fd));
    assert_eq!(io.available(), free, "a receive buffer leaked");
    assert_eq!(io.in_flight(), 0);
}

/// Collect payloads until the final completion.
fn drain_msgs(io: &mut Io, id: OpId) -> (Vec<u8>, Completion) {
    let mut tail = Vec::new();
    loop {
        let got = next_msg(io, id);
        tail.extend_from_slice(&got.payload);
        if got.c.is_final() {
            return (tail, got.c);
        }
    }
}

fn check_a_stopped_receive_loses_nothing(back: Backend) {
    driver!(io, back, ops = 16, bufs = 32, size = 256);
    let free = io.available();
    let (fd, mut peer) = pair(&mut io);
    let id = io.recv_msg(fd).expect("recv_msg");
    peer.write_all(b"last words").expect("write");
    std::thread::sleep(Duration::from_millis(30));
    assert!(io.stop(id));
    let (tail, end) = drain_msgs(&mut io, id);
    assert_eq!(end.result, Err(Errno::CANCELED));
    assert!(!io.is_live(id));
    assert!(!io.stop(id), "stopping twice reported success");
    // On epoll nothing was in flight, bytes stay in the socket.
    let want: &[u8] = match back {
        Backend::Uring => b"last words",
        Backend::Epoll => b"",
    };
    assert_eq!(tail, want, "a stop dropped bytes");

    let id = io.recv_msg(fd).expect("recv_msg again");
    if back == Backend::Epoll {
        assert_eq!(next_msg(&mut io, id).payload, b"last words");
    }
    peer.write_all(b"and more").expect("write");
    assert_eq!(next_msg(&mut io, id).payload, b"and more");

    // Same for a plain repeating receive.
    assert!(io.stop(id));
    assert_eq!(drain_msgs(&mut io, id).1.result, Err(Errno::CANCELED));
    let id = io.recv(fd, true).expect("recv");
    peer.write_all(b"plain").expect("write");
    std::thread::sleep(Duration::from_millis(30));
    assert!(io.stop(id));
    let mut tail = Vec::new();
    let end = loop {
        let mut c = pump(&mut io).expect("completion after a stop");
        assert_eq!(c.op, id);
        let more = !c.is_final();
        let Some(b) = c.buf.take_if(|_| more) else {
            break c;
        };
        tail.extend_from_slice(io.filled(&b));
        io.release(b);
    };
    assert_eq!(end.result, Err(Errno::CANCELED));
    if back == Backend::Uring {
        assert_eq!(tail, b"plain");
    }
    assert!(io.close(fd));
    assert_eq!(io.available(), free, "a receive buffer leaked");
}

/// Cancel may race a delivery. The picked buffer must return to the ring.
#[test]
fn a_cancel_that_races_a_delivery_keeps_the_ring_whole() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    driver!(io, Backend::Uring, ops = 16, bufs = 32, size = 256);
    let free = io.available();
    let (fd, mut peer) = pair(&mut io);
    for round in 0..40 {
        let id = match round % 2 {
            0 => io.recv_msg(fd).expect("recv_msg"),
            _ => io.recv(fd, true).expect("recv"),
        };
        io.flush().expect("flush");
        peer.write_all(b"in flight").expect("write");
        // Let the kernel complete it unseen.
        std::thread::sleep(Duration::from_millis(2));
        assert!(io.cancel(id));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(20) {
            io.tick(Some(Duration::from_millis(5))).expect("tick");
            assert!(
                io.next_completion().is_none(),
                "a cancelled receive reported"
            );
        }
    }
    assert_eq!(io.available(), free, "ring buffers were lost to cancels");
    // More deliveries than ring buffers proves they are reusable.
    let id = io.recv_msg(fd).expect("recv_msg");
    let mut all = Vec::new();
    for _ in 0..40 {
        peer.write_all(b"again").expect("write");
        all.extend_from_slice(&next_msg(&mut io, id).payload);
    }
    assert!(all.len() >= 200 && all.ends_with(b"again"));
    assert!(io.close(fd));
}

/// Send completion and resulting delivery, in either order.
fn sent_and_received(io: &mut Io, sent: OpId, recv: OpId) -> (Completion, Got) {
    let (mut done, mut got) = (None, None);
    loop {
        if done.is_some()
            && got.is_some()
            && let (Some(c), Some(g)) = (done.take(), got.take())
        {
            return (c, g);
        }
        let c = pump(io).expect("completion");
        if c.op == sent {
            done = Some(c);
            continue;
        }
        assert_eq!(c.op, recv);
        got = Some(read_msg(io, c));
    }
}

/// Datagram round trip carrying `IP_TOS`.
fn tos_round(io: &mut Io, tx: RawFd, recv: OpId, tos: u8, text: &[u8]) {
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..text.len()].copy_from_slice(text);
    let control = Control {
        level: IPPROTO_IP,
        kind: IP_TOS,
        data: &i32::from(tos).to_ne_bytes(),
    };
    let sent = io
        .send_msg(tx, b, text.len() as u32, control)
        .expect("send_msg");
    let (c, got) = sent_and_received(io, sent, recv);
    assert_eq!((c.kind, c.result), (Kind::SendMsg, Ok(text.len() as u32)));
    assert!(c.buf.is_none(), "a send returns no buffer");
    assert!(got.c.more, "a datagram ended the receive");
    assert_eq!(got.payload, text);
    let want = Some((IPPROTO_IP, IP_TOS, vec![tos]));
    assert_eq!(got.control, want, "the control message was not applied");
}

#[test]
fn a_control_message_travels_each_way_on_the_ring() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    driver!(io, Backend::Uring, ops = 16, bufs = 32, size = 256);
    let free = io.available();
    let rx = io.bind_udp("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = io.local_addr(rx).expect("addr");
    let tx = io.connect_udp(addr).expect("connect");
    let id = opt(&mut io, rx, IPPROTO_IP, IP_RECVTOS, &1i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    let recv = io.recv_msg(rx).expect("recv_msg");
    tos_round(&mut io, tx, recv, 0x28, b"expedited");
    tos_round(&mut io, tx, recv, 0x00, b"plain");
    tos_round(&mut io, tx, recv, 0xb8, b"voice");

    // Kernel ends a multishot on an empty datagram.
    let b = io.alloc().expect("buffer");
    let sent = io.send(tx, b, 0).expect("send");
    let (_, end) = sent_and_received(&mut io, sent, recv);
    assert_eq!(end.c.result, Ok(0));
    assert!(end.c.is_final());
    assert!(!io.is_live(recv));
    let recv = io.recv_msg(rx).expect("recv_msg again");
    tos_round(&mut io, tx, recv, 0x10, b"on");

    // Oversized cmsg refused before submit.
    let before = io.available();
    let b = io.alloc().expect("buffer");
    let long = [0u8; msg::CMSG_MAX + 1];
    let control = Control {
        level: IPPROTO_IP,
        kind: IP_TOS,
        data: &long,
    };
    assert!(matches!(
        io.send_msg(tx, b, 1, control),
        Err(IoError::ControlTooLong { .. })
    ));
    assert_eq!(io.available(), before, "the refused send kept its buffer");
    assert!(io.cancel(recv));
    assert!(io.close(rx));
    assert!(io.close(tx));
    let start = Instant::now();
    while io.available() < free && start.elapsed() < LIMIT {
        let _ = io.tick(Some(Duration::from_millis(10)));
        while io.next_completion().is_some() {}
    }
    assert_eq!(io.available(), free);
}

#[test]
fn the_readiness_backend_sends_a_control_message_too() {
    driver!(io, Backend::Epoll, ops = 16, bufs = 8, size = 256);
    let (fd, _peer) = pair(&mut io);
    let b = io.alloc().expect("buffer");
    let control = Control {
        level: IPPROTO_IP,
        kind: IP_TOS,
        data: &[0],
    };
    let id = io.send_msg(fd, b, 1, control).expect("send_msg");
    assert_eq!(result_of(&mut io, id), Ok(1));
    assert_eq!(io.available(), 8, "the buffer of a send leaked");
}

#[test]
fn a_receive_with_control_needs_room_behind_its_header() {
    for back in backends() {
        driver!(io, back, ops = 8, bufs = 16, size = 64);
        assert!(matches!(io.recv_msg(0), Err(IoError::BadArena { .. })));
    }
    if backends().contains(&Backend::Uring) {
        // Too few buffers for a provided ring.
        driver!(io, Backend::Uring, ops = 8, bufs = 4, size = 256);
        assert!(matches!(io.recv_msg(0), Err(IoError::NoBufRing)));
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

fn drain(mut peer: TcpStream) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut got = Vec::new();
        peer.read_to_end(&mut got).expect("read");
        got
    })
}

/// Splice `len` bytes, continuing short ones. Returns (ops, short ops).
fn splice_all(io: &mut Io, from: RawFd, mut offset: u64, to: RawFd, len: usize) -> (u32, u32) {
    let (mut ops, mut short, mut left) = (0, 0, len);
    while left > 0 {
        if short > 0 {
            // Wait for room instead of spinning.
            let id = io.ready(to, true).expect("ready");
            let c = pump(io).expect("ready completion");
            assert_eq!((c.op, c.kind, c.result), (id, Kind::Ready, Ok(0)));
        }
        let id = io.splice(from, offset, to, left as u32).expect("splice");
        let c = pump(io).expect("splice completion");
        assert_eq!(c.op, id);
        assert_eq!(c.kind, Kind::Splice);
        ops += 1;
        let n = match c.result {
            Ok(n) => n as usize,
            Err(Errno::AGAIN) => 0,
            Err(e) => panic!("splice failed with {e}"),
        };
        assert!(n <= left, "a splice moved more than it was asked");
        short += u32::from(n < left);
        left -= n;
        if offset != APPEND {
            offset += n as u64;
        }
    }
    (ops, short)
}

fn check_a_file_goes_through_a_pipe_to_a_socket(back: Backend) {
    const PIECE: usize = 1 << 16;
    let body = pattern(1_000_003);
    let p = scratch_file("splice", back, &body);
    driver!(io, back, ops = 16, bufs = 8, size = 512);
    let (sock, peer) = pair(&mut io);
    let reader = drain(peer);
    io.open(&path_bytes(&p)).expect("open");
    let file = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    let (pr, pw) = io.pipe().expect("pipe");

    let mut at = 0;
    while at < body.len() {
        let n = PIECE.min(body.len() - at);
        // Empty pipe takes a whole piece.
        let id = io.splice(file, at as u64, pw, n as u32).expect("splice in");
        let c = pump(&mut io).expect("splice in completion");
        assert_eq!((c.op, c.result), (id, Ok(n as u32)));
        splice_all(&mut io, pr, APPEND, sock, n);
        at += n;
    }
    // Zero past EOF.
    let id = io.splice(file, body.len() as u64, pw, 16).expect("splice");
    assert_eq!(result_of(&mut io, id), Ok(0));

    for fd in [sock, file, pr, pw] {
        assert!(io.close(fd));
    }
    let got = reader.join().expect("reader");
    assert_eq!(got.len(), body.len());
    assert!(got == body, "the bytes that arrived are not the file's");
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn linked_splices_carry_a_file() {
    if !backends().contains(&Backend::Uring) {
        return;
    }
    const PIECE: usize = 1 << 16;
    let body = pattern(700_001);
    let p = scratch_file("linked", Backend::Uring, &body);
    driver!(io, Backend::Uring, ops = 16, bufs = 8, size = 512);
    let (sock, peer) = pair(&mut io);
    let reader = drain(peer);
    io.open(&path_bytes(&p)).expect("open");
    let file = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    let (pr, pw) = io.pipe().expect("pipe");

    let (mut at, mut pairs, mut cut) = (0, 0u32, 0u32);
    while at < body.len() {
        let n = PIECE.min(body.len() - at);
        io.chain();
        let a = io.splice(file, at as u64, pw, n as u32).expect("splice in");
        let b = io.splice(pr, APPEND, sock, n as u32).expect("splice out");
        let first = pump(&mut io).expect("first completion");
        let second = pump(&mut io).expect("second completion");
        assert_eq!(
            (first.op, second.op),
            (a, b),
            "the pair completed out of order"
        );
        assert_eq!(first.result, Ok(n as u32));
        pairs += 1;
        // Leftover stays in the pipe.
        let out = second.result.expect("the second splice") as usize;
        if out < n {
            cut += 1;
            splice_all(&mut io, pr, APPEND, sock, n - out);
        }
        at += n;
    }
    eprintln!("linked splices: {pairs} pairs, {cut} with the second cut short");

    // A short first splice breaks the link, the second is cancelled.
    io.chain();
    let a = io
        .splice(file, body.len() as u64 - 100, pw, 4096)
        .expect("splice in");
    let b = io.splice(pr, APPEND, sock, 4096).expect("splice out");
    let first = pump(&mut io).expect("first completion");
    let second = pump(&mut io).expect("second completion");
    assert_eq!((first.op, first.result), (a, Ok(100)));
    assert_eq!((second.op, second.result), (b, Err(Errno::CANCELED)));
    splice_all(&mut io, pr, APPEND, sock, 100);

    for fd in [sock, file, pr, pw] {
        assert!(io.close(fd));
    }
    let got = reader.join().expect("reader");
    let mut want = body.clone();
    want.extend_from_slice(&body[body.len() - 100..]);
    assert!(got == want, "the bytes that arrived are not the file's");
    let _ = std::fs::remove_file(&p);
}

/// Slow reader. Unmoved bytes stay in the pipe in order.
fn check_a_splice_into_a_full_socket_is_cut_short(back: Backend) {
    const PIECE: usize = 1 << 16;
    let body = pattern(40 * PIECE);
    let p = scratch_file("slow", back, &body);
    driver!(io, back, ops = 16, bufs = 32, size = 512);
    let (sock, mut peer) = pair(&mut io);
    // Small buffers so it fills at once.
    let id = opt(&mut io, sock, SOL_SOCKET, SO_SNDBUF, &8192i32.to_ne_bytes());
    assert_eq!(result_of(&mut io, id), Ok(0));
    rustix::net::sockopt::set_socket_recv_buffer_size(&peer, 8192).expect("rcvbuf");
    io.open(&path_bytes(&p)).expect("open");
    let file = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    let (pr, pw) = io.pipe().expect("pipe");

    let reader = std::thread::spawn(move || {
        // Let the sender stall first.
        std::thread::sleep(Duration::from_millis(300));
        let mut got = Vec::new();
        let mut chunk = [0u8; 3000];
        loop {
            match peer.read(&mut chunk).expect("read") {
                0 => break,
                n => got.extend_from_slice(&chunk[..n]),
            }
            let pause = u64::from(got.len() % 7 == 0) * 200;
            std::thread::sleep(Duration::from_micros(pause));
        }
        got
    });

    let (mut ops, mut short, mut at) = (0, 0, 0);
    while at < body.len() {
        let id = io
            .splice(file, at as u64, pw, PIECE as u32)
            .expect("splice in");
        let c = pump(&mut io).expect("splice in completion");
        assert_eq!((c.op, c.result), (id, Ok(PIECE as u32)));
        let (o, s) = splice_all(&mut io, pr, APPEND, sock, PIECE);
        ops += o;
        short += s;
        at += PIECE;
    }
    eprintln!("{back:?}: 40 pieces left in {ops} splices, {short} of them short");
    assert!(
        short > 0,
        "the socket never filled, the test showed nothing"
    );
    for fd in [sock, file, pr, pw] {
        assert!(io.close(fd));
    }
    let got = reader.join().expect("reader");
    assert_eq!(got.len(), body.len());
    assert!(
        got == body,
        "bytes were lost or reordered around a short splice"
    );
    let _ = std::fs::remove_file(&p);
}

fn check_a_pipe_carries_bytes(back: Backend) {
    driver!(io, back, ops = 8, bufs = 8, size = 512);
    let (r, w) = io.pipe().expect("pipe");
    assert!(io.fd(r).is_some() && io.fd(w).is_some());
    let b = io.alloc().expect("buffer");
    io.buf_mut(&b)[..5].copy_from_slice(b"piped");
    io.write(w, b, 5, APPEND).expect("write");
    assert_eq!(pump(&mut io).expect("write").result, Ok(5));
    io.read_stream(r).expect("read");
    let c = pump(&mut io).expect("read");
    let got = c.buf.expect("buffer");
    assert_eq!(io.filled(&got), b"piped");
    io.release(got);
    // File to file has no pipe, `EINVAL`.
    let p = scratch_file("nopipe", back, b"abc");
    io.open_rw(&path_bytes(&p)).expect("open");
    let f = pump(&mut io).expect("open").result.expect("opens") as RawFd;
    let id = io.splice(f, 0, f, 3).expect("splice");
    assert_eq!(result_of(&mut io, id), Err(Errno::INVAL));
    let id = io.splice(9999, 0, w, 3).expect("splice");
    assert_eq!(result_of(&mut io, id), Err(Errno::BADF));
    for fd in [r, w, f] {
        assert!(io.close(fd));
    }
    assert_eq!(io.in_flight(), 0);
    let _ = std::fs::remove_file(&p);
}

/// A length past the buffer size was cut to it silently, which a sized receive took for
/// end of stream. It is refused now, the buffer freed, and a whole buffer still moves.
fn check_a_length_past_the_buffer_is_refused(back: Backend) {
    use std::io::Read as _;
    driver!(io, back, ops = 8, bufs = 4, size = 64);
    let (fd, mut peer) = pair(&mut io);
    let (r, w) = io.pipe().expect("pipe");
    let free = io.available();
    let refused =
        |r: Result<OpId, IoError>| matches!(r, Err(IoError::TooLong { len: 65, max: 64 }));
    let to = SocketAddr::from(([127, 0, 0, 1], 9));
    let control = Control {
        level: IPPROTO_IP,
        kind: IP_TOS,
        data: &[0],
    };
    let b = io.alloc().expect("buffer");
    assert!(refused(io.send(fd, b, 65)), "{back:?} send");
    let b = io.alloc().expect("buffer");
    assert!(refused(io.send_to(fd, b, 65, to)), "{back:?} send_to");
    let b = io.alloc().expect("buffer");
    assert!(
        refused(io.send_msg(fd, b, 65, control)),
        "{back:?} send_msg"
    );
    let b = io.alloc().expect("buffer");
    assert!(refused(io.write(w, b, 65, APPEND)), "{back:?} write");
    let b = io.alloc().expect("buffer");
    let opt = io.set_sockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, b, 65);
    assert!(refused(opt), "{back:?} set_sockopt");
    let b = io.alloc().expect("buffer");
    assert!(refused(io.recv_into(fd, b, 0, 65)), "{back:?} recv_into");
    let b = io.alloc().expect("buffer");
    assert!(refused(io.recv_into(fd, b, 60, 5)), "{back:?} recv_into at");
    let b = io.alloc().expect("buffer");
    assert!(
        matches!(
            io.recv_into(fd, b, u32::MAX, u32::MAX),
            Err(IoError::TooLong { max: 64, .. })
        ),
        "{back:?} recv_into overflow"
    );
    let b = io.alloc().expect("buffer");
    let read = io.read_at_into(r, 0, b, 1, 64);
    assert!(refused(read), "{back:?} read_at_into");
    assert_eq!(
        io.available(),
        free,
        "{back:?} a refused call kept its buffer"
    );
    assert_eq!(io.in_flight(), 0, "{back:?} a refused call left an op");

    let b = io.alloc().expect("buffer");
    io.buf_mut(&b).fill(7);
    let id = io.send(fd, b, 64).expect("send");
    assert_eq!(result_of(&mut io, id), Ok(64), "{back:?}");
    let mut got = [0u8; 64];
    peer.read_exact(&mut got).expect("read");
    assert_eq!(got, [7u8; 64], "{back:?}");

    // A range ending at the buffer's end fills to it.
    let b = io.alloc().expect("buffer");
    let id = io.recv_into(fd, b, 60, 4).expect("recv_into");
    peer.write_all(b"tail").expect("write");
    let c = pump(&mut io).expect("recv_into completion");
    assert_eq!((c.op, c.result), (id, Ok(4)), "{back:?}");
    let b = c.buf.expect("the buffer comes back");
    assert_eq!(&io.filled(&b)[60..], b"tail", "{back:?}");
    io.release(b);
    for fd in [fd, r, w] {
        assert!(io.close(fd));
    }
}

macro_rules! both {
    ($($name:ident => $check:ident,)*) => {$(
        #[test]
        fn $name() {
            for b in backends() {
                $check(b);
            }
        }
    )*};
}

both! {
    socket_options_are_set => check_socket_options_are_set,
    linked_options_run_in_order => check_linked_options_run_in_order,
    a_sized_receive_takes_exactly_its_length => check_a_sized_receive_takes_exactly_its_length,
    a_cancelled_sized_receive_returns_its_buffer => check_a_cancelled_sized_receive_returns_its_buffer,
    messages_arrive_behind_a_header => check_messages_arrive_behind_a_header,
    a_stopped_receive_loses_nothing => check_a_stopped_receive_loses_nothing,
    a_file_goes_through_a_pipe_to_a_socket => check_a_file_goes_through_a_pipe_to_a_socket,
    a_splice_into_a_full_socket_is_cut_short => check_a_splice_into_a_full_socket_is_cut_short,
    a_pipe_carries_bytes => check_a_pipe_carries_bytes,
    a_length_past_the_buffer_is_refused => check_a_length_past_the_buffer_is_refused,
}
