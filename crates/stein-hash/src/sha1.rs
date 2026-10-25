//! SHA-1, FIPS 180-1.

use crate::block::{BLOCK, Block};

#[cfg(feature = "sha1x8")]
mod lanes;

#[cfg(feature = "sha1x8")]
pub use lanes::{LANES, Sha1x8, sha1_many};

#[rustfmt::skip]
const INIT: [u32; 5] = [
    0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0,
];

#[derive(Clone, Copy)]
pub struct Sha1 {
    state: [u32; 5],
    block: Block,
}

impl Default for Sha1 {
    fn default() -> Sha1 {
        Sha1::new()
    }
}

impl Sha1 {
    pub const fn new() -> Sha1 {
        Sha1 {
            state: INIT,
            block: Block::new(),
        }
    }

    /// Continue after `bytes` of whole blocks were compressed into `state`.
    #[cfg(feature = "sha1x8")]
    fn resume(state: [u32; 5], bytes: u64) -> Sha1 {
        let mut block = Block::new();
        block.total = bytes;
        Sha1 { state, block }
    }

    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        let state = &mut self.state;
        self.block.feed(bytes, |b| compress(state, b));
    }

    pub fn finish(self) -> [u8; 20] {
        let mut state = self.state;
        self.block.finish(|b| compress(&mut state, b));
        let mut out = [0u8; 20];
        for (dst, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
            dst.copy_from_slice(&w.to_be_bytes());
        }
        out
    }
}

/// One shot.
pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(bytes);
    h.finish()
}

/// One round. Five calls with rotated names cycle back, so no register moves.
macro_rules! step {
    ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:expr, $k:expr, $w:expr) => {
        $e = $e
            .wrapping_add($a.rotate_left(5))
            .wrapping_add($f)
            .wrapping_add($k)
            .wrapping_add($w);
        $b = $b.rotate_left(30);
    };
}

/// Schedule word `i` from a rolling 16 word window.
#[inline(always)]
fn word(w: &mut [u32; 16], i: usize) -> u32 {
    if i < 16 {
        return w[i];
    }
    let v = (w[(i + 13) & 15] ^ w[(i + 8) & 15] ^ w[(i + 2) & 15] ^ w[i & 15]).rotate_left(1);
    w[i & 15] = v;
    v
}

#[inline(always)]
fn ch(b: u32, c: u32, d: u32) -> u32 {
    d ^ (b & (c ^ d))
}

#[inline(always)]
fn parity(b: u32, c: u32, d: u32) -> u32 {
    b ^ c ^ d
}

#[inline(always)]
fn maj(b: u32, c: u32, d: u32) -> u32 {
    (b & c) | (d & (b | c))
}

/// Twenty rounds of one function, five at a time.
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

#[inline]
fn compress(state: &mut [u32; 5], block: &[u8; BLOCK]) {
    let mut w = [0u32; 16];
    for (dst, word) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *dst = u32::from_be_bytes(*word);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    rounds!(w, a, b, c, d, e, ch, 0x5a82_7999, 0);
    rounds!(w, a, b, c, d, e, parity, 0x6ed9_eba1, 20);
    rounds!(w, a, b, c, d, e, maj, 0x8f1b_bcdc, 40);
    rounds!(w, a, b, c, d, e, parity, 0xca62_c1d6, 60);
    for (s, v) in state.iter_mut().zip([a, b, c, d, e]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests;
