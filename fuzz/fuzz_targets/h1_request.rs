//! Server HTTP/1.1 request heads over a pipeline.
//! `parse_request`, `locate_request` and `request_len` must agree on head end and framing, checked against a naive model.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::h1::Error;
use stein_http::h1::server::{self, Located, Request, Transfer};
use stein_http::header;

type Parsed<'a> = Result<Option<Request<'a>>, Error>;

/// Head as the naive model reads it.
struct Head<'a> {
    minor: u8,
    fields: Vec<(&'a [u8], &'a [u8])>,
}

fuzz_target!(|data: &[u8]| {
    let mut at = 0;
    while let Some(buf) = data.get(at..).filter(|b| !b.is_empty()) {
        check(buf);
        match server::request_len(buf) {
            Ok(Some(n)) => {
                assert!(n > 0, "empty request");
                at += n;
            }
            _ => break,
        }
    }
});

fn check(buf: &[u8]) {
    let refuse = server::parse_request(buf);
    assert_eq!(refuse, server::parse_request_with(buf, Transfer::Refuse));
    let chunked = server::parse_request_with(buf, Transfer::Chunked);
    // Refuse only rejects more, so anything it accepts reads the same.
    if refuse.is_ok() {
        assert_eq!(refuse, chunked, "transfer mode changed an accepted head");
    }
    mode(buf, Transfer::Refuse, &refuse);
    mode(buf, Transfer::Chunked, &chunked);
    request_len(buf, &refuse);
}

fn mode(buf: &[u8], t: Transfer, parsed: &Parsed<'_>) {
    prefixes(buf, t, parsed);
    let loc = server::locate_request_with(buf, t);
    let r = match parsed {
        Err(e) => {
            assert!(matches!(loc, Err(x) if x == *e), "locate error differs");
            return;
        }
        Ok(None) => {
            assert!(matches!(loc, Ok(None)), "locate completed a partial head");
            return;
        }
        Ok(Some(r)) => r,
    };
    let h = head(buf, r);
    framing(&h, t, r);
    let too_large = h
        .fields
        .iter()
        .any(|(n, v)| n.len() > u8::MAX as usize || v.len() > u16::MAX as usize);
    match loc {
        Ok(Some(l)) => {
            assert!(!too_large, "located a field a record cannot hold");
            located(buf, r, &l, &h);
        }
        Err(Error::HeadersTooLarge) => assert!(too_large, "spurious HeadersTooLarge"),
        _ => assert!(too_large && loc.is_err(), "locate disagrees with parse"),
    }
}

/// A decided prefix stays decided. A complete head is never complete earlier, and trailing bytes change nothing.
fn prefixes(buf: &[u8], t: Transfer, full: &Parsed<'_>) {
    let n = match full {
        Ok(Some(r)) => r.head_len,
        _ => usize::MAX,
    };
    let len = buf.len();
    let probe = |k: usize| {
        let Some(p) = buf.get(..k) else { return };
        let got = server::parse_request_with(p, t);
        if k < n {
            let undecided = matches!(got, Ok(None));
            assert!(
                undecided || (n == usize::MAX && got == *full),
                "prefix {k} decided early"
            );
        } else {
            assert_eq!(got, *full, "bytes after the head changed it");
        }
    };
    (0..=len.min(160)).for_each(probe);
    (0..16).for_each(|i| probe(len * i / 16));
    if n != usize::MAX {
        [n.wrapping_sub(1), n, n + 1].into_iter().for_each(probe);
    }
}

/// Split the head by hand. httparse already accepted it. Every line ends in CRLF.
fn head<'a>(buf: &'a [u8], r: &Request<'a>) -> Head<'a> {
    let whole = buf.get(..r.head_len).unwrap_or(&[]);
    let bare = (0..whole.len()).any(|i| whole[i] == b'\n' && (i == 0 || whole[i - 1] != b'\r'));
    assert!(!bare, "accepted a bare LF");
    let start = (r.method.as_ptr() as usize).checked_sub(buf.as_ptr() as usize);
    let start = start.filter(|s| *s < r.head_len && r.head_len <= buf.len());
    assert!(start.is_some(), "method outside the head");
    let start = start.unwrap_or(0);
    let skipped = buf.get(..start).unwrap_or(&[]);
    assert!(
        skipped.iter().all(|b| matches!(b, b'\r' | b'\n')),
        "junk before method"
    );

    let bytes = buf.get(start..r.head_len).unwrap_or(&[]);
    let mut lines = bytes
        .split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let line = lines.next().unwrap_or(&[]);
    let mut parts = line.split(|b| *b == b' ');
    assert_eq!(parts.next(), Some(r.method.as_bytes()));
    assert_eq!(parts.next(), Some(r.path.as_bytes()));
    let version = parts.next();
    assert!(parts.next().is_none(), "extra request line parts");
    let minor = u8::from(version == Some(b"HTTP/1.1"));
    assert!(minor == 1 || version == Some(b"HTTP/1.0"), "odd version");

    let mut fields = Vec::new();
    for l in lines.by_ref() {
        if l.is_empty() {
            break;
        }
        let colon = l.iter().position(|b| *b == b':');
        assert!(colon.is_some(), "field line without a colon");
        let c = colon.unwrap_or(0);
        let name = l.get(..c).unwrap_or(&[]);
        fields.push((name, trim(l.get(c + 1..).unwrap_or(&[]))));
    }
    assert_eq!(
        lines.next(),
        Some(&b""[..]),
        "head ends past its blank line"
    );
    assert!(lines.next().is_none(), "head ends past its blank line");
    Head { minor, fields }
}

