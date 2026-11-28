//! Coverage blending, the inner loop of glyph painting.
//!
//! A glyph is a rectangle of 8-bit coverage in the atlas and a rectangle of
//! XRGB pixels in the framebuffer, row for row. Per channel the blend is
//!
//! ```text
//! out = (dst * (255 - a) + fg * a + 127) / 255
//! ```
//!
//! which rounds half up, lands exactly on `fg` at `a = 255` and exactly on
//! `dst` at `a = 0`. The unsigned form matters: `dst + (fg - dst) * a / 255`
//! rounds toward zero on a negative delta and so is one level off from the
//! positive direction, and it cannot be done in unsigned lanes at all. The
//! X byte is a channel like the others with `fg = 0`, so it is left alone at
//! zero coverage and cleared at full, and the compositor ignores it anyway.
//!
//! The division is the shift identity `(t + (t >> 8)) >> 8` with `t = v + 128`,
//! exact for every `v` the blend can produce (checked exhaustively in the
//! tests). That keeps everything in 16-bit lanes.
//!
//! # Lane layout
//!
//! Eight pixels are one `u32x8`, reinterpreted as `u16x16`. Each 16-bit lane
//! holds two channels, so `lane & 0xff` is the even channels of the whole
//! vector and `lane >> 8` the odd ones, and the blend runs twice, once per
//! half, against a coverage vector that repeats each byte into both lanes of
//! its pixel. No widening, no narrowing, no cross-lane permute: the pixel
//! goes back the way it came with a shift and an or.
//!
//! # Rows shorter than a step
//!
//! Glyph rows are six to twelve pixels, so most rows have a remainder. The
//! remainder is not padded through memory, which would cost a store to load
//! forwarding stall or a `memcpy` call per row. Instead the last step is
//! widened to eight pixels of the framebuffer past the row's end with zero
//! coverage there, and zero coverage is an exact identity, so those pixels
//! are written back unchanged. Only a row whose widened window would run off
//! the end of the buffer falls back to the scalar reference.

use fearless_simd::{Bytes, Level, Simd, SimdBase, dispatch, u8x16, u16x16, u32x8};

/// Pixels per kernel step.
pub const LANES: usize = 8;

/// Painted colours are XRGB with the X byte clear. Whatever a caller put in
/// the top byte of `color` is ignored.
const RGB_MASK: u32 = 0x00ff_ffff;

/// Which blend runs. `Scalar` is the reference; `Simd` carries the CPU level
/// detected once at startup so the per glyph dispatch is a match, not a probe.
#[derive(Clone, Copy, Debug)]
pub enum Blend {
    Scalar,
    Simd(Level),
}

impl Blend {
    /// Probe the CPU. Do this once per process and pass the result around,
    /// feature detection is not free.
    pub fn detect() -> Blend {
        Blend::Simd(Level::new())
    }
}

/// One glyph's worth of rows, described by strides rather than slices so the
/// same shape addresses the framebuffer and the atlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Index of the first destination pixel.
    pub dst: usize,
    /// Destination pixels per row.
    pub dst_stride: usize,
    /// Index of the first coverage byte.
    pub src: usize,
    /// Coverage bytes per row.
    pub src_stride: usize,
    pub width: usize,
    pub rows: usize,
}

/// Blend `span` of `cov` over `pixels` in `color`.
///
/// A row whose destination or coverage falls past the end of its slice is
/// skipped, along with every row after it. Both slices are kernel or packer
/// output with known dimensions, so that never fires in practice, but a
/// painter that panics on a short atlas is worse than one that draws nothing.
pub fn blend_span(blend: Blend, pixels: &mut [u32], cov: &[u8], span: Span, color: u32) {
    blend_span_top(blend, pixels, cov, span, color & RGB_MASK);
}

/// [`blend_span`] onto premultiplied ARGB: the top byte is alpha and the
/// colour's is taken as full, so coverage `a` over alpha `d` leaves
/// `a + d * (255 - a) / 255`, the over operator, and the colour channels
/// come out premultiplied.
pub fn blend_span_argb(blend: Blend, pixels: &mut [u32], cov: &[u8], span: Span, color: u32) {
    blend_span_top(blend, pixels, cov, span, color | !RGB_MASK);
}

/// The kernel with the colour's top byte as given.
fn blend_span_top(blend: Blend, pixels: &mut [u32], cov: &[u8], span: Span, color: u32) {
    if span.width == 0 || span.rows == 0 {
        return;
    }
    match blend {
        Blend::Scalar => span_scalar(pixels, cov, span, color),
        Blend::Simd(level) => {
            dispatch!(level, simd => span_simd(simd, pixels, cov, span, color));
        }
    }
}

