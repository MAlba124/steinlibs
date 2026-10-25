#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! h2 server tests. The client machine is the peer for full exchanges, frames are hand built elsewhere.

use super::*;
use crate::h2::{self, Connection, HeapStreamStorage};
use crate::hpack;

const EH: u8 = flag::END_HEADERS;
const ES: u8 = flag::END_STREAM;
/// Step bound per feed before calling it a hang.
const STEPS: usize = 10_000;

type Fields = Vec<(Vec<u8>, Vec<u8>)>;
type Frame = (Kind, u8, u32, Vec<u8>);

/// Owned copy of an [`Event`].
#[derive(Debug, PartialEq, Eq, Clone)]
enum Ev {
    NeedMore,
    Nothing,
    Blocked,
    Request {
        stream: u32,
        end_stream: bool,
        fields: Fields,
    },
    Data {
        stream: u32,
        end_stream: bool,
        body: Vec<u8>,
    },
    Reset {
        stream: u32,
        code: u32,
        tag: u64,
    },
    Window {
        stream: u32,
    },
    GoAway {
        last_stream: u32,
        code: u32,
    },
}

fn own(event: &Event<'_>, src: &[u8]) -> Ev {
    match *event {
        Event::NeedMore => Ev::NeedMore,
        Event::Nothing => Ev::Nothing,
        Event::Blocked => Ev::Blocked,
        Event::Request {
            stream,
            end_stream,
            fields,
        } => Ev::Request {
            stream,
            end_stream,
            fields: header::iter(fields)
                .map(|(n, v)| (n.to_vec(), v.to_vec()))
                .collect(),
        },
        Event::Data {
            stream,
            end_stream,
            at,
            len,
        } => Ev::Data {
            stream,
            end_stream,
            body: src[at..at + len].to_vec(),
        },
        Event::Reset { stream, code, tag } => Ev::Reset { stream, code, tag },
        Event::Window { stream } => Ev::Window { stream },
        Event::GoAway { last_stream, code } => Ev::GoAway { last_stream, code },
    }
}

/// Server with exactly `mem_bytes` of storage, collected ctl and unused input.
struct Srv {
    conn: Conn,
    mem: Vec<u8>,
    ctl: Vec<u8>,
    pending: Vec<u8>,
}

fn srv(cfg: Config) -> Srv {
    let mut conn = Conn::new(cfg);
    let mut mem = vec![0xa5u8; mem_bytes(&cfg)];
    conn.init(&mut mem).unwrap();
    Srv {
        conn,
        mem,
        ctl: Vec::new(),
        pending: Vec::new(),
    }
}

/// Past the preface and client SETTINGS.
fn ready(cfg: Config) -> Srv {
    let mut s = srv(cfg);
    assert_eq!(s.ok(&hello()), []);
    assert_eq!(s.take_ctl(), f(Kind::Settings, flag::ACK, 0, &[]));
    s
}

impl Srv {
    /// One step with a fresh 64 byte ctl.
    fn step(&mut self, src: &[u8]) -> Result<(usize, Ev), Error> {
        let mut buf = [0u8; 64];
        let mut ctl = Ctl {
            buf: &mut buf,
            len: 0,
        };
        let stepped = self.conn.step(&mut self.mem, src, &mut ctl)?;
        assert!(stepped.consumed <= src.len());
        let out = (stepped.consumed, own(&stepped.event, src));
        self.ctl.extend_from_slice(&ctl.buf[..ctl.len]);
        Ok(out)
    }

    /// Append input and step until dry. Drops NeedMore and Nothing.
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Ev>, Error> {
        self.pending.extend_from_slice(bytes);
        let input = core::mem::take(&mut self.pending);
        let mut events = Vec::new();
        let mut pos = 0;
        let mut dry = false;
        for _ in 0..STEPS {
            let (used, ev) = self.step(&input[pos..])?;
            pos += used;
            match ev {
                Ev::NeedMore => {
                    assert_eq!(used, 0);
                    dry = true;
                    break;
                }
                Ev::Nothing => assert!(used > 0),
                Ev::Blocked => panic!("a fresh ctl cannot block"),
                other => events.push(other),
            }
        }
        assert!(dry, "input never ran dry");
        self.pending = input[pos..].to_vec();
        Ok(events)
    }

    fn ok(&mut self, bytes: &[u8]) -> Vec<Ev> {
        let events = self.feed(bytes).unwrap();
        assert!(self.pending.is_empty(), "whole frames leave nothing over");
        events
    }

    fn err(&mut self, bytes: &[u8]) -> Error {
        self.feed(bytes).unwrap_err()
    }

    fn take_ctl(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.ctl)
    }

    fn take_frames(&mut self) -> Vec<Frame> {
        frames(&self.take_ctl())
    }

    fn live(&self) -> Vec<(u32, u64)> {
        let max = self.conn.cfg.max_streams;
        let mut all: Vec<_> = (0..max + 2)
            .filter_map(|i| self.conn.stream_at(&self.mem, i))
            .collect();
        assert_eq!(all.len() as u32, self.conn.open_streams());
        all.sort_unstable();
        all
    }
}

fn f(kind: Kind, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    frame::write_header(&mut v, kind, flags, stream, payload.len() as u32);
    v.extend_from_slice(payload);
    v
}

fn settings(list: &[(u16, u32)]) -> Vec<u8> {
    let list: Vec<_> = list
        .iter()
        .map(|(id, value)| frame::Setting {
            id: *id,
            value: *value,
        })
        .collect();
    let mut v = Vec::new();
    frame::write_settings(&mut v, &list);
    v
}

fn hello() -> Vec<u8> {
    let mut v = frame::PREFACE.to_vec();
    v.extend(settings(&[]));
    v
}

fn block(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (n, v) in fields {
        hpack::encode_header(n, v, &mut b);
    }
    b
}

fn request_block(method: &str, path: &str) -> Vec<u8> {
    block(&[
        (":method", method),
        (":scheme", "https"),
        (":path", path),
        (":authority", "example.com"),
    ])
}

fn get(stream: u32) -> Vec<u8> {
    f(Kind::Headers, EH | ES, stream, &request_block("GET", "/"))
}

fn post(stream: u32) -> Vec<u8> {
    f(Kind::Headers, EH, stream, &request_block("POST", "/up"))
}

fn data(stream: u32, flags: u8, body: &[u8]) -> Vec<u8> {
    f(Kind::Data, flags, stream, body)
}

fn window_update(stream: u32, increment: u32) -> Vec<u8> {
    f(Kind::WindowUpdate, 0, stream, &increment.to_be_bytes())
}

fn rst_stream(stream: u32, code: u32) -> Vec<u8> {
    f(Kind::RstStream, 0, stream, &code.to_be_bytes())
}

/// Literal with incremental indexing.
fn indexing(name: &str, value: &str) -> Vec<u8> {
    let mut b = vec![0x40];
    hpack::encode_string(name.as_bytes(), &mut b);
    hpack::encode_string(value.as_bytes(), &mut b);
    b
}

fn frames(bytes: &[u8]) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut pos = 0;
    for _ in 0..bytes.len() + 1 {
        let Some(h) = frame::parse_header(&bytes[pos..]) else {
            break;
        };
        let end = pos + FRAME_HEADER + h.len as usize;
        let payload = bytes[pos + FRAME_HEADER..end].to_vec();
        out.push((h.kind, h.flags, h.stream, payload));
        pos = end;
    }
    assert_eq!(pos, bytes.len(), "ctl holds whole frames only");
    out
}

fn rst(stream: u32, code: u32) -> Frame {
    (Kind::RstStream, 0, stream, code.to_be_bytes().to_vec())
}

fn wu(stream: u32, increment: u32) -> Frame {
    (
        Kind::WindowUpdate,
        0,
        stream,
        increment.to_be_bytes().to_vec(),
    )
}

fn reset(stream: u32, code: u32, tag: u64) -> Ev {
    Ev::Reset { stream, code, tag }
}

fn field(ev: &Ev, name: &str) -> Option<Vec<u8>> {
    let Ev::Request { fields, .. } = ev else {
        panic!("not a request: {ev:?}");
    };
    fields
        .iter()
        .find(|(n, _)| n == name.as_bytes())
        .map(|(_, v)| v.clone())
}

