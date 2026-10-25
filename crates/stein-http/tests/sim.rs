//! Deterministic simulation. Any split of a response must yield the same events.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::{Client, Config, Event, HeapStorage, Millis, Request};

fn cfg() -> Config {
    Config::default()
}

/// Declare storage and a client borrowing it in the caller's scope.
macro_rules! client {
    ($c:ident) => {
        client!($c, 8, 8, 1024, 32);
    };
    ($c:ident, $conns:expr, $reqs:expr, $bsize:expr, $bcount:expr) => {
        let mut storage = HeapStorage::new($conns, $reqs, $bsize, $bcount);
        // some tests only read the client
        #[allow(unused_mut)]
        let mut $c = Client::new(cfg(), storage.view()).expect("storage lengths agree");
    };
}

fn get() -> Request {
    Request {
        method: "GET".into(),
        path: "/".into(),
        host: "example.com".into(),
    }
}

/// Xorshift PRNG.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Simplified event shape for comparison across runs.
#[derive(Debug, PartialEq, Eq)]
enum E {
    Head(u16, Option<u64>),
    Body(usize),
    Complete,
    Failed,
    TimedOut,
}

fn simplify(events: Vec<Event>) -> Vec<E> {
    events
        .into_iter()
        .map(|e| match e {
            Event::Head {
                status,
                content_length,
                ..
            } => E::Head(status, content_length),
            Event::Body { len, .. } => E::Body(len),
            Event::Complete { .. } => E::Complete,
            Event::Failed { .. } => E::Failed,
            Event::TimedOut { .. } => E::TimedOut,
        })
        .collect()
}

/// Merge adjacent Body events. Only the concatenated body is split-invariant.
fn coalesce(ev: Vec<E>) -> Vec<E> {
    let mut out: Vec<E> = Vec::new();
    for e in ev {
        match (out.last_mut(), &e) {
            (Some(E::Body(acc)), E::Body(n)) => *acc += *n,
            _ => out.push(e),
        }
    }
    out
}

/// Run one request, feeding `response` in the given chunk sizes.
fn run(response: &[u8], splits: &[usize]) -> Vec<E> {
    client!(c);
    let conn = c.open().expect("open");
    let _req = c.submit(conn, Millis(0), get()).expect("submit");

    let mut out = [0u8; 2048];
    let mut sent = 0;
    while let Some(n) = c.poll_transmit(&mut out) {
        sent += n;
    }
    assert!(sent > 0, "client must have produced a request");

    let mut pos = 0;
    let mut i = 0;
    while pos < response.len() {
        let want = if splits.is_empty() {
            response.len()
        } else {
            splits[i % splits.len()].max(1)
        };
        let end = (pos + want).min(response.len());
        c.handle_input(conn, Millis(1), &response[pos..end])
            .expect("input");
        pos = end;
        i += 1;
    }

    let mut events = Vec::new();
    while let Some(e) = c.poll_event() {
        events.push(e);
    }
    simplify(events)
}

/// Body bytes, which `simplify` discards.
fn body_of(response: &[u8], splits: &[usize]) -> Vec<u8> {
    // Larger block so reassembly, not the receive ceiling, is tested.
    client!(c, 8, 8, 16384, 32);
    let conn = c.open().expect("open");
    let _req = c.submit(conn, Millis(0), get()).expect("submit");
    let mut out = [0u8; 2048];
    while c.poll_transmit(&mut out).is_some() {}

    let mut pos = 0;
    let mut i = 0;
    while pos < response.len() {
        let want = if splits.is_empty() {
            response.len()
        } else {
            splits[i % splits.len()].max(1)
        };
        let end = (pos + want).min(response.len());
        c.handle_input(conn, Millis(1), &response[pos..end])
            .expect("input");
        pos = end;
        i += 1;
    }

    let mut body = Vec::new();
    while let Some(e) = c.poll_event() {
        if let Event::Body { req, .. } = e {
            body.extend_from_slice(c.body(req));
        }
    }
    body
}

#[test]
fn the_body_arrives_byte_for_byte() {
    assert_eq!(body_of(SIMPLE, &[]), b"hello");
}

#[test]
fn the_body_is_the_same_whatever_the_read_boundaries() {
    let whole = body_of(SIMPLE, &[]);
    for splits in [&[1usize][..], &[2][..], &[3][..], &[7][..], &[1, 5, 2][..]] {
        assert_eq!(
            body_of(SIMPLE, splits),
            whole,
            "split {splits:?} changed it"
        );
    }
}

