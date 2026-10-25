//! Outbound and inbound headers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::h1::{Error as H1Error, MAX_HEADERS};
use stein_http::{Client, Config, Error, Event, Header, HeaderError, HeapStorage, Millis, Request};

/// Declare storage and a client borrowing it in the caller's scope.
macro_rules! client {
    ($c:ident) => {
        client!($c, Config::default(), 8, 8, 4096, 16, 2048);
    };
    ($c:ident, $cfg:expr, $conns:expr, $reqs:expr, $bsize:expr, $bcount:expr, $hbytes:expr) => {
        let mut storage = HeapStorage::with_header_bytes($conns, $reqs, $bsize, $bcount, $hbytes);
        // some tests only read the client
        #[allow(unused_mut)]
        let mut $c = Client::new($cfg, storage.view()).expect("storage lengths agree");
    };
}

fn get() -> Request {
    Request {
        method: "GET".into(),
        path: "/feed.xml".into(),
        host: "example.com".into(),
    }
}

/// Drain pending transmit bytes as text.
fn sent(c: &mut Client<'_>) -> String {
    let mut out = [0u8; 4096];
    let mut bytes = Vec::new();
    while let Some(n) = c.poll_transmit(&mut out) {
        bytes.extend_from_slice(&out[..n]);
    }
    String::from_utf8(bytes).expect("a request head is ascii")
}

fn drain(c: &mut Client<'_>) -> Vec<Event> {
    core::iter::from_fn(|| c.poll_event()).collect()
}

fn head_count(events: &[Event]) -> Option<usize> {
    events.iter().find_map(|e| match e {
        Event::Head { header_count, .. } => Some(*header_count),
        _ => None,
    })
}

fn failure(events: &[Event]) -> Option<H1Error> {
    events.iter().find_map(|e| match e {
        Event::Failed { error, .. } => Some(*error),
        _ => None,
    })
}

const CONDITIONAL: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Location: https://example.com/new\r\n\
    ETag: \"v7\"\r\n\
    Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\
    Content-Encoding: gzip\r\n\
    Content-Length: 2\r\n\
    \r\nhi";

// outbound

#[test]
fn a_submitted_header_reaches_the_wire() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit_with(
        conn,
        Millis(0),
        get(),
        &[
            Header::new("If-None-Match", "\"abc\""),
            Header::new("Accept-Encoding", "gzip"),
        ],
    )
    .expect("submit");
    let text = sent(&mut c);
    assert!(text.contains("\r\nIf-None-Match: \"abc\"\r\n"), "{text}");
    assert!(text.contains("\r\nAccept-Encoding: gzip\r\n"), "{text}");
    // Host leads and Connection trails.
    assert!(
        text.starts_with("GET /feed.xml HTTP/1.1\r\nHost: example.com\r\n"),
        "{text}"
    );
    assert!(text.ends_with("\r\nConnection: close\r\n\r\n"), "{text}");
}

#[test]
fn a_request_without_headers_is_byte_for_byte_what_it_was() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get()).expect("submit");
    assert_eq!(
        sent(&mut c),
        "GET /feed.xml HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn a_header_value_carrying_crlf_is_refused() {
    client!(c);
    let conn = c.open().expect("open");
    let smuggled = Header::new("If-None-Match", "x\r\nGET /admin HTTP/1.1\r\nHost: e");
    assert_eq!(
        c.submit_with(conn, Millis(0), get(), &[smuggled]),
        Err(Error::BadRequest(HeaderError::Injection))
    );
    assert_eq!(sent(&mut c), "", "a refused request queues nothing");
}

#[test]
fn a_header_name_carrying_a_newline_is_refused() {
    client!(c);
    let conn = c.open().expect("open");
    assert_eq!(
        c.submit_with(conn, Millis(0), get(), &[Header::new("X-Bad\nHost", "1")]),
        Err(Error::BadRequest(HeaderError::Injection))
    );
}

#[test]
fn a_caller_cannot_override_the_headers_the_crate_manages() {
    client!(c);
    let conn = c.open().expect("open");
    for name in ["Host", "content-length", "Transfer-Encoding", "CONNECTION"] {
        assert_eq!(
            c.submit_with(conn, Millis(0), get(), &[Header::new(name, "x")]),
            Err(Error::BadRequest(HeaderError::Reserved)),
            "{name} is the crate's to write"
        );
    }
    assert_eq!(sent(&mut c), "", "nothing was queued");
}

#[test]
fn a_path_that_would_split_the_request_line_is_refused() {
    client!(c);
    let conn = c.open().expect("open");
    let req = Request {
        method: "GET".into(),
        path: "/feed.xml HTTP/1.1\r\nHost: evil".into(),
        host: "example.com".into(),
    };
    assert_eq!(
        c.submit(conn, Millis(0), req),
        Err(Error::BadRequest(HeaderError::BadTarget))
    );
}