fn stream_of(ev: &Ev) -> u32 {
    match ev {
        Ev::Request { stream, .. } => *stream,
        other => panic!("not a request: {other:?}"),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> usize {
        (self.next() % n) as usize
    }
}

fn body_byte(request: usize, offset: usize) -> u8 {
    (request * 31 + offset * 7) as u8
}

struct Reply {
    stream: u32,
    request: usize,
    len: usize,
    sent: usize,
    head: bool,
    done: bool,
}

/// Send what the windows allow of every reply, framing first, then accounting.
fn pump(s: &mut Srv, replies: &mut [Reply], wire: &mut Vec<u8>) {
    for r in replies.iter_mut().filter(|r| !r.done) {
        if !r.head {
            let mut out = [0u8; 256];
            let length = r.len.to_string();
            let fields: [(&[u8], &[u8]); 2] = [
                (b"content-length", length.as_bytes()),
                (b"x-request", b"seen"),
            ];
            let n = headers_frame(&mut out, r.stream, 200, &fields, r.len == 0).unwrap();
            assert!(s.conn.sent_headers(&mut s.mem, r.stream, r.len == 0));
            wire.extend_from_slice(&out[..n]);
            r.head = true;
            r.done = r.len == 0;
        }
        for _ in 0..64 {
            let left = r.len - r.sent;
            if left == 0 {
                break;
            }
            let n = s.conn.grant(&mut s.mem, r.stream, left);
            if n == 0 {
                break;
            }
            let mut head = [0u8; FRAME_HEADER];
            data_header(&mut head, r.stream, n, n == left).unwrap();
            wire.extend_from_slice(&head);
            wire.extend((r.sent..r.sent + n).map(|i| body_byte(r.request, i)));
            r.sent += n;
            if n == left {
                s.conn.sent_end(&mut s.mem, r.stream);
                r.done = true;
            }
        }
    }
}

#[derive(Default)]
struct Seen {
    status: u16,
    length: Vec<u8>,
    body: Vec<u8>,
    complete: bool,
}

/// Run one request per body through client and server. Returns server window events.
fn interop(bodies: &[usize]) -> Vec<u32> {
    let mut storage = HeapStreamStorage::new(64);
    let mut client = Connection::new(h2::Config::default(), storage.view()).unwrap();
    let mut to_server = Vec::new();
    client.write_preface(&mut to_server);
    let mut handles = Vec::new();
    for i in 0..bodies.len() {
        let path = format!("/r/{i}");
        let h = client
            .send_request(&mut to_server, "GET", &path, "example.com", "https", &[])
            .unwrap();
        handles.push(h);
    }

    let mut s = srv(Config::default());
    let mut to_client = vec![0u8; 64];
    let n = s.conn.write_settings(&mut to_client).unwrap();
    to_client.truncate(n);

    let mut replies: Vec<Reply> = Vec::new();
    let mut seen: Vec<Seen> = bodies.iter().map(|_| Seen::default()).collect();
    let mut windows = Vec::new();
    let mut finished = false;
    for _round in 0..1000 {
        for ev in s.ok(&to_server) {
            match ev {
                Ev::Window { stream } => windows.push(stream),
                Ev::Request { .. } => replies.push(accept(&ev, bodies)),
                other => panic!("unexpected {other:?}"),
            }
        }
        to_server.clear();
        pump(&mut s, &mut replies, &mut to_client);
        to_client.extend(s.take_ctl());

        let mut pos = 0;
        for _ in 0..64 {
            let mut events = Vec::new();
            let src = &to_client[pos..];
            let n = client.step(src, &mut to_server, &mut events).unwrap();
            for ev in events {
                record(&handles, &mut seen, ev, src);
            }
            pos += n;
            if n == 0 {
                break;
            }
        }
        assert_eq!(pos, to_client.len(), "the client took every frame");
        to_client.clear();
        if seen.iter().all(|r| r.complete) && to_server.is_empty() {
            finished = true;
            break;
        }
    }
    assert!(finished, "every response completes");

    for (i, r) in seen.iter().enumerate() {
        assert_eq!(r.status, 200);
        assert_eq!(r.length, bodies[i].to_string().into_bytes());
        assert_eq!(r.body.len(), bodies[i], "request {i}");
        assert!(
            r.body
                .iter()
                .enumerate()
                .all(|(o, b)| *b == body_byte(i, o))
        );
    }
    assert_eq!(s.conn.open_streams(), 0, "every slot was freed");
    assert_eq!(s.conn.last_stream(), bodies.len() as u32 * 2 - 1);
    windows
}

fn accept(ev: &Ev, bodies: &[usize]) -> Reply {
    let Ev::Request {
        stream,
        end_stream,
        fields,
    } = ev
    else {
        panic!("not a request");
    };
    assert!(*end_stream, "the client sends no body");
    let packed: Vec<u8> = fields.iter().fold(Vec::new(), |mut acc, (n, v)| {
        let at = acc.len();
        acc.resize(at + 3 + n.len() + v.len(), 0);
        header::push(&mut acc, at, n, v).unwrap();
        acc
    });
    assert_eq!(header::find(&packed, ":method"), Some(&b"GET"[..]));
    assert_eq!(header::find(&packed, ":scheme"), Some(&b"https"[..]));
    assert_eq!(
        header::find(&packed, ":authority"),
        Some(&b"example.com"[..])
    );
    let path = header::find(&packed, ":path").unwrap();
    let request: usize = core::str::from_utf8(&path[3..]).unwrap().parse().unwrap();
    assert_eq!(*stream as usize, request * 2 + 1);
    Reply {
        stream: *stream,
        request,
        len: bodies[request],
        sent: 0,
        head: false,
        done: false,
    }
}

/// `src` is the step input that `ev` body spans index into.
fn record(handles: &[h2::StreamId], seen: &mut [Seen], ev: h2::Event, src: &[u8]) {
    let index = |h: h2::StreamId| handles.iter().position(|x| *x == h).unwrap();
    match ev {
        h2::Event::Head {
            stream,
            status,
            headers,
        } => {
            let r = &mut seen[index(stream)];
            r.status = status;
            let length = headers.iter().find(|h| h.name == b"content-length");
            r.length = length.unwrap().value.clone();
            assert!(headers.iter().any(|h| h.name == b"x-request"));
        }
        h2::Event::Body { stream, at, len } => seen[index(stream)]
            .body
            .extend_from_slice(&src[at..at + len]),
        h2::Event::Complete { stream } => seen[index(stream)].complete = true,
        other => panic!("unexpected client event {other:?}"),
    }
}

#[test]
fn interop_one_request_of_each_size() {
    for len in [0usize, 1, 100, 16_384, 40_000] {
        interop(&[len]);
    }
}

#[test]
fn interop_many_requests_on_one_connection() {
    interop(&[0, 1, 100, 16_384, 40_000]);
    interop(&[0; 40]);
}

#[test]
fn interop_bodies_past_the_connection_window() {
    // 120000 bytes against a 65535 connection window
    let windows = interop(&[40_000, 40_000, 40_000]);
    assert!(windows.contains(&0), "connection window updates were seen");
}

#[test]
fn interop_a_body_past_the_stream_window() {
    let windows = interop(&[200_000]);
    assert!(windows.contains(&0));
    assert!(windows.contains(&1), "stream window updates were seen");
}

/// Client byte stream covering most of the machine.
fn recorded() -> Vec<u8> {
    let mut storage = HeapStreamStorage::new(8);
    let mut client = Connection::new(h2::Config::default(), storage.view()).unwrap();
    let mut v = Vec::new();
    client.write_preface(&mut v);
    for path in ["/", "/a", "/index.html"] {
        client
            .send_request(&mut v, "GET", path, "example.com", "https", &[])
            .unwrap();
    }
    // body plus a header indexed into the HPACK table
    let mut b = request_block("POST", "/upload");
    b.extend(indexing("x-trace", "abc123"));
    v.extend(f(Kind::Headers, EH, 7, &b));
    v.extend(data(7, 0, b"first part, "));
    v.extend(f(Kind::Ping, 0, 0, b"12345678"));
    v.extend(data(7, flag::PADDED, b"\x04padded part\0\0\0\0"));
    v.extend(window_update(0, 1000));
    v.extend(window_update(1, 500));
    v.extend(data(7, ES, b"the end"));
    // three-frame block reading that entry back
    let mut b = request_block("GET", "/split");
    b.push(0x80 | 62);
    v.extend(f(Kind::Headers, ES, 9, &b[..5]));
    v.extend(f(Kind::Continuation, 0, 9, &b[5..9]));
    v.extend(f(Kind::Continuation, EH, 9, &b[9..]));
    v.extend(f(Kind::Priority, 0, 9, &[0, 0, 0, 0, 16]));
    v.extend(rst_stream(9, 8));
    v.extend(f(Kind::Unknown(0x42), 0xff, 3, b"ignored"));
    v.extend(settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 100_000)]));
    v.extend(f(Kind::GoAway, 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]));
    v
}

#[test]
fn the_recorded_stream_means_what_it_should() {
    let mut s = srv(Config::default());
    let events = s.ok(&recorded());
    let requests: Vec<u32> = events
        .iter()
        .filter(|e| matches!(e, Ev::Request { .. }))
        .map(stream_of)
        .collect();
    assert_eq!(requests, [1, 3, 5, 7, 9]);
    assert_eq!(field(&events[3], "x-trace"), Some(b"abc123".to_vec()));
    let tail = &events[4..];
    let body = |b: &[u8], end| Ev::Data {
        stream: 7,
        end_stream: end,
        body: b.to_vec(),
    };
    assert_eq!(tail[0], body(b"first part, ", false));
    assert_eq!(tail[1], body(b"padded part", false));
    assert_eq!(tail[2], Ev::Window { stream: 0 });
    assert_eq!(tail[3], Ev::Window { stream: 1 });
    assert_eq!(tail[4], body(b"the end", true));
    assert_eq!(field(&tail[5], "x-trace"), Some(b"abc123".to_vec()));
    assert_eq!(field(&tail[5], ":path"), Some(b"/split".to_vec()));
    assert_eq!(tail[6], reset(9, 8, 0));
    assert_eq!(tail[7], Ev::Window { stream: 0 });
    let last = Ev::GoAway {
        last_stream: 0,
        code: 0,
    };
    assert_eq!(tail[8], last);
    assert_eq!(tail.len(), 9);
    assert!(s.conn.goaway_received());
}

#[test]
fn any_split_of_the_input_gives_the_same_events() {
    let stream = recorded();
    let mut whole = srv(Config::default());
    let want = whole.ok(&stream);
    let want_ctl = whole.take_ctl();

    for seed in 0..200u64 {
        let mut rng = Rng(seed);
        let mut s = srv(Config::default());
        let mut got = Vec::new();
        let mut pos = 0;
        let most = [3u64, 20, 200][rng.below(3)];
        for _ in 0..stream.len() + 1 {
            if pos == stream.len() {
                break;
            }
            let n = (1 + rng.below(most)).min(stream.len() - pos);
            got.extend(s.feed(&stream[pos..pos + n]).unwrap());
            pos += n;
        }
        assert_eq!(pos, stream.len());
        assert!(s.pending.is_empty(), "seed {seed}");
        assert_eq!(got, want, "seed {seed}");
        assert_eq!(s.take_ctl(), want_ctl, "seed {seed}");
    }
}

#[test]
fn every_prefix_waits_for_a_whole_frame() {
    let mut stream = hello();
    stream.extend(get(1));
    stream.extend(f(Kind::Ping, 0, 0, b"abcdefgh"));
    let mut bounds = vec![frame::PREFACE.len()];
    for _ in 0..3 {
        let at = *bounds.last().unwrap();
        let h = frame::parse_header(&stream[at..]).unwrap();
        bounds.push(at + FRAME_HEADER + h.len as usize);
    }
    assert_eq!(*bounds.last().unwrap(), stream.len());

    for cut in 0..=stream.len() {
        let mut s = srv(Config::default());
        let src = &stream[..cut];
        let mut pos = 0;
        let mut handled = 0;
        for _ in 0..bounds.len() + 1 {
            let (used, ev) = s.step(&src[pos..]).unwrap();
            if ev == Ev::NeedMore {
                assert_eq!(used, 0, "cut {cut}");
                break;
            }
            pos += used;
            handled += 1;
            assert_eq!(pos, bounds[handled - 1], "one frame a step, cut {cut}");
        }
        let whole = bounds.iter().filter(|b| **b <= cut).count();
        assert_eq!(handled, whole, "cut {cut}");
        assert_eq!(s.step(&src[pos..]).unwrap(), (0, Ev::NeedMore));
    }
}

#[test]
fn a_bad_preface_is_refused_at_the_first_wrong_byte() {
    let mut s = srv(Config::default());
    assert_eq!(
        s.step(b"PRI * HTTP/1.1\r\n\r\nSM\r\n\r\n"),
        Err(Error::BadPreface)
    );
    let mut s = srv(Config::default());
    assert_eq!(s.step(b"GET"), Err(Error::BadPreface));
    let mut s = srv(Config::default());
    assert_eq!(s.step(b"PRI * HT"), Ok((0, Ev::NeedMore)));
    assert_eq!(s.step(b""), Ok((0, Ev::NeedMore)));
    assert_eq!(s.step(frame::PREFACE), Ok((24, Ev::Nothing)));
}

#[test]
fn the_first_frame_must_be_settings() {
    let mut s = srv(Config::default());
    let mut v = frame::PREFACE.to_vec();
    v.extend(f(Kind::Ping, 0, 0, b"12345678"));
    assert_eq!(s.err(&v), Error::Protocol);

    let mut s = srv(Config::default());
    let mut v = frame::PREFACE.to_vec();
    v.extend(f(Kind::Settings, flag::ACK, 0, &[]));
    assert_eq!(s.err(&v), Error::Protocol);

    // refused on the header alone
    let mut s = srv(Config::default());
    let mut v = frame::PREFACE.to_vec();
    v.extend(&get(1)[..FRAME_HEADER]);
    assert_eq!(s.err(&v), Error::Protocol);
}

#[test]
fn settings_are_acked() {
    let mut s = srv(Config::default());
    assert_eq!(s.ok(&hello()), []);
    assert_eq!(s.take_ctl(), [0, 0, 0, 4, 1, 0, 0, 0, 0]);
    assert_eq!(s.ok(&f(Kind::Settings, flag::ACK, 0, &[])), []);
    assert_eq!(s.take_ctl(), []);
}

#[test]
fn malformed_settings_are_connection_errors() {
    let cases: [(Vec<u8>, Error); 7] = [
        (f(Kind::Settings, 0, 1, &[]), Error::Protocol),
        (f(Kind::Settings, 0, 0, &[0; 5]), Error::FrameSize),
        (f(Kind::Settings, flag::ACK, 0, &[0; 6]), Error::FrameSize),
        (
            settings(&[(frame::SETTINGS_ENABLE_PUSH, 2)]),
            Error::Protocol,
        ),
        (
            settings(&[(frame::SETTINGS_MAX_FRAME_SIZE, 16_383)]),
            Error::Protocol,
        ),
        (
            settings(&[(frame::SETTINGS_MAX_FRAME_SIZE, 1 << 24)]),
            Error::Protocol,
        ),
        (
            settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 1 << 31)]),
            Error::FlowControl,
        ),
    ];
    for (bytes, want) in cases {
        let mut s = ready(Config::default());
        assert_eq!(s.err(&bytes), want, "{bytes:?}");
    }
    // legal edges and unknown ids
    let mut s = ready(Config::default());
    let fine = settings(&[
        (frame::SETTINGS_ENABLE_PUSH, 0),
        (frame::SETTINGS_ENABLE_PUSH, 1),
        (frame::SETTINGS_MAX_FRAME_SIZE, (1 << 24) - 1),
        (frame::SETTINGS_HEADER_TABLE_SIZE, 0),
        (0x99, 7),
    ]);
    assert_eq!(s.ok(&fine), []);
    assert_eq!(s.conn.peer_max_frame(), (1 << 24) - 1);
}

