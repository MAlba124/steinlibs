//! Pipelining and direct body reads.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::{Client, Config, Error, Event, Header, HeaderError, HeapStorage, Millis, Request};

macro_rules! client {
    ($c:ident) => {
        let mut storage = HeapStorage::with_header_bytes(1, 4, 4096, 2, 2048);
        let mut $c = Client::new(Config::default(), storage.view()).expect("storage lengths agree");
    };
}

fn get(path: &str) -> Request {
    Request {
        method: "GET".into(),
        path: path.into(),
        host: "example.com".into(),
    }
}

fn post(path: &str) -> Request {
    Request {
        method: "POST".into(),
        path: path.into(),
        host: "example.com".into(),
    }
}

fn sent(c: &mut Client<'_>) -> String {
    let mut out = [0u8; 4096];
    let mut bytes = Vec::new();
    while let Some(n) = c.poll_transmit(&mut out) {
        bytes.extend_from_slice(&out[..n]);
    }
    String::from_utf8(bytes).expect("a request head is ascii")
}

/// Every event, with body bytes captured before they are consumed.
fn drain(c: &mut Client<'_>) -> Vec<(Event, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(e) = c.poll_event() {
        let body = match &e {
            Event::Body { req, .. } => c.body(*req).to_vec(),
            _ => Vec::new(),
        };
        out.push((e, body));
    }
    out
}

const KEPT: &[u8] = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 2\r\n\r\nhi";

#[test]
fn two_requests_pipelined_are_answered_in_order() {
    client!(c);
    let conn = c.open().expect("open");
    let first = c
        .submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("first");
    let second = c
        .submit_pipelined(conn, Millis(0), get("/b"), &[])
        .expect("second behind it");
    let wire = sent(&mut c);
    assert!(
        wire.contains("GET /a ") && wire.contains("GET /b "),
        "{wire:?}"
    );
    assert!(wire.find("GET /a ") < wire.find("GET /b "), "in order");

    let mut both = KEPT.to_vec();
    both.extend_from_slice(b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nnot");
    c.handle_input(conn, Millis(1), &both).expect("input");
    let events = drain(&mut c);
    let shape: Vec<String> = events
        .iter()
        .map(|(e, body)| match e {
            Event::Head { req, status, .. } => format!("head {} {status}", (*req == first) as u8),
            Event::Body { req, len } => format!(
                "body {} {len} {}",
                (*req == first) as u8,
                String::from_utf8_lossy(body)
            ),
            Event::Complete { req } => format!("done {}", (*req == first) as u8),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "head 1 200",
            "body 1 2 hi",
            "done 1",
            "head 0 404",
            "body 0 3 not",
            "done 0"
        ]
    );
    assert_eq!(
        c.header(first, "etag"),
        None,
        "the first row went when the second completed"
    );
    assert_eq!(
        c.header_str(second, "content-length"),
        Some("3"),
        "the last row stays for its headers"
    );
}

#[test]
fn a_request_pipelines_only_behind_one_that_asked_to_be_kept() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get("/a"))
        .expect("first, which asks for the close");
    assert_eq!(
        c.submit_pipelined(conn, Millis(0), get("/b"), &[]).err(),
        Some(Error::ConnectionBusy)
    );
    c.close(conn);

    let conn = c.open().expect("open");
    c.submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("first, kept");
    c.submit_pipelined(conn, Millis(0), get("/b"), &[])
        .expect("second behind it");
    // Non-pipelined submit is refused behind a pipeline.
    assert_eq!(
        c.submit_keep_alive(conn, Millis(0), get("/c"), &[]).err(),
        Some(Error::ConnectionBusy)
    );
}

