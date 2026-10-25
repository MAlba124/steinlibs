//! Keep-alive, several requests on one connection.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::{Client, Config, Error, Event, HeapStorage, Millis, Request};

/// Client with one connection and one request row.
macro_rules! client {
    ($c:ident) => {
        let mut storage = HeapStorage::with_header_bytes(1, 1, 4096, 2, 2048);
        let mut $c = Client::new(Config::default(), storage.view()).expect("storage lengths agree");
    };
}

fn get() -> Request {
    Request {
        method: "GET".into(),
        path: "/segment.ts".into(),
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

fn completed(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Complete { .. }))
}

const KEPT: &[u8] = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 2\r\n\r\nhi";

#[test]
fn a_request_asks_for_the_close_unless_told_not_to() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit(conn, Millis(0), get()).expect("submit");
    let closing = sent(&mut c);
    assert!(
        closing.ends_with("\r\nConnection: close\r\n\r\n"),
        "{closing:?}"
    );
    c.close(conn);

    let conn = c.open().expect("open");
    c.submit_keep_alive(conn, Millis(0), get(), &[])
        .expect("submit");
    let keeping = sent(&mut c);
    assert!(
        !keeping.to_ascii_lowercase().contains("connection"),
        "{keeping:?}"
    );
    assert!(
        keeping.ends_with("Host: example.com\r\n\r\n"),
        "{keeping:?}"
    );
}

#[test]
fn a_second_request_rides_the_same_connection_once_the_first_is_complete() {
    client!(c);
    let conn = c.open().expect("open");
    let first = c
        .submit_keep_alive(conn, Millis(0), get(), &[])
        .expect("submit");
    sent(&mut c);

    // Half a response, the rest is still owed.
    c.handle_input(conn, Millis(1), &KEPT[..20]).expect("input");
    assert_eq!(
        c.submit_keep_alive(conn, Millis(1), get(), &[]).err(),
        Some(Error::ConnectionBusy)
    );
    c.handle_input(conn, Millis(2), &KEPT[20..]).expect("input");
    assert!(completed(&drain(&mut c)));
    assert!(c.keep_alive(first));
    // Headers stay readable after completion.
    assert_eq!(c.header_str(first, "etag"), Some("\"v1\""));

    // The single row is reused by the second request.
    let second = c
        .submit_keep_alive(conn, Millis(3), get(), &[])
        .expect("second submit");
    assert_eq!(c.header(first, "etag"), None, "the first row is gone");
    assert!(!sent(&mut c).is_empty());
    c.handle_input(conn, Millis(4), KEPT).expect("input");
    let events = drain(&mut c);
    assert!(matches!(events.first(), Some(Event::Head { req, status: 200, .. }) if *req == second));
    assert!(completed(&events));
    assert_eq!(c.header_str(second, "etag"), Some("\"v1\""));
}

#[test]
fn the_response_says_whether_the_connection_may_be_kept() {
    let cases: &[(&[u8], bool, &str)] = &[
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            true,
            "1.1 with a length",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            true,
            "1.1 chunked",
        ),
        (
            b"HTTP/1.1 304 Not Modified\r\n\r\n",
            true,
            "a 304 has no body to frame",
        ),
        (
            b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            false,
            "1.1 that says close",
        ),
        (
            b"HTTP/1.1 200 OK\r\nConnection: Keep-Alive, Close\r\nContent-Length: 0\r\n\r\n",
            false,
            "close among other options",
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n",
            false,
            "a body that runs to the close",
        ),
        (
            b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n",
            false,
            "1.0 closes by default",
        ),
        (
            b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n",
            true,
            "1.0 that says keep-alive",
        ),
        (
            b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\n",
            false,
            "1.0 keep-alive with a body to the close",
        ),
        (
            b"HTTP/1.1 103 Early Hints\r\n\r\n",
            false,
            "an interim response",
        ),
    ];
    for (response, kept, what) in cases {
        client!(c);
        let conn = c.open().expect("open");
        let req = c
            .submit_keep_alive(conn, Millis(0), get(), &[])
            .expect("submit");
        assert!(
            !c.keep_alive(req),
            "{what}: nothing is known before the head"
        );
        c.handle_input(conn, Millis(1), response).expect("input");
        assert_eq!(c.keep_alive(req), *kept, "{what}");
    }
}

#[test]
fn a_request_that_asked_for_the_close_is_never_kept() {
    client!(c);
    let conn = c.open().expect("open");
    let req = c.submit(conn, Millis(0), get()).expect("submit");
    // Server ignores the close it was sent.
    c.handle_input(conn, Millis(1), KEPT).expect("input");
    assert!(completed(&drain(&mut c)));
    assert!(!c.keep_alive(req));
}

#[test]
fn bytes_nobody_asked_for_refuse_the_next_request() {
    client!(c);
    let conn = c.open().expect("open");
    let first = c
        .submit_keep_alive(conn, Millis(0), get(), &[])
        .expect("submit");
    sent(&mut c);
    // Two bytes past Content-Length.
    c.handle_input(conn, Millis(1), KEPT).expect("input");
    c.handle_input(conn, Millis(1), b"!!").expect("input");
    assert!(completed(&drain(&mut c)));
    assert!(c.keep_alive(first));
    assert_eq!(
        c.submit_keep_alive(conn, Millis(2), get(), &[]).err(),
        Some(Error::UnsolicitedInput),
        "they would be read as the head of the next response"
    );
    c.close(conn);
    let conn = c.open().expect("open");
    c.submit_keep_alive(conn, Millis(3), get(), &[])
        .expect("submit on a new connection");
}

#[test]
fn a_complete_request_never_times_out() {
    client!(c);
    let conn = c.open().expect("open");
    c.submit_keep_alive(conn, Millis(0), get(), &[])
        .expect("submit");
    assert_eq!(c.poll_timeout(), Some(Millis(30_000)));
    c.handle_input(conn, Millis(1), KEPT).expect("input");
    assert!(completed(&drain(&mut c)));

    // Kept idle connection has no deadline.
    assert_eq!(c.poll_timeout(), None);
    c.handle_timeout(Millis(60_000));
    assert!(drain(&mut c).is_empty());

    let second = c
        .submit_keep_alive(conn, Millis(60_000), get(), &[])
        .expect("second submit");
    assert_eq!(c.poll_timeout(), Some(Millis(90_000)));
    c.handle_timeout(Millis(90_000));
    assert_eq!(drain(&mut c), vec![Event::TimedOut { req: second }]);
}