#[test]
fn initial_window_changes_reach_open_streams() {
    let window = |v| settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, v)]);
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 65_535);

    assert_eq!(s.ok(&window(70_000)), [Ev::Window { stream: 0 }]);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 70_000);
    assert_eq!(s.conn.conn_send_window(), 65_535, "not the connection's");
    s.ok(&get(3));
    assert_eq!(s.conn.stream_send_window(&s.mem, 3), 70_000);

    // shrinking is no event
    assert_eq!(s.ok(&window(10)), []);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 10);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 100), 10);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 100), 0);

    // negative is legal and grants nothing
    assert_eq!(s.ok(&window(5)), []);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), -5);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 100), 0);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), -5);
    assert_eq!(s.ok(&window(100)), [Ev::Window { stream: 0 }]);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 1000), 90);
    assert_eq!(s.conn.grant(&mut s.mem, 3, 1000), 100);

    assert_eq!(s.ok(&window(100)), []);
}

#[test]
fn an_initial_window_change_may_not_overflow_a_stream() {
    let mut s = ready(Config::default());
    s.ok(&get(1));
    let room = i32::MAX as u32 - 65_535;
    assert_eq!(s.ok(&window_update(1, room)), [Ev::Window { stream: 1 }]);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), i64::from(i32::MAX));
    let up = settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 65_536)]);
    assert_eq!(s.err(&up), Error::FlowControl);
}

#[test]
fn peer_max_frame_bounds_a_grant() {
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert_eq!(s.conn.peer_max_frame(), 16_384);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 100_000), 16_384);
    s.ok(&settings(&[(frame::SETTINGS_MAX_FRAME_SIZE, 32_768)]));
    assert_eq!(s.conn.peer_max_frame(), 32_768);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 100_000), 32_768);
}

#[test]
fn our_settings_frame_says_what_the_config_says() {
    let cfg = Config {
        max_streams: 7,
        initial_window: 1000,
        max_frame: 20_000,
        header_table_size: 512,
        max_header_block: 300,
        max_header_list: 700,
    };
    let mut conn = Conn::new(cfg);
    let mut out = [0u8; 64];
    let n = conn.write_settings(&mut out).unwrap();
    let all = frames(&out[..n]);
    assert_eq!(all.len(), 1);
    let (kind, flags, stream, payload) = &all[0];
    assert_eq!((*kind, *flags, *stream), (Kind::Settings, 0, 0));
    let got: Vec<(u16, u32)> = frame::parse_settings(payload)
        .unwrap()
        .map(|s| (s.id, s.value))
        .collect();
    let want = [(1, 512), (3, 7), (4, 1000), (5, 20_000), (6, 700)];
    assert_eq!(got, want, "and no ENABLE_PUSH");
    assert_eq!(conn.write_settings(&mut out[..n - 1]), None);
    // raised to the protocol minimum
    let mut conn = Conn::new(Config {
        max_frame: 100,
        ..cfg
    });
    conn.write_settings(&mut out).unwrap();
    assert_eq!(out[9 + 18..9 + 24], [0, 5, 0, 0, 0x40, 0]);
}

#[test]
fn ping_is_echoed() {
    let mut s = ready(Config::default());
    assert_eq!(
        s.ok(&f(Kind::Ping, 0, 0, b"\x01\x02\x03\x04\x05\x06\x07\x08")),
        []
    );
    let want = (Kind::Ping, flag::ACK, 0, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(s.take_frames(), [want]);
    assert_eq!(s.ok(&f(Kind::Ping, flag::ACK, 0, &[0; 8])), []);
    assert_eq!(s.take_ctl(), []);

    assert_eq!(s.err(&f(Kind::Ping, 0, 0, &[0; 7])), Error::FrameSize);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&f(Kind::Ping, 0, 1, &[0; 8])), Error::Protocol);
}

#[test]
fn an_oversized_frame_is_refused_on_its_header() {
    let mut s = ready(Config::default());
    let mut head = Vec::new();
    frame::write_header(&mut head, Kind::Data, 0, 1, 16_385);
    assert_eq!(s.step(&head), Err(Error::FrameSize));

    let mut s = ready(Config::default());
    let mut head = Vec::new();
    frame::write_header(&mut head, Kind::Unknown(0x30), 0, 1, 16_384);
    assert_eq!(s.step(&head), Ok((0, Ev::NeedMore)));

    let cfg = Config {
        max_frame: 32_768,
        ..Config::default()
    };
    let mut s = ready(cfg);
    let mut head = Vec::new();
    frame::write_header(&mut head, Kind::Unknown(0x30), 0, 1, 32_768);
    assert_eq!(s.step(&head), Ok((0, Ev::NeedMore)));
    head[2] = 1;
    assert_eq!(s.step(&head), Err(Error::FrameSize));
}

#[test]
fn priority_push_promise_and_unknown_frames() {
    let mut s = ready(Config::default());
    assert_eq!(s.ok(&f(Kind::Priority, 0, 5, &[0; 5])), []);
    assert_eq!(s.ok(&f(Kind::Unknown(0xee), 0xff, 9, b"whatever")), []);
    assert_eq!(s.take_ctl(), []);
    assert_eq!(s.conn.last_stream(), 0, "neither opens a stream");
    assert_eq!(s.err(&f(Kind::Priority, 0, 5, &[0; 4])), Error::FrameSize);

    let mut s = ready(Config::default());
    assert_eq!(s.err(&f(Kind::Priority, 0, 0, &[0; 5])), Error::Protocol);
    let mut s = ready(Config::default());
    assert_eq!(
        s.err(&f(Kind::PushPromise, EH, 1, &[0; 4])),
        Error::Protocol
    );
    let mut s = ready(Config::default());
    assert_eq!(s.err(&f(Kind::Continuation, EH, 1, &[])), Error::Protocol);
}

#[test]
fn goaway_is_recorded_and_reported() {
    let mut s = ready(Config::default());
    assert!(!s.conn.goaway_received());
    let got = s.ok(&f(Kind::GoAway, 0, 0, &[0x80, 0, 0, 5, 0, 0, 0, 2, b'x']));
    let want = Ev::GoAway {
        last_stream: 5,
        code: 2,
    };
    assert_eq!(got, [want]);
    assert!(s.conn.goaway_received());
    assert_eq!(s.err(&f(Kind::GoAway, 0, 0, &[0; 7])), Error::FrameSize);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&f(Kind::GoAway, 0, 1, &[0; 8])), Error::Protocol);
}

#[test]
fn a_request_is_reported_with_its_fields() {
    let mut s = ready(Config::default());
    let events = s.ok(&get(1));
    assert_eq!(events.len(), 1);
    assert_eq!(stream_of(&events[0]), 1);
    assert_eq!(field(&events[0], ":method"), Some(b"GET".to_vec()));
    assert_eq!(field(&events[0], ":path"), Some(b"/".to_vec()));
    assert_eq!(field(&events[0], ":scheme"), Some(b"https".to_vec()));
    assert_eq!(
        field(&events[0], ":authority"),
        Some(b"example.com".to_vec())
    );
    assert!(matches!(
        events[0],
        Ev::Request {
            end_stream: true,
            ..
        }
    ));
    assert_eq!(s.conn.open_streams(), 1);
    assert_eq!(s.conn.last_stream(), 1);
    assert!(s.conn.is_open(&s.mem, 1));

    let events = s.ok(&post(3));
    assert!(matches!(
        events[0],
        Ev::Request {
            end_stream: false,
            ..
        }
    ));
    assert_eq!(s.take_ctl(), []);
}

#[test]
fn the_fields_borrow_lasts_while_a_response_is_framed() {
    let mut s = ready(Config::default());
    let src = get(1);
    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 0,
    };
    let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
    let Event::Request { stream, fields, .. } = stepped.event else {
        panic!("not a request");
    };
    let path = header::find(fields, ":path").unwrap();
    let mut out = [0u8; 128];
    let echo: [(&[u8], &[u8]); 1] = [(b"x-path", path)];
    let n = headers_frame(&mut out, stream, 200, &echo, true).unwrap();
    assert!(s.conn.sent_headers(&mut s.mem, stream, true));
    assert_eq!(s.conn.open_streams(), 0);
    assert_eq!(out[n - 1], b'/');
}

#[test]
fn stream_ids_must_be_odd_and_climb() {
    let mut s = ready(Config::default());
    assert_eq!(s.err(&get(2)), Error::Protocol);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&get(0)), Error::Protocol);

    let mut s = ready(Config::default());
    s.ok(&get(5));
    assert_eq!(s.err(&get(3)), Error::Protocol);

    // reusing a closed id
    let mut s = ready(Config::default());
    s.ok(&get(5));
    assert!(s.conn.sent_headers(&mut s.mem, 5, true));
    assert_eq!(s.conn.open_streams(), 0);
    assert_eq!(s.err(&get(5)), Error::Protocol);
}

#[test]
fn continuation_reassembles_a_block_across_three_frames() {
    let mut b = request_block("GET", "/some/longer/path");
    b.extend(block(&[("accept", "text/html"), ("x-long", "0123456789")]));
    let (one, rest) = b.split_at(b.len() / 3);
    let (two, three) = rest.split_at(rest.len() / 2);

    let mut s = ready(Config::default());
    assert_eq!(s.ok(&f(Kind::Headers, ES, 1, one)), []);
    assert_eq!(s.conn.open_streams(), 0, "not a request until it is whole");
    assert_eq!(s.ok(&f(Kind::Continuation, 0, 1, two)), []);
    assert_eq!(s.ok(&f(Kind::Continuation, 0, 1, &[])), []);
    let events = s.ok(&f(Kind::Continuation, EH, 1, three));
    assert_eq!(events.len(), 1);
    assert_eq!(
        field(&events[0], ":path"),
        Some(b"/some/longer/path".to_vec())
    );
    assert_eq!(field(&events[0], "x-long"), Some(b"0123456789".to_vec()));
    assert!(matches!(
        events[0],
        Ev::Request {
            end_stream: true,
            ..
        }
    ));

    assert_eq!(s.ok(&f(Kind::Headers, 0, 3, one)), []);
    let mut tail = two.to_vec();
    tail.extend_from_slice(three);
    let events = s.ok(&f(Kind::Continuation, EH, 3, &tail));
    assert!(matches!(
        events[0],
        Ev::Request {
            end_stream: false,
            ..
        }
    ));
    assert_eq!(field(&events[0], "accept"), Some(b"text/html".to_vec()));
}

#[test]
fn nothing_may_interleave_with_a_header_block() {
    let b = request_block("GET", "/");
    let others = [
        f(Kind::Ping, 0, 0, &[0; 8]),
        f(Kind::Continuation, EH, 3, &b[2..]),
        data(1, 0, b"x"),
        get(3),
        f(Kind::Unknown(0x77), 0, 1, &[]),
        settings(&[]),
        window_update(0, 1),
    ];
    for other in others {
        let mut s = ready(Config::default());
        assert_eq!(s.ok(&f(Kind::Headers, ES, 1, &b[..2])), []);
        assert_eq!(s.err(&other), Error::Protocol, "{other:?}");
    }
}

