//! SHA-1, streamed, one shot and eight at a time, does not allocate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::counting;
use stein_hash::{Sha1, sha1};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

#[test]
fn hashing_does_not_allocate() {
    let data = [0x61u8; 5000];
    counting::arm();
    let before = counting::thread_count();

    let whole = sha1(&data);
    let mut s = Sha1::new();
    for piece in data.chunks(77) {
        s.update(piece);
    }
    assert_eq!(s.finish(), whole);
    lanes(&data, whole);

    assert_eq!(
        counting::thread_count(),
        before,
        "allocations while hashing"
    );
}

#[cfg(feature = "sha1x8")]
fn lanes(data: &[u8], whole: [u8; 20]) {
    let msgs = [data; 9];
    let mut out = [[0u8; 20]; 9];
    stein_hash::sha1_many(fearless_simd::Level::new(), &msgs, &mut out);
    assert_eq!(out, [whole; 9]);
}

#[cfg(not(feature = "sha1x8"))]
fn lanes(_: &[u8], _: [u8; 20]) {}