#[test]
fn a_larger_body_arrives_whole_and_in_order() {
    let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    )
    .into_bytes();
    resp.extend_from_slice(&payload);
    assert_eq!(body_of(&resp, &[]), payload);
    assert_eq!(body_of(&resp, &[64]), payload);
}

#[test]
fn a_chunked_body_arrives_dechunked() {
    let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    assert_eq!(body_of(resp, &[]), b"hello world");
    assert_eq!(body_of(resp, &[3]), b"hello world");
}

/// Close must free request rows too, or close and reopen runs out of slots.
#[test]
fn closing_returns_the_request_slot_too() {
    client!(c, 2, 1, 1024, 8);
    for round in 0..4 {
        let conn = c.open().expect("open");
        c.submit(conn, Millis(0), get())
            .unwrap_or_else(|e| panic!("round {round} had no request slot: {e:?}"));
        c.close(conn);
    }
}

const SIMPLE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";

#[test]
fn simple_get_in_one_read() {
    let ev = run(SIMPLE, &[]);
    assert_eq!(
        ev,
        vec![E::Head(200, Some(5)), E::Body(5), E::Complete],
        "one-shot delivery"
    );
}

#[test]
fn byte_at_a_time_produces_the_same_events() {
    let whole = coalesce(run(SIMPLE, &[]));
    let dribbled = coalesce(run(SIMPLE, &[1]));
    assert_eq!(
        whole, dribbled,
        "split must not change the coalesced events"
    );
}

#[test]
fn any_split_produces_the_same_events() {
    let reference = coalesce(run(SIMPLE, &[]));
    let mut rng = Rng(0x2545F4914F6CDD1D);
    for trial in 0..200 {
        let splits: Vec<usize> = (0..4).map(|_| 1 + rng.below(8)).collect();
        let got = coalesce(run(SIMPLE, &splits));
        assert_eq!(got, reference, "trial {trial} splits {splits:?} diverged");
    }
}

#[test]
fn zero_length_body_completes() {
    let ev = run(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n", &[]);
    assert_eq!(ev, vec![E::Head(204, Some(0)), E::Complete]);
}

#[test]
fn chunked_body() {
    let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let ev = coalesce(run(r, &[]));
    assert!(
        matches!(ev.first(), Some(E::Head(200, None))),
        "chunked head, got {ev:?}"
    );
    assert!(
        ev.contains(&E::Complete),
        "chunked must complete, got {ev:?}"
    );
}

#[test]
fn malformed_status_line_fails() {
    let ev = run(b"NOT-HTTP garbage\r\n\r\n", &[]);
    assert!(ev.contains(&E::Failed), "expected failure, got {ev:?}");
}

#[test]
fn buffer_ceiling_is_statable() {
    client!(c);
    assert_eq!(c.buffer_ceiling(), 1024 * 32);
}

#[test]
fn connection_ceiling_is_enforced() {
    client!(c, 2, 8, 1024, 8);
    assert!(c.open().is_ok());
    assert!(c.open().is_ok());
    assert!(c.open().is_err(), "must refuse rather than grow");
}

#[test]
fn closing_returns_blocks_to_the_pool() {
    client!(c);
    let a = c.open().expect("open");
    c.close(a);
    for _ in 0..100 {
        let h = c.open().expect("blocks must be reused");
        c.close(h);
    }
}

#[test]
fn timeout_fires_and_frees_the_connection() {
    client!(c);
    let conn = c.open().expect("open");
    let _ = c.submit(conn, Millis(0), get()).expect("submit");
    let deadline = c.poll_timeout().expect("a request is in flight");
    assert_eq!(deadline, Millis(30_000));
    c.handle_timeout(Millis(30_000));
    let ev = simplify(core::iter::from_fn(|| c.poll_event()).collect());
    assert_eq!(ev, vec![E::TimedOut]);
    assert!(c.submit(conn, Millis(31_000), get()).is_ok());
}

#[test]
fn stale_request_handle_is_rejected() {
    client!(c);
    let conn = c.open().expect("open");
    let _r = c.submit(conn, Millis(0), get()).expect("submit");
    c.close(conn);
    assert!(c.submit(conn, Millis(0), get()).is_err());
}