#[test]
fn a_header_block_past_the_limit_is_a_compression_error() {
    let cfg = Config {
        max_header_block: 64,
        ..Config::default()
    };
    let mut b = request_block("GET", "/");
    b.extend(block(&[("x-pad", &"p".repeat(64))]));
    let mut s = ready(cfg);
    assert_eq!(s.err(&f(Kind::Headers, EH | ES, 1, &b)), Error::Compression);

    let mut s = ready(cfg);
    assert_eq!(s.ok(&f(Kind::Headers, ES, 1, &b[..40])), []);
    assert_eq!(
        s.err(&f(Kind::Continuation, EH, 1, &b[40..])),
        Error::Compression
    );

    let mut b = request_block("GET", "/");
    let pad = 64 - b.len() - 2 - 6;
    b.extend(block(&[("x-pad", &"p".repeat(pad))]));
    assert_eq!(b.len(), 64);
    let mut s = ready(cfg);
    assert_eq!(s.ok(&f(Kind::Headers, EH | ES, 1, &b)).len(), 1);
}

#[test]
fn undecodable_or_too_large_header_lists_are_compression_errors() {
    let mut s = ready(Config::default());
    assert_eq!(
        s.err(&f(Kind::Headers, EH | ES, 1, &[0x80 | 70])),
        Error::Compression
    );

    let cfg = Config {
        max_header_list: 48,
        ..Config::default()
    };
    let mut s = ready(cfg);
    assert_eq!(s.err(&get(1)), Error::Compression);
    assert_eq!(Error::Compression.code(), 9);
}

#[test]
fn padded_and_priority_flagged_headers() {
    let b = request_block("GET", "/padded");
    let mut payload = vec![3u8];
    payload.extend([0x80, 0, 0, 1, 200]);
    payload.extend(&b);
    payload.extend([0, 0, 0]);
    let flags = EH | ES | flag::PADDED | flag::PRIORITY;
    let mut s = ready(Config::default());
    let events = s.ok(&f(Kind::Headers, flags, 1, &payload));
    assert_eq!(field(&events[0], ":path"), Some(b"/padded".to_vec()));

    let mut s = ready(Config::default());
    let bad = f(Kind::Headers, EH | flag::PADDED, 1, &[9, 0x82, 0, 0]);
    assert_eq!(s.err(&bad), Error::Protocol);
    let mut s = ready(Config::default());
    let bad = f(Kind::Headers, EH | flag::PRIORITY, 1, &[0, 0, 0, 1]);
    assert_eq!(s.err(&bad), Error::FrameSize);
}

#[test]
fn a_malformed_request_is_a_stream_error() {
    let ok = [(":method", "GET"), (":scheme", "https"), (":path", "/")];
    let with = |extra: &[(&str, &str)], at: usize| {
        let mut list = ok.to_vec();
        for (i, e) in extra.iter().enumerate() {
            list.insert(at + i, *e);
        }
        block(&list)
    };
    let cases: [(&str, Vec<u8>); 8] = [
        ("no method", block(&ok[1..])),
        ("no path", block(&ok[..2])),
        ("pseudo after regular", with(&[("accept", "*/*")], 1)),
        ("uppercase name", with(&[("Accept", "*/*")], 3)),
        ("connection header", with(&[("connection", "close")], 3)),
        ("response pseudo header", with(&[(":status", "200")], 0)),
        ("two methods", with(&[(":method", "POST")], 3)),
        ("empty name", with(&[("", "x")], 3)),
    ];
    let mut s = ready(Config::default());
    let mut stream = 1;
    for (what, b) in cases {
        assert_eq!(s.ok(&f(Kind::Headers, EH | ES, stream, &b)), [], "{what}");
        assert_eq!(s.take_frames(), [rst(stream, 1)], "{what}");
        assert_eq!(s.conn.open_streams(), 0, "{what}");
        assert_eq!(s.conn.last_stream(), stream, "the id is spent, {what}");
        stream += 2;
    }
    let events = s.ok(&f(
        Kind::Headers,
        EH | ES,
        stream,
        &with(&[("te", "trailers")], 3),
    ));
    assert_eq!(events.len(), 1);
    assert_eq!(s.take_ctl(), []);
}

#[test]
fn a_full_stream_table_refuses_but_keeps_hpack_in_step() {
    let cfg = Config {
        max_streams: 2,
        ..Config::default()
    };
    let mut s = ready(cfg);
    s.ok(&get(1));
    s.ok(&get(3));
    assert_eq!(s.conn.open_streams(), 2);

    // refused but still indexed
    let mut b = request_block("GET", "/");
    b.extend(indexing("x-key", "from the refused one"));
    assert_eq!(s.ok(&f(Kind::Headers, EH | ES, 5, &b)), []);
    assert_eq!(s.take_frames(), [rst(5, 7)]);
    assert_eq!(s.conn.open_streams(), 2);
    assert_eq!(s.live(), [(1, 0), (3, 0)]);

    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    let mut b = request_block("GET", "/");
    b.push(0x80 | 62);
    let events = s.ok(&f(Kind::Headers, EH | ES, 7, &b));
    assert_eq!(
        field(&events[0], "x-key"),
        Some(b"from the refused one".to_vec())
    );
    assert_eq!(s.live(), [(3, 0), (7, 0)]);
}

#[test]
fn trailers_end_the_body() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&data(1, 0, b"body"));
    let trailers = block(&[("x-checksum", "abc")]);
    let got = s.ok(&f(Kind::Headers, EH | ES, 1, &trailers));
    let want = Ev::Data {
        stream: 1,
        end_stream: true,
        body: Vec::new(),
    };
    assert_eq!(got, [want]);
    assert_eq!(s.conn.open_streams(), 1, "our side is still open");
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    assert_eq!(s.conn.open_streams(), 0);

    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert_eq!(s.err(&f(Kind::Headers, EH, 1, &trailers)), Error::Protocol);

    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert!(s.conn.set_tag(&mut s.mem, 1, 77));
    let got = s.ok(&f(Kind::Headers, EH | ES, 1, &trailers));
    assert_eq!(got, [reset(1, 5, 77)]);
    assert_eq!(s.take_frames(), [rst(1, 5)]);
    assert_eq!(s.conn.open_streams(), 0);
}

#[test]
fn data_is_reported_in_place_and_the_windows_are_given_back() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    let src = data(1, flag::PADDED, b"\x05ten bytes!\0\0\0\0\0");
    assert_eq!(s.step(&src).unwrap().0, src.len());
    // padding and its length byte count
    assert_eq!(s.take_frames(), [wu(0, 16), wu(1, 16)]);

    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 0,
    };
    let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
    let want = Event::Data {
        stream: 1,
        end_stream: false,
        at: 10,
        len: 10,
    };
    assert_eq!(stepped.event, want);
    assert_eq!(&src[10..20], b"ten bytes!");

    assert_eq!(s.ok(&data(1, 0, b"")), []);
    assert_eq!(s.take_ctl(), []);

    // last frame only replenishes the connection
    let got = s.ok(&data(1, ES, b"bye"));
    let want = Ev::Data {
        stream: 1,
        end_stream: true,
        body: b"bye".to_vec(),
    };
    assert_eq!(got, [want]);
    assert_eq!(s.take_frames(), [wu(0, 3)]);
    assert!(s.conn.is_open(&s.mem, 1), "we have not answered yet");

    s.ok(&post(3));
    let got = s.ok(&data(3, ES, b""));
    assert!(matches!(
        got[0],
        Ev::Data {
            end_stream: true,
            ..
        }
    ));
    assert_eq!(s.take_ctl(), []);
}

#[test]
fn data_on_the_wrong_stream() {
    let mut s = ready(Config::default());
    assert_eq!(s.err(&data(1, 0, b"x")), Error::Protocol);
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert_eq!(s.err(&data(2, 0, b"x")), Error::Protocol);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&data(0, 0, b"x")), Error::Protocol);
    let mut s = ready(Config::default());
    s.ok(&post(1));
    let bad = data(1, flag::PADDED, &[4, 1, 2]);
    assert_eq!(s.err(&bad), Error::Protocol);

    // peer already ended it, stream error
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert!(s.conn.set_tag(&mut s.mem, 1, 41));
    assert_eq!(s.ok(&data(1, 0, b"late")), [reset(1, 5, 41)]);
    assert_eq!(s.take_frames(), [wu(0, 4), rst(1, 5)]);
    assert_eq!(s.conn.open_streams(), 0);

    // gone, only the connection window counts it
    assert_eq!(s.ok(&data(1, ES, b"later")), []);
    assert_eq!(s.take_frames(), [wu(0, 5)]);
}

#[test]
fn a_frame_larger_than_our_window_is_a_flow_control_error() {
    // connection window is 65535 regardless of settings
    let cfg = Config {
        max_frame: 100_000,
        initial_window: 1 << 20,
        ..Config::default()
    };
    let mut s = ready(cfg);
    s.ok(&post(1));
    assert_eq!(s.ok(&data(1, 0, &vec![0; 65_535])).len(), 1);
    assert_eq!(s.err(&data(1, 0, &vec![0; 65_536])), Error::FlowControl);

    // stream window, only that stream is reset
    let cfg = Config {
        initial_window: 100,
        ..Config::default()
    };
    let mut s = ready(cfg);
    s.ok(&f(Kind::Settings, flag::ACK, 0, &[]));
    s.ok(&post(1));
    s.ok(&post(3));
    assert_eq!(s.ok(&data(1, 0, &[0; 100])).len(), 1);
    assert_eq!(s.ok(&data(1, 0, &[0; 100])).len(), 1, "it was given back");
    s.take_ctl();
    assert!(s.conn.set_tag(&mut s.mem, 1, 0xfeed));
    assert_eq!(s.ok(&data(1, 0, &[0; 101])), [reset(1, 3, 0xfeed)]);
    assert_eq!(s.take_frames(), [wu(0, 101), rst(1, 3)]);
    assert_eq!(s.live(), [(3, 0)]);
    assert_eq!(Error::FlowControl.code(), 3);
}

#[test]
fn our_window_binds_only_once_the_peer_has_acked_it() {
    let cfg = Config {
        initial_window: 100,
        ..Config::default()
    };
    let mut s = ready(cfg);
    // before the ack the peer may assume 65535
    s.ok(&post(1));
    assert_eq!(s.ok(&data(1, 0, &[0; 5000])).len(), 1);
    s.ok(&f(Kind::Settings, flag::ACK, 0, &[]));
    s.ok(&post(3));
    s.take_ctl();
    assert_eq!(s.ok(&data(3, 0, &[0; 101])), [reset(3, 3, 0)]);
    assert_eq!(s.ok(&data(1, 0, &[0; 101])), [reset(1, 3, 0)]);
    assert_eq!(
        s.take_frames(),
        [wu(0, 101), rst(3, 3), wu(0, 101), rst(1, 3)]
    );
    s.ok(&post(5));
    s.ok(&f(Kind::Settings, flag::ACK, 0, &[]));
    assert_eq!(s.ok(&data(5, 0, &[0; 100])).len(), 1);
}

#[test]
fn a_grant_that_never_left_goes_back_to_both_windows() {
    let mut s = ready(Config::default());
    s.ok(&settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 1000)]));
    s.ok(&get(1));
    assert_eq!(s.conn.grant(&mut s.mem, 1, 600), 600);
    s.conn.ungrant(&mut s.mem, 1, 600);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 1000);
    assert_eq!(s.conn.conn_send_window(), 65_535);
    // stream reset meanwhile
    assert_eq!(s.conn.grant(&mut s.mem, 1, 600), 600);
    s.ok(&f(Kind::RstStream, 0, 1, &8u32.to_be_bytes()));
    s.conn.ungrant(&mut s.mem, 1, 600);
    assert_eq!(s.conn.conn_send_window(), 65_535);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 0);
}