#[test]
fn a_refused_request_leaves_the_connection_usable() {
    client!(c);
    let conn = c.open().expect("open");
    let bad = Header::new("Host", "elsewhere");
    assert!(c.submit_with(conn, Millis(0), get(), &[bad]).is_err());
    // No row opened, so the connection is not busy.
    assert!(c.submit(conn, Millis(0), get()).is_ok());
}

// inbound

#[test]
fn the_headers_a_client_acts_on_are_readable() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(conn, Millis(1), CONDITIONAL).expect("input");
    assert_eq!(
        c.header_str(req, "Location"),
        Some("https://example.com/new")
    );
    assert_eq!(c.header_str(req, "ETag"), Some("\"v7\""));
    assert_eq!(
        c.header_str(req, "Last-Modified"),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(c.header_str(req, "Content-Encoding"), Some("gzip"));
    assert_eq!(c.header_str(req, "X-Not-Sent"), None);
}

#[test]
fn a_lookup_ignores_case_on_both_sides() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    // Sent mixed case, asked for lower.
    c.handle_input(conn, Millis(1), CONDITIONAL).expect("input");
    assert_eq!(c.header_str(req, "etag"), Some("\"v7\""));
    assert_eq!(c.header_str(req, "content-encoding"), Some("gzip"));

    // Sent lower, asked for mixed.
    client!(d);
    let conn = d.open().expect("open");
    let req = d.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut d);
    d.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 200 OK\r\netag: \"lower\"\r\nContent-Length: 0\r\n\r\n",
    )
    .expect("input");
    assert_eq!(d.header_str(req, "ETag"), Some("\"lower\""));
}

#[test]
fn a_header_sent_twice_keeps_both_and_returns_the_first() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 200 OK\r\nVia: one\r\nVia: two\r\nContent-Length: 0\r\n\r\n",
    )
    .expect("input");
    assert_eq!(c.header_str(req, "Via"), Some("one"), "first wins");
    let both: Vec<&[u8]> = c
        .headers(req)
        .filter(|(n, _)| n.eq_ignore_ascii_case(b"via"))
        .map(|(_, v)| v)
        .collect();
    assert_eq!(both, vec![&b"one"[..], &b"two"[..]], "both are kept");
}

#[test]
fn a_set_cookie_is_dropped_rather_than_stored() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nETag: \"v\"\r\nContent-Length: 0\r\n\r\n",
    )
    .expect("input");
    assert_eq!(
        c.header_str(req, "Set-Cookie"),
        None,
        "a feed client acts on no cookie"
    );
    assert_eq!(c.header_str(req, "ETag"), Some("\"v\""));
    assert_eq!(c.header_count(req), 2, "ETag and Content-Length, no cookie");
}

#[test]
fn a_response_with_no_headers_at_all_is_not_an_error() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(conn, Millis(1), b"HTTP/1.1 200 OK\r\n\r\n")
        .expect("input");
    let ev = drain(&mut c);
    assert_eq!(head_count(&ev), Some(0), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, Event::Complete { .. })));
    assert_eq!(c.header_count(req), 0);
    assert_eq!(c.headers(req).count(), 0);
    assert_eq!(c.header_str(req, "ETag"), None);
}

#[test]
fn headers_are_unreadable_until_the_head_is_complete() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(conn, Millis(1), b"HTTP/1.1 200 OK\r\nETag: ")
        .expect("input");
    assert_eq!(c.header_str(req, "ETag"), None);
    assert_eq!(c.header_count(req), 0);
}

#[test]
fn a_header_split_across_two_inputs_arrives_intact() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    let whole = b"HTTP/1.1 200 OK\r\nETag: \"split-value\"\r\nContent-Length: 0\r\n\r\n";
    // 24 splits the ETag value.
    let cut = 24;
    c.handle_input(conn, Millis(1), &whole[..cut]).expect("a");
    assert_eq!(c.header_str(req, "ETag"), None, "head is not complete yet");
    c.handle_input(conn, Millis(2), &whole[cut..]).expect("b");
    assert_eq!(c.header_str(req, "ETag"), Some("\"split-value\""));
}

#[test]
fn every_split_of_a_head_yields_the_same_headers() {
    for chunk in 1..=13 {
        client!(c);
        let conn = c.open().expect("open");
        let req = c.submit(conn, Millis(0), get()).expect("submit");
        sent(&mut c);
        let mut at = 0;
        while at < CONDITIONAL.len() {
            let end = (at + chunk).min(CONDITIONAL.len());
            c.handle_input(conn, Millis(1), &CONDITIONAL[at..end])
                .expect("input");
            at = end;
        }
        assert_eq!(
            c.header_str(req, "ETag"),
            Some("\"v7\""),
            "chunk size {chunk} lost the ETag"
        );
        assert_eq!(c.header_count(req), 5, "chunk size {chunk}");
    }
}

