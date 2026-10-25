//! Chunked request framing, parser `Transfer-Encoding` handling, and parser plus decoder end to end.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::h1::Error;
use stein_http::h1::dechunk::{Decoder, Event, Limits};
use stein_http::h1::server::{
    Transfer, locate_request, locate_request_with, parse_request, parse_request_with, request_len,
};

/// POST with the given header lines, without line ends.
fn post(version: &str, lines: &[&str]) -> Vec<u8> {
    let mut raw = format!("POST /up HTTP/{version}\r\nHost: x\r\n").into_bytes();
    for line in lines {
        raw.extend_from_slice(line.as_bytes());
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\n");
    raw
}

/// Both entry points must agree.
fn framed(raw: &[u8], transfer: Transfer) -> Result<(Option<u64>, bool), Error> {
    let parsed = parse_request_with(raw, transfer).map(|r| {
        let r = r.expect("a whole head");
        (r.content_length, r.chunked)
    });
    let located = locate_request_with(raw, transfer).map(|l| {
        let l = l.expect("a whole head");
        (l.content_length, l.chunked)
    });
    assert_eq!(parsed, located);
    parsed
}

#[test]
fn a_chunked_request_is_reported_to_a_caller_that_decodes_it() {
    let spellings = [
        "Transfer-Encoding: chunked",
        "transfer-encoding: chunked",
        "TRANSFER-ENCODING: CHUNKED",
        "Transfer-Encoding: Chunked",
        "Transfer-Encoding:chunked",
        "Transfer-Encoding:   chunked  ",
        "Transfer-Encoding:\tchunked\t",
    ];
    for line in spellings {
        let raw = post("1.1", &[line]);
        assert_eq!(framed(&raw, Transfer::Chunked), Ok((None, true)), "{line}");
    }
    for lines in [&[][..], &["Content-Length: 5"], &["Connection: close"]] {
        let raw = post("1.1", lines);
        let asked = framed(&raw, Transfer::Chunked).unwrap();
        assert_eq!(framed(&raw, Transfer::Refuse).unwrap(), asked);
        assert!(!asked.1);
        assert_eq!(
            parse_request(&raw),
            parse_request_with(&raw, Transfer::Chunked)
        );
    }
}

/// Decoding from `head_len` ends exactly at the next pipelined request.
#[test]
fn a_chunked_request_and_the_one_behind_it() {
    let mut raw = post("1.1", &["Transfer-Encoding: chunked"]);
    let head = raw.len();
    raw.extend_from_slice(b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nTrailer: v\r\n\r\n");
    let next = raw.len();
    raw.extend_from_slice(b"GET /next HTTP/1.1\r\nHost: x\r\n\r\n");

    let r = parse_request_with(&raw, Transfer::Chunked)
        .unwrap()
        .unwrap();
    assert!(r.chunked && r.keep_alive);
    assert_eq!((r.content_length, r.head_len), (None, head));
    let mut dec = Decoder::new(Limits::default());
    let mut body = Vec::new();
    let mut at = head;
    for _ in 0..100 {
        let s = dec.step(&raw[at..]).unwrap();
        if let Event::Data { at: from, len } = s.event {
            body.extend_from_slice(&raw[at + from..at + from + len]);
        }
        at += s.consumed;
        if s.event == Event::End {
            break;
        }
    }
    assert!(dec.is_done());
    assert_eq!(body, b"hello world");
    assert_eq!(at, next);
    let r = parse_request_with(&raw[at..], Transfer::Chunked)
        .unwrap()
        .unwrap();
    assert_eq!((r.path, r.chunked), ("/next", false));
}

/// With `Transfer::Refuse` any `Transfer-Encoding` is refused.
#[test]
fn a_caller_that_does_not_ask_refuses_as_before() {
    let refused = [
        &["Transfer-Encoding: chunked"][..],
        &["Transfer-Encoding: gzip"],
        &["Transfer-Encoding: gzip, chunked"],
        &["Transfer-Encoding: "],
        &["Content-Length: 5", "Transfer-Encoding: chunked"],
        &["Transfer-Encoding: chunked", "Content-Length: 5"],
        &["Transfer-Encoding: chunked", "Content-Length: x"],
    ];
    for lines in refused {
        let raw = post("1.1", lines);
        assert_eq!(
            parse_request(&raw),
            Err(Error::UnsupportedTransfer),
            "{lines:?}"
        );
        assert_eq!(locate_request(&raw).err(), Some(Error::UnsupportedTransfer));
        assert_eq!(request_len(&raw), Err(Error::UnsupportedTransfer));
        assert_eq!(
            framed(&raw, Transfer::Refuse),
            Err(Error::UnsupportedTransfer)
        );
    }
    // an earlier bad length is reported first
    let raw = post("1.1", &["Content-Length: x", "Transfer-Encoding: chunked"]);
    assert_eq!(parse_request(&raw), Err(Error::Malformed));
    let raw = post("1.0", &["Transfer-Encoding: chunked"]);
    assert_eq!(parse_request(&raw), Err(Error::UnsupportedTransfer));
}

/// RFC 9112 6.1 and 6.3 with `Transfer::Chunked`.
#[test]
fn the_framing_rules_of_rfc_9112() {
    let te = "Transfer-Encoding: chunked";
    let cases: [(&str, &[&str], Error); 14] = [
        ("1.1", &["Content-Length: 5", te], Error::TransferWithLength),
        ("1.1", &[te, "Content-Length: 5"], Error::TransferWithLength),
        ("1.1", &[te, "Content-Length: 0"], Error::TransferWithLength),
        (
            "1.1",
            &["Transfer-Encoding: gzip"],
            Error::TransferNotChunked,
        ),
        (
            "1.1",
            &["Transfer-Encoding: identity"],
            Error::TransferNotChunked,
        ),
        (
            "1.1",
            &["Transfer-Encoding: chunked, gzip"],
            Error::TransferNotChunked,
        ),
        (
            "1.1",
            &[te, "Transfer-Encoding: gzip"],
            Error::TransferNotChunked,
        ),
        (
            "1.1",
            &["Transfer-Encoding: gzip, chunked"],
            Error::UnsupportedTransfer,
        ),
        (
            "1.1",
            &["Transfer-Encoding: gzip", te],
            Error::UnsupportedTransfer,
        ),
        (
            "1.1",
            &["Transfer-Encoding: identity, chunked"],
            Error::UnsupportedTransfer,
        ),
        (
            "1.1",
            &["Transfer-Encoding: chunked, chunked"],
            Error::Malformed,
        ),
        ("1.1", &[te, te], Error::Malformed),
        ("1.0", &[te], Error::TransferOnOldVersion),
        (
            "1.0",
            &["Content-Length: 5", te],
            Error::TransferOnOldVersion,
        ),
    ];
    for (version, lines, want) in cases {
        let raw = post(version, lines);
        assert_eq!(
            framed(&raw, Transfer::Chunked),
            Err(want),
            "{version} {lines:?}"
        );
    }
}

/// Ambiguous `Transfer-Encoding` spellings. All must be refused.
#[test]
fn transfer_encoding_obfuscations_are_refused() {
    let cases: [&str; 24] = [
        "Transfer-Encoding: xchunked",
        "Transfer-Encoding: chunkedx",
        "Transfer-Encoding: chunk",
        "Transfer-Encoding: chunked;q=1",
        "Transfer-Encoding: chunked; foo",
        "Transfer-Encoding: \"chunked\"",
        "Transfer-Encoding: 'chunked'",
        "Transfer-Encoding: chunked,",
        "Transfer-Encoding: ,chunked",
        "Transfer-Encoding: chunked, ,",
        "Transfer-Encoding: chunked chunked",
        "Transfer-Encoding: chunked\x0b",
        "Transfer-Encoding: \x0bchunked",
        "Transfer-Encoding: \x0cchunked",
        "Transfer-Encoding: chunked\x00",
        "Transfer-Encoding: ch\u{16b}nked",
        "Transfer-Encoding: chunked\r\n\tgzip",
        "Transfer-Encoding:\r\n chunked",
        "Transfer-Encoding : chunked",
        "Transfer-Encoding\t: chunked",
        " Transfer-Encoding: chunked",
        "Transfer-Encoding\n: chunked",
        "Transfer-Encoding: chunked\r\nTransfer-encoding: cow",
        "Transfer-Encoding: cow\r\nTransfer-Encoding: chunked",
    ];
    for line in cases {
        let raw = post("1.1", &[line]);
        let parsed = parse_request_with(&raw, Transfer::Chunked);
        assert!(parsed.is_err(), "{line:?} came out as {parsed:?}");
        assert!(
            locate_request_with(&raw, Transfer::Chunked).is_err(),
            "{line:?}"
        );
        // and with Transfer::Refuse
        assert!(parse_request(&raw).is_err(), "{line:?}");
    }
}