/// Blend one row. `dst` and `cov` must be the same length; the shorter wins.
pub fn blend_row(blend: Blend, dst: &mut [u32], cov: &[u8], color: u32) {
    blend_span(blend, dst, cov, row_span(dst, cov), color);
}

/// [`blend_row`] onto premultiplied ARGB, see [`blend_span_argb`].
pub fn blend_row_argb(blend: Blend, dst: &mut [u32], cov: &[u8], color: u32) {
    blend_span_argb(blend, dst, cov, row_span(dst, cov), color);
}

fn row_span(dst: &[u32], cov: &[u8]) -> Span {
    let width = dst.len().min(cov.len());
    Span {
        dst: 0,
        dst_stride: width,
        src: 0,
        src_stride: width,
        width,
        rows: 1,
    }
}

/// The row addresses of a span, stopping at the first row either slice
/// cannot hold.
#[inline(always)]
fn rows(span: Span, dst_len: usize, src_len: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..span.rows)
        .map(move |r| (span.dst + r * span.dst_stride, span.src + r * span.src_stride))
        .take_while(move |&(d, s)| d + span.width <= dst_len && s + span.width <= src_len)
}

// Scalar reference.

#[inline]
fn channel(dst: u32, fg: u32, a: u32, shift: u32) -> u32 {
    let d = (dst >> shift) & 0xff;
    let f = (fg >> shift) & 0xff;
    (d * (255 - a) + f * a + 127) / 255
}

/// One pixel, the definition the vector path has to match bit for bit.
#[inline]
pub fn blend_px(dst: u32, cov: u8, fg: u32) -> u32 {
    blend_px_top(dst, cov, fg & RGB_MASK)
}

/// [`blend_px`] onto premultiplied ARGB, see [`blend_span_argb`].
#[inline]
pub fn blend_px_argb(dst: u32, cov: u8, fg: u32) -> u32 {
    blend_px_top(dst, cov, fg | !RGB_MASK)
}

#[inline]
fn blend_px_top(dst: u32, cov: u8, fg: u32) -> u32 {
    let a = cov as u32;
    (channel(dst, fg, a, 24) << 24)
        | (channel(dst, fg, a, 16) << 16)
        | (channel(dst, fg, a, 8) << 8)
        | channel(dst, fg, a, 0)
}

#[inline]
fn row_scalar(dst: &mut [u32], cov: &[u8], color: u32) {
    for (p, &a) in dst.iter_mut().zip(cov) {
        *p = blend_px_top(*p, a, color);
    }
}

fn span_scalar(pixels: &mut [u32], cov: &[u8], span: Span, color: u32) {
    for (d, s) in rows(span, pixels.len(), cov.len()) {
        row_scalar(&mut pixels[d..d + span.width], &cov[s..s + span.width], color);
    }
}

// Vector path.

/// The exact `(v + 127) / 255` for `v <= 255 * 255`, in 16-bit lanes.
#[inline(always)]
fn div255<S: Simd>(v: u16x16<S>) -> u16x16<S> {
    let t = v + 128u16;
    (t + (t >> 8)) >> 8
}

/// The colour split the same way the pixels are: even channels in one
/// vector, odd in the other, repeated for eight pixels. Built by the same
/// reinterpret the pixels go through so lane order needs no endian
/// assumption.
#[inline(always)]
fn fg_lanes<S: Simd>(simd: S, color: u32) -> (u16x16<S>, u16x16<S>) {
    let px: u16x16<S> = simd.splat_u32x8(color).bitcast();
    (px & 0x00ff, px >> 8)
}

/// Eight coverage bytes, little-endian packed, each repeated into both
/// 16-bit lanes of its pixel.
#[inline(always)]
fn cov_lanes<S: Simd>(simd: S, cov: u64) -> u16x16<S> {
    let c = u8x16::load_array(simd, u128::from(cov).to_le_bytes());
    let (lo, hi) = simd.widen_u8x16(simd.zip_low_u8x16(c, c));
    simd.combine_u16x8(lo, hi)
}

/// Eight pixels against eight coverage bytes.
#[inline(always)]
fn blend8<S: Simd>(
    simd: S,
    dst: &mut [u32; LANES],
    cov: u64,
    fg_even: u16x16<S>,
    fg_odd: u16x16<S>,
) {
    let px: u16x16<S> = u32x8::load_array_ref(simd, dst).bitcast();
    let a = cov_lanes(simd, cov);
    let inv = simd.splat_u16x16(255) - a;
    let even = div255((px & 0x00ff) * inv + fg_even * a);
    let odd = div255((px >> 8) * inv + fg_odd * a);
    let out: u32x8<S> = (even | (odd << 8)).bitcast();
    out.store_array(dst);
}

