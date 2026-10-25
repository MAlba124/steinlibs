//! Eight SHA-1 messages at once, one per 32-bit lane.
//!
//! Lanes run the common whole blocks. Tails are finished by the scalar hasher.

use fearless_simd::{Level, Simd, SimdBase, dispatch, u32x8};

use super::{INIT, Sha1, sha1};
use crate::block::BLOCK;

/// Messages per vector.
pub const LANES: usize = 8;

/// `out[i] = sha1(msgs[i])`. Falls back to scalar without vector units.
pub fn sha1_many(level: Level, msgs: &[&[u8]], out: &mut [[u8; 20]]) {
    for (group, dst) in msgs.chunks(LANES).zip(out.chunks_mut(LANES)) {
        if level.is_fallback() {
            for (m, d) in group.iter().zip(dst.iter_mut()) {
                *d = sha1(m);
            }
            continue;
        }
        // Spare lanes repeat the first message.
        let mut lanes = [group[0]; LANES];
        lanes[..group.len()].copy_from_slice(group);
        let mut x = Sha1x8::new();
        let done = x.update(level, lanes);
        for (l, d) in dst.iter_mut().enumerate().take(group.len()) {
            let mut h = x.lane(l);
            h.update(&lanes[l][done..]);
            *d = h.finish();
        }
    }
}

/// Eight SHA-1 states advanced together.
#[derive(Clone, Copy)]
pub struct Sha1x8 {
    state: [[u32; 5]; LANES],
    /// Bytes taken per lane, equal for all.
    bytes: u64,
}

impl Default for Sha1x8 {
    fn default() -> Sha1x8 {
        Sha1x8::new()
    }
}

impl Sha1x8 {
    pub const fn new() -> Sha1x8 {
        Sha1x8 {
            state: [INIT; LANES],
            bytes: 0,
        }
    }

    /// Take the whole blocks all eight chunks have. Returns bytes taken per lane.
    pub fn update(&mut self, level: Level, chunks: [&[u8]; LANES]) -> usize {
        let blocks = chunks.iter().map(|m| m.len() / BLOCK).min().unwrap_or(0);
        if blocks == 0 {
            return 0;
        }
        if level.is_fallback() {
            for (s, m) in self.state.iter_mut().zip(chunks) {
                let mut h = Sha1::resume(*s, self.bytes);
                h.update(&m[..blocks * BLOCK]);
                *s = h.state;
            }
        } else {
            let state = &mut self.state;
            dispatch!(level, simd => blocks8(simd, &chunks, blocks, state));
        }
        self.bytes += (blocks * BLOCK) as u64;
        blocks * BLOCK
    }

    /// Lane `l` as a scalar hasher, for its tail.
    pub fn lane(&self, l: usize) -> Sha1 {
        Sha1::resume(self.state[l % LANES], self.bytes)
    }
}

#[inline(always)]
fn rotl<S: Simd>(x: u32x8<S>, n: u32) -> u32x8<S> {
    (x << n) | (x >> (32 - n))
}

#[inline(always)]
fn ch<S: Simd>(b: u32x8<S>, c: u32x8<S>, d: u32x8<S>) -> u32x8<S> {
    d ^ (b & (c ^ d))
}

#[inline(always)]
fn parity<S: Simd>(b: u32x8<S>, c: u32x8<S>, d: u32x8<S>) -> u32x8<S> {
    b ^ c ^ d
}

#[inline(always)]
fn maj<S: Simd>(b: u32x8<S>, c: u32x8<S>, d: u32x8<S>) -> u32x8<S> {
    (b & c) | (d & (b | c))
}

/// Same as the scalar `word`.
#[inline(always)]
fn word<S: Simd>(w: &mut [u32x8<S>; 16], i: usize) -> u32x8<S> {
    if i < 16 {
        return w[i];
    }
    let v = rotl(
        w[(i + 13) & 15] ^ w[(i + 8) & 15] ^ w[(i + 2) & 15] ^ w[i & 15],
        1,
    );
    w[i & 15] = v;
    v
}

/// Same as the scalar `step`.
macro_rules! step {
    ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:expr, $k:expr, $w:expr) => {
        $e = $e + rotl($a, 5) + $f + $w + $k;
        $b = rotl($b, 30);
    };
}

macro_rules! rounds {
    ($w:ident, $a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident, $k:expr, $from:expr) => {
        let mut i = $from;
        while i < $from + 20 {
            step!($a, $b, $c, $d, $e, $f($b, $c, $d), $k, word(&mut $w, i));
            step!($e, $a, $b, $c, $d, $f($a, $b, $c), $k, word(&mut $w, i + 1));
            step!($d, $e, $a, $b, $c, $f($e, $a, $b), $k, word(&mut $w, i + 2));
            step!($c, $d, $e, $a, $b, $f($d, $e, $a), $k, word(&mut $w, i + 3));
            step!($b, $c, $d, $e, $a, $f($c, $d, $e), $k, word(&mut $w, i + 4));
            i += 5;
        }
    };
}

/// Compress the first `blocks` blocks of every lane into `state`.
#[inline(always)]
fn blocks8<S: Simd>(simd: S, msgs: &[&[u8]; LANES], blocks: usize, state: &mut [[u32; 5]; LANES]) {
    let mut h: [u32x8<S>; 5] =
        core::array::from_fn(|i| u32x8::load_array(simd, core::array::from_fn(|l| state[l][i])));
    for b in 0..blocks {
        let at = b * BLOCK;
        let words: [&[[u8; 4]]; LANES] =
            core::array::from_fn(|l| msgs[l][at..at + BLOCK].as_chunks::<4>().0);
        // Transpose by scalar loads, cheap next to eighty vector rounds.
        let mut w: [u32x8<S>; 16] = core::array::from_fn(|t| {
            u32x8::load_array(
                simd,
                core::array::from_fn(|l| u32::from_be_bytes(words[l][t])),
            )
        });
        let [mut a, mut b, mut c, mut d, mut e] = h;
        rounds!(w, a, b, c, d, e, ch, 0x5a82_7999, 0);
        rounds!(w, a, b, c, d, e, parity, 0x6ed9_eba1, 20);
        rounds!(w, a, b, c, d, e, maj, 0x8f1b_bcdc, 40);
        rounds!(w, a, b, c, d, e, parity, 0xca62_c1d6, 60);
        for (s, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *s += v;
        }
    }
    for (i, v) in h.iter().enumerate() {
        let mut lanes = [0u32; LANES];
        v.store_array(&mut lanes);
        for (s, x) in state.iter_mut().zip(lanes) {
            s[i] = x;
        }
    }
}