#[test]
fn a_timed_out_request_takes_the_pipeline_behind_it() {
    client!(c);
    let conn = c.open().expect("open");
    let first = c
        .submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("first");
    let second = c
        .submit_pipelined(conn, Millis(10), get("/b"), &[])
        .expect("second");
    sent(&mut c);
    c.handle_timeout(Millis(30_000));
    let timed_out: Vec<_> = drain(&mut c).into_iter().map(|(e, _)| e).collect();
    assert_eq!(
        timed_out,
        vec![
            Event::TimedOut { req: first },
            Event::TimedOut { req: second }
        ]
    );
    // Nothing in flight, so a new request may go out.
    c.submit_keep_alive(conn, Millis(30_000), get("/a"), &[])
        .expect("a new request");
}

#[test]
fn a_body_framed_by_its_length_may_be_taken_off_the_wire_by_the_caller() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c
        .submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("submit");
    sent(&mut c);
    assert_eq!(c.direct_body(conn), None, "nothing to take before the head");
    c.handle_input(
        conn,
        Millis(1),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
    )
    .expect("head");
    assert!(matches!(
        drain(&mut c).first(),
        Some((Event::Head { status: 200, .. }, _))
    ));
    assert_eq!(c.direct_body(conn), Some((req, 5)));
    c.took_body(conn, 3).expect("three taken");
    assert_eq!(c.direct_body(conn), Some((req, 2)));
    assert_eq!(
        c.took_body(conn, 3).err(),
        Some(Error::ReceiveOverflow),
        "more than is owed"
    );
    c.took_body(conn, 2).expect("the rest");
    assert_eq!(
        drain(&mut c)
            .into_iter()
            .map(|(e, _)| e)
            .collect::<Vec<_>>(),
        vec![Event::Complete { req }]
    );
    assert!(c.keep_alive(req));
    // Buffered bytes come first, no direct offer behind them.
    let req = c
        .submit_keep_alive(conn, Millis(2), get("/b"), &[])
        .expect("second");
    c.handle_input(
        conn,
        Millis(3),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab",
    )
    .expect("head and two");
    let events = drain(&mut c);
    assert!(
        events
            .iter()
            .any(|(e, body)| matches!(e, Event::Body { .. }) && body == b"ab")
    );
    assert_eq!(
        c.direct_body(conn),
        Some((req, 3)),
        "what is left after the block ran dry"
    );
}

#[test]
fn body_bytes_stay_until_polled_past() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c
        .submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("submit");
    sent(&mut c);
    c.handle_input(conn, Millis(1), KEPT).expect("input");
    assert!(matches!(c.poll_event(), Some(Event::Head { .. })));
    assert_eq!(c.poll_event(), Some(Event::Body { req, len: 2 }));
    assert_eq!(c.body(req), b"hi");
    c.handle_input(conn, Millis(1), b"").expect("nothing");
    assert_eq!(c.body(req), b"hi");
    assert_eq!(c.poll_event(), Some(Event::Complete { req }));
    assert_eq!(c.body(req), b"", "consumed");
}

/// Body runs are tracked per connection. Input on another connection does not disturb them.
#[test]
fn a_body_run_is_held_per_connection() {
    let mut storage = HeapStorage::with_header_bytes(2, 4, 4096, 4, 2048);
    let mut c = Client::new(Config::default(), storage.view()).expect("storage lengths agree");
    let (a, b) = (c.open().expect("open a"), c.open().expect("open b"));
    let ra = c.submit(a, Millis(0), get("/a")).expect("a");
    let rb = c.submit(b, Millis(0), get("/b")).expect("b");
    sent(&mut c);
    c.handle_input(
        a,
        Millis(1),
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\naaaa",
    )
    .expect("input a");
    assert!(matches!(c.poll_event(), Some(Event::Head { req, .. }) if req == ra));
    assert_eq!(c.poll_event(), Some(Event::Body { req: ra, len: 4 }));
    assert_eq!(c.body(ra), b"aaaa");
    c.handle_input(
        b,
        Millis(2),
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbbbb",
    )
    .expect("input b");
    assert_eq!(c.body(ra), b"aaaa", "the run of a is still there");
    let events = drain(&mut c);
    let bodies: Vec<_> = events
        .iter()
        .filter(|(e, _)| matches!(e, Event::Body { .. }))
        .map(|(_, b)| b.clone())
        .collect();
    assert_eq!(bodies, [b"bbbb".to_vec()]);
    let done = |r| events.iter().any(|(e, _)| *e == Event::Complete { req: r });
    assert!(done(ra) && done(rb), "both responses complete, {events:?}");
}

