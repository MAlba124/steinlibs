#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! h2 client connection tests on synthetic frames.

use super::*;

/// Declare stream storage and a connection borrowing it. A macro since a fn cannot return both.
macro_rules! conn {
    ($c:ident) => {
        conn!($c, 128, Config::default());
    };
    ($c:ident, $n:expr) => {
        conn!($c, $n, Config::default());
    };
    ($c:ident, $n:expr, $cfg:expr) => {
        let mut storage = HeapStreamStorage::new($n);
        let mut $c = Connection::new($cfg, storage.view()).expect("storage lengths agree");
    };
}

fn f(kind: Kind, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    frame::write_header(&mut v, kind, flags, stream, payload.len() as u32);
    v.extend_from_slice(payload);
    v
}

fn head_block(status: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    hpack::encode_header(":status", status, &mut b);
    for (n, v) in extra {
        hpack::encode_header(n, v, &mut b);
    }
    b
}

fn drive(c: &mut Connection<'_>, bytes: &[u8]) -> (Vec<Event>, Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut ev = Vec::new();
    let n = c.step(bytes, &mut out, &mut ev).expect("valid frames");
    (ev, out, n)
}

/// Body bytes of `target` (all streams if `None`) from spans of `wire`.
fn body_in(ev: &[Event], wire: &[u8], target: Option<StreamId>) -> Vec<u8> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Body { stream, at, len } if target.is_none_or(|t| t == *stream) => {
                Some(&wire[*at..*at + *len])
            }
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

#[test]
fn preface_is_written_once() {
    conn!(c);
    let mut out = Vec::new();
    c.write_preface(&mut out);
    assert!(
        out.starts_with(frame::PREFACE),
        "must begin with the h2 preface"
    );
    let after = out.len();
    c.write_preface(&mut out);
    assert_eq!(out.len(), after, "preface must not be written twice");
}

#[test]
fn request_opens_an_odd_numbered_stream() {
    conn!(c);
    let mut out = Vec::new();
    let s1 = c
        .send_request(&mut out, "GET", "/", "example.com", "https", &[])
        .expect("open");
    let s2 = c
        .send_request(&mut out, "GET", "/b", "example.com", "https", &[])
        .expect("open");
    assert_eq!(c.get(s1).expect("live").id, 1);
    assert_eq!(c.get(s2).expect("live").id, 3, "client streams are odd");
    assert_eq!(c.stream_count(), 2);
}

#[test]
fn stream_ceiling_is_enforced() {
    conn!(c, 2);
    let mut out = Vec::new();
    assert!(
        c.send_request(&mut out, "GET", "/", "h", "https", &[])
            .is_ok()
    );
    assert!(
        c.send_request(&mut out, "GET", "/", "h", "https", &[])
            .is_ok()
    );
    assert_eq!(
        c.send_request(&mut out, "GET", "/", "h", "https", &[]),
        Err(Error::TooManyStreams),
        "must refuse rather than grow"
    );
}

#[test]
fn headers_then_data_produces_head_body_complete() {
    conn!(c);
    let mut out = Vec::new();
    let s = c
        .send_request(&mut out, "GET", "/", "example.com", "https", &[])
        .expect("open");

    let mut wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
    wire.extend(f(Kind::Data, 0, 1, b"hello"));
    wire.extend(f(Kind::Data, flag::END_STREAM, 1, b" world"));

    let (ev, _out, n) = drive(&mut c, &wire);
    assert_eq!(n, wire.len(), "all frames consumed");
    assert!(
        matches!(ev[0], Event::Head { status: 200, .. }),
        "got {:?}",
        ev[0]
    );
    assert_eq!(body_in(&ev, &wire, None), b"hello world");
    assert!(ev.iter().any(|e| matches!(e, Event::Complete { .. })));
    assert!(c.get(s).is_none(), "closed stream is reaped");
}

