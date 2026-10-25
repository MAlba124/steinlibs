//! h2 server machine vs an untrusted client. Fed whole and in seeded chunks, both runs must match.
//!
//! ```text
//! [cfg0][cfg1][split seed][action count][actions..][client bytes..]
//! ```
//!
//! Client bytes follow the preface raw or as frame specs. Each event triggers one server action.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::h2::frame::{self, Kind, flag};
use stein_http::h2::server::{Config, Conn, Ctl, Error, Event, FRAME_HEADER, mem_bytes};
use stein_http::header;

const WINDOW_MAX: i64 = i32::MAX as i64;
const MAX_FRAME_CEILING: u32 = 16_777_215;
/// Highest error code in RFC 9113 7.
const CODE_MAX: u32 = 0xd;
/// Cap on the synthesized client stream. Zero filled payloads are slow to run.
const STREAM_CAP: usize = 256 * 1024;

struct Setup {
    cfg: Config,
    connect: bool,
    drain_each: bool,
    ctl_cap: usize,
    seed: u8,
    actions: Vec<u8>,
    stream: Vec<u8>,
}

/// Event with borrowed data copied out.
#[derive(Debug, PartialEq, Eq)]
enum Ev {
    NeedMore,
    Nothing,
    Blocked,
    Request {
        stream: u32,
        end_stream: bool,
        fields: Vec<u8>,
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

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Ev(Ev),
    Act(u8, u32, u64),
}

#[derive(Debug, PartialEq, Eq)]
enum End {
    /// Input ran dry with this many bytes left.
    Dry(usize),
    Failed(Error),
}

struct Run {
    trace: Vec<Step>,
    ctl_out: Vec<u8>,
    end: End,
}

/// Run state besides the machine and its memory.
struct St {
    ctl: Vec<u8>,
    ctl_len: usize,
    ctl_out: Vec<u8>,
    trace: Vec<Step>,
    next_action: usize,
    /// Outstanding grants, so ungrant only returns what was taken.
    grants: Vec<(u32, usize)>,
    tags: u64,
}

fuzz_target!(|data: &[u8]| {
    let Some(s) = setup(data) else {
        return;
    };
    let whole = run(&s, false);
    let split = run(&s, true);
    assert_eq!(whole.end, split.end, "chunking changed the result");
    assert_eq!(whole.trace, split.trace, "chunking changed the events");
    assert_eq!(
        whole.ctl_out, split.ctl_out,
        "chunking changed the control bytes"
    );
});

fn setup(data: &[u8]) -> Option<Setup> {
    let (&c0, rest) = data.split_first()?;
    let (&c1, rest) = rest.split_first()?;
    let (&seed, rest) = rest.split_first()?;
    let (&n, rest) = rest.split_first()?;
    let n = (n as usize).min(rest.len());
    let (actions, body) = rest.split_at(n);
    let cfg = Config {
        max_streams: [1, 2, 3, 8][usize::from(c0 >> 3 & 3)],
        initial_window: [0, 100, 65_535, i32::MAX as u32][usize::from(c1 & 3)],
        max_frame: 16_384,
        header_table_size: [0, 64, 256, 4096][usize::from(c0 >> 5 & 3)],
        max_header_block: if c0 & 0x80 != 0 { 64 } else { 8192 },
        max_header_list: if c1 & 4 != 0 { 64 } else { 8192 },
    };
    let mut stream = frame::PREFACE.to_vec();
    if c1 & 0x10 != 0 {
        // empty client SETTINGS to start in the open phase
        stream.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    }
    if c0 & 1 != 0 {
        stream.extend_from_slice(body);
    } else {
        frames(body, &mut stream);
    }
    Some(Setup {
        cfg,
        connect: c0 & 2 != 0,
        drain_each: c0 & 4 != 0,
        ctl_cap: if c1 & 8 != 0 { 64 } else { 256 },
        seed,
        actions: actions.to_vec(),
        stream,
    })
}

/// Frame specs `[kind][flags][stream][len]`, `len` 0xff means a u16 length follows. Payload zero filled to length.
fn frames(mut b: &[u8], out: &mut Vec<u8>) {
    while let [kind, flags, stream, len, rest @ ..] = b {
        let (len, rest) = match (*len, rest) {
            (0xff, [hi, lo, tail @ ..]) => (usize::from(u16::from_be_bytes([*hi, *lo])), tail),
            (l, tail) => (usize::from(l), tail),
        };
        if out.len() + FRAME_HEADER + len > STREAM_CAP {
            return;
        }
        let take = len.min(rest.len());
        let l = len as u32;
        out.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8, *kind, *flags]);
        out.extend_from_slice(&u32::from(*stream).to_be_bytes());
        out.extend_from_slice(rest.get(..take).unwrap_or(&[]));
        out.resize(out.len() + len - take, 0);
        b = rest.get(take..).unwrap_or(&[]);
    }
}