// bounds

#[test]
fn a_response_with_more_headers_than_the_limit_fails_cleanly() {
    client!(c, Config::default(), 4, 4, 16 * 1024, 8, 2048);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    let mut r = b"HTTP/1.1 200 OK\r\n".to_vec();
    for i in 0..=MAX_HEADERS {
        r.extend_from_slice(format!("X-Pad-{i}: v\r\n").as_bytes());
    }
    r.extend_from_slice(b"\r\n");
    c.handle_input(conn, Millis(1), &r).expect("input");
    assert_eq!(failure(&drain(&mut c)), Some(H1Error::TooManyHeaders));
}

#[test]
fn a_single_header_longer_than_the_head_limit_fails_cleanly() {
    let cfg = Config {
        max_head: 256,
        ..Config::default()
    };
    client!(c, cfg, 4, 4, 4096, 8, 2048);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    let mut r = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
    r.extend_from_slice(&[b'a'; 512]);
    r.extend_from_slice(b"\r\n\r\n");
    c.handle_input(conn, Millis(1), &r).expect("input");
    assert_eq!(failure(&drain(&mut c)), Some(H1Error::HeadersTooLarge));
}

#[test]
fn a_head_that_overruns_the_capture_budget_fails_rather_than_truncates() {
    // 64 byte header store per request, head needs more.
    client!(c, Config::default(), 4, 4, 4096, 8, 64);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    let mut r = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
    r.extend_from_slice(&[b'a'; 200]);
    r.extend_from_slice(b"\r\n\r\n");
    c.handle_input(conn, Millis(1), &r).expect("input");
    assert_eq!(failure(&drain(&mut c)), Some(H1Error::HeadersTooLarge));
    assert_eq!(c.header_str(req, "X-Big"), None, "nothing half-stored");
}

#[test]
fn the_header_ceiling_is_statable() {
    client!(c, Config::default(), 8, 8, 4096, 16, 2048);
    assert_eq!(c.header_ceiling(), 8 * 2048);
}

// conditional GET

#[test]
fn a_304_has_headers_and_no_body() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c
        .submit_with(
            conn,
            Millis(0),
            get(),
            &[
                Header::new("If-None-Match", "\"v7\""),
                Header::new("If-Modified-Since", "Wed, 21 Oct 2015 07:28:00 GMT"),
            ],
        )
        .expect("submit");
    let wire = sent(&mut c);
    assert!(wire.contains("\r\nIf-None-Match: \"v7\"\r\n"), "{wire}");
    assert!(
        wire.contains("\r\nIf-Modified-Since: Wed, 21 Oct 2015 07:28:00 GMT\r\n"),
        "{wire}"
    );

    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 304 Not Modified\r\nETag: \"v7\"\r\nCache-Control: max-age=60\r\n\r\n",
    )
    .expect("input");
    let ev = drain(&mut c);
    assert!(
        matches!(ev.first(), Some(Event::Head { status: 304, .. })),
        "{ev:?}"
    );
    assert!(
        !ev.iter().any(|e| matches!(e, Event::Body { .. })),
        "a 304 has no body, {ev:?}"
    );
    assert!(
        ev.iter().any(|e| matches!(e, Event::Complete { .. })),
        "{ev:?}"
    );
    assert_eq!(c.header_str(req, "etag"), Some("\"v7\""));
    assert_eq!(c.header_str(req, "Cache-Control"), Some("max-age=60"));
}

#[test]
fn a_304_declaring_a_length_still_completes() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 304 Not Modified\r\nETag: \"v7\"\r\nContent-Length: 4096\r\n\r\n",
    )
    .expect("input");
    let ev = drain(&mut c);
    assert!(
        ev.iter().any(|e| matches!(e, Event::Complete { .. })),
        "{ev:?}"
    );
    assert!(
        !ev.iter().any(|e| matches!(e, Event::Body { .. })),
        "{ev:?}"
    );
}

#[test]
fn a_redirect_hands_back_its_location() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 301 Moved Permanently\r\nLocation: /feed/atom.xml\r\nContent-Length: 0\r\n\r\n",
    )
    .expect("input");
    let ev = drain(&mut c);
    assert!(
        matches!(ev.first(), Some(Event::Head { status: 301, .. })),
        "{ev:?}"
    );
    assert_eq!(c.header_str(req, "location"), Some("/feed/atom.xml"));
}

#[test]
fn headers_do_not_survive_the_request_they_came_with() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    sent(&mut c);
    c.handle_input(conn, Millis(1), CONDITIONAL).expect("input");
    assert!(c.header_str(req, "ETag").is_some());
    // Row recycled on timeout, stale handle must not read it.
    c.handle_timeout(Millis(30_000));
    assert_eq!(
        c.header_str(req, "ETag"),
        None,
        "stale handle reads nothing"
    );
}