#[test]
fn end_stream_on_headers_completes_immediately() {
    conn!(c);
    let mut out = Vec::new();
    let _s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let wire = f(
        Kind::Headers,
        flag::END_HEADERS | flag::END_STREAM,
        1,
        &head_block("204", &[]),
    );
    let (ev, _, _) = drive(&mut c, &wire);
    assert!(matches!(ev[0], Event::Head { status: 204, .. }));
    assert!(ev.iter().any(|e| matches!(e, Event::Complete { .. })));
}

#[test]
fn continuation_frames_are_reassembled() {
    conn!(c);
    let mut out = Vec::new();
    let _s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");

    let block = head_block("200", &[("x-a", "1"), ("x-b", "2")]);
    let (a, b) = block.split_at(block.len() / 2);
    let mut wire = f(Kind::Headers, 0, 1, a); // no END_HEADERS
    wire.extend(f(Kind::Continuation, flag::END_HEADERS, 1, b));

    let (ev, _, _) = drive(&mut c, &wire);
    match &ev[0] {
        Event::Head {
            status, headers, ..
        } => {
            assert_eq!(*status, 200);
            assert!(
                headers.iter().any(|h| h.name == b"x-b" && h.value == b"2"),
                "header split across CONTINUATION must survive"
            );
        }
        other => panic!("expected Head, got {other:?}"),
    }
}

#[test]
fn continuation_on_a_different_stream_is_a_protocol_error() {
    conn!(c);
    let mut out = Vec::new();
    let _ = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let block = head_block("200", &[]);
    let mut wire = f(Kind::Headers, 0, 1, &block[..1]);
    wire.extend(f(Kind::Continuation, flag::END_HEADERS, 3, &block[1..]));
    let mut o = Vec::new();
    let mut ev = Vec::new();
    assert_eq!(c.step(&wire, &mut o, &mut ev), Err(Error::Protocol));
}

#[test]
fn partial_frame_is_not_consumed() {
    conn!(c);
    let mut out = Vec::new();
    let _s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
    let (ev, _, n) = drive(&mut c, &wire[..wire.len() - 1]);
    assert_eq!(n, 0, "incomplete frame must consume nothing");
    assert!(ev.is_empty());
    let (ev, _, n) = drive(&mut c, &wire);
    assert_eq!(n, wire.len());
    assert!(!ev.is_empty());
}

#[test]
fn settings_are_acked() {
    conn!(c);
    let mut payload = Vec::new();
    payload.extend_from_slice(&frame::SETTINGS_MAX_FRAME_SIZE.to_be_bytes());
    payload.extend_from_slice(&32768u32.to_be_bytes());
    let wire = f(Kind::Settings, 0, 0, &payload);
    let (_, out, _) = drive(&mut c, &wire);
    let h = frame::parse_header(&out).expect("an ack was written");
    assert_eq!(h.kind, Kind::Settings);
    assert_eq!(h.flags & flag::ACK, flag::ACK);
    assert_eq!(h.len, 0);
}

#[test]
fn settings_ack_is_not_itself_acked() {
    conn!(c);
    let wire = f(Kind::Settings, flag::ACK, 0, &[]);
    let (_, out, _) = drive(&mut c, &wire);
    assert!(out.is_empty(), "acking an ack would loop forever");
}

/// RFC 9113 section 6.9.2.
#[test]
fn initial_window_change_adjusts_open_streams() {
    conn!(c);
    let mut out = Vec::new();
    let s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let before = c.get(s).expect("live").send_window;
    assert_eq!(before, frame::DEFAULT_WINDOW);

    let mut payload = Vec::new();
    payload.extend_from_slice(&frame::SETTINGS_INITIAL_WINDOW_SIZE.to_be_bytes());
    payload.extend_from_slice(&(frame::DEFAULT_WINDOW as u32 + 1000).to_be_bytes());
    let wire = f(Kind::Settings, 0, 0, &payload);
    drive(&mut c, &wire);

    assert_eq!(
        c.get(s).expect("live").send_window,
        before + 1000,
        "open stream window must move by the delta"
    );
}