/// Chunk sizes for the split run. Seed zero feeds bytewise.
struct Chunks(u64, u8);

impl Chunks {
    fn next(&mut self) -> usize {
        if self.1 == 0 {
            return 1;
        }
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        1 + (self.0 % (u64::from(self.1) * 4)) as usize
    }
}

fn run(s: &Setup, split: bool) -> Run {
    let mut conn = Conn::new(s.cfg);
    if s.connect {
        conn.enable_connect_protocol();
    }
    let mut mem = vec![0xa5u8; mem_bytes(&s.cfg)];
    assert!(conn.init(&mut mem).is_ok(), "init refused exact storage");
    let mut out = [0u8; 64];
    assert!(
        conn.write_settings(&mut out).is_some(),
        "settings do not fit 64 bytes"
    );

    let mut st = St {
        ctl: vec![0u8; s.ctl_cap],
        ctl_len: 0,
        ctl_out: Vec::new(),
        trace: Vec::new(),
        next_action: 0,
        grants: Vec::new(),
        tags: 0,
    };
    let len = s.stream.len();
    let mut chunks = Chunks(u64::from(s.seed) | 0x100, s.seed);
    let mut pos = 0;
    let mut limit = if split { 0 } else { len };
    let mut blocked = false;
    let mut end = None;
    // every step consumes, feeds more input, or blocks once
    for _ in 0..3 * len + 16 {
        let src = s.stream.get(pos..limit).unwrap_or(&[]);
        let before = live(&conn, &mem, &s.cfg);
        let r = step(&mut conn, &mut mem, src, &mut st);
        let (n, ev) = match r {
            Err(e) => {
                assert!(e.code() <= CODE_MAX, "bad connection error code");
                end = Some(End::Failed(e));
                break;
            }
            Ok(r) => r,
        };
        assert!(n <= src.len(), "consumed past the input");
        match ev {
            Ev::NeedMore => {
                assert_eq!(n, 0, "need more consumed input");
                if limit == len {
                    end = Some(End::Dry(len - pos));
                    break;
                }
                limit = (limit + chunks.next()).min(len);
                continue;
            }
            Ev::Blocked => {
                assert!(n == 0 && !blocked, "blocked twice or consumed");
                blocked = true;
                drain(&mut st);
                st.trace.push(Step::Ev(ev));
                continue;
            }
            _ => {}
        }
        assert!(n > 0, "an event without progress");
        pos += n;
        blocked = false;
        check_event(&conn, &mem, &s.cfg, &ev, &before);
        st.trace.push(Step::Ev(ev));
        act(&mut conn, &mut mem, s, &mut st, &before);
        if s.drain_each {
            drain(&mut st);
        }
        check_state(&conn, &mem, &s.cfg);
    }
    drain(&mut st);
    check_ctl(&st.ctl_out);
    assert!(end.is_some(), "the step loop never ended");
    let end = end.unwrap_or(End::Dry(0));
    Run {
        trace: st.trace,
        ctl_out: st.ctl_out,
        end,
    }
}

fn step(conn: &mut Conn, mem: &mut [u8], src: &[u8], st: &mut St) -> Result<(usize, Ev), Error> {
    let mut ctl = Ctl {
        buf: &mut st.ctl,
        len: st.ctl_len,
    };
    let r = conn.step(mem, src, &mut ctl);
    st.ctl_len = ctl.len;
    let s = r?;
    Ok((s.consumed, own(&s.event, src, s.consumed)))
}

fn own(e: &Event<'_>, src: &[u8], consumed: usize) -> Ev {
    match *e {
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
            fields: fields.to_vec(),
        },
        Event::Data {
            stream,
            end_stream,
            at,
            len,
        } => {
            // trailers report an empty range at zero
            assert!(
                len == 0 || at >= FRAME_HEADER,
                "body inside the frame header"
            );
            assert!(at + len <= consumed, "body past its frame");
            let body = src.get(at..at + len).unwrap_or(&[]).to_vec();
            Ev::Data {
                stream,
                end_stream,
                body,
            }
        }
        Event::Reset { stream, code, tag } => Ev::Reset { stream, code, tag },
        Event::Window { stream } => Ev::Window { stream },
        Event::GoAway { last_stream, code } => Ev::GoAway { last_stream, code },
    }
}

fn drain(st: &mut St) {
    st.ctl_out
        .extend_from_slice(st.ctl.get(..st.ctl_len).unwrap_or(&[]));
    st.ctl_len = 0;
}

/// Live stream ids in slot order.
fn live(conn: &Conn, mem: &[u8], cfg: &Config) -> Vec<u32> {
    (0..cfg.max_streams)
        .filter_map(|i| conn.stream_at(mem, i))
        .map(|(id, _)| id)
        .collect()
}

