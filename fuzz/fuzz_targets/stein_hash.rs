//! stein-hash SHA-1 against RustCrypto's: one shot, any split of the input, the eight
//! lane hasher and its lanes resumed all give the reference digest.

#![no_main]

use fearless_simd::Level;
use libfuzzer_sys::fuzz_target;
use rustcrypto_sha1::Digest;
use stein_hash::{LANES, Sha1, Sha1x8, sha1, sha1_many};

/// Pieces of 0 to 199 bytes from an LCG seeded by the input, so splits cross blocks.
fn pieces(msg: &[u8], seed: u8) -> Vec<&[u8]> {
    let mut x = u32::from(seed) | 1;
    let mut out = Vec::new();
    let mut rest = msg;
    while !rest.is_empty() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let (piece, tail) = rest.split_at(((x >> 24) as usize % 200).min(rest.len()));
        out.push(piece);
        rest = tail;
    }
    out
}

fn reference(m: &[u8]) -> [u8; 20] {
    rustcrypto_sha1::Sha1::digest(m).into()
}

fuzz_target!(|data: &[u8]| {
    let Some((&knob, msg)) = data.split_first() else {
        return;
    };
    let want = reference(msg);
    assert_eq!(sha1(msg), want, "sha1 against the reference");
    let mut s = Sha1::new();
    for p in pieces(msg, knob) {
        s.update(p);
    }
    assert_eq!(s.finish(), want, "sha1 of a split");
    lanes(knob, msg);
});

/// Messages cut from the input, hashed side by side and one by one.
fn lanes(knob: u8, msg: &[u8]) {
    let mut msgs: Vec<&[u8]> = Vec::new();
    let mut rest = msg;
    let step = usize::from(knob) * 3 + 1;
    for _ in 0..LANES + usize::from(knob % 3) {
        let (m, tail) = rest.split_at(step.min(rest.len()));
        msgs.push(m);
        rest = tail;
    }
    msgs.push(rest);
    let want: Vec<[u8; 20]> = msgs.iter().map(|m| reference(m)).collect();
    for level in [Level::new(), Level::baseline()] {
        let mut out = vec![[0u8; 20]; msgs.len()];
        sha1_many(level, &msgs, &mut out);
        assert_eq!(out, want, "sha1_many against the reference");
        let chunks: [&[u8]; LANES] = core::array::from_fn(|l| msgs[l]);
        let mut x8 = Sha1x8::new();
        let took = x8.update(level, chunks);
        let took = took + x8.update(level, chunks.map(|m| &m[took..]));
        for (l, m) in chunks.iter().enumerate() {
            let mut s = x8.lane(l);
            s.update(&m[took..]);
            assert_eq!(s.finish(), want[l], "lane {l} resumed");
        }
    }
}