#[test]
fn window_update_credits_the_right_scope() {
    conn!(c);
    let mut out = Vec::new();
    let s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let conn_before = c.conn_send_window();
    let stream_before = c.get(s).expect("live").send_window;

    let mut wire = f(Kind::WindowUpdate, 0, 0, &100u32.to_be_bytes());
    wire.extend(f(Kind::WindowUpdate, 0, 1, &50u32.to_be_bytes()));
    drive(&mut c, &wire);

    assert_eq!(c.conn_send_window(), conn_before + 100);
    assert_eq!(c.get(s).expect("live").send_window, stream_before + 50);
}

#[test]
fn zero_window_update_is_a_protocol_error() {
    conn!(c);
    let wire = f(Kind::WindowUpdate, 0, 0, &0u32.to_be_bytes());
    let mut o = Vec::new();
    let mut ev = Vec::new();
    assert_eq!(c.step(&wire, &mut o, &mut ev), Err(Error::Protocol));
}

#[test]
fn ping_is_answered_with_the_same_payload() {
    conn!(c);
    let wire = f(Kind::Ping, 0, 0, b"12345678");
    let (_, out, _) = drive(&mut c, &wire);
    let h = frame::parse_header(&out).expect("a pong");
    assert_eq!(h.kind, Kind::Ping);
    assert_eq!(h.flags & flag::ACK, flag::ACK);
    assert_eq!(&out[frame::HEADER_LEN..], b"12345678");
}

#[test]
fn ping_ack_is_not_answered() {
    conn!(c);
    let wire = f(Kind::Ping, flag::ACK, 0, b"12345678");
    let (_, out, _) = drive(&mut c, &wire);
    assert!(out.is_empty());
}

#[test]
fn rst_stream_reports_and_closes() {
    conn!(c);
    let mut out = Vec::new();
    let s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let wire = f(Kind::RstStream, 0, 1, &8u32.to_be_bytes()); // CANCEL
    let (ev, _, _) = drive(&mut c, &wire);
    assert!(
        matches!(ev[0], Event::Reset { code: 8, .. }),
        "got {:?}",
        ev[0]
    );
    assert!(c.get(s).is_none(), "reset stream is reaped");
}

#[test]
fn goaway_is_reported() {
    conn!(c);
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    let wire = f(Kind::GoAway, 0, 0, &payload);
    let (ev, _, _) = drive(&mut c, &wire);
    assert!(matches!(
        ev[0],
        Event::GoAway {
            last_stream: 1,
            code: 0
        }
    ));
    assert!(c.goaway_received());
}

#[test]
fn unknown_frame_types_are_ignored() {
    conn!(c);
    let wire = f(Kind::Unknown(99), 0, 0, b"whatever");
    let (ev, out, n) = drive(&mut c, &wire);
    assert_eq!(n, wire.len(), "must be consumed");
    assert!(ev.is_empty() && out.is_empty(), "and otherwise ignored");
}

#[test]
fn padded_data_strips_padding_but_counts_it_for_flow_control() {
    conn!(c);
    let mut out = Vec::new();
    let _s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    drive(
        &mut c,
        &f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[])),
    );

    // pad length 3, body "ab", 3 pad bytes
    let mut payload = vec![3u8];
    payload.extend_from_slice(b"ab");
    payload.extend_from_slice(&[0, 0, 0]);
    let wire = f(Kind::Data, flag::PADDED, 1, &payload);
    let (ev, out2, _) = drive(&mut c, &wire);
    assert_eq!(
        body_in(&ev, &wire, None),
        b"ab",
        "padding must be stripped from the body"
    );
    assert!(
        out2.is_empty(),
        "a few octets of a window of 65 535 earn no WINDOW_UPDATE yet"
    );

    // Grant covers whole padded payloads, including the first frame's six octets.
    let mut payload = vec![100u8];
    payload.extend(std::iter::repeat_n(b'x', 16_000));
    payload.extend_from_slice(&[0; 100]);
    let mut wire = Vec::new();
    for _ in 0..3 {
        wire.extend(f(Kind::Data, flag::PADDED, 1, &payload));
    }
    let (_, out3, _) = drive(&mut c, &wire);
    let h = frame::parse_header(&out3).expect("a window update");
    assert_eq!(h.kind, Kind::WindowUpdate);
    let credited = frame::parse_u32(&out3[frame::HEADER_LEN..]).expect("u32");
    assert_eq!(
        credited as usize,
        3 * payload.len() + 6,
        "flow control counts padding, over every frame"
    );
}

