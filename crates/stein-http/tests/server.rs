//! Server-side h1.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_http::h1::Error;
use stein_http::h1::server::{
    encode_response, locate_request, pack_headers, parse_request, request_len,
};
use stein_http::header;

#[test]
fn a_complete_post_parses() {
    let raw = b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                Content-Length: 5\r\n\r\nhello";
    let r = parse_request(raw).expect("valid").expect("complete");
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/mcp");
    assert_eq!(r.content_length, Some(5));
    assert!(r.keep_alive, "HTTP/1.1 keeps alive unless told otherwise");
    assert_eq!(&raw[r.head_len..], b"hello");
}

#[test]
fn a_partial_head_is_not_an_error() {
    let raw = b"POST /mcp HTTP/1.1\r\nContent-Len";
    assert_eq!(parse_request(raw).expect("not an error"), None);
}

#[test]
fn connection_close_is_honoured() {
    let raw = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
    let r = parse_request(raw).expect("valid").expect("complete");
    assert!(!r.keep_alive);
    assert_eq!(r.content_length, None);
}

#[test]
fn request_len_waits_for_the_whole_body() {
    let head = b"POST /mcp HTTP/1.1\r\nContent-Length: 10\r\n\r\n";
    let mut buf = head.to_vec();
    assert_eq!(request_len(&buf).expect("valid"), None, "no body yet");
    buf.extend_from_slice(b"0123456");
    assert_eq!(request_len(&buf).expect("valid"), None, "partial body");
    buf.extend_from_slice(b"789");
    assert_eq!(request_len(&buf).expect("valid"), Some(head.len() + 10));
}

#[test]
fn a_chunked_request_is_refused_rather_than_misread() {
    let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
    assert_eq!(parse_request(raw), Err(Error::UnsupportedTransfer));
}

#[test]
fn a_malformed_request_line_is_refused() {
    let raw = b"not a request\r\n\r\n";
    assert!(matches!(parse_request(raw), Err(Error::Malformed)));
}

/// Locate, pack in place and read back. Checks the request line and body did not move.
fn packed(raw: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut buf = raw.to_vec();
    let l = locate_request(&buf).expect("valid").expect("complete");
    let (method, path) = (l.method, l.path);
    let head_len = l.head_len;
    let (at, len) = pack_headers(&mut buf, &l);
    assert_eq!(
        &buf[method.0..method.0 + method.1],
        &raw[method.0..method.0 + method.1]
    );
    assert_eq!(&buf[path.0..path.0 + path.1], &raw[path.0..path.0 + path.1]);
    assert!(at + len <= head_len, "records ran past the head");
    assert_eq!(&buf[head_len..], &raw[head_len..], "the body moved");
    header::iter(&buf[at..at + len])
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect()
}

#[test]
fn headers_pack_in_place_behind_the_request_line() {
    let raw = b"GET /a?b=1 HTTP/1.1\r\nHost: x\r\nCookie: sid=1; t=2\r\nX-Empty:\r\n\
                X-Tabbed:\t  v \r\nX:y\r\n\r\nbody";
    let got = packed(raw);
    let want: Vec<(&[u8], &[u8])> = vec![
        (b"Host", b"x"),
        (b"Cookie", b"sid=1; t=2"),
        (b"X-Empty", b""),
        (b"X-Tabbed", b"v"),
        (b"X", b"y"),
    ];
    assert_eq!(got.len(), want.len());
    for ((gn, gv), (wn, wv)) in got.iter().zip(&want) {
        assert_eq!((gn.as_slice(), gv.as_slice()), (*wn, *wv));
    }
    assert!(packed(b"GET / HTTP/1.1\r\n\r\n").is_empty());
    // No OWS, so records equal line length and everything shifts right.
    let got = packed(b"GET / HTTP/1.1\r\na:b\r\ncc:dd\r\neee:fff\r\ngggg:hhhh\r\n\r\n");
    assert_eq!(got[0], (b"a".to_vec(), b"b".to_vec()));
    assert_eq!(got[3], (b"gggg".to_vec(), b"hhhh".to_vec()));
}

