#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::h3::prng::Rng;

/// Reserved types `0x1f * N + 0x21`, skipped as unknown (RFC 9114 6.2.3, 7.2.8).
fn is_reserved(ty: u64) -> bool {
    ty >= 0x21 && (ty - 0x21).is_multiple_of(0x1f)
}

fn encoded(f: impl FnOnce(&mut Out<'_>) -> Result<(), Full>) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let mut out = Out::new(&mut buf);
    f(&mut out).unwrap();
    out.written().to_vec()
}

#[test]
fn varints_match_rfc_9000_appendix_a1() {
    let cases: [(&[u8], u64); 5] = [
        (
            &[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c],
            151_288_809_941_952_652,
        ),
        (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
        (&[0x7b, 0xbd], 15_293),
        (&[0x25], 37),
        (&[0x40, 0x25], 37),
    ];
    for (bytes, v) in cases {
        assert_eq!(varint(bytes), Some((v, bytes.len())));
    }
    for v in [
        0,
        63,
        64,
        16383,
        16384,
        1_073_741_823,
        1_073_741_824,
        VARINT_MAX,
    ] {
        let out = encoded(|o| o.varint(v));
        assert_eq!(
            (out.len(), varint(&out)),
            (varint_len(v), Some((v, varint_len(v))))
        );
    }
    assert_eq!(varint(&[0x40]), None, "short of its second byte");
    assert_eq!(varint(&[]), None);
}

#[test]
fn a_full_buffer_refuses_and_keeps_what_fit() {
    let mut buf = [0u8; 3];
    let mut out = Out::new(&mut buf);
    assert_eq!(out.varint(16384), Err(Full));
    assert!(out.is_empty());
    out.varint(64).unwrap();
    assert_eq!(out.bytes(b"ab"), Err(Full));
    out.byte(9).unwrap();
    assert_eq!(out.written(), &[0x40, 0x40, 9]);
    assert_eq!(out.byte(1), Err(Full));
}

/// Frames of `wire` read in pieces of `cut` bytes, as `(type, payload)`.
fn read_all(wire: &[u8], cut: usize) -> Vec<(u64, Vec<u8>)> {
    let mut r = Reader::default();
    let mut frames: Vec<(u64, Vec<u8>)> = Vec::new();
    for piece in wire.chunks(cut) {
        let mut at = 0;
        while at < piece.len() {
            let (n, got) = r.step(&piece[at..]);
            at += n;
            match got {
                Some(Piece::Frame { ty, len }) => {
                    frames.push((ty, Vec::new()));
                    assert!(len > 0 || r.at_boundary());
                }
                Some(Piece::Payload { bytes, .. }) => {
                    frames.last_mut().unwrap().1.extend_from_slice(bytes)
                }
                None => assert!(n > 0, "a step with nothing to show took a byte"),
            }
        }
    }
    assert!(r.at_boundary(), "the wire ends between frames");
    frames
}

fn sample_wire() -> Vec<u8> {
    encoded(|o| {
        put_settings(o, 65536)?;
        put_header(o, HEADERS, 3)?;
        o.bytes(&[0, 0, 0xd1])?;
        put_header(o, DATA, 0)?;
        // Reserved type, 300 byte payload, two byte length.
        put_header(o, 0x21 + 0x1f * 3, 300)?;
        o.bytes(&[7u8; 300])?;
        put_header(o, DATA, 5)?;
        o.bytes(b"hello")
    })
}

#[test]
fn frames_read_the_same_cut_anywhere() {
    let wire = sample_wire();
    let whole = read_all(&wire, wire.len());
    let types: Vec<u64> = whole.iter().map(|f| f.0).collect();
    assert_eq!(types, [SETTINGS, HEADERS, DATA, 0x21 + 0x1f * 3, DATA]);
    assert_eq!(
        whole[0].1,
        [0x01, 0x00, 0x06, 0x80, 0x01, 0x00, 0x00, 0x07, 0x00]
    );
    assert_eq!(whole[4].1, b"hello");
    for cut in 1..12 {
        assert_eq!(read_all(&wire, cut), whole, "cut every {cut} bytes");
    }
    assert!(is_reserved(0x21) && is_reserved(0x21 + 0x1f * 3));
    assert!(!is_reserved(0x20) && !is_reserved(DATA));
}

/// Feeding exactly [`Reader::want`] bytes never mixes framing into a payload piece.
#[test]
fn want_splits_framing_from_payload() {
    let wire = sample_wire();
    let mut r = Reader::default();
    let mut at = 0;
    let mut payload = 0;
    while at < wire.len() {
        let want = r.want() as usize;
        assert!(want > 0);
        let end = (at + want).min(wire.len());
        let in_payload = r.in_payload().is_some();
        let (n, got) = r.step(&wire[at..end]);
        assert_eq!(n, end - at, "a step takes all it wanted");
        if in_payload {
            assert!(matches!(got, Some(Piece::Payload { .. })));
            payload += n;
        }
        at += n;
    }
    assert!(r.at_boundary());
    assert_eq!(payload, 9 + 3 + 300 + 5);
}

/// Stream bytes come from the network. Damage reads as some frames, never a panic.
#[test]
fn the_reader_survives_any_bytes() {
    let seed = sample_wire();
    let mut rng = Rng::new(7);
    for _ in 0..4000 {
        let data = rng.damage(&seed, 200);
        let cut = 1 + rng.below(9) as usize;
        let mut r = Reader::default();
        for piece in data.chunks(cut) {
            let mut at = 0;
            while at < piece.len() {
                let (n, got) = r.step(&piece[at..]);
                at += n;
                assert!(
                    n > 0 || got.is_some(),
                    "a step took nothing and made nothing"
                );
            }
        }
    }
}
