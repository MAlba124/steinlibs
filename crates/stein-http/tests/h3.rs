//! h3::Client against a scripted quinn-proto peer, both sans-IO, no network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "h3/peer.rs"]
mod peer;

use std::net::SocketAddr;
use std::time::Duration;

use peer::{Got, Peer, Reply, frame_header, headers_frame, ok, varint};
use stein_http::Header;
use stein_http::HeaderError;
use stein_http::h3::quinn_proto as q;
use stein_http::h3::{
    self, Client, Config, ConnId, ConnState, Error, Event, H3Error, HeapStorage, Nanos, QuicLoss,
    Request, RequestId, frame,
};

const CLIENT: &str = "10.0.0.1:50000";
const SERVER: &str = "10.0.0.2:443";
/// One way delay per exchange.
const HOP: u64 = 500_000;
const SECOND: u64 = 1_000_000_000;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

struct Net<'a> {
    c: Client<'a>,
    conn: ConnId,
    p: Peer,
    now: Nanos,
    buf: Vec<u8>,
    log: Vec<Event>,
}

impl<'a> Net<'a> {
    /// A client on `storage`, connected to a peer answering with `answer`.
    fn up(storage: &'a mut HeapStorage, cfg: Config, peer: Peer) -> Net<'a> {
        let mut net = Net::start(storage, cfg, peer);
        let conn = net.conn;
        let connected = net.run(5 * SECOND, |_, e| e == Event::Connected { conn });
        assert!(connected, "handshake done: {:?}", net.log);
        assert_eq!(net.c.state(conn), Some(ConnState::Up));
        net
    }

    fn start(storage: &'a mut HeapStorage, cfg: Config, peer: Peer) -> Net<'a> {
        let mut c = Client::new(cfg, peer::client_crypto(), storage.view()).unwrap();
        let conn = c.connect(Nanos(0), "origin.test", addr(SERVER)).unwrap();
        Net {
            c,
            conn,
            p: peer,
            now: Nanos(0),
            buf: vec![0u8; 2048],
            log: Vec::new(),
        }
    }

    /// Move datagrams both ways once. True when any moved.
    fn exchange(&mut self) -> bool {
        let mut to_peer = Vec::new();
        while let Some(n) = self.c.poll_transmit(self.conn, self.now, &mut self.buf) {
            to_peer.push(self.buf[..n].to_vec());
        }
        if !to_peer.is_empty() {
            self.now.0 += HOP;
        }
        let mut to_client = Vec::new();
        for d in &to_peer {
            self.p
                .input(self.now, Some((addr(CLIENT), d)), &mut to_client);
        }
        self.p.input(self.now, None, &mut to_client);
        if !to_client.is_empty() {
            self.now.0 += HOP;
        }
        for d in &to_client {
            self.c.handle_datagram(self.conn, self.now, d).unwrap();
        }
        !to_peer.is_empty() || !to_client.is_empty()
    }

    /// Run until `on` says stop or nothing is due before `limit` from now.
    fn run(&mut self, limit: u64, mut on: impl FnMut(&mut Client<'a>, Event) -> bool) -> bool {
        let limit = Nanos(self.now.0 + limit);
        loop {
            while let Some(e) = self.c.poll_event() {
                self.log.push(e);
                if on(&mut self.c, e) {
                    return true;
                }
            }
            if self.exchange() {
                continue;
            }
            let next = [self.c.poll_timeout(self.conn), self.p.timeout()]
                .into_iter()
                .flatten()
                .min();
            let Some(t) = next.filter(|t| *t <= limit) else {
                return false;
            };
            self.now = self.now.max(t);
            self.c.handle_timeout(self.conn, self.now);
            let mut out = Vec::new();
            self.p.input(self.now, None, &mut out);
            for d in &out {
                self.c.handle_datagram(self.conn, self.now, d).unwrap();
            }
        }
    }

    fn get(&mut self, path: &str, headers: &[Header<'_>]) -> RequestId {
        let target = Request {
            method: "GET",
            authority: "origin.test",
            path,
        };
        self.c.submit(self.conn, &target, headers, None).unwrap()
    }

    /// Run `req` to its end. Returns (status, body, terminal event).
    fn fetch(&mut self, req: RequestId, piece: usize) -> (u16, Vec<u8>, Event) {
        let mut body = Vec::new();
        let mut status = 0;
        let mut end = None;
        let mut out = vec![0u8; piece];
        let now = self.now;
        // A Body event may have been polled past already.
        while let Ok(n @ 1..) = self.c.read_body(req, now, &mut out) {
            body.extend_from_slice(&out[..n]);
        }
        self.run(10 * SECOND, |c, e| {
            match e {
                Event::Head { req: r, status: s } if r == req => status = s,
                Event::Body { req: r } if r == req => loop {
                    let n = c.read_body(req, now, &mut out).unwrap();
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&out[..n]);
                },
                Event::Done { req: r }
                | Event::Reset { req: r, .. }
                | Event::Failed { req: r, .. }
                | Event::Unprocessed { req: r }
                    if r == req =>
                {
                    end = Some(e)
                }
                Event::Closed { .. } => end = Some(e),
                _ => {}
            }
            end.is_some()
        });
        (status, body, end.expect("the request ended"))
    }
}

fn answering(f: impl FnMut(&Got) -> Reply + 'static) -> Peer {
    Peer::new(9, Box::new(f))
}

fn storage() -> HeapStorage {
    HeapStorage::new(2, 8, 4096)
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn a_get_reads_its_head_and_a_body_of_many_data_frames() {
    let body = pattern(30_000);
    let reply = body.clone();
    let peer = answering(move |_| ok(200, &[("x-a", "1"), ("x-b", "two")], &reply, 7));
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let req = net.get("/file?x=1", &[Header::new("User-Agent", "stein")]);
    let mut seen = None;
    let now = net.now;
    let mut got = Vec::new();
    net.run(10 * SECOND, |c, e| match e {
        Event::Head { req: r, status } => {
            assert_eq!((r, status), (req, 200));
            assert_eq!(c.status(req), Some(200));
            seen = Some((
                c.header_str(req, "X-B").map(str::to_string),
                c.header_count(req),
                c.headers(req).count(),
            ));
            false
        }
        Event::Body { req: r } => {
            let mut out = [0u8; 1000];
            loop {
                let n = c.read_body(r, now, &mut out).unwrap();
                if n == 0 {
                    break false;
                }
                got.extend_from_slice(&out[..n]);
            }
        }
        Event::Done { req: r } => r == req,
        _ => false,
    });
    assert_eq!(seen, Some((Some("two".to_string()), 2, 2)));
    assert_eq!(got, body);
    // The row went with Done.
    assert_eq!(
        net.c.read_body(req, net.now, &mut [0; 8]),
        Err(Error::UnknownRequest)
    );
    assert_eq!(net.c.header(req, "x-a"), None);
    let r = &net.p.got[0];
    assert_eq!(r.field(":method"), Some("GET"));
    assert_eq!(r.field(":scheme"), Some("https"));
    assert_eq!(r.field(":authority"), Some("origin.test"));
    assert_eq!(r.field(":path"), Some("/file?x=1"));
    assert_eq!(r.field("user-agent"), Some("stein"));
    assert_eq!(r.stream, 0);
}

#[test]
fn a_ranged_get_gets_its_part() {
    let whole = pattern(10_000);
    let source = whole.clone();
    let peer = answering(move |g| {
        let range = g.field("range").unwrap_or("bytes=0-");
        let spec = range.trim_start_matches("bytes=");
        let (a, b) = spec.split_once('-').unwrap();
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        let cr = format!("bytes {a}-{b}/{}", source.len());
        ok(206, &[("content-range", &cr)], &source[a..=b], 3)
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let req = net.get("/v", &[Header::new("Range", "bytes=1000-4999")]);
    let (status, body, end) = net.fetch(req, 777);
    assert_eq!((status, end), (206, Event::Done { req }));
    assert_eq!(body, whole[1000..5000]);
}

#[test]
fn a_post_body_goes_out_as_the_window_allows() {
    let peer = answering(|g| {
        let n = g.body.len().to_string();
        ok(200, &[("x-got", &n)], b"", 1)
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let body = pattern(3_000_000);
    let target = Request {
        method: "POST",
        authority: "origin.test",
        path: "/up",
    };
    let len = Some(body.len() as u64);
    let req = net.c.submit(net.conn, &target, &[], len).unwrap();
    assert_eq!(net.c.write_body(req, b"short"), Err(Error::BodyMismatch));
    net.c.write_body(req, &body).unwrap();
    let mut header = None;
    net.run(30 * SECOND, |c, e| match e {
        Event::Writable { req: r } => {
            c.write_body(r, &body).unwrap();
            false
        }
        Event::Head { req: r, .. } => {
            header = c.header_str(r, "x-got").map(str::to_string);
            false
        }
        Event::Done { req: r } => r == req,
        _ => false,
    });
    assert_eq!(header.as_deref(), Some("3000000"));
    assert_eq!(net.p.got[0].body, body);
    assert_eq!(net.p.got[0].field("content-length"), Some("3000000"));
}

#[test]
fn a_reset_stream_ends_the_request_and_keeps_the_connection() {
    let peer = answering(|g| match g.field(":path") {
        Some("/no") => Reply::Reset(h3::H3_REQUEST_REJECTED),
        _ => ok(200, &[], b"fine", 1),
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let req = net.get("/no", &[]);
    let (_, _, end) = net.fetch(req, 64);
    assert_eq!(
        end,
        Event::Reset {
            req,
            code: h3::H3_REQUEST_REJECTED
        }
    );
    assert!(net.c.usable(net.conn));
    let again = net.get("/yes", &[]);
    let (status, body, end) = net.fetch(again, 64);
    assert_eq!(
        (status, &body[..], end),
        (200, &b"fine"[..], Event::Done { req: again })
    );
}

#[test]
fn goaway_ends_the_requests_past_its_id() {
    let peer = answering(|g| match g.field(":path") {
        Some("/hold") => Reply::Silent,
        _ => ok(200, &[], b"a", 1),
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let first = net.get("/a", &[]);
    let held = net.get("/hold", &[]);
    let (_, _, end) = net.fetch(first, 64);
    assert_eq!(end, Event::Done { req: first });
    // Stream 4 and later were not processed.
    net.p.goaway(4);
    let conn = net.conn;
    let mut seen = Vec::new();
    net.run(5 * SECOND, |_, e| {
        seen.push(e);
        matches!(e, Event::Unprocessed { .. })
    });
    assert!(seen.contains(&Event::GoAway { conn, id: 4 }), "{seen:?}");
    assert!(seen.contains(&Event::Unprocessed { req: held }));
    assert_eq!(net.c.goaway(conn), Some(4));
    assert!(!net.c.usable(conn));
    let target = Request {
        method: "GET",
        authority: "origin.test",
        path: "/",
    };
    assert_eq!(
        net.c.submit(conn, &target, &[], None),
        Err(Error::GoingAway)
    );
    // A later GOAWAY may not raise the id.
    net.p.goaway(8);
    net.run(5 * SECOND, |_, e| matches!(e, Event::Closed { .. }));
    assert!(net.log.contains(&Event::Closed {
        conn,
        error: H3Error::Id { id: 8 }
    }));
}

#[test]
fn an_idle_connection_times_out() {
    let mut cfg = Config::default();
    cfg.transport.idle = Duration::from_secs(2);
    cfg.transport.keep_alive = None;
    let peer = answering(|_| Reply::Silent);
    let mut st = storage();
    let mut net = Net::up(&mut st, cfg, peer);
    net.p.black_hole = true;
    let conn = net.conn;
    let started = net.now;
    let closed = net.run(10 * SECOND, |_, e| matches!(e, Event::Closed { .. }));
    assert!(closed);
    assert_eq!(
        net.log.last(),
        Some(&Event::Closed {
            conn,
            error: H3Error::Lost {
                loss: QuicLoss::TimedOut
            }
        })
    );
    assert!(net.now.0 - started.0 >= 2 * SECOND);
    assert_eq!(net.c.state(conn), Some(ConnState::Closed));
    let mut out = [0u8; 2048];
    net.c.close(conn, net.now, h3::H3_NO_ERROR, &mut out);
    assert_eq!(net.c.state(conn), None);
}

/// The peer closed with `code`, as it saw the client's CONNECTION_CLOSE.
fn peer_saw_close(p: &Peer, code: u64) -> bool {
    p.lost.iter().any(|l| match l {
        q::ConnectionError::ApplicationClosed(c) => c.error_code.into_inner() == code,
        _ => false,
    })
}

#[test]
fn a_frame_on_the_wrong_stream_closes_the_connection() {
    let cases: [(Vec<u8>, H3Error); 3] = [
        // DATA before HEADERS (RFC 9114 4.1).
        (
            [frame_header(frame::DATA, 1), b"x".to_vec()].concat(),
            H3Error::FrameUnexpected {
                frame: frame::DATA,
                stream: 0,
            },
        ),
        // SETTINGS on a request stream (7.2.4).
        (
            [frame_header(frame::SETTINGS, 0)].concat(),
            H3Error::FrameUnexpected {
                frame: frame::SETTINGS,
                stream: 0,
            },
        ),
        // PUSH_PROMISE with no MAX_PUSH_ID (7.2.5).
        (
            [frame_header(frame::PUSH_PROMISE, 1), vec![0]].concat(),
            H3Error::Id { id: 0 },
        ),
    ];
    for (bytes, error) in cases {
        let peer = answering(move |_| Reply::Raw {
            bytes: bytes.clone(),
            fin: true,
        });
        let mut st = storage();
        let mut net = Net::up(&mut st, Config::default(), peer);
        let req = net.get("/", &[]);
        let (_, _, end) = net.fetch(req, 64);
        let conn = net.conn;
        assert_eq!(end, Event::Closed { conn, error });
        assert_eq!(
            net.c.read_body(req, net.now, &mut [0; 4]),
            Err(Error::UnknownRequest)
        );
        net.run(SECOND, |_, _| false);
        assert!(peer_saw_close(&net.p, error.code()), "{:?}", net.p.lost);
    }
}

#[test]
fn a_qpack_error_fails_the_request_and_keeps_the_connection() {
    let peer = answering(|g| match g.field(":path") {
        // RFC 9204 B.2, a section that needs the dynamic table.
        Some("/bad") => Reply::Raw {
            bytes: [
                frame_header(frame::HEADERS, 4),
                vec![0x03, 0x81, 0x10, 0x11],
            ]
            .concat(),
            fin: true,
        },
        _ => ok(204, &[], b"", 1),
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let req = net.get("/bad", &[]);
    let (_, _, end) = net.fetch(req, 64);
    let Event::Failed { req: r, error } = end else {
        panic!("{end:?}")
    };
    assert_eq!(r, req);
    assert_eq!(error.code(), h3::QPACK_DECOMPRESSION_FAILED);
    assert!(net.c.usable(net.conn));
    let next = net.get("/fine", &[]);
    let (status, body, end) = net.fetch(next, 64);
    assert_eq!(
        (status, body.len(), end),
        (204, 0, Event::Done { req: next })
    );
}

#[test]
fn interim_heads_trailers_and_unknown_frames_are_skipped() {
    let bytes = [
        headers_frame(&[(b":status", b"103"), (b"link", b"</a>")]),
        frame_header(0x21 + 0x1f * 2, 3),
        b"zzz".to_vec(),
        headers_frame(&[(b":status", b"200"), (b"x-final", b"yes")]),
        frame_header(frame::DATA, 5),
        b"hello".to_vec(),
        frame_header(0x21, 0),
        frame_header(frame::DATA, 6),
        b" world".to_vec(),
        headers_frame(&[(b"x-trailer", b"t")]),
    ]
    .concat();
    let peer = answering(move |_| Reply::Raw {
        bytes: bytes.clone(),
        fin: true,
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let req = net.get("/", &[]);
    let before = net.log.len();
    let (status, body, end) = net.fetch(req, 3);
    let heads = net.log[before..]
        .iter()
        .filter(|e| matches!(e, Event::Head { .. }))
        .count();
    assert_eq!(heads, 1, "one final head");
    assert_eq!(
        (status, &body[..], end),
        (200, &b"hello world"[..], Event::Done { req })
    );
}

#[test]
fn a_bad_or_oversized_head_fails_only_its_request() {
    let big = "v".repeat(5000);
    let peer = answering(move |g| match g.field(":path") {
        Some("/big") => ok(200, &[("x", &big)], b"", 1),
        Some("/upper") => Reply::Raw {
            bytes: headers_frame(&[(b":status", b"200"), (b"X-Up", b"1")]),
            fin: true,
        },
        Some("/empty") => Reply::Raw {
            bytes: Vec::new(),
            fin: true,
        },
        Some("/cut") => Reply::Raw {
            bytes: [
                headers_frame(&[(b":status", b"200")]),
                frame_header(frame::DATA, 9),
                b"abc".to_vec(),
            ]
            .concat(),
            fin: true,
        },
        _ => ok(200, &[], b"", 1),
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    for (path, want) in [
        ("/big", h3::H3_EXCESSIVE_LOAD),
        ("/upper", h3::H3_MESSAGE_ERROR),
        ("/empty", h3::H3_MESSAGE_ERROR),
        ("/cut", h3::H3_MESSAGE_ERROR),
    ] {
        let req = net.get(path, &[]);
        let (_, _, end) = net.fetch(req, 64);
        let Event::Failed { error, .. } = end else {
            panic!("{path}: {end:?}")
        };
        assert_eq!(error.code(), want, "{path}: {error}");
        assert!(net.c.usable(net.conn), "{path}");
    }
}

#[test]
fn control_stream_violations_close_the_connection() {
    let settings_twice = [frame_header(frame::SETTINGS, 0)].concat();
    let data_on_control = [frame_header(frame::DATA, 0)].concat();
    let cases: [(Option<Vec<u8>>, Vec<u8>, u64); 4] = [
        (None, settings_twice, h3::H3_FRAME_UNEXPECTED),
        (None, data_on_control, h3::H3_FRAME_UNEXPECTED),
        // A setting twice, and an HTTP/2 setting (RFC 9114 7.2.4.1).
        (
            Some([varint(6), varint(1), varint(6), varint(2)].concat()),
            Vec::new(),
            h3::H3_SETTINGS_ERROR,
        ),
        (
            Some([varint(2), varint(1)].concat()),
            Vec::new(),
            h3::H3_SETTINGS_ERROR,
        ),
    ];
    for (settings, tail, code) in cases {
        let mut peer = answering(|_| Reply::Silent);
        peer.settings = settings;
        peer.control_tail = tail;
        let mut st = storage();
        let mut net = Net::start(&mut st, Config::default(), peer);
        net.run(5 * SECOND, |_, e| matches!(e, Event::Closed { .. }));
        let Some(Event::Closed { error, .. }) = net.log.last() else {
            panic!("{:?}", net.log)
        };
        assert_eq!(error.code(), code, "{error}");
        net.run(SECOND, |_, _| false);
        assert!(peer_saw_close(&net.p, code));
    }
}

#[test]
fn the_peers_field_section_limit_holds_requests_back() {
    let mut peer = answering(|_| ok(200, &[], b"", 1));
    peer.settings = Some([varint(6), varint(200)].concat());
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    // SETTINGS may trail the handshake.
    net.run(SECOND, |_, _| false);
    let target = Request {
        method: "GET",
        authority: "origin.test",
        path: "/",
    };
    let long = "a".repeat(300);
    let r = net
        .c
        .submit(net.conn, &target, &[Header::new("x-long", &long)], None);
    assert!(
        matches!(r, Err(Error::H3(H3Error::ExcessiveLoad { max: 200, .. }))),
        "{r:?}"
    );
    assert!(net.c.submit(net.conn, &target, &[], None).is_ok());
}

#[test]
fn bad_requests_are_refused_before_a_stream_opens() {
    let peer = answering(|_| Reply::Silent);
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let target = Request {
        method: "GET",
        authority: "origin.test",
        path: "/",
    };
    let conn = net.conn;
    for (h, e) in [
        (Header::new("connection", "close"), HeaderError::Reserved),
        (
            Header::new("Transfer-Encoding", "chunked"),
            HeaderError::Reserved,
        ),
        (Header::new("x", "a\r\nb: c"), HeaderError::Injection),
        (Header::new("bad name", "v"), HeaderError::BadName),
    ] {
        assert_eq!(
            net.c.submit(conn, &target, &[h], None),
            Err(Error::BadRequest(e))
        );
    }
    let blank = Request { path: "", ..target };
    assert_eq!(
        net.c.submit(conn, &blank, &[], None),
        Err(Error::BadRequest(HeaderError::BadTarget))
    );
    assert_eq!(net.c.requests_on(conn), 0);
}

#[test]
fn a_cancelled_request_stops_its_stream_and_keeps_the_connection() {
    let peer = answering(|g| match g.field(":path") {
        Some("/big") => ok(200, &[], &pattern(2_000_000), 1),
        _ => ok(200, &[], b"small", 1),
    });
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let big = net.get("/big", &[]);
    net.run(10 * SECOND, |c, e| {
        if e == (Event::Head {
            req: big,
            status: 200,
        }) {
            c.cancel(big);
            return true;
        }
        false
    });
    assert_eq!(
        net.c.read_body(big, net.now, &mut [0; 4]),
        Err(Error::UnknownRequest)
    );
    let next = net.get("/small", &[]);
    let (status, body, end) = net.fetch(next, 64);
    assert_eq!(
        (status, &body[..], end),
        (200, &b"small"[..], Event::Done { req: next })
    );
    assert_eq!(net.c.requests_on(net.conn), 0);
}

#[test]
fn an_unread_body_holds_the_stream_window_shut() {
    let mut cfg = Config::default();
    cfg.transport.stream_window = 64 * 1024;
    let peer = answering(|_| ok(200, &[], &pattern(1_000_000), 1));
    let mut st = storage();
    let mut net = Net::up(&mut st, cfg, peer);
    let req = net.get("/", &[]);
    // Read nothing until quiet.
    net.run(5 * SECOND, |_, _| false);
    let stats = net.c.stats(net.conn).unwrap();
    assert!(
        stats.udp_rx.bytes < 200 * 1024,
        "{} bytes came with nothing read",
        stats.udp_rx.bytes
    );
    let (_, body, end) = net.fetch(req, 16 * 1024);
    assert_eq!(end, Event::Done { req });
    assert_eq!(body, pattern(1_000_000));
}

#[test]
fn the_connection_closes_with_the_code_given() {
    let peer = answering(|_| Reply::Silent);
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let held = net.get("/", &[]);
    // Past the handshake's confirmation, so the close is the application's.
    net.run(SECOND, |_, _| false);
    let mut out = [0u8; 2048];
    let n = net
        .c
        .close(net.conn, net.now, h3::H3_NO_ERROR, &mut out)
        .unwrap();
    assert_eq!(
        net.c.read_body(held, net.now, &mut [0; 4]),
        Err(Error::UnknownRequest)
    );
    let mut back = Vec::new();
    net.p
        .input(net.now, Some((addr(CLIENT), &out[..n])), &mut back);
    assert!(peer_saw_close(&net.p, h3::H3_NO_ERROR), "{:?}", net.p.lost);
}

#[test]
fn seeded_connection_ids_replay() {
    let first = |seed: Option<u64>| {
        let mut st = HeapStorage::new(1, 1, 1024);
        let cfg = Config {
            seed,
            ..Config::default()
        };
        let mut c = Client::new(cfg, peer::client_crypto(), st.view()).unwrap();
        let conn = c.connect(Nanos(0), "origin.test", addr(SERVER)).unwrap();
        let mut out = [0u8; 2048];
        let n = c.poll_transmit(conn, Nanos(0), &mut out).unwrap();
        assert!(n >= 1200, "an Initial is padded (RFC 9000 14.1)");
        // Version, DCID length and the DCID, past the protected first byte.
        out[1..22].to_vec()
    };
    assert_eq!(first(Some(5)), first(Some(5)));
    assert_ne!(first(Some(5)), first(Some(6)));
}

#[test]
fn storage_bounds_hold() {
    let peer = answering(|_| Reply::Silent);
    let mut st = HeapStorage::new(1, 2, 1024);
    let mut net = Net::up(&mut st, Config::default(), peer);
    let a = net.get("/a", &[]);
    let _b = net.get("/b", &[]);
    let target = Request {
        method: "GET",
        authority: "origin.test",
        path: "/c",
    };
    assert_eq!(
        net.c.submit(net.conn, &target, &[], None),
        Err(Error::TooManyRequests)
    );
    net.c.cancel(a);
    assert!(net.c.submit(net.conn, &target, &[], None).is_ok());
    assert_eq!(
        net.c.connect(net.now, "origin.test", addr(SERVER)),
        Err(Error::TooManyConnections)
    );
    // Header regions must divide evenly.
    let mut ragged = HeapStorage::new(1, 3, 10);
    let mut odd = [0u8; 31];
    let mut view = ragged.view();
    view.header_bytes = &mut odd;
    assert!(matches!(
        Client::new(Config::default(), peer::client_crypto(), view),
        Err(Error::BadStorage(_))
    ));
}

#[test]
fn the_peers_close_ends_the_connection_and_its_requests() {
    let peer = answering(|_| Reply::Silent);
    let mut st = storage();
    let mut net = Net::up(&mut st, Config::default(), peer);
    let held = net.get("/", &[]);
    net.run(SECOND, |_, _| false);
    net.p.close(net.now, h3::H3_NO_ERROR);
    let conn = net.conn;
    let closed = net.run(5 * SECOND, |_, e| matches!(e, Event::Closed { .. }));
    assert!(closed);
    let loss = QuicLoss::Application {
        code: h3::H3_NO_ERROR,
    };
    assert_eq!(
        net.log.last(),
        Some(&Event::Closed {
            conn,
            error: H3Error::Lost { loss }
        })
    );
    assert!(!net.c.usable(conn));
    assert_eq!(
        net.c.read_body(held, net.now, &mut [0; 4]),
        Err(Error::UnknownRequest)
    );
    let target = Request {
        method: "GET",
        authority: "origin.test",
        path: "/",
    };
    assert_eq!(
        net.c.submit(conn, &target, &[], None),
        Err(Error::NotConnected)
    );
}