/// `n` coverage bytes from `at`, packed little-endian, zero above. Reads a
/// whole step when the slice has one, which is every row but the atlas's
/// last, and masks the neighbour's bytes off; otherwise packs by hand.
#[inline(always)]
fn tail_cov(cov: &[u8], at: usize, n: usize) -> u64 {
    match cov[at..].first_chunk::<LANES>() {
        Some(c) => u64::from_le_bytes(*c) & ((1u64 << (8 * n as u32)) - 1),
        None => cov[at..at + n]
            .iter()
            .enumerate()
            .fold(0u64, |acc, (i, &b)| acc | (u64::from(b) << (8 * i as u32))),
    }
}

#[inline(always)]
fn span_simd<S: Simd>(simd: S, pixels: &mut [u32], cov: &[u8], span: Span, color: u32) {
    let (fg_even, fg_odd) = fg_lanes(simd, color);
    let full = span.width / LANES * LANES;
    let rest = span.width - full;
    for (d, s) in rows(span, pixels.len(), cov.len()) {
        let (dst_full, _) = pixels[d..d + full].as_chunks_mut::<LANES>();
        let (cov_full, _) = cov[s..s + full].as_chunks::<LANES>();
        for (px, c) in dst_full.iter_mut().zip(cov_full) {
            blend8(simd, px, u64::from_le_bytes(*c), fg_even, fg_odd);
        }
        if rest == 0 {
            continue;
        }
        let (td, ts) = (d + full, s + full);
        match pixels[td..].first_chunk_mut::<LANES>() {
            Some(window) => blend8(simd, window, tail_cov(cov, ts, rest), fg_even, fg_odd),
            None => row_scalar(&mut pixels[td..td + rest], &cov[ts..ts + rest], color),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64, so the property tests are reproducible without a crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn u32(&mut self) -> u32 {
            self.next() as u32
        }

        fn u8(&mut self) -> u8 {
            self.next() as u8
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn simd() -> Blend {
        Blend::detect()
    }

    #[test]
    fn the_shift_division_is_exact_over_the_blend_range() {
        // Every v the lerp can produce is at most 255 * 255. One past that
        // range is not needed and the identity is not claimed there.
        for v in 0..=255u32 * 255 {
            let t = v + 128;
            assert_eq!((t + (t >> 8)) >> 8, (v + 127) / 255, "v = {v}");
        }
    }

    #[test]
    fn full_coverage_is_the_colour_and_none_is_the_background() {
        for &bg in &[0u32, 0x0018_1820, 0xffff_ffff, 0x0012_3456] {
            for &fg in &[0u32, 0x00d8_d8e4, 0x00ff_00ff, 0x0080_0001] {
                assert_eq!(blend_px(bg, 255, fg), fg);
                assert_eq!(blend_px(bg, 0, fg), bg);
            }
        }
    }

    #[test]
    fn half_coverage_of_white_over_black_is_mid_grey() {
        assert_eq!(blend_px(0, 128, 0x00ff_ffff), 0x0080_8080);
    }

    #[test]
    fn the_x_byte_of_the_colour_is_ignored() {
        assert_eq!(blend_px(0, 255, 0xffd8_d8e4), 0x00d8_d8e4);
    }

    #[test]
    fn the_blend_is_symmetric_in_direction() {
        // Lightening and darkening by the same amount at the same coverage
        // must move the channel by the same number of levels.
        for a in 0..=255u8 {
            let up = blend_px(0x0000_0040, a, 0x0000_00c0) & 0xff;
            let down = blend_px(0x0000_00c0, a, 0x0000_0040) & 0xff;
            assert_eq!(up - 0x40, 0xc0 - down, "a = {a}");
        }
    }

    #[test]
    fn simd_matches_scalar_for_every_coverage_value() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for a in 0..=255u8 {
            for _ in 0..8 {
                let color = rng.u32();
                let bg: Vec<u32> = (0..LANES).map(|_| rng.u32()).collect();
                let cov = [a; LANES];
                let mut want = bg.clone();
                let mut got = bg.clone();
                blend_row(Blend::Scalar, &mut want, &cov, color);
                blend_row(simd(), &mut got, &cov, color);
                assert_eq!(got, want, "a = {a} color = {color:08x}");
            }
        }
    }

    #[test]
    fn simd_matches_scalar_on_random_rows_of_every_width() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for width in 0..40 {
            for _ in 0..64 {
                let color = rng.u32();
                // Slack after the row, so the widened tail has somewhere to
                // go and something to leave alone.
                let slack = rng.below(12);
                let bg: Vec<u32> = (0..width + slack).map(|_| rng.u32()).collect();
                let cov: Vec<u8> = (0..width + slack).map(|_| rng.u8()).collect();
                let mut want = bg.clone();
                let mut got = bg.clone();
                blend_row(Blend::Scalar, &mut want[..width], &cov[..width], color);
                blend_row(simd(), &mut got[..width], &cov[..width], color);
                assert_eq!(got, want, "width = {width} slack = {slack}");
                assert_eq!(&got[width..], &bg[width..], "slack was touched");
            }
        }
    }

    #[test]
    fn a_row_ending_at_the_buffer_end_still_blends() {
        // No room for a widened window past the row, so the tail falls back
        // to the reference. Every width below a step, at the very end.
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for width in 1..LANES {
            let bg: Vec<u32> = (0..width).map(|_| rng.u32()).collect();
            let cov: Vec<u8> = (0..width).map(|_| rng.u8()).collect();
            let mut want = bg.clone();
            let mut got = bg.clone();
            blend_row(Blend::Scalar, &mut want, &cov, 0x00d8_d8e4);
            blend_row(simd(), &mut got, &cov, 0x00d8_d8e4);
            assert_eq!(got, want, "width = {width}");
        }
    }

    #[test]
    fn a_span_only_touches_its_own_rows_and_columns() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let (w, h) = (23, 9);
        let (aw, ah) = (17, 6);
        for _ in 0..200 {
            let bg: Vec<u32> = (0..w * h).map(|_| rng.u32()).collect();
            let cov: Vec<u8> = (0..aw * ah).map(|_| rng.u8()).collect();
            let width = rng.below(aw) + 1;
            let rows = rng.below(ah) + 1;
            let span = Span {
                dst: rng.below(h - rows + 1) * w + rng.below(w - width + 1),
                dst_stride: w,
                src: rng.below(ah - rows + 1) * aw + rng.below(aw - width + 1),
                src_stride: aw,
                width,
                rows,
            };
            let color = rng.u32();
            let mut want = bg.clone();
            let mut got = bg.clone();
            blend_span(Blend::Scalar, &mut want, &cov, span, color);
            blend_span(simd(), &mut got, &cov, span, color);
            assert_eq!(got, want, "{span:?}");
            // And the reference itself left everything outside the span alone.
            for r in 0..h {
                for c in 0..w {
                    let i = r * w + c;
                    let inside = i >= span.dst
                        && (i - span.dst) / w < rows
                        && (i - span.dst) % w < width;
                    if !inside {
                        assert_eq!(want[i], bg[i], "{span:?} touched {r},{c}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_span_past_either_slice_draws_the_rows_that_fit() {
        let mut px = [0u32; 8];
        let cov = [255u8; 6];
        // Two rows of three fit the pixels but the second overruns coverage.
        let span = Span {
            dst: 0,
            dst_stride: 4,
            src: 0,
            src_stride: 4,
            width: 3,
            rows: 2,
        };
        for blend in [Blend::Scalar, simd()] {
            px.fill(0);
            blend_span(blend, &mut px, &cov, span, 0x00ff_ffff);
            assert_eq!(px, [0x00ff_ffff, 0x00ff_ffff, 0x00ff_ffff, 0, 0, 0, 0, 0]);
        }
    }

    /// Over a clear pixel the result is the colour premultiplied by the
    /// coverage, and alpha is the coverage.
    #[test]
    fn argb_over_clear_is_premultiplied() {
        assert_eq!(blend_px_argb(0, 128, 0x00ff_ffff), 0x8080_8080);
        assert_eq!(blend_px_argb(0, 255, 0x0012_3456), 0xff12_3456);
        assert_eq!(blend_px_argb(0, 0, 0x00ff_ffff), 0);
        // Whatever the colour's top byte says, it paints opaque.
        assert_eq!(blend_px_argb(0, 255, 0), 0xff00_0000);
    }

    /// Alpha follows the over operator, so painting over something opaque
    /// stays opaque and two half covers make three quarters.
    #[test]
    fn argb_alpha_is_the_over_operator() {
        assert_eq!(blend_px_argb(0xff00_0000, 77, 0x00ff_ffff) >> 24, 0xff);
        let half = blend_px_argb(0, 128, 0);
        assert_eq!(blend_px_argb(half, 128, 0) >> 24, 192);
    }

    #[test]
    fn argb_simd_matches_scalar_on_random_rows() {
        let mut rng = Rng(0x5eed_a1fa);
        for width in 1..40 {
            let dst: Vec<u32> = (0..width).map(|_| rng.u32()).collect();
            let cov: Vec<u8> = (0..width).map(|_| rng.u8()).collect();
            let color = rng.u32();
            let mut scalar = dst.clone();
            let mut vector = dst.clone();
            blend_row_argb(Blend::Scalar, &mut scalar, &cov, color);
            blend_row_argb(simd(), &mut vector, &cov, color);
            assert_eq!(scalar, vector, "width {width}");
            let want: Vec<u32> = dst
                .iter()
                .zip(&cov)
                .map(|(&d, &c)| blend_px_argb(d, c, color))
                .collect();
            assert_eq!(scalar, want, "width {width}");
        }
    }
}