#[test]
fn a_post_is_framed_by_its_length_with_the_body_behind_the_head() {
    client!(c);
    let conn = c.open().expect("open");
    let headers = [
        Header::new("User-Agent", "pf"),
        Header::new("Content-Type", "application/x-protobuf"),
    ];
    let req = c
        .submit_body(conn, Millis(0), post("/up"), &headers, b"hello", true)
        .expect("submit");
    let wire = sent(&mut c);
    let (head, body) = wire.split_once("\r\n\r\n").expect("a head");
    assert!(
        head.starts_with("POST /up HTTP/1.1\r\nHost: example.com\r\n"),
        "{head:?}"
    );
    assert_eq!(head.matches("Content-Length: 5").count(), 1, "{head:?}");
    assert_eq!(
        head.matches("User-Agent: pf").count(),
        1,
        "the caller's header once"
    );
    assert!(
        !head.contains("Transfer-Encoding") && !head.contains("Connection"),
        "kept and never chunked"
    );
    assert_eq!(
        body, "hello",
        "the body whole behind the head, nothing after it"
    );
    c.handle_input(conn, Millis(1), KEPT).expect("input");
    assert!(
        drain(&mut c)
            .iter()
            .any(|(e, b)| matches!(e, Event::Body { .. }) && b == b"hi")
    );
    assert!(c.keep_alive(req));
    c.submit_body(conn, Millis(2), post("/up"), &[], b"", false)
        .expect("an empty body");
    let wire = sent(&mut c);
    assert!(
        wire.ends_with("Content-Length: 0\r\nConnection: close\r\n\r\n"),
        "{wire:?}"
    );
}

#[test]
fn a_caller_cannot_frame_a_body_itself() {
    client!(c);
    let conn = c.open().expect("open");
    for name in ["Content-Length", "transfer-encoding", "Host", "Connection"] {
        let headers = [Header::new(name, "9")];
        let refused = c.submit_body(conn, Millis(0), post("/"), &headers, b"abc", true);
        assert_eq!(
            refused.err(),
            Some(Error::BadRequest(HeaderError::Reserved)),
            "{name}"
        );
    }
    assert_eq!(sent(&mut c), "", "nothing queued");
    c.submit_body(conn, Millis(0), post("/"), &[], b"abc", true)
        .expect("the connection is unharmed");
}

#[test]
fn a_body_that_does_not_fit_the_transmit_block_is_refused_whole() {
    client!(c);
    let conn = c.open().expect("open");
    let big = vec![b'x'; 4096];
    assert_eq!(
        c.submit_body(conn, Millis(0), post("/"), &[], &big, true)
            .err(),
        Some(Error::BuffersExhausted)
    );
    assert_eq!(sent(&mut c), "", "neither the head nor a part of the body");
    c.submit_body(conn, Millis(0), post("/"), &[], &big[..1000], true)
        .expect("one that fits");
    assert!(sent(&mut c).ends_with(&"x".repeat(1000)));
}

#[test]
fn a_body_is_never_pipelined_and_a_get_still_is() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit_keep_alive(conn, Millis(0), get("/a"), &[])
        .expect("first");
    assert_eq!(
        c.submit_body(conn, Millis(0), post("/b"), &[], b"x", true)
            .err(),
        Some(Error::ConnectionBusy)
    );
    c.submit_pipelined(conn, Millis(0), get("/c"), &[])
        .expect("a GET behind it");
    let wire = sent(&mut c);
    assert!(
        wire.contains("GET /a ") && wire.contains("GET /c ") && !wire.contains("POST"),
        "{wire:?}"
    );
}