#[test]
fn ungrant_never_lifts_a_window_past_the_maximum() {
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert_eq!(s.conn.grant(&mut s.mem, 1, 512), 512);
    let room = i64::from(i32::MAX) - s.conn.conn_send_window();
    s.ok(&window_update(0, room as u32));
    let room = i64::from(i32::MAX) - s.conn.stream_send_window(&s.mem, 1);
    s.ok(&window_update(1, room as u32));
    s.conn.ungrant(&mut s.mem, 1, 512);
    assert_eq!(s.conn.conn_send_window(), i64::from(i32::MAX));
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), i64::from(i32::MAX));
}

#[test]
fn grant_is_bounded_by_every_window() {
    let mut s = ready(Config::default());
    s.ok(&settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 1000)]));
    s.ok(&get(1));
    assert_eq!(s.conn.grant(&mut s.mem, 1, 0), 0);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 300), 300, "what was asked");
    assert_eq!(s.conn.grant(&mut s.mem, 1, 5000), 700, "the stream window");
    assert_eq!(s.conn.grant(&mut s.mem, 1, 5000), 0);
    assert_eq!(s.conn.stream_send_window(&s.mem, 1), 0);
    assert_eq!(s.conn.conn_send_window(), 64_535);

    assert_eq!(s.ok(&window_update(1, 500)), [Ev::Window { stream: 1 }]);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 5000), 500);

    s.ok(&settings(&[(frame::SETTINGS_INITIAL_WINDOW_SIZE, 65_535)]));
    let mut total = 1500;
    let mut stream = 3;
    for _ in 0..8 {
        s.ok(&get(stream));
        let n = s.conn.grant(&mut s.mem, stream, 100_000);
        assert!(n <= 16_384, "never more than a frame");
        total += n;
        stream += 2;
    }
    assert_eq!(total, 65_535);
    assert_eq!(s.conn.conn_send_window(), 0);
    assert_eq!(s.conn.grant(&mut s.mem, 3, 1), 0);
    assert_eq!(s.ok(&window_update(0, 1000)), [Ev::Window { stream: 0 }]);
    assert_eq!(s.conn.grant(&mut s.mem, 3, 100_000), 1000);

    assert_eq!(s.conn.grant(&mut s.mem, 99, 10), 0);
    s.ok(&window_update(0, 1000));
    s.ok(&post(101));
    assert!(s.conn.sent_headers(&mut s.mem, 101, true));
    assert_eq!(s.conn.grant(&mut s.mem, 101, 10), 0);
    assert_eq!(s.conn.conn_send_window(), 1000);
}

#[test]
fn window_update_errors() {
    let mut s = ready(Config::default());
    assert_eq!(s.err(&window_update(0, 0)), Error::Protocol);
    let mut s = ready(Config::default());
    let most = i32::MAX as u32 - 65_535;
    assert_eq!(s.ok(&window_update(0, most)), [Ev::Window { stream: 0 }]);
    assert_eq!(s.conn.conn_send_window(), i64::from(i32::MAX));
    assert_eq!(s.err(&window_update(0, 1)), Error::FlowControl);
    let mut s = ready(Config::default());
    let short = f(Kind::WindowUpdate, 0, 0, &[0, 0, 1]);
    assert_eq!(s.err(&short), Error::FrameSize);

    // stream errors
    let mut s = ready(Config::default());
    s.ok(&get(1));
    s.ok(&get(3));
    assert!(s.conn.set_tag(&mut s.mem, 1, 11));
    assert!(s.conn.set_tag(&mut s.mem, 3, 33));
    assert_eq!(s.ok(&window_update(1, 0)), [reset(1, 1, 11)]);
    assert_eq!(s.take_frames(), [rst(1, 1)]);
    let got = s.ok(&window_update(3, i32::MAX as u32));
    assert_eq!(got, [reset(3, 3, 33)]);
    assert_eq!(s.take_frames(), [rst(3, 3)]);
    assert_eq!(s.conn.open_streams(), 0);

    // reserved bit ignored
    s.ok(&get(5));
    assert_eq!(
        s.ok(&window_update(5, 0x8000_0001)),
        [Ev::Window { stream: 5 }]
    );
    assert_eq!(s.conn.stream_send_window(&s.mem, 5), 65_536);

    assert_eq!(s.ok(&window_update(1, 10)), []);
    assert_eq!(s.ok(&window_update(1, 0)), []);
    assert_eq!(s.take_ctl(), []);
    assert_eq!(s.err(&window_update(7, 10)), Error::Protocol);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&window_update(2, 10)), Error::Protocol);
}

#[test]
fn a_window_update_after_our_last_frame_wakes_nobody() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    assert_eq!(s.conn.open_streams(), 1, "the peer is still sending");
    assert_eq!(s.ok(&window_update(1, 10)), []);
}

#[test]
fn a_reset_from_the_peer_frees_the_stream() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert!(s.conn.set_tag(&mut s.mem, 1, u64::MAX - 1));
    assert_eq!(s.ok(&rst_stream(1, 8)), [reset(1, 8, u64::MAX - 1)]);
    assert_eq!(s.conn.open_streams(), 0);
    assert!(!s.conn.is_open(&s.mem, 1));
    assert!(
        !s.conn.sent_headers(&mut s.mem, 1, false),
        "drop the response"
    );
    assert_eq!(s.conn.grant(&mut s.mem, 1, 10), 0);
    assert_eq!(s.conn.tag(&s.mem, 1), None);

    assert_eq!(s.ok(&rst_stream(1, 8)), []);
    assert_eq!(s.ok(&window_update(1, 5)), []);
    assert_eq!(s.ok(&data(1, 0, b"abc")), []);
    assert_eq!(s.take_frames(), [wu(0, 3)]);

    assert_eq!(s.err(&rst_stream(3, 8)), Error::Protocol);
    let mut s = ready(Config::default());
    assert_eq!(s.err(&rst_stream(0, 8)), Error::Protocol);
    let mut s = ready(Config::default());
    s.ok(&get(1));
    let short = f(Kind::RstStream, 0, 1, &[0, 0, 8]);
    assert_eq!(s.err(&short), Error::FrameSize);
}

#[test]
fn reset_from_our_side_writes_the_frame_and_frees() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&post(3));
    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 0,
    };
    assert!(s.conn.reset(&mut s.mem, &mut ctl, 1, 8));
    assert_eq!(frames(&ctl.buf[..ctl.len]), [rst(1, 8)]);
    assert_eq!(s.live(), [(3, 0)]);
    // gone or unknown streams send nothing
    assert!(s.conn.reset(&mut s.mem, &mut ctl, 1, 8));
    assert!(s.conn.reset(&mut s.mem, &mut ctl, 99, 8));
    assert_eq!(ctl.len, 13);

    // no room, no change
    ctl.len = 64 - 12;
    assert!(!s.conn.reset(&mut s.mem, &mut ctl, 3, 2));
    assert_eq!(ctl.len, 64 - 12);
    assert_eq!(s.live(), [(3, 0)]);
    ctl.len = 64 - 13;
    assert!(s.conn.reset(&mut s.mem, &mut ctl, 3, 2));
    assert_eq!(frames(&ctl.buf[64 - 13..]), [rst(3, 2)]);
    assert_eq!(s.conn.open_streams(), 0);

    assert_eq!(s.ok(&data(3, ES, b"x")), []);
}

#[test]
fn goaway_names_the_last_stream() {
    let mut s = ready(Config::default());
    s.ok(&get(1));
    s.ok(&get(7));
    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 64 - 16,
    };
    assert!(!s.conn.goaway(&mut ctl, 0), "one byte short");
    ctl.len = 0;
    assert!(s.conn.goaway(&mut ctl, 11));
    let want = (Kind::GoAway, 0, 0, vec![0, 0, 0, 7, 0, 0, 0, 11]);
    assert_eq!(frames(&ctl.buf[..ctl.len]), core::slice::from_ref(&want));

    // later requests decoded and dropped
    let mut b = request_block("GET", "/");
    b.extend(indexing("x-late", "1"));
    assert_eq!(s.ok(&f(Kind::Headers, EH | ES, 9, &b)), []);
    assert_eq!(s.take_ctl(), []);
    assert_eq!(s.live(), [(1, 0), (7, 0)]);
    ctl.len = 0;
    assert!(s.conn.goaway(&mut ctl, 11));
    assert_eq!(frames(&ctl.buf[..ctl.len]), [want]);
    assert!(s.conn.sent_headers(&mut s.mem, 7, true));
    assert_eq!(s.live(), [(1, 0)]);
}

#[test]
fn a_stream_is_freed_when_both_sides_are_done() {
    let mut s = ready(Config::default());
    s.ok(&get(1));
    assert!(s.conn.sent_headers(&mut s.mem, 1, false));
    assert!(s.conn.is_open(&s.mem, 1));
    assert!(!s.conn.sent_headers(&mut s.mem, 3, false), "no such stream");
    assert_eq!(s.conn.grant(&mut s.mem, 1, 10), 10);
    s.conn.sent_end(&mut s.mem, 1);
    assert_eq!(s.conn.open_streams(), 0);
    assert!(!s.conn.is_open(&s.mem, 1));
    s.conn.sent_end(&mut s.mem, 1);
    assert_eq!(s.conn.open_streams(), 0, "a second end is harmless");

    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    assert!(!s.conn.is_open(&s.mem, 1), "our side is closed");
    assert_eq!(s.conn.open_streams(), 1, "but the slot waits for the peer");
    assert!(!s.conn.sent_headers(&mut s.mem, 1, false));
    assert_eq!(s.ok(&data(1, 0, b"still coming")).len(), 1);
    assert_eq!(s.ok(&data(1, ES, b"")).len(), 1);
    assert_eq!(s.conn.open_streams(), 0);
}

#[test]
fn tags_round_trip_and_stream_at_walks_the_live_streams() {
    let mut s = ready(Config::default());
    for stream in [1u32, 3, 5, 7] {
        s.ok(&get(stream));
        assert_eq!(s.conn.tag(&s.mem, stream), Some(0), "a fresh slot is clean");
        assert!(
            s.conn
                .set_tag(&mut s.mem, stream, u64::from(stream) << 40 | 9)
        );
    }
    assert!(!s.conn.set_tag(&mut s.mem, 9, 1));
    assert_eq!(s.conn.tag(&s.mem, 9), None);
    assert_eq!(s.conn.tag(&s.mem, 5), Some(5 << 40 | 9));
    assert!(s.conn.set_tag(&mut s.mem, 5, u64::MAX));
    assert_eq!(s.conn.tag(&s.mem, 5), Some(u64::MAX));

    assert!(s.conn.sent_headers(&mut s.mem, 3, true));
    let t = |n: u64| n << 40 | 9;
    assert_eq!(s.live(), [(1, t(1)), (5, u64::MAX), (7, t(7))]);
    assert_eq!(s.conn.stream_at(&s.mem, 64), None, "past the table");
    assert_eq!(s.conn.stream_at(&s.mem, u32::MAX), None);

    s.conn.grant(&mut s.mem, 1, 100);
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    s.ok(&get(9));
    assert_eq!(s.conn.tag(&s.mem, 9), Some(0));
    assert_eq!(s.conn.stream_send_window(&s.mem, 9), 65_535);
}