#[test]
fn bad_padding_is_rejected() {
    conn!(c);
    let mut out = Vec::new();
    let _ = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    // pad length 200 in a 3-byte payload
    let payload = vec![200u8, b'a', b'b'];
    let wire = f(Kind::Data, flag::PADDED, 1, &payload);
    let mut o = Vec::new();
    let mut ev = Vec::new();
    assert_eq!(
        c.step(&wire, &mut o, &mut ev),
        Err(Error::Frame(frame::Error::BadPadding))
    );
}

#[test]
fn oversized_frame_is_rejected() {
    conn!(
        c,
        128,
        Config {
            max_frame: 16,
            ..Default::default()
        }
    );
    let mut v = Vec::new();
    frame::write_header(&mut v, Kind::Data, 0, 1, 1000);
    let mut o = Vec::new();
    let mut ev = Vec::new();
    assert_eq!(
        c.step(&v, &mut o, &mut ev),
        Err(Error::Frame(frame::Error::FrameTooLarge))
    );
}

#[test]
fn frames_per_step_is_bounded() {
    conn!(
        c,
        128,
        Config {
            max_frames_per_step: 4,
            ..Default::default()
        }
    );
    let mut wire = Vec::new();
    for _ in 0..50 {
        wire.extend(f(Kind::Ping, flag::ACK, 0, b"12345678"));
    }
    let (_, _, n) = drive(&mut c, &wire);
    assert_eq!(
        n,
        4 * (frame::HEADER_LEN + 8),
        "stopped at the frame budget"
    );
}

#[test]
fn unknown_stream_id_is_a_miss_not_a_panic() {
    conn!(c);
    let wire = f(Kind::Data, flag::END_STREAM, 77, b"x");
    let (ev, _, n) = drive(&mut c, &wire);
    assert_eq!(n, wire.len());
    assert!(
        !ev.iter().any(|e| matches!(e, Event::Body { .. })),
        "must not deliver body for an unknown stream"
    );
}

#[test]
fn two_streams_interleave_without_crossing() {
    conn!(c);
    let mut out = Vec::new();
    let a = c
        .send_request(&mut out, "GET", "/a", "h", "https", &[])
        .expect("open");
    let b = c
        .send_request(&mut out, "GET", "/b", "h", "https", &[])
        .expect("open");

    let mut wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
    wire.extend(f(
        Kind::Headers,
        flag::END_HEADERS,
        3,
        &head_block("404", &[]),
    ));
    wire.extend(f(Kind::Data, flag::END_STREAM, 3, b"bbb"));
    wire.extend(f(Kind::Data, flag::END_STREAM, 1, b"aaa"));

    let (ev, _, _) = drive(&mut c, &wire);

    assert_eq!(body_in(&ev, &wire, Some(a)), b"aaa", "stream 1 body");
    assert_eq!(body_in(&ev, &wire, Some(b)), b"bbb", "stream 3 body");

    let statuses: Vec<u16> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Head { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, vec![200, 404]);
}