fn check_event(conn: &Conn, mem: &[u8], cfg: &Config, ev: &Ev, before: &[u32]) {
    match ev {
        Ev::Request { stream, fields, .. } => {
            assert!(stream % 2 == 1, "even request stream");
            assert_eq!(
                *stream,
                conn.last_stream(),
                "request is not the newest stream"
            );
            assert!(!before.contains(stream), "request on a live stream");
            assert!(
                fields.len() <= cfg.max_header_list,
                "fields over the list limit"
            );
            request_sound(fields);
        }
        Ev::Data {
            stream,
            end_stream,
            body,
        } => {
            assert!(
                before.contains(stream),
                "data on a stream that was not live"
            );
            let t = conn.trailers(mem, *stream);
            if t.is_some() {
                assert!(*end_stream && body.is_empty(), "trailers without the end");
            }
            t.into_iter()
                .flat_map(header::iter)
                .for_each(|(n, _)| field_name(n, false));
        }
        Ev::Reset { stream, .. } => {
            assert!(
                before.contains(stream),
                "reset of a stream that was not live"
            );
            assert!(
                !live(conn, mem, cfg).contains(stream),
                "reset stream still live"
            );
        }
        Ev::Window { stream } => {
            assert!(
                *stream == 0 || before.contains(stream),
                "window for a dead stream"
            );
        }
        Ev::GoAway { .. } => assert!(conn.goaway_received()),
        Ev::NeedMore | Ev::Nothing | Ev::Blocked => {}
    }
}

/// RFC 9113 8.3 request head rules the machine must have checked.
fn request_sound(fields: &[u8]) {
    let mut regular = false;
    let (mut method, mut path) = (0, 0);
    for (name, _) in header::iter(fields) {
        let pseudo = name.first() == Some(&b':');
        assert!(!(pseudo && regular), "pseudo header after a regular one");
        regular |= !pseudo;
        field_name(name, pseudo);
        method += usize::from(name == b":method");
        path += usize::from(name == b":path");
    }
    assert!(
        method == 1 && path == 1,
        "request without one method and one path"
    );
}

fn field_name(name: &[u8], pseudo_ok: bool) {
    assert!(!name.is_empty(), "empty field name");
    assert!(
        pseudo_ok || name[0] != b':',
        "pseudo header where none may be"
    );
    assert!(
        !name.iter().any(u8::is_ascii_uppercase),
        "uppercase field name"
    );
    assert!(name != b"connection", "connection header passed");
}

fn check_state(conn: &Conn, mem: &[u8], cfg: &Config) {
    let ids = live(conn, mem, cfg);
    assert_eq!(
        ids.len() as u32,
        conn.open_streams(),
        "open count disagrees with slots"
    );
    assert!(
        conn.open_streams() <= cfg.max_streams,
        "more streams than slots"
    );
    assert!(
        conn.conn_send_window() <= WINDOW_MAX,
        "connection window over 2^31-1"
    );
    let pm = conn.peer_max_frame();
    assert!(
        (16_384..=MAX_FRAME_CEILING).contains(&pm),
        "peer max frame out of range"
    );
    for (i, id) in ids.iter().enumerate() {
        assert!(
            id % 2 == 1 && *id <= conn.last_stream(),
            "stream id never opened"
        );
        assert!(
            !ids.get(..i).unwrap_or(&[]).contains(id),
            "stream in two slots"
        );
        let w = conn.stream_send_window(mem, *id);
        assert!(w <= WINDOW_MAX, "stream window over 2^31-1");
    }
}

/// Control frames are whole, of unsolicited kinds, with valid lengths and codes.
fn check_ctl(mut b: &[u8]) {
    while !b.is_empty() {
        let h = frame::parse_header(b);
        assert!(h.is_some(), "partial control frame header");
        let Some(h) = h else { return };
        let end = FRAME_HEADER + h.len as usize;
        let p = b.get(FRAME_HEADER..end);
        assert!(p.is_some(), "partial control frame");
        let Some(p) = p else { return };
        let word = frame::parse_u32(p).unwrap_or(0);
        let known = matches!(
            h.kind,
            Kind::Settings | Kind::Ping | Kind::WindowUpdate | Kind::RstStream | Kind::GoAway
        );
        assert!(known, "unexpected control frame {:?}", h.kind);
        match h.kind {
            Kind::Settings => assert!(h.flags == flag::ACK && p.is_empty() && h.stream == 0),
            Kind::Ping => assert!(h.flags == flag::ACK && p.len() == 8 && h.stream == 0),
            Kind::WindowUpdate => {
                assert!(p.len() == 4, "window update length");
                assert!(
                    word > 0 && i64::from(word) <= WINDOW_MAX,
                    "bad window increment"
                );
            }
            Kind::RstStream => {
                assert!(p.len() == 4 && h.stream != 0, "bad rst stream");
                assert!(word <= CODE_MAX, "rst with an unknown code");
            }
            Kind::GoAway => {
                assert!(p.len() == 8 && h.stream == 0, "bad goaway");
                let code = frame::parse_u32(p.get(4..).unwrap_or(&[])).unwrap_or(u32::MAX);
                assert!(code <= CODE_MAX, "goaway with an unknown code");
            }
            _ => {}
        }
        b = b.get(end..).unwrap_or(&[]);
    }
}