#[test]
fn slots_are_reused_after_streams_close() {
    let cfg = Config {
        max_streams: 4,
        ..Config::default()
    };
    let mut s = ready(cfg);
    let mut stream = 1;
    for round in 0..5 {
        let first = stream;
        for _ in 0..4 {
            assert_eq!(s.ok(&get(stream)).len(), 1, "round {round}");
            stream += 2;
        }
        assert_eq!(s.conn.open_streams(), 4);
        assert_eq!(s.ok(&get(stream)), []);
        assert_eq!(s.take_frames(), [rst(stream, 7)]);
        stream += 2;
        for id in (first..first + 8).step_by(2) {
            assert!(s.conn.sent_headers(&mut s.mem, id, true));
        }
        assert_eq!(s.conn.open_streams(), 0);
        assert_eq!(s.live(), []);
    }
}

#[test]
fn blocked_consumes_nothing_and_the_retry_is_clean() {
    let mut s = ready(Config::default());
    let mut b = request_block("GET", "/");
    b.extend(indexing("x-once", "only"));
    let src = f(Kind::Headers, EH | ES, 1, &b);

    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 64 - 25,
    };
    for _ in 0..3 {
        let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
        assert_eq!(stepped.consumed, 0);
        assert_eq!(stepped.event, Event::Blocked);
    }
    assert_eq!(ctl.len, 64 - 25);
    assert_eq!(s.conn.open_streams(), 0);
    assert_eq!(s.conn.last_stream(), 0);

    let stepped = s.conn.step(&mut s.mem, &src[..12], &mut ctl).unwrap();
    assert_eq!(stepped.event, Event::NeedMore);

    ctl.len = 64 - 26;
    let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
    assert_eq!(stepped.consumed, src.len());
    assert!(matches!(stepped.event, Event::Request { stream: 1, .. }));

    // decoded exactly once, entry 62 exists and 63 does not
    let mut b = request_block("GET", "/");
    b.push(0x80 | 62);
    let events = s.ok(&f(Kind::Headers, EH | ES, 3, &b));
    assert_eq!(field(&events[0], "x-once"), Some(b"only".to_vec()));
    let mut b = request_block("GET", "/");
    b.push(0x80 | 63);
    assert_eq!(s.err(&f(Kind::Headers, EH | ES, 5, &b)), Error::Compression);
}

#[test]
fn twenty_six_free_bytes_are_enough_for_any_frame() {
    // worst cases are DATA with two updates, DATA with update and reset, PING
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&get(3));
    let inputs = [
        data(1, 0, b"abc"),
        data(3, 0, b"abc"),
        f(Kind::Ping, 0, 0, &[7; 8]),
        settings(&[]),
    ];
    let wrote = [26, 26, 17, 9];
    for (src, wrote) in inputs.iter().zip(wrote) {
        let mut buf = [0u8; 64];
        let mut ctl = Ctl {
            buf: &mut buf,
            len: 64 - 26,
        };
        let stepped = s.conn.step(&mut s.mem, src, &mut ctl).unwrap();
        assert_eq!(stepped.consumed, src.len());
        assert_eq!(ctl.len, 64 - 26 + wrote);
        frames(&ctl.buf[64 - 26..ctl.len]);
    }
}

#[test]
fn mem_bytes_is_the_whole_cost() {
    let cfg = Config::default();
    assert_eq!(mem_bytes(&cfg), 64 * 32 + (4096 + 128 * 4) + 8192 + 8192);
    assert_eq!(mem_bytes(&cfg), 23_040);

    let mut conn = Conn::new(cfg);
    let mut short = vec![0u8; mem_bytes(&cfg) - 1];
    assert_eq!(conn.init(&mut short), Err(Error::BadStorage));
    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 0,
    };
    let mut mem = vec![0u8; mem_bytes(&cfg)];
    let got = conn.step(&mut mem, frame::PREFACE, &mut ctl);
    assert_eq!(got.unwrap_err(), Error::BadStorage);
    conn.init(&mut mem).unwrap();
    let got = conn.step(&mut short, frame::PREFACE, &mut ctl);
    assert_eq!(got.unwrap_err(), Error::BadStorage);
    assert!(conn.step(&mut mem, frame::PREFACE, &mut ctl).is_ok());
    assert_eq!(Error::BadStorage.code(), 2);

    assert_eq!(conn.tag(&[], 1), None);
    assert_eq!(conn.stream_at(&[], 0), None);
    assert!(!conn.is_open(&[], 1));
    assert_eq!(conn.grant(&mut [], 1, 10), 0);
}

#[test]
fn the_machine_stays_inside_its_storage() {
    // guard bytes on both sides
    let cfg = Config {
        max_streams: 3,
        header_table_size: 96,
        max_header_block: 80,
        max_header_list: 200,
        ..Config::default()
    };
    let need = mem_bytes(&cfg);
    let mut backing = vec![0x5au8; need + 64];
    let mut conn = Conn::new(cfg);
    conn.init(&mut backing[32..32 + need]).unwrap();
    let mut input = hello();
    let mut stream = 1;
    for i in 0..40 {
        let mut b = request_block("POST", "/");
        b.extend(indexing("x-fill", &format!("{i:030}")));
        input.extend(f(Kind::Headers, 0, stream, &b[..10]));
        input.extend(f(Kind::Continuation, EH, stream, &b[10..]));
        input.extend(data(stream, ES, b"body"));
        stream += 2;
    }
    let mut pos = 0;
    let mut requests = 0;
    for _ in 0..STEPS {
        let mut buf = [0u8; 64];
        let mut ctl = Ctl {
            buf: &mut buf,
            len: 0,
        };
        let mem = &mut backing[32..32 + need];
        let stepped = conn.step(mem, &input[pos..], &mut ctl).unwrap();
        pos += stepped.consumed;
        let id = match stepped.event {
            Event::NeedMore => break,
            Event::Request { stream, .. } => stream,
            _ => continue,
        };
        requests += 1;
        assert!(conn.set_tag(mem, id, u64::MAX));
        assert!(conn.sent_headers(mem, id, true));
    }
    assert_eq!(pos, input.len());
    assert_eq!(requests, 40);
    assert!(backing[..32].iter().all(|b| *b == 0x5a));
    assert!(backing[32 + need..].iter().all(|b| *b == 0x5a));
}

#[test]
fn error_codes_are_the_rfc_ones() {
    assert_eq!(Error::BadPreface.code(), 1);
    assert_eq!(Error::Protocol.code(), 1);
    assert_eq!(Error::FlowControl.code(), 3);
    assert_eq!(Error::FrameSize.code(), 6);
    assert_eq!(Error::Compression.code(), 9);
    assert_eq!(Error::BadStorage.code(), 2);
}

#[test]
fn headers_frame_is_one_frame_a_client_can_decode() {
    let fields: [(&[u8], &[u8]); 3] = [
        (b"content-type", b"text/plain"),
        (b"content-length", b"12"),
        (b"x-custom", b"yes"),
    ];
    let mut out = [0u8; 256];
    let n = headers_frame(&mut out, 5, 404, &fields, false).unwrap();
    let all = frames(&out[..n]);
    assert_eq!(all.len(), 1);
    let (kind, flags, stream, payload) = &all[0];
    assert_eq!((*kind, *flags, *stream), (Kind::Headers, EH, 5));
    assert_eq!(payload[0], 0x8d, ":status 404 is a static hit");

    let mut table = hpack::DynamicTable::new(4096);
    let mut got = Vec::new();
    hpack::decode_block(payload, &mut table, 1024, &mut got).unwrap();
    let got: Vec<_> = got.iter().map(|h| (&h.name[..], &h.value[..])).collect();
    let mut want: Vec<(&[u8], &[u8])> = vec![(b":status", b"404")];
    want.extend(fields);
    assert_eq!(got, want);

    let n = headers_frame(&mut out, 5, 204, &[], true).unwrap();
    assert_eq!(out[..n], [0, 0, 1, 1, EH | ES, 0, 0, 0, 5, 0x89]);

    let n = headers_frame(&mut out, 5, 404, &fields, false).unwrap();
    for room in 0..n {
        let mut small = vec![0u8; room];
        assert_eq!(headers_frame(&mut small, 5, 404, &fields, false), None);
    }
    assert_eq!(
        headers_frame(&mut out, 5, 42, &[], false),
        None,
        "no such status"
    );
}

#[test]
fn headers_frame_refuses_a_block_past_one_frame() {
    // 1 for :status, 1 + 1 + 5 for the literal name, 3 for the value length
    let big = vec![b'v'; 16_384 - 11];
    let fields: [(&[u8], &[u8]); 1] = [(b"x-big", &big)];
    let mut out = vec![0u8; 40_000];
    let n = headers_frame(&mut out, 1, 200, &fields, false).unwrap();
    assert_eq!(n, FRAME_HEADER + 16_384, "exactly one full frame");
    assert_eq!(frames(&out[..n]).len(), 1);
    let big = vec![b'v'; 16_384 - 10];
    let fields: [(&[u8], &[u8]); 1] = [(b"x-big", &big)];
    assert_eq!(headers_frame(&mut out, 1, 200, &fields, false), None);
}

#[test]
fn data_header_is_nine_bytes() {
    let mut out = [0xffu8; 12];
    assert_eq!(data_header(&mut out, 7, 0x01_0203, true), Some(9));
    assert_eq!(out[..9], [1, 2, 3, 0, ES, 0, 0, 0, 7]);
    assert_eq!(out[9], 0xff);
    assert_eq!(data_header(&mut out, 7, 0, false), Some(9));
    assert_eq!(out[..9], [0, 0, 0, 0, 0, 0, 0, 0, 7]);
    assert_eq!(data_header(&mut out[..8], 7, 0, false), None);
    assert_eq!(data_header(&mut out, 7, 1 << 24, false), None);
}

#[test]
fn end_stream_is_set_on_a_written_frame() {
    let mut out = [0u8; 64];
    let n = headers_frame(&mut out, 5, 200, &[], false).unwrap();
    assert_eq!(out[4], flag::END_HEADERS);
    assert!(end_stream(&mut out[..n]));
    assert_eq!(out[4], flag::END_HEADERS | ES);
    assert!(end_stream(&mut out[..n]), "twice is the same");
    assert_eq!(out[4], flag::END_HEADERS | ES);
    let mut data = [0u8; 9];
    data_header(&mut data, 5, 0, false).unwrap();
    assert!(end_stream(&mut data));
    assert_eq!(data, [0, 0, 0, 0, ES, 0, 0, 0, 5]);
    assert!(!end_stream(&mut data[..8]));
    assert_eq!(data[4], ES, "a short slice is left alone");
}

/// Step until input is used, an error, or the bound. Checks invariants for any input.
fn run_any(input: &[u8], cfg: Config, what: &str) {
    let mut s = srv(cfg);
    let mut pos = 0;
    let mut buf = [0u8; 64];
    let mut len = 0;
    for _ in 0..STEPS {
        let mut ctl = Ctl { buf: &mut buf, len };
        let src = &input[pos..];
        let stepped = match s.conn.step(&mut s.mem, src, &mut ctl) {
            Ok(stepped) => stepped,
            Err(e) => {
                assert!([1, 2, 3, 6, 9].contains(&e.code()));
                return;
            }
        };
        assert!(stepped.consumed <= src.len(), "{what}");
        pos += stepped.consumed;
        let ev = own(&stepped.event, src);
        len = ctl.len;
        assert!(len <= 64, "{what}");
        frames(&buf[..len]);
        assert!(s.conn.open_streams() <= cfg.max_streams, "{what}");
        s.live();
        match ev {
            Ev::NeedMore => return,
            // leave ctl to fill so Blocked is reached too
            Ev::Blocked => len = 0,
            Ev::Request { stream, .. } if stream % 4 == 1 => {
                s.conn.sent_headers(&mut s.mem, stream, stream % 8 == 1);
            }
            _ => {}
        }
    }
    panic!("{what} did not finish in {STEPS} steps");
}