#[test]
fn transitions_are_logged_in_commit_order() {
    conn!(c);
    let mut out = Vec::new();
    let _s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    let mut wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
    wire.extend(f(Kind::Data, flag::END_STREAM, 1, b"x"));
    drive(&mut c, &wire);

    let states: Vec<(StreamState, StreamState)> =
        c.transitions.iter().map(|t| (t.from, t.to)).collect();
    assert_eq!(
        states,
        vec![
            (StreamState::Open, StreamState::ReadingBody),
            (StreamState::ReadingBody, StreamState::Closed),
        ],
        "every state change is recorded, in order"
    );
}

#[test]
fn arbitrary_byte_splits_produce_the_same_events() {
    fn run(splits: &[usize]) -> Vec<String> {
        conn!(c);
        let mut out = Vec::new();
        let _s = c
            .send_request(&mut out, "GET", "/", "h", "https", &[])
            .expect("open");
        let mut wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
        wire.extend(f(Kind::Data, 0, 1, b"hello"));
        wire.extend(f(Kind::Data, flag::END_STREAM, 1, b" world"));

        let mut pending: Vec<u8> = Vec::new();
        let mut all = Vec::new();
        let mut pos = 0;
        let mut i = 0;
        while pos < wire.len() {
            let want = if splits.is_empty() {
                wire.len()
            } else {
                splits[i % splits.len()].max(1)
            };
            let end = (pos + want).min(wire.len());
            pending.extend_from_slice(&wire[pos..end]);
            pos = end;
            i += 1;
            let mut o = Vec::new();
            let mut ev = Vec::new();
            let n = c.step(&pending, &mut o, &mut ev).expect("valid");
            // compare body bytes, spans differ per piece
            all.extend(ev.into_iter().map(|e| match e {
                Event::Body { stream, at, len } => {
                    format!("Body {stream:?} {:?}", &pending[at..at + len])
                }
                e => format!("{e:?}"),
            }));
            pending.drain(..n);
        }
        all
    }

    let reference = run(&[]);
    assert!(!reference.is_empty());
    for splits in [
        vec![1],
        vec![2],
        vec![3],
        vec![7],
        vec![1, 5, 2],
        vec![9, 1],
    ] {
        assert_eq!(
            run(&splits),
            reference,
            "splits {splits:?} changed the events"
        );
    }
}

#[test]
fn the_preface_grants_the_connection_window_asked_for() {
    let cfg = Config {
        conn_window: 1 << 20,
        ..Config::default()
    };
    conn!(c, 8, cfg);
    let mut out = Vec::new();
    c.write_preface(&mut out);
    let settings = frame::parse_header(&out[frame::PREFACE.len()..]).expect("settings");
    assert_eq!(settings.kind, Kind::Settings);
    let at = frame::PREFACE.len() + frame::HEADER_LEN + settings.len as usize;
    let grant = frame::parse_header(&out[at..]).expect("a window update behind the settings");
    assert_eq!((grant.kind, grant.stream), (Kind::WindowUpdate, 0));
    let credited = frame::parse_u32(&out[at + frame::HEADER_LEN..]).expect("u32");
    assert_eq!(credited as i64, (1 << 20) - frame::DEFAULT_WINDOW);

    conn!(d);
    let mut out = Vec::new();
    d.write_preface(&mut out);
    assert!(
        frame::parse_header(&out[at..]).is_none(),
        "no grant at the default window"
    );
}

#[test]
fn a_stream_reset_by_us_is_gone_and_its_frames_are_dropped() {
    conn!(c);
    let mut out = Vec::new();
    let s = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    out.clear();
    c.reset_stream(&mut out, s, 8);
    let h = frame::parse_header(&out).expect("rst_stream");
    assert_eq!((h.kind, h.stream), (Kind::RstStream, 1));
    assert_eq!(frame::parse_u32(&out[frame::HEADER_LEN..]), Ok(8), "CANCEL");
    assert_eq!(c.stream_count(), 0, "reaped at once");
    assert!(c.get(s).is_none());
    // late frames dropped
    let mut wire = f(Kind::Headers, flag::END_HEADERS, 1, &head_block("200", &[]));
    wire.extend(f(Kind::Data, flag::END_STREAM, 1, b"late"));
    let (ev, _, n) = drive(&mut c, &wire);
    assert_eq!(n, wire.len());
    assert!(ev.is_empty(), "nothing for a stream that is gone, {ev:?}");
    // id not reused
    let next = c
        .send_request(&mut out, "GET", "/", "h", "https", &[])
        .expect("open");
    assert_eq!(c.get(next).expect("live").id, 3);
}