/// One server action per event, picked by the next action byte.
fn act(conn: &mut Conn, mem: &mut [u8], s: &Setup, st: &mut St, before: &[u32]) {
    let n = s.actions.len();
    if n == 0 {
        return;
    }
    let a = s.actions[st.next_action % n];
    let b = s.actions[(st.next_action + 1) % n];
    st.next_action += 1;
    let ids = live(conn, mem, &s.cfg);
    // mostly live, sometimes dead or never opened
    let stream = match ids.get(usize::from(b) % (ids.len() + 1)) {
        Some(id) => *id,
        None => before.first().copied().unwrap_or(u32::from(b)),
    };
    let op = a & 7;
    let result = match op {
        1 | 2 => headers(conn, mem, stream, op == 2),
        3 => grant(conn, mem, st, stream, usize::from(b) << (a >> 4 & 7)),
        4 => ungrant(conn, mem, st, usize::from(b)),
        5 => {
            conn.sent_end(mem, stream);
            u64::from(conn.is_open(mem, stream))
        }
        6 => reset(conn, mem, st, stream, u32::from(b) % (CODE_MAX + 1)),
        7 if a & 8 != 0 => goaway(conn, st, u32::from(b) % (CODE_MAX + 1)),
        7 => tag(conn, mem, st, stream),
        _ => 0,
    };
    st.trace.push(Step::Act(op, stream, result));
    check_state(conn, mem, &s.cfg);
}

fn headers(conn: &mut Conn, mem: &mut [u8], stream: u32, end: bool) -> u64 {
    let open = conn.is_open(mem, stream);
    let ok = conn.sent_headers(mem, stream, end);
    assert_eq!(ok, open, "sent_headers disagrees with is_open");
    u64::from(ok)
}

fn grant(conn: &mut Conn, mem: &mut [u8], st: &mut St, stream: u32, want: usize) -> u64 {
    let open = conn.is_open(mem, stream);
    let sw = conn.stream_send_window(mem, stream);
    let cw = conn.conn_send_window();
    let n = conn.grant(mem, stream, want);
    assert!(
        n <= want && n <= conn.peer_max_frame() as usize,
        "grant over the ask or frame"
    );
    if !open {
        assert_eq!(n, 0, "grant on a stream we may not send on");
        return 0;
    }
    assert!(n as i64 <= sw.min(cw).max(0), "grant over a window");
    assert_eq!(conn.stream_send_window(mem, stream), sw - n as i64);
    assert_eq!(conn.conn_send_window(), cw - n as i64);
    if n > 0 {
        st.grants.push((stream, n));
    }
    n as u64
}

fn ungrant(conn: &mut Conn, mem: &mut [u8], st: &mut St, pick: usize) -> u64 {
    if st.grants.is_empty() {
        return 0;
    }
    let (stream, n) = st.grants.swap_remove(pick % st.grants.len());
    let cw = conn.conn_send_window();
    conn.ungrant(mem, stream, n);
    // Capped, the peer may have raised the window meanwhile.
    let want = (cw + n as i64).min(i64::from(i32::MAX));
    assert_eq!(conn.conn_send_window(), want, "ungrant lost bytes");
    n as u64
}

fn reset(conn: &mut Conn, mem: &mut [u8], st: &mut St, stream: u32, code: u32) -> u64 {
    let mut ctl = Ctl {
        buf: &mut st.ctl,
        len: st.ctl_len,
    };
    let ok = conn.reset(mem, &mut ctl, stream, code);
    st.ctl_len = ctl.len;
    if ok {
        assert!(
            conn.tag(mem, stream).is_none(),
            "reset stream still has a slot"
        );
    }
    u64::from(ok)
}

fn goaway(conn: &mut Conn, st: &mut St, code: u32) -> u64 {
    let mut ctl = Ctl {
        buf: &mut st.ctl,
        len: st.ctl_len,
    };
    let ok = conn.goaway(&mut ctl, code);
    st.ctl_len = ctl.len;
    u64::from(ok)
}

fn tag(conn: &mut Conn, mem: &mut [u8], st: &mut St, stream: u32) -> u64 {
    st.tags += 1;
    let ok = conn.set_tag(mem, stream, st.tags);
    assert_eq!(conn.tag(mem, stream), ok.then_some(st.tags), "tag not kept");
    u64::from(ok)
}