fn small() -> Config {
    Config {
        max_streams: 4,
        header_table_size: 128,
        max_header_block: 256,
        max_header_list: 256,
        ..Config::default()
    }
}

#[test]
fn random_bytes_never_panic() {
    for seed in 0..300u64 {
        let mut rng = Rng(seed);
        let mut input = hello();
        let len = rng.below(600);
        input.extend((0..len).map(|_| rng.next() as u8));
        run_any(&input, small(), &format!("seed {seed}"));
    }
}

#[test]
fn random_frames_never_panic() {
    let kinds = [0u8, 0, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 8, 9, 9, 0x20];
    let streams = [0u32, 0, 1, 1, 2, 3, 3, 5, 7, 9, 11];
    let flags = [
        0u8,
        ES,
        EH,
        EH | ES,
        flag::PADDED | EH,
        flag::PRIORITY | EH,
        0xff,
    ];
    for seed in 0..300u64 {
        let mut rng = Rng(seed ^ 0x5eed);
        let mut input = hello();
        for _ in 0..rng.below(60) {
            let kind = Kind::from_u8(kinds[rng.below(kinds.len() as u64)]);
            let stream = streams[rng.below(streams.len() as u64)];
            let flag = flags[rng.below(flags.len() as u64)];
            let payload: Vec<u8> = match rng.below(4) {
                0 => request_block("GET", "/"),
                1 => vec![0, 0, 0, 1 + rng.below(9) as u8],
                _ => (0..rng.below(24)).map(|_| rng.next() as u8).collect(),
            };
            input.extend(f(kind, flag, stream, &payload));
        }
        run_any(&input, small(), &format!("seed {seed}"));
    }
}

#[test]
fn a_mutated_byte_never_panics() {
    let base = recorded();
    for seed in 0..300u64 {
        let mut rng = Rng(seed ^ 0xface);
        let mut input = base.clone();
        let at = frame::PREFACE.len() + rng.below((base.len() - frame::PREFACE.len()) as u64);
        input[at] ^= 1 << rng.below(8);
        if seed % 3 == 0 {
            input[at] = rng.next() as u8;
        }
        run_any(&input, Config::default(), &format!("seed {seed} at {at}"));
        let cut = rng.below(input.len() as u64);
        run_any(&input[..cut], small(), &format!("seed {seed} cut {cut}"));
    }
}

#[test]
fn ctl_that_is_never_emptied_only_blocks() {
    let mut s = ready(Config::default());
    s.ok(&post(1));
    let src = data(1, 0, b"abc");
    let mut buf = [0u8; 64];
    let mut ctl = Ctl {
        buf: &mut buf,
        len: 0,
    };
    let mut taken = 0;
    for _ in 0..10 {
        let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
        if stepped.event == Event::Blocked {
            assert_eq!(stepped.consumed, 0);
            continue;
        }
        taken += 1;
    }
    // two rounds of two updates fit, the third finds 12 free
    assert_eq!(taken, 2);
    assert_eq!(ctl.len, 52);
}

/// WebSocket over h2, RFC 8441.
const WS: [(&str, &str); 6] = [
    (":method", "CONNECT"),
    (":protocol", "websocket"),
    (":scheme", "https"),
    (":path", "/chat"),
    (":authority", "example.com"),
    ("sec-websocket-version", "13"),
];

/// As [`ready`] with extended CONNECT enabled.
fn extended() -> Srv {
    let mut s = srv(Config::default());
    s.conn.enable_connect_protocol();
    assert_eq!(s.ok(&hello()), []);
    assert_eq!(s.take_ctl(), f(Kind::Settings, flag::ACK, 0, &[]));
    s
}

/// Default SETTINGS without extended CONNECT.
const OLD_SETTINGS: [u8; 39] = [
    0, 0, 30, 4, 0, 0, 0, 0, 0, // 30 bytes, SETTINGS, stream 0
    0, 1, 0, 0, 0x10, 0x00, // HEADER_TABLE_SIZE 4096
    0, 3, 0, 0, 0x00, 0x40, // MAX_CONCURRENT_STREAMS 64
    0, 4, 0, 0, 0xff, 0xff, // INITIAL_WINDOW_SIZE 65535
    0, 5, 0, 0, 0x40, 0x00, // MAX_FRAME_SIZE 16384
    0, 6, 0, 0, 0x20, 0x00, // MAX_HEADER_LIST_SIZE 8192
];

#[test]
fn settings_without_the_switch_are_byte_for_byte_what_they_were() {
    let mut conn = Conn::new(Config::default());
    let mut out = [0xaau8; 64];
    let n = conn.write_settings(&mut out).unwrap();
    assert_eq!(out[..n], OLD_SETTINGS);
    assert_eq!(out[n], 0xaa);
}

#[test]
fn the_switch_adds_one_setting_and_survives_init() {
    let mut conn = Conn::new(Config::default());
    conn.enable_connect_protocol();
    let mut out = [0xaau8; 64];
    let n = conn.write_settings(&mut out).unwrap();
    assert_eq!(n, 45);
    assert_eq!(out[..9], [0, 0, 36, 4, 0, 0, 0, 0, 0]);
    assert_eq!(out[9..39], OLD_SETTINGS[9..]);
    assert_eq!(
        out[39..45],
        [0, 8, 0, 0, 0, 1],
        "ENABLE_CONNECT_PROTOCOL is 1"
    );
    assert_eq!(out[45], 0xaa);
    assert_eq!(conn.write_settings(&mut out[..44]), None);
    let mut mem = vec![0u8; mem_bytes(&Config::default())];
    conn.init(&mut mem).unwrap();
    assert_eq!(conn.write_settings(&mut out), Some(45));
}

#[test]
fn connect_with_a_protocol_is_refused_when_not_advertised() {
    let mut s = ready(Config::default());
    assert_eq!(s.ok(&f(Kind::Headers, EH, 1, &block(&WS))), []);
    assert_eq!(s.take_frames(), [rst(1, 1)], "PROTOCOL_ERROR");
    assert_eq!(s.conn.open_streams(), 0);
    assert_eq!(s.conn.last_stream(), 1);
    assert_eq!(s.ok(&get(3)).len(), 1);
    assert_eq!(s.take_ctl(), []);
}

#[test]
fn connect_with_a_protocol_is_a_request_when_advertised() {
    let mut s = extended();
    let events = s.ok(&f(Kind::Headers, EH, 1, &block(&WS)));
    assert_eq!(events.len(), 1);
    assert_eq!(stream_of(&events[0]), 1);
    assert!(matches!(
        events[0],
        Ev::Request {
            end_stream: false,
            ..
        }
    ));
    assert_eq!(field(&events[0], ":method"), Some(b"CONNECT".to_vec()));
    assert_eq!(field(&events[0], ":protocol"), Some(b"websocket".to_vec()));
    assert_eq!(field(&events[0], ":path"), Some(b"/chat".to_vec()));
    assert_eq!(s.take_ctl(), [], "nothing is refused");
    assert!(s.conn.sent_headers(&mut s.mem, 1, false));
    let frame = [
        0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
    ];
    let body = |end_stream, bytes: &[u8]| Ev::Data {
        stream: 1,
        end_stream,
        body: bytes.to_vec(),
    };
    assert_eq!(s.ok(&data(1, 0, &frame)), [body(false, &frame)]);
    assert_eq!(s.conn.grant(&mut s.mem, 1, 7), 7);
    assert_eq!(s.ok(&data(1, ES, b"")), [body(true, b"")]);
    s.conn.sent_end(&mut s.mem, 1);
    assert_eq!(s.conn.open_streams(), 0);
}

#[test]
fn the_protocol_of_a_request_is_read_off_its_fields() {
    let mut s = extended();
    for (stream, src, want) in [
        (
            1,
            f(Kind::Headers, EH, 1, &block(&WS)),
            Some(&b"websocket"[..]),
        ),
        (3, get(3), None),
    ] {
        let mut buf = [0u8; 64];
        let mut ctl = Ctl {
            buf: &mut buf,
            len: 0,
        };
        let stepped = s.conn.step(&mut s.mem, &src, &mut ctl).unwrap();
        let Event::Request { fields, .. } = stepped.event else {
            panic!("stream {stream} is not a request");
        };
        assert_eq!(protocol(fields), want, "stream {stream}");
    }
}

#[test]
fn a_malformed_extended_connect_is_a_stream_error() {
    let without = |name: &str| {
        let list: Vec<_> = WS.iter().copied().filter(|(n, _)| *n != name).collect();
        block(&list)
    };
    let mut get_with = WS.to_vec();
    get_with[0] = (":method", "GET");
    let mut twice = WS.to_vec();
    twice.insert(2, (":protocol", "websocket"));
    let mut late = without(":protocol");
    late.extend(block(&[(":protocol", "websocket")]));
    let cases: [(&str, Vec<u8>); 6] = [
        ("a protocol on a GET", block(&get_with)),
        ("two protocols", block(&twice)),
        ("no scheme", without(":scheme")),
        ("no path", without(":path")),
        ("no method", without(":method")),
        ("a protocol behind a regular field", late),
    ];
    let mut s = extended();
    let mut stream = 1;
    for (what, b) in cases {
        assert_eq!(s.ok(&f(Kind::Headers, EH, stream, &b)), [], "{what}");
        assert_eq!(s.take_frames(), [rst(stream, 1)], "{what}");
        assert_eq!(s.conn.open_streams(), 0, "{what}");
        stream += 2;
    }
    // plain CONNECT has no path
    let plain = block(&[(":method", "CONNECT"), (":authority", "example.com:443")]);
    for mut s in [extended(), ready(Config::default())] {
        assert_eq!(s.ok(&f(Kind::Headers, EH, 1, &plain)), []);
        assert_eq!(s.take_frames(), [rst(1, 1)]);
    }
}

#[test]
fn the_switch_changes_nothing_else() {
    let input = recorded();
    let mut off = srv(Config::default());
    let mut on = srv(Config::default());
    on.conn.enable_connect_protocol();
    let events = off.ok(&input);
    assert_eq!(on.ok(&input), events);
    assert_eq!(on.take_ctl(), off.take_ctl());
    assert!(!events.is_empty());
    for seed in 0..100u64 {
        let mut rng = Rng(seed ^ 0x8441);
        let mut input = hello();
        for _ in 0..rng.below(30) {
            let kind = Kind::from_u8(rng.below(10) as u8);
            let stream = [0u32, 1, 1, 3, 5][rng.below(5)];
            let flags = [0u8, ES, EH, EH | ES][rng.below(4)];
            let payload = match rng.below(3) {
                0 => request_block("GET", "/"),
                1 => vec![0, 0, 0, 1 + rng.below(9) as u8],
                _ => (0..rng.below(24)).map(|_| rng.next() as u8).collect(),
            };
            input.extend(f(kind, flags, stream, &payload));
        }
        let mut off = srv(Config::default());
        let mut on = srv(Config::default());
        on.conn.enable_connect_protocol();
        let (a, b) = (off.feed(&input), on.feed(&input));
        assert_eq!(a, b, "seed {seed}");
        assert_eq!(off.take_ctl(), on.take_ctl(), "seed {seed}");
    }
}

/// Golden bytes, a head, a DATA header and a bodyless response.
const OLD_RESPONSE: [u8; 60] = [
    0, 0, 32, 1, 4, 0, 0, 0, 5, 136, 15, 16, 10, 116, 101, 120, 116, 47, 112, 108, 97, 105, 110,
    15, 13, 1, 53, 0, 8, 120, 45, 99, 117, 115, 116, 111, 109, 3, 121, 101, 115, 0, 0, 5, 0, 1, 0,
    0, 0, 5, 0, 0, 1, 1, 5, 0, 0, 0, 7, 137,
];

/// Golden ctl for a body ending in DATA, two updates for four bytes.
const OLD_CTL: [u8; 26] = [
    0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 4, 8, 0, 0, 0, 0, 1, 0, 0, 0, 4,
];

