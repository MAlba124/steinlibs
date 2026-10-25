#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::vec::Vec;

fn hex(bytes: &[u8]) -> std::string::String {
    bytes.iter().map(|b| std::format!("{b:02x}")).collect()
}

/// Matches the python that made the table.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + n) & 0xff) as u8).collect()
}

/// Block and padding boundaries, from hashlib.
const VECTORS: &[(usize, &str)] = &[
    (0, "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
    (1, "bf8b4530d8d246dd74ac53a13471bba17941dff7"),
    (3, "4201de9f98cb0a9b8cf52398be7802b55d45266a"),
    (55, "63ac9708fb581dd150d3647549f9e0cb51dcf1df"),
    (56, "5a0c4d0eb2f15e05bbbff6b8dada9ae767fc03f0"),
    (57, "4e77d3077d2ab5167db3434c31cac3d15a3afc12"),
    (63, "d94ff3ad53f9b9327704b812ee9c1f3abbba3d48"),
    (64, "5f55a35e13f1865e7ffc22dca1275b1a49b0ff56"),
    (65, "d7f8931010f8342a97383e1410a58c47d057708b"),
    (119, "774001b5ad1f46b40c48e1b94cb092c593ead25b"),
    (120, "6ebf3111dc761f11b13f29df0484848785385973"),
    (127, "82d007f6e7e5672722c0717b6bea2ba6f17cd35c"),
    (128, "0a44ac02432f6ead67d86fcb70a4d8ce589e50ec"),
    (129, "461b146c523c63566b8bf8852dc8462116cf2e3f"),
    (255, "d100f6aacf902d21bf48637fa5a1e4b50b224ea9"),
    (256, "c725aa98c74f16132c57accdce18de6f6cfbcf4a"),
    (1000, "7461968a9edb9b418224f1acb305d2feeaa10ba5"),
    (4096, "9412c7e4bcc7e0573b1a84af05c7429b05b658f4"),
];

#[test]
fn fips_vectors() {
    assert_eq!(
        hex(&sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        hex(&sha1(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
}

#[test]
fn million_a() {
    let mut h = Sha1::new();
    let chunk = [b'a'; 1000];
    for _ in 0..1000 {
        h.update(&chunk);
    }
    assert_eq!(hex(&h.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
}

#[test]
fn boundaries() {
    for &(n, want) in VECTORS {
        assert_eq!(hex(&sha1(&pattern(n))), want, "len {n}");
    }
}

#[test]
fn streaming_matches_one_shot() {
    let data = pattern(300);
    let want = sha1(&data);
    for split in 0..data.len() {
        for second in [1usize, 7, 63, 64, 65, 200] {
            let mut h = Sha1::new();
            h.update(&data[..split]);
            let mut at = split;
            while at < data.len() {
                let end = (at + second).min(data.len());
                h.update(&data[at..end]);
                at = end;
            }
            assert_eq!(h.finish(), want, "split {split} then {second}");
        }
    }
}

#[test]
fn empty_updates_are_free() {
    let mut h = Sha1::new();
    h.update(&[]);
    h.update(b"abc");
    h.update(&[]);
    assert_eq!(hex(&h.finish()), "a9993e364706816aba3e25717850c26c9cd0d89d");
}

/// Native and baseline level, group sizes around 8, lengths around boundaries.
#[cfg(feature = "sha1x8")]
#[test]
fn eight_lanes_match_one_at_a_time() {
    use fearless_simd::Level;
    let lens = [0usize, 1, 55, 56, 63, 64, 65, 127, 128, 1000, 4096, 4097];
    let data: Vec<Vec<u8>> = (0..20).map(|i| pattern(lens[i % lens.len()] + i)).collect();
    let equal: Vec<Vec<u8>> = (0..17)
        .map(|i| {
            let mut p = pattern(4096);
            p[i] ^= 0xff;
            p
        })
        .collect();
    for level in [Level::new(), Level::baseline()] {
        for set in [&data, &equal] {
            for n in [1usize, 7, 8, 9, 16, 17] {
                let msgs: Vec<&[u8]> = set.iter().take(n).map(Vec::as_slice).collect();
                let mut out = std::vec![[0u8; 20]; msgs.len()];
                sha1_many(level, &msgs, &mut out);
                for (m, h) in msgs.iter().zip(&out) {
                    assert_eq!(*h, sha1(m), "{} bytes in a group of {n}", m.len());
                }
            }
        }
    }
}

#[cfg(feature = "sha1x8")]
#[test]
fn a_short_output_takes_what_fits() {
    let a = pattern(100);
    let msgs = [&a[..]; 3];
    let mut out = [[0u8; 20]; 2];
    sha1_many(fearless_simd::Level::new(), &msgs, &mut out);
    assert_eq!(out, [sha1(&a); 2]);
}

#[cfg(feature = "sha1x8")]
#[test]
fn eight_states_stream_like_one_shot() {
    use fearless_simd::Level;
    let msgs: Vec<Vec<u8>> = (0..8).map(|i| pattern(3 * 4096 + 100 + i)).collect();
    for level in [Level::new(), Level::baseline()] {
        let mut x = Sha1x8::new();
        for step in 0..3 {
            let at = step * 4096;
            let chunks = core::array::from_fn(|l| &msgs[l][at..at + 4096]);
            assert_eq!(x.update(level, chunks), 4096);
        }
        // A ragged step takes only the whole blocks every lane has.
        let ragged = core::array::from_fn(|l| &msgs[l][3 * 4096..3 * 4096 + 64 + l]);
        assert_eq!(x.update(level, ragged), 64);
        for (l, m) in msgs.iter().enumerate() {
            let mut h = x.lane(l);
            h.update(&m[3 * 4096 + 64..]);
            assert_eq!(h.finish(), sha1(m), "lane {l}");
        }
    }
}