fn trim(mut v: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = v {
        v = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = v {
        v = rest;
    }
    v
}

fn digits(v: &[u8]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.iter().try_fold(0u64, |n, b| {
        let d = b.is_ascii_digit().then(|| u64::from(b - b'0'))?;
        n.checked_mul(10)?.checked_add(d)
    })
}

fn is(a: &[u8], b: &str) -> bool {
    header::eq_ignore_case(a, b.as_bytes())
}

/// RFC 9112 6, written plainly.
fn framing(h: &Head<'_>, t: Transfer, r: &Request<'_>) {
    let mut cl = None;
    let mut cl_ok = true;
    let (mut close, mut keep_opt) = (false, false);
    let mut te: Vec<&[u8]> = Vec::new();
    let mut has_te = false;
    for &(n, v) in &h.fields {
        if is(n, "content-length") {
            match digits(v) {
                Some(x) if cl.is_none_or(|p| p == x) => cl = Some(x),
                _ => cl_ok = false,
            }
        } else if is(n, "connection") {
            for o in v.split(|b| *b == b',').map(trim) {
                close |= is(o, "close");
                keep_opt |= is(o, "keep-alive");
            }
        } else if is(n, "transfer-encoding") {
            has_te = true;
            te.extend(v.split(|b| *b == b',').map(trim));
        }
    }
    assert!(cl_ok, "accepted a bad or conflicting Content-Length");
    let keep = !close && (h.minor == 1 || keep_opt);
    assert_eq!(r.keep_alive, keep, "keep alive");
    if !has_te {
        assert!(!r.chunked, "chunked without Transfer-Encoding");
        assert_eq!(r.content_length, cl, "content length");
        return;
    }
    assert!(
        t == Transfer::Chunked,
        "Transfer-Encoding accepted when refused"
    );
    assert!(r.chunked, "Transfer-Encoding accepted but not chunked");
    assert!(h.minor == 1, "chunked on HTTP/1.0");
    assert!(
        cl.is_none() && r.content_length.is_none(),
        "chunked beside a length"
    );
    assert!(
        te.len() == 1 && is(te.first().copied().unwrap_or(&[]), "chunked"),
        "codings"
    );
}

fn span(buf: &[u8], s: (usize, usize)) -> &[u8] {
    let got = buf.get(s.0..s.0 + s.1);
    assert!(got.is_some(), "located span out of bounds");
    got.unwrap_or(&[])
}

fn located(buf: &[u8], r: &Request<'_>, l: &Located, h: &Head<'_>) {
    assert_eq!(span(buf, l.method), r.method.as_bytes());
    assert_eq!(span(buf, l.path), r.path.as_bytes());
    assert_eq!(l.head_len, r.head_len);
    assert_eq!(l.content_length, r.content_length);
    assert_eq!(l.chunked, r.chunked);
    assert_eq!(l.keep_alive, r.keep_alive);
    assert_eq!(l.minor, h.minor);

    let keep = buf.len().min(r.head_len + 64);
    let mut copy = buf.get(..keep).unwrap_or(&[]).to_vec();
    let (at, len) = server::pack_headers(&mut copy, l);
    assert!(at + len <= r.head_len, "records ran past the head");
    assert_eq!(
        copy.get(r.head_len..),
        buf.get(r.head_len..keep),
        "moved the body"
    );
    assert_eq!(copy.get(..at), buf.get(..at), "moved the request line");
    if h.fields.is_empty() {
        assert_eq!((at, len), (r.head_len, 0));
    }
    let packed = copy.get(at..at + len).unwrap_or(&[]);
    let got: Vec<_> = header::iter(packed).collect();
    assert_eq!(got, h.fields, "packed fields differ from the lines");
    let used: usize = got.iter().map(|(n, v)| 3 + n.len() + v.len()).sum();
    assert_eq!(used, len, "packed region has a tail");
    for &(n, v) in &got {
        assert!(
            header::check_field(n, v).is_ok(),
            "accepted an unsendable field"
        );
        let name = core::str::from_utf8(n);
        assert!(name.is_ok(), "name not ascii");
        let name = name.unwrap_or("");
        let first = h.fields.iter().find(|(m, _)| is(m, name)).map(|(_, v)| *v);
        assert_eq!(header::find(packed, name), first, "find");
    }
}

fn request_len(buf: &[u8], parsed: &Parsed<'_>) {
    let got = server::request_len(buf);
    let want = match parsed {
        Err(e) => Err(*e),
        Ok(None) => Ok(None),
        Ok(Some(r)) => {
            let body = usize::try_from(r.content_length.unwrap_or(0)).unwrap_or(usize::MAX);
            let total = r.head_len.saturating_add(body);
            Ok((buf.len() >= total).then_some(total))
        }
    };
    assert_eq!(got, want, "request_len disagrees with parse");
}