#[test]
fn without_trailers_everything_is_byte_for_byte_what_it_was() {
    let fields: [(&[u8], &[u8]); 3] = [
        (b"content-type", b"text/plain"),
        (b"content-length", b"5"),
        (b"x-custom", b"yes"),
    ];
    let mut out = [0u8; 128];
    let mut n = headers_frame(&mut out, 5, 200, &fields, false).unwrap();
    n += data_header(&mut out[n..], 5, 5, true).unwrap();
    n += headers_frame(&mut out[n..], 7, 204, &[], true).unwrap();
    assert_eq!(out[..n], OLD_RESPONSE);

    let mut s = ready(Config::default());
    let mut wire = post(1);
    wire.extend(data(1, 0, b"body"));
    wire.extend(data(1, ES, b""));
    let events = s.ok(&wire);
    assert_eq!(events.len(), 3);
    assert_eq!(stream_of(&events[0]), 1);
    let body = |end_stream, body: &[u8]| Ev::Data {
        stream: 1,
        end_stream,
        body: body.to_vec(),
    };
    assert_eq!(events[1..], [body(false, b"body"), body(true, b"")]);
    assert_eq!(s.take_ctl(), OLD_CTL);
    assert_eq!(s.conn.trailers(&s.mem, 1), None, "the body ended in DATA");
}

fn trailer_fields(s: &Srv, stream: u32) -> Option<Fields> {
    let fields = s.conn.trailers(&s.mem, stream)?;
    let own = |(n, v): (&[u8], &[u8])| (n.to_vec(), v.to_vec());
    Some(header::iter(fields).map(own).collect())
}

fn pairs(fields: &[(&str, &str)]) -> Fields {
    let own = |(n, v): &(&str, &str)| (n.as_bytes().to_vec(), v.as_bytes().to_vec());
    fields.iter().map(own).collect()
}

fn ended(stream: u32) -> Ev {
    Ev::Data {
        stream,
        end_stream: true,
        body: Vec::new(),
    }
}

#[test]
fn trailers_are_read_until_the_next_frame() {
    let sent = [("x-checksum", "abc"), ("grpc-status", "0")];
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&post(3));
    s.ok(&data(1, 0, b"body"));
    assert_eq!(trailer_fields(&s, 1), None);
    let got = s.ok(&f(Kind::Headers, EH | ES, 1, &block(&sent)));
    assert_eq!(got, [ended(1)]);
    assert_eq!(trailer_fields(&s, 1), Some(pairs(&sent)));
    assert_eq!(trailer_fields(&s, 3), None);
    assert_eq!(trailer_fields(&s, 0), None);
    let ping = f(Kind::Ping, 0, 0, &[0; 8]);
    assert_eq!(s.step(&ping[..5]).unwrap(), (0, Ev::NeedMore));
    assert_eq!(trailer_fields(&s, 1), Some(pairs(&sent)));
    s.ok(&ping);
    assert_eq!(trailer_fields(&s, 1), None);

    let got = s.ok(&f(Kind::Headers, EH | ES, 3, &[]));
    assert_eq!(got, [ended(3)]);
    assert_eq!(trailer_fields(&s, 3), Some(Vec::new()));

    s.conn.init(&mut s.mem).unwrap();
    assert_eq!(trailer_fields(&s, 3), None);
}

#[test]
fn trailers_come_whole_through_continuation() {
    let sent = [("x-a", "1"), ("x-b", "22"), ("x-c", "333")];
    let all = block(&sent);
    let (a, rest) = all.split_at(3);
    let (b, c) = rest.split_at(4);
    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert_eq!(s.ok(&f(Kind::Headers, ES, 1, a)), []);
    assert_eq!(s.ok(&f(Kind::Continuation, 0, 1, b)), []);
    assert_eq!(trailer_fields(&s, 1), None, "not before the block is whole");
    assert_eq!(s.ok(&f(Kind::Continuation, EH, 1, c)), [ended(1)]);
    assert_eq!(trailer_fields(&s, 1), Some(pairs(&sent)));
}

#[test]
fn malformed_trailers_are_a_stream_error() {
    let bad: [&[(&str, &str)]; 5] = [
        &[(":status", "200")],
        &[("x-a", "1"), (":path", "/")],
        &[(":method", "POST"), (":path", "/")],
        &[("X-Upper", "1")],
        &[("connection", "close")],
    ];
    for fields in bad {
        let mut s = ready(Config::default());
        s.ok(&post(1));
        assert!(s.conn.set_tag(&mut s.mem, 1, 9));
        s.ok(&data(1, 0, b"body"));
        s.take_ctl();
        // indexed before being refused
        let mut trailers = indexing("x-key", "kept");
        trailers.extend(block(fields));
        let got = s.ok(&f(Kind::Headers, EH | ES, 1, &trailers));
        assert_eq!(got, [reset(1, 1, 9)], "{fields:?}");
        assert_eq!(s.take_frames(), [rst(1, 1)]);
        assert_eq!(trailer_fields(&s, 1), None);
        assert_eq!(s.conn.open_streams(), 0);
        // 62 is the first dynamic entry
        let mut next = request_block("GET", "/");
        next.push(0x80 | 62);
        let events = s.ok(&f(Kind::Headers, EH | ES, 3, &next));
        assert_eq!(field(&events[0], "x-key"), Some(b"kept".to_vec()));
    }
}

#[test]
fn trailers_must_end_the_stream_and_come_once() {
    let trailers = block(&[("x-checksum", "abc")]);
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&data(1, 0, b"body"));
    assert_eq!(s.err(&f(Kind::Headers, EH, 1, &trailers)), Error::Protocol);
    assert_eq!(trailer_fields(&s, 1), None);

    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&data(1, ES, b"body"));
    s.take_ctl();
    let got = s.ok(&f(Kind::Headers, EH | ES, 1, &trailers));
    assert_eq!(got, [reset(1, 5, 0)]);
    assert_eq!(s.take_frames(), [rst(1, 5)]);
    assert_eq!(trailer_fields(&s, 1), None);

    let mut s = ready(Config::default());
    s.ok(&post(1));
    assert_eq!(s.ok(&f(Kind::Headers, EH | ES, 1, &trailers)), [ended(1)]);
    let got = s.ok(&f(Kind::Headers, EH | ES, 1, &trailers));
    assert_eq!(got, [reset(1, 5, 0)]);
    assert_eq!(trailer_fields(&s, 1), None);
}

fn decoded(payload: &[u8]) -> Fields {
    let mut table = hpack::DynamicTable::new(4096);
    let mut got = Vec::new();
    hpack::decode_block(payload, &mut table, 1 << 20, &mut got).unwrap();
    got.iter()
        .map(|h| (h.name.to_vec(), h.value.to_vec()))
        .collect()
}

#[test]
fn trailers_are_sent_behind_the_data() {
    let fields: [(&[u8], &[u8]); 2] = [(b"grpc-status", b"0"), (b"x-checksum", b"abc")];
    let mut s = ready(Config::default());
    s.ok(&post(1));
    s.ok(&data(1, ES, b"body"));

    let mut out = [0u8; 256];
    let head: [(&[u8], &[u8]); 1] = [(b"content-type", b"application/grpc")];
    let mut n = headers_frame(&mut out, 1, 200, &head, false).unwrap();
    assert!(s.conn.sent_headers(&mut s.mem, 1, false));
    assert_eq!(s.conn.grant(&mut s.mem, 1, 5), 5);
    n += data_header(&mut out[n..], 1, 5, false).unwrap();
    out[n..n + 5].copy_from_slice(b"hello");
    n += 5;
    n += trailers_frame(&mut out[n..], 1, &fields).unwrap();
    assert_eq!(s.conn.open_streams(), 1);
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    assert_eq!(s.conn.open_streams(), 0, "the trailers ended our side");
    assert!(!s.conn.sent_headers(&mut s.mem, 1, true), "and only once");

    let all = frames(&out[..n]);
    let shape: Vec<_> = all.iter().map(|(k, fl, st, _)| (*k, *fl, *st)).collect();
    let want = [
        (Kind::Headers, EH, 1),
        (Kind::Data, 0, 1),
        (Kind::Headers, EH | ES, 1),
    ];
    assert_eq!(shape, want);
    let want = pairs(&[("grpc-status", "0"), ("x-checksum", "abc")]);
    assert_eq!(decoded(&all[2].3), want, "and no :status");

    let n = trailers_frame(&mut out, 9, &[]).unwrap();
    assert_eq!(out[..n], [0, 0, 0, 1, EH | ES, 0, 0, 0, 9]);
    let a = trailers_frame(&mut out, 1, &fields).unwrap();
    let first = out[..a].to_vec();
    let b = trailers_frame_from(&mut out, 1, fields.iter().copied()).unwrap();
    assert_eq!(out[..b], first);
    for room in 0..a {
        let mut small = vec![0u8; room];
        assert_eq!(trailers_frame(&mut small, 1, &fields), None);
    }
}

#[test]
fn a_response_of_trailers_only_is_one_frame() {
    let fields: [(&[u8], &[u8]); 3] = [
        (b"content-type", b"application/grpc"),
        (b"grpc-status", b"12"),
        (b"grpc-message", b"no such method"),
    ];
    let mut s = ready(Config::default());
    s.ok(&post(1));
    let mut out = [0u8; 256];
    let n = headers_frame(&mut out, 1, 200, &fields, true).unwrap();
    assert!(s.conn.sent_headers(&mut s.mem, 1, true));
    assert!(!s.conn.is_open(&s.mem, 1));
    assert_eq!(s.conn.open_streams(), 1, "the request has not ended");
    let all = frames(&out[..n]);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].0, all[0].1, all[0].2), (Kind::Headers, EH | ES, 1));
    let want = pairs(&[
        (":status", "200"),
        ("content-type", "application/grpc"),
        ("grpc-status", "12"),
        ("grpc-message", "no such method"),
    ]);
    assert_eq!(decoded(&all[0].3), want);
}

#[test]
fn trailers_past_one_frame_are_refused() {
    // 1 + 1 + 5 for the literal name, 3 for the value length
    let big = vec![b'v'; 16_384 - 10];
    let fields: [(&[u8], &[u8]); 1] = [(b"x-big", &big)];
    let mut out = vec![0u8; 40_000];
    let n = trailers_frame(&mut out, 1, &fields).unwrap();
    assert_eq!(n, FRAME_HEADER + 16_384, "exactly one full frame");
    let all = frames(&out[..n]);
    assert_eq!(all.len(), 1);
    assert_eq!(decoded(&all[0].3)[0].1, big);
    let big = vec![b'v'; 16_384 - 9];
    let fields: [(&[u8], &[u8]); 1] = [(b"x-big", &big)];
    assert_eq!(trailers_frame(&mut out, 1, &fields), None);
}

#[test]
fn random_trailers_never_panic() {
    let names = ["x-a", ":status", "X-B", "connection", "grpc-status", ""];
    for seed in 0..300u64 {
        let mut rng = Rng(seed ^ 0x7a11);
        let mut input = hello();
        for _ in 0..rng.below(20) {
            let stream = [1u32, 3, 5][rng.below(3)];
            let flags = [EH, EH | ES, ES, 0][rng.below(4)];
            let name = names[rng.below(names.len() as u64)];
            input.extend(match rng.below(4) {
                0 => f(Kind::Headers, EH, stream, &request_block("POST", "/")),
                1 => data(stream, flags & ES, b"abc"),
                2 => f(Kind::Continuation, flags, stream, &[]),
                _ => f(Kind::Headers, flags, stream, &block(&[(name, "v")])),
            });
        }
        run_any(&input, small(), &format!("seed {seed}"));
    }
}
