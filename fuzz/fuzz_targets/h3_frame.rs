//! HTTP/3 frames off one stream: any split of the bytes reads the same frames and
//! payload, every step makes progress, and varints and SETTINGS read back as written.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::h3::frame::{self, Out, Piece, Reader, VARINT_MAX};

/// Frames (type, length) and their payload bytes in order.
#[derive(Debug, Default, PartialEq, Eq)]
struct Read {
    frames: Vec<(u64, u64)>,
    payload: Vec<u8>,
    boundary: bool,
}

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let whole = read(rest, |_| usize::MAX);
    let mut x = u32::from(knob) | 1;
    let split = read(rest, |_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (x >> 24) as usize % 9 + 1
    });
    assert_eq!(whole, split, "a split changed what was read");
    let wanted = read(rest, |r| usize::try_from(r.want()).unwrap_or(usize::MAX));
    assert_eq!(
        whole, wanted,
        "reading what want says changed what was read"
    );
    varints(rest);
    settings(rest);
});

/// Feed `b` in pieces of `size` bytes, checking each step against the reader's state.
fn read(b: &[u8], mut size: impl FnMut(&Reader) -> usize) -> Read {
    let mut r = Reader::default();
    let mut got = Read::default();
    let mut frame: Option<(u64, u64, u64)> = None;
    let mut at = 0;
    while at < b.len() {
        let end = at + size(&r).clamp(1, b.len() - at);
        let mut piece = &b[at..end];
        while !piece.is_empty() {
            let (n, p) = r.step(piece);
            assert!(
                n > 0 && n <= piece.len(),
                "a step took {n} of {}",
                piece.len()
            );
            match p {
                Some(Piece::Frame { ty, len }) => {
                    assert!(frame.is_none(), "a frame inside a frame");
                    assert!(len <= VARINT_MAX && ty <= VARINT_MAX);
                    got.frames.push((ty, len));
                    if len == 0 {
                        assert!(r.at_boundary());
                    } else {
                        assert_eq!(r.in_payload(), Some((ty, len)));
                        frame = Some((ty, len, 0));
                    }
                }
                Some(Piece::Payload { ty, bytes, end }) => {
                    assert!(frame.is_some(), "payload before any frame");
                    let Some((fty, len, seen)) = frame.as_mut() else {
                        return got;
                    };
                    assert_eq!(ty, *fty);
                    assert_eq!(bytes.len(), n);
                    *seen += n as u64;
                    assert!(*seen <= *len);
                    assert_eq!(end, *seen == *len);
                    got.payload.extend_from_slice(bytes);
                    if end {
                        assert!(r.at_boundary());
                        frame = None;
                    }
                }
                None => assert!(r.in_payload().is_none(), "a payload step made nothing"),
            }
            piece = &piece[n..];
        }
        at = end;
    }
    got.boundary = r.at_boundary();
    got
}

/// Each 8 bytes as a value, written shortest and read back.
fn varints(b: &[u8]) {
    for chunk in b.as_chunks::<8>().0 {
        let v = u64::from_le_bytes(*chunk) >> (chunk[0] % 64);
        let mut buf = [0u8; 8];
        let mut out = Out::new(&mut buf);
        assert!(out.varint(v).is_ok());
        let want = v.min(VARINT_MAX);
        assert_eq!(out.len(), frame::varint_len(want));
        assert_eq!(frame::varint(out.written()), Some((want, out.len())));
        let short = out.len() - 1;
        assert_eq!(frame::varint(&out.written()[..short]), None);
        let mut small = [0u8; 8];
        let mut out = Out::new(&mut small[..short]);
        assert!(out.varint(v).is_err(), "a varint written past its room");
    }
    if let Some((v, n)) = frame::varint(b) {
        assert!(v <= VARINT_MAX && n <= b.len() && n.is_power_of_two());
    }
}

/// This side's SETTINGS read back as one frame of the three settings.
fn settings(b: &[u8]) {
    let mut v = [0u8; 8];
    let n = b.len().min(8);
    v[..n].copy_from_slice(&b[..n]);
    let max = u64::from_le_bytes(v);
    let mut buf = [0u8; 32];
    let mut out = Out::new(&mut buf);
    assert!(frame::put_settings(&mut out, max).is_ok());
    let got = read(out.written(), |_| usize::MAX);
    assert!(got.boundary);
    assert_eq!(got.frames, [(frame::SETTINGS, got.payload.len() as u64)]);
    let mut p = got.payload.as_slice();
    let mut pairs = Vec::new();
    while let Some((v, n)) = frame::varint(p) {
        pairs.push(v);
        p = &p[n..];
    }
    assert!(p.is_empty());
    assert_eq!(
        pairs,
        [
            frame::SETTING_QPACK_MAX_TABLE_CAPACITY,
            0,
            frame::SETTING_MAX_FIELD_SECTION_SIZE,
            max.min(VARINT_MAX),
            frame::SETTING_QPACK_BLOCKED_STREAMS,
            0,
        ]
    );
}