fn frames(out: &[u8]) -> Vec<(frame::Header, Vec<u8>)> {
    let mut at = 0;
    let mut all = Vec::new();
    while let Some(h) = frame::parse_header(&out[at..]) {
        let end = at + frame::HEADER_LEN + h.len as usize;
        all.push((h, out[at + frame::HEADER_LEN..end].to_vec()));
        at = end;
    }
    assert_eq!(at, out.len(), "whole frames");
    all
}

fn fields(block: &[u8]) -> Vec<(String, String)> {
    let mut table = DynamicTable::new(4096);
    let mut out = Vec::new();
    hpack::decode_block(block, &mut table, 64 * 1024, &mut out).expect("a block");
    out.iter()
        .map(|h| {
            (
                String::from_utf8_lossy(&h.name).into_owned(),
                String::from_utf8_lossy(&h.value).into_owned(),
            )
        })
        .collect()
}

fn ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn a_post_sends_its_body_in_data_with_end_stream_on_the_last() {
    conn!(c);
    let mut out = Vec::new();
    let body = ramp(40_000);
    let ua = [
        Header::new("User-Agent", "pf"),
        Header::new("cookie", "a=1"),
    ];
    let s = c
        .send_request_body(&mut out, "POST", "/up", "h", "https", &ua, &body)
        .expect("open");
    let all = frames(&out);
    let (head, block) = &all[0];
    assert_eq!(
        (head.kind, head.flags & flag::END_STREAM),
        (Kind::Headers, 0),
        "no END_STREAM on HEADERS"
    );
    let got = fields(block);
    let count = |n: &str| got.iter().filter(|(k, _)| k == n).count();
    assert_eq!(count("user-agent"), 1, "lowercased, once, {got:?}");
    assert_eq!(count("cookie"), 1);
    assert!(got.contains(&(":method".into(), "POST".into())));
    assert!(
        got.contains(&("content-length".into(), "40000".into())),
        "{got:?}"
    );
    let data: Vec<_> = all[1..].iter().collect();
    assert!(
        data.iter()
            .all(|(h, p)| h.kind == Kind::Data && p.len() <= frame::DEFAULT_MAX_FRAME as usize)
    );
    let ends: Vec<bool> = data
        .iter()
        .map(|(h, _)| h.flags & flag::END_STREAM != 0)
        .collect();
    assert_eq!(ends.iter().filter(|e| **e).count(), 1);
    assert_eq!(ends.last(), Some(&true), "END_STREAM on the last DATA");
    let sent: Vec<u8> = data.iter().flat_map(|(_, p)| p.iter().copied()).collect();
    assert_eq!(sent, body);
    assert_eq!(c.body_owed(s), 0);
}

#[test]
fn an_empty_body_ends_the_stream_on_headers() {
    conn!(c);
    let mut out = Vec::new();
    c.send_request_body(&mut out, "POST", "/", "h", "https", &[], &[])
        .expect("open");
    let all = frames(&out);
    assert_eq!(all.len(), 1);
    assert_ne!(all[0].0.flags & flag::END_STREAM, 0);
    assert!(fields(&all[0].1).contains(&("content-length".into(), "0".into())));
}