#[test]
fn packing_agrees_with_a_copy_for_many_shapes() {
    // Random shapes against packing into a separate buffer.
    let mut seed = 7u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..2000 {
        let n = (next() % 9) as usize;
        let mut raw = b"POST /p HTTP/1.1\r\n".to_vec();
        let mut want = Vec::new();
        for i in 0..n {
            let nl = 1 + (next() % 12) as usize;
            let vl = (next() % 20) as usize;
            let name: Vec<u8> = (0..nl).map(|j| b'a' + ((i + j) % 26) as u8).collect();
            let value: Vec<u8> = (0..vl).map(|j| b'0' + ((i * 3 + j) % 10) as u8).collect();
            raw.extend_from_slice(&name);
            raw.push(b':');
            let blank = if next() % 2 == 0 { b' ' } else { b'\t' };
            raw.extend(std::iter::repeat_n(blank, (next() % 3) as usize));
            raw.extend_from_slice(&value);
            raw.extend(std::iter::repeat_n(b' ', (next() % 2) as usize));
            raw.extend_from_slice(b"\r\n");
            want.push((name, value));
        }
        raw.extend_from_slice(b"\r\nthe body");
        let got = packed(&raw);
        assert_eq!(got, want, "{}", String::from_utf8_lossy(&raw));
    }
}

#[test]
fn locating_reports_what_parsing_does() {
    let raw =
        b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    let l = locate_request(raw).expect("valid").expect("complete");
    assert_eq!(&raw[l.method.0..l.method.0 + l.method.1], b"POST");
    assert_eq!(&raw[l.path.0..l.path.0 + l.path.1], b"/mcp");
    assert_eq!(l.content_length, Some(5));
    assert!(!l.keep_alive);
    assert_eq!(&raw[l.head_len..], b"hello");
    assert!(
        locate_request(b"GET / HTTP/1.1\r\nHo")
            .expect("partial")
            .is_none()
    );
    let long = format!("GET / HTTP/1.1\r\n{}: v\r\n\r\n", "n".repeat(300));
    assert_eq!(
        locate_request(long.as_bytes()).err(),
        Some(Error::HeadersTooLarge)
    );
}

#[test]
fn responses_round_trip_through_the_client_decoder() {
    let mut out = Vec::new();
    encode_response(&mut out, 200, "application/json", br#"{"ok":true}"#);
    let text = String::from_utf8(out).expect("ascii");
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("Content-Type: application/json\r\n"));
    assert!(text.contains("Content-Length: 11\r\n"));
    assert!(text.ends_with("\r\n\r\n{\"ok\":true}"));
}

#[test]
fn an_empty_body_still_declares_its_length() {
    let mut out = Vec::new();
    encode_response(&mut out, 202, "application/json", b"");
    let text = String::from_utf8(out).expect("ascii");
    assert!(text.contains("Content-Length: 0\r\n"), "{text}");
    assert!(text.ends_with("\r\n\r\n"));
}

#[test]
fn encoding_clears_whatever_was_in_the_buffer() {
    let mut out = b"leftover".to_vec();
    encode_response(&mut out, 204, "text/plain", b"");
    assert!(out.starts_with(b"HTTP/1.1 204 "), "buffer was not cleared");
}

#[test]
fn statuses_the_mcp_endpoint_uses_all_have_reasons() {
    for (code, reason) in [
        (200u16, "OK"),
        (202, "Accepted"),
        (204, "No Content"),
        (405, "Method Not Allowed"),
    ] {
        let mut out = Vec::new();
        encode_response(&mut out, code, "text/plain", b"");
        let text = String::from_utf8(out).expect("ascii");
        assert!(
            text.starts_with(&format!("HTTP/1.1 {code} {reason}\r\n")),
            "{text}"
        );
    }
}

#[test]
fn extra_headers_land_before_the_body() {
    let mut out = Vec::new();
    stein_http::h1::server::encode_response_with(
        &mut out,
        200,
        "application/json",
        &[
            ("Access-Control-Allow-Origin", "*"),
            ("Connection", "keep-alive"),
        ],
        b"{}",
    );
    let text = String::from_utf8(out).expect("ascii");
    assert!(
        text.contains("\r\nAccess-Control-Allow-Origin: *\r\n"),
        "{text}"
    );
    assert!(
        text.contains("\r\nConnection: keep-alive\r\n\r\n"),
        "{text}"
    );
    assert!(text.ends_with("\r\n\r\n{}"));
    let (head, body) = text.split_once("\r\n\r\n").expect("head and body");
    assert!(head.contains("Content-Length: 2"));
    assert_eq!(body, "{}");
}

#[test]
fn a_content_length_is_digits_and_nothing_else() {
    let with = |v: &str| format!("POST / HTTP/1.1\r\nContent-Length:{v}\r\n\r\n").into_bytes();
    for ok in [" 5", "5", " 5 \t", "0", "0005", "18446744073709551615"] {
        assert!(parse_request(&with(ok)).is_ok(), "{ok:?} is a length");
    }
    for bad in [
        "+5",
        "-5",
        "5, 5",
        "5,5",
        "0x5",
        "5.0",
        "5 5",
        " ",
        "",
        "five",
        "18446744073709551616",
        "\u{a0}5",
        "5\u{2003}",
    ] {
        assert_eq!(parse_request(&with(bad)), Err(Error::Malformed), "{bad:?}");
    }
}

