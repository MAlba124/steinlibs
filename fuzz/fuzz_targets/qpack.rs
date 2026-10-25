//! QPACK with a zero capacity table: a response section decodes inside its limits or
//! is refused, more room never refuses what less took, and a request reads back as encoded.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::h3::frame::Out;
use stein_http::h3::qpack::{self, field_size};
use stein_http::header;

const MAXES: [usize; 5] = [0, 64, 256, 4096, 1 << 16];
const LINES: [usize; 5] = [0, 8, 64, 512, 4096];
const OUTS: [usize; 4] = [0, 16, 256, 8192];

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let knob = usize::from(knob);
    let (max, line, out) = (MAXES[knob % 5], LINES[knob / 5 % 5], OUTS[knob / 25 % 4]);
    response(rest, max, line, out);
    request(rest);
});

/// Decode `section` in the room given, then in the most.
fn response(section: &[u8], max: usize, line_len: usize, out_len: usize) {
    let mut line = vec![0xa5; line_len];
    let mut size = 0;
    let raw = qpack::decode(section, max, &mut line, |n, v| {
        size += field_size(n, v);
        assert!(size <= max, "a section past its limit");
        assert!(n.len() + v.len() <= line_len, "a line past its buffer");
        Ok(())
    });
    let mut out = vec![0xa5; out_len];
    let Ok(head) = qpack::decode_response(section, max, &mut line, &mut out) else {
        return;
    };
    assert!(raw.is_ok(), "a response that decodes as fields does not");
    assert!((100..600).contains(&head.status));
    assert!(head.len <= out_len);
    let small = records(&out[..head.len]);
    assert_eq!(small.len(), head.count);
    for (n, v) in &small {
        assert!(!n.is_empty() && !n.starts_with(b":"));
        assert!(!n.iter().any(u8::is_ascii_uppercase));
        assert!(!v.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)));
    }
    let mut line = vec![0x5a; LINES[4] * 16];
    let mut out = vec![0x5a; OUTS[3] * 16];
    let big = qpack::decode_response(section, MAXES[4], &mut line, &mut out);
    assert!(
        big.is_ok_and(|b| b.status == head.status && records(&out[..b.len]) == small),
        "more room changed what less took"
    );
}

fn records(packed: &[u8]) -> Fields {
    header::iter(packed)
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect()
}

/// Method, authority, path and field pairs split on newlines, encoded and read back.
fn request(b: &[u8]) {
    let text = String::from_utf8_lossy(b);
    let mut parts = text.split('\n');
    let (Some(method), Some(authority), Some(path)) = (parts.next(), parts.next(), parts.next())
    else {
        return;
    };
    let fields: Vec<(&str, &str)> = core::iter::from_fn(|| Some((parts.next()?, parts.next()?)))
        .take(64)
        .collect();
    let mut buf = vec![0u8; text.len() * 2 + 64 * 20 + 64];
    let mut out = Out::new(&mut buf);
    let put = qpack::put_request(&mut out, method, authority, path, fields.iter().copied());
    assert!(put.is_ok(), "a request past a buffer twice its size");
    let mut want: Fields = vec![
        (b":method".to_vec(), method.as_bytes().to_vec()),
        (b":scheme".to_vec(), b"https".to_vec()),
        (b":authority".to_vec(), authority.as_bytes().to_vec()),
        (b":path".to_vec(), path.as_bytes().to_vec()),
    ];
    want.extend(
        fields
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase().into_bytes(), v.as_bytes().to_vec())),
    );
    let mut line = vec![0u8; text.len() + 128];
    let mut got = Fields::new();
    let read = qpack::decode(out.written(), usize::MAX, &mut line, |n, v| {
        got.push((n.to_vec(), v.to_vec()));
        Ok(())
    });
    assert!(read.is_ok(), "a request this side encoded does not decode");
    assert_eq!(got, want);
}