#[test]
fn a_body_past_the_window_waits_for_window_update() {
    conn!(c);
    let mut out = Vec::new();
    let body = ramp(100_000);
    let s = c
        .send_request_body(&mut out, "POST", "/", "h", "https", &[], &body)
        .expect("open");
    let sent = |out: &[u8]| -> (usize, bool) {
        let all = frames(out);
        let data = all.iter().filter(|(h, _)| h.kind == Kind::Data);
        let n = data.clone().map(|(_, p)| p.len()).sum();
        (
            n,
            data.clone().any(|(h, _)| h.flags & flag::END_STREAM != 0),
        )
    };
    assert_eq!(
        sent(&out),
        (65_535, false),
        "the initial window and no more"
    );
    assert_eq!(c.body_owed(s), 100_000 - 65_535);
    out.clear();
    assert_eq!(c.send_body(&mut out, s, &body), Ok(0), "shut until a grant");
    // connection window is spent too
    let (_, _, _) = drive(
        &mut c,
        &f(Kind::WindowUpdate, 0, 1, &50_000u32.to_be_bytes()),
    );
    assert_eq!(c.send_body(&mut out, s, &body), Ok(0));
    drive(
        &mut c,
        &f(Kind::WindowUpdate, 0, 0, &10_000u32.to_be_bytes()),
    );
    assert_eq!(
        c.send_body(&mut out, s, &body),
        Ok(10_000),
        "the smaller window"
    );
    drive(
        &mut c,
        &f(Kind::WindowUpdate, 0, 0, &1_000_000u32.to_be_bytes()),
    );
    assert_eq!(c.send_body(&mut out, s, &body), Ok(100_000 - 75_535));
    let (n, ended) = sent(&out);
    assert_eq!((n, ended), (100_000 - 65_535, true));
    let all: Vec<u8> = frames(&out)
        .iter()
        .filter(|(h, _)| h.kind == Kind::Data)
        .flat_map(|(_, p)| p.clone())
        .collect();
    assert_eq!(all, body[65_535..], "the rest, in order");
    assert_eq!(
        c.send_body(&mut out, s, &body[..10]),
        Err(Error::BodyMismatch)
    );
}

#[test]
fn caller_headers_cannot_spoof_what_the_engine_writes() {
    conn!(c);
    let mut out = Vec::new();
    for (name, value) in [
        ("Host", "evil"),
        (":authority", "evil"),
        ("content-length", "1"),
        ("Connection", "close"),
        ("te", "trailers"),
        ("x", "a\r\nb"),
    ] {
        let bad = [Header::new(name, value)];
        let refused = c.send_request_body(&mut out, "POST", "/", "h", "https", &bad, b"abc");
        assert!(
            matches!(refused, Err(Error::BadHeader(_))),
            "{name}: {refused:?}"
        );
        assert!(
            matches!(
                c.send_request(&mut out, "GET", "/", "h", "https", &bad),
                Err(Error::BadHeader(_))
            ),
            "{name}"
        );
    }
    assert!(
        out.is_empty() && c.stream_count() == 0,
        "nothing written, no stream opened"
    );
}

#[test]
fn a_response_that_ends_before_the_body_cancels_the_rest() {
    conn!(c);
    let mut out = Vec::new();
    let body = ramp(100_000);
    let s = c
        .send_request_body(&mut out, "POST", "/", "h", "https", &[], &body)
        .expect("open");
    let wire = f(
        Kind::Headers,
        flag::END_HEADERS | flag::END_STREAM,
        1,
        &head_block("413", &[]),
    );
    let (ev, back, _) = drive(&mut c, &wire);
    assert!(matches!(ev[0], Event::Head { status: 413, .. }));
    let rst: Vec<_> = frames(&back)
        .into_iter()
        .filter(|(h, _)| h.kind == Kind::RstStream)
        .collect();
    assert_eq!(rst.len(), 1, "our half closed");
    assert_eq!(frame::parse_u32(&rst[0].1), Ok(8), "CANCEL");
    assert!(c.get(s).is_none());
    assert_eq!(c.send_body(&mut out, s, &body), Err(Error::UnknownStream));
}