#[test]
fn two_lengths_that_disagree_are_refused() {
    let raw = b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!";
    assert_eq!(parse_request(raw), Err(Error::Malformed));
    let same = b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
    let r = parse_request(same).expect("valid").expect("complete");
    assert_eq!(r.content_length, Some(5));
}

/// Bare LF lines are shorter than their packed records and would overrun the buffer.
#[test]
fn a_bare_lf_line_end_is_refused() {
    for raw in [
        &b"GET / HTTP/1.1\na:\nb:\nc:\n\n"[..],
        b"GET / HTTP/1.1\r\nHost: x\nA: b\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: x\r\n\n",
        b"GET / HTTP/1.1\n\r\n",
    ] {
        let text = String::from_utf8_lossy(raw);
        assert_eq!(parse_request(raw), Err(Error::Malformed), "{text}");
        assert!(
            matches!(locate_request(raw), Err(Error::Malformed)),
            "{text}"
        );
    }
    // Incomplete, not refused, until the head ends.
    assert_eq!(parse_request(b"GET / HTTP/1.1\na:"), Ok(None));
}

#[test]
fn connection_close_in_a_list_is_honoured() {
    let keep = |raw: &[u8]| {
        parse_request(raw)
            .expect("valid")
            .expect("complete")
            .keep_alive
    };
    assert!(!keep(b"GET / HTTP/1.1\r\nConnection: close, TE\r\n\r\n"));
    assert!(!keep(b"GET / HTTP/1.1\r\nConnection: TE ,Close\r\n\r\n"));
    assert!(!keep(
        b"GET / HTTP/1.1\r\nConnection: keep-alive, close\r\n\r\n"
    ));
    assert!(!keep(
        b"GET / HTTP/1.1\r\nConnection: close\r\nConnection: keep-alive\r\n\r\n"
    ));
    assert!(!keep(
        b"GET / HTTP/1.0\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n"
    ));
    assert!(keep(
        b"GET / HTTP/1.0\r\nConnection: TE, keep-alive\r\n\r\n"
    ));
    assert!(!keep(b"GET / HTTP/1.0\r\nConnection: TE\r\n\r\n"));
    assert!(keep(b"GET / HTTP/1.1\r\nConnection: TE\r\n\r\n"));
}

#[test]
fn a_head_alone_is_the_response_without_its_body() {
    use stein_http::h1::server::{Body, encode_head, encode_response_with};
    let extra = [("Cache-Control", "no-cache")];
    let mut whole = Vec::new();
    encode_response_with(&mut whole, 200, "video/mp4", &extra, b"segment");
    let mut head = b"leftover".to_vec();
    encode_head(&mut head, 200, "video/mp4", &extra, Body::Length(7));
    head.extend_from_slice(b"segment");
    assert_eq!(head, whole);
}

#[test]
fn a_chunked_head_declares_no_length_and_its_body_decodes() {
    use stein_http::h1::chunk;
    use stein_http::h1::server::{Body, encode_head};
    let mut out = Vec::new();
    encode_head(&mut out, 200, "video/mp4", &[], Body::Chunked);
    let text = String::from_utf8(out.clone()).expect("ascii");
    assert!(
        text.contains("\r\nTransfer-Encoding: chunked\r\n\r\n"),
        "{text}"
    );
    assert!(!text.contains("Content-Length"), "{text}");
    // two parts as they come, then the end
    let mut body = Vec::new();
    for part in [&b"moof"[..], &b"mdat bytes"[..]] {
        let mut framed = vec![0u8; part.len() + chunk::MAX_HEADER + chunk::TAIL];
        let n = chunk::encode(&mut framed, part).expect("room");
        body.extend_from_slice(&framed[..n]);
    }
    body.extend_from_slice(chunk::LAST);
    assert_eq!(body, b"4\r\nmoof\r\na\r\nmdat bytes\r\n0\r\n\r\n");
}

#[test]
fn statuses_of_ranges_caching_and_overload_have_reasons() {
    use stein_http::h1::server::{Body, encode_head};
    for (code, reason) in [
        (206u16, "Partial Content"),
        (304, "Not Modified"),
        (403, "Forbidden"),
        (416, "Range Not Satisfiable"),
        (429, "Too Many Requests"),
        (501, "Not Implemented"),
        (503, "Service Unavailable"),
    ] {
        let mut out = Vec::new();
        encode_head(&mut out, code, "text/plain", &[], Body::Length(0));
        let text = String::from_utf8(out).expect("ascii");
        assert!(
            text.starts_with(&format!("HTTP/1.1 {code} {reason}\r\n")),
            "{text}"
        );
    }
}
