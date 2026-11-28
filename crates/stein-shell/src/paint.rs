//! Software painter. Quads in, pixels out.

use crate::blend::{self, Blend, Span};
use stein_ui::draw::{Damage, Quad};

// SEAM: stein-layout produces the quads, this turns them into pixels. It is the
// only place that knows about the framebuffer.
//
// Each quad carries its own scissor rect, so a scrolled row inside a clipped
// container stops at the container's edge instead of painting over whatever is
// below it. A GPU backend replaces this function and nothing else.
/// What textured quads sample: the glyph atlas, 8-bit coverage, row major,
/// and the image atlas.
pub struct Atlas<'a> {
    pub pixels: &'a [u8],
    pub size: [u32; 2],
    pub images: Images<'a>,
}

/// An RGBA8 image atlas, straight alpha, row major. `version` changes
/// whenever the pixels do, which is what tells a GPU to upload again.
#[derive(Clone, Copy, Debug)]
pub struct Images<'a> {
    pub pixels: &'a [u8],
    pub size: [u32; 2],
    pub version: u64,
}

impl Images<'_> {
    pub const NONE: Images<'static> = Images {
        pixels: &[],
        size: [0, 0],
        version: 0,
    };

    /// Whether the pixels cover the stated size.
    pub fn is_usable(&self) -> bool {
        let n = self.size[0] as usize * self.size[1] as usize * 4;
        n > 0 && self.pixels.len() >= n
    }
}

pub const BG: u32 = 0x0018_1820;

/// What a surface is painted over. `Opaque` is the background colour in
/// XRGB. `Clear` is nothing, in premultiplied ARGB, for a surface with
/// something of its own under it, a video layer say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backdrop {
    #[default]
    Opaque,
    Clear,
}

impl Backdrop {
    /// The pixel a region is cleared to.
    pub fn fill(self) -> u32 {
        match self {
            Backdrop::Opaque => BG,
            Backdrop::Clear => 0,
        }
    }

    /// An opaque fill colour as it lands in the buffer.
    fn solid(self, color: u32) -> u32 {
        match self {
            Backdrop::Opaque => color,
            Backdrop::Clear => color | 0xff00_0000,
        }
    }
}

/// Coverage bytes one blended fill run takes at a time.
const FILL_RUN: usize = 256;

/// A solid quad. The top byte of the colour is alpha: 1 to 254 blends over
/// what is there, the way a glyph at that coverage would. 255 and 0 both
/// fill opaque, 0 because callers that predate alpha write plain XRGB.
fn fill_quad(blend: Blend, bd: Backdrop, pixels: &mut [u32], stride_px: usize, height: usize, q: &Quad) {
    {
        // Intersect the quad with its scissor, then with the buffer. Negative and
        // oversized rects are normal once a real layout engine is driving this.
        let x0 = q.rect.min[0].max(q.clip.min[0]).max(0.0) as usize;
        let y0 = q.rect.min[1].max(q.clip.min[1]).max(0.0) as usize;
        let x1 = (q.rect.max[0].min(q.clip.max[0]).max(0.0) as usize).min(stride_px);
        let y1 = (q.rect.max[1].min(q.clip.max[1]).max(0.0) as usize).min(height);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let a = (q.color >> 24) as u8;
        if a == 0 || a == 0xff {
            let color = bd.solid(q.color);
            for y in y0..y1 {
                pixels[y * stride_px + x0..y * stride_px + x1].fill(color);
            }
            return;
        }
        let cov = [a; FILL_RUN];
        let row_blend = match bd {
            Backdrop::Opaque => blend::blend_row,
            Backdrop::Clear => blend::blend_row_argb,
        };
        for y in y0..y1 {
            let row = &mut pixels[y * stride_px + x0..y * stride_px + x1];
            for run in row.chunks_mut(FILL_RUN) {
                let n = run.len();
                row_blend(blend, run, &cov[..n], q.color);
            }
        }
    }
}

/// Clamp a damage rect to the surface, or return the whole surface for `None`.
/// Grown by a pixel on each side, since geometry is float and rounding must not
/// leave a seam of stale pixels at the edge of the repainted region.
pub fn clamp_region(damage: Option<stein_layout::Rect>, w: u32, h: u32) -> [u32; 4] {
    let Some(d) = damage else {
        return [0, 0, w, h];
    };
    let x0 = (d.min[0].floor() as i64 - 1).clamp(0, w as i64) as u32;
    let y0 = (d.min[1].floor() as i64 - 1).clamp(0, h as i64) as u32;
    let x1 = (d.max[0].ceil() as i64 + 1).clamp(0, w as i64) as u32;
    let y1 = (d.max[1].ceil() as i64 + 1).clamp(0, h as i64) as u32;
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// Clamp each of a frame's damage rects to the surface.
///
/// Returns how many were written. Regions may overlap: the damage list decides
/// what to merge when it is built, and painting a pixel twice is correct.
pub fn clamp_regions(damage: &Damage, w: u32, h: u32, out: &mut [[u32; 4]]) -> usize {
    let mut n = 0;
    for r in damage.rects() {
        let Some(slot) = out.get_mut(n) else {
            break;
        };
        let region = clamp_region(Some(*r), w, h);
        // A rect that clamps to nothing is off the surface entirely, and
        // handing it to the compositor as damage would be a lie.
        if region[2] == 0 || region[3] == 0 {
            continue;
        }
        *slot = region;
        n += 1;
    }
    n
}

/// Rows a band gets at least, below which a region paints on one thread.
const BAND_ROWS: usize = 96;
/// Most threads a region is painted on.
const MAX_BANDS: usize = 8;

/// Clear and repaint every region in turn, a tall one in horizontal bands
/// across threads, each with its own rows of the buffer.
pub fn paint_regions(
    blend: Blend,
    pixels: &mut [u32],
    stride_px: usize,
    quads: &[Quad],
    atlas: &Atlas<'_>,
    regions: &[[u32; 4]],
) {
    paint_regions_on(blend, Backdrop::Opaque, pixels, stride_px, quads, atlas, regions);
}

/// [`paint_regions`] over `bd`.
pub fn paint_regions_on(
    blend: Blend,
    bd: Backdrop,
    pixels: &mut [u32],
    stride_px: usize,
    quads: &[Quad],
    atlas: &Atlas<'_>,
    regions: &[[u32; 4]],
) {
    let p = Painter { blend, bd, stride_px, quads, atlas };
    for region in regions.iter().copied() {
        let bands = bands_for(region[3] as usize);
        if bands <= 1 || stride_px == 0 {
            clear_region(pixels, stride_px, region, bd.fill());
            paint_lifted(&p, pixels, region, 0.0);
            continue;
        }
        paint_banded(&p, pixels, region, bands);
    }
}

/// What every region of one paint shares.
#[derive(Clone, Copy)]
struct Painter<'a> {
    blend: Blend,
    bd: Backdrop,
    stride_px: usize,
    quads: &'a [Quad],
    atlas: &'a Atlas<'a>,
}

/// How many bands a region `rows` tall is painted in.
fn bands_for(rows: usize) -> usize {
    use std::sync::OnceLock;
    static CORES: OnceLock<usize> = OnceLock::new();
    let cores = *CORES
        .get_or_init(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    (rows / BAND_ROWS).min(cores).min(MAX_BANDS)
}

/// `region` split into `bands` runs of rows, painted at once. A band is
/// its own slice of the buffer, and the quads are moved up into it.
fn paint_banded(p: &Painter<'_>, pixels: &mut [u32], region: [u32; 4], bands: usize) {
    let stride_px = p.stride_px;
    let rows = pixels.len() / stride_px;
    let [x, y, w, h] = region.map(|v| v as usize);
    let end = (y + h).min(rows);
    if y >= end {
        return;
    }
    let per = (end - y).div_ceil(bands);
    let (_, mut rest) = pixels.split_at_mut(y * stride_px);
    std::thread::scope(|scope| {
        let mut y0 = y;
        while y0 < end {
            let y1 = (y0 + per).min(end);
            let (band, tail) = std::mem::take(&mut rest).split_at_mut((y1 - y0) * stride_px);
            rest = tail;
            let local = [x as u32, 0, w as u32, (y1 - y0) as u32];
            let lift = y0 as f32;
            scope.spawn(move || {
                clear_region(band, stride_px, local, p.bd.fill());
                paint_lifted(p, band, local, lift);
            });
            y0 = y1;
        }
    });
}

pub fn clear_region(pixels: &mut [u32], stride_px: usize, region: [u32; 4], color: u32) {
    let [x, y, w, h] = region.map(|v| v as usize);
    if w == 0 || h == 0 {
        return;
    }
    for row in y..(y + h).min(pixels.len() / stride_px.max(1)) {
        let start = row * stride_px + x;
        let end = (start + w).min(row * stride_px + stride_px);
        pixels[start..end].fill(color);
    }
}

/// Paint the quads that touch `region`. Quads entirely outside it are skipped
/// without ever reaching the inner loops.
pub fn paint_region(
    blend: Blend,
    pixels: &mut [u32],
    stride_px: usize,
    quads: &[Quad],
    atlas: &Atlas<'_>,
    region: [u32; 4],
) {
    let bd = Backdrop::Opaque;
    let p = Painter { blend, bd, stride_px, quads, atlas };
    paint_lifted(&p, pixels, region, 0.0);
}

/// [`paint_region`] into a buffer whose first row is row `lift` of the
/// frame, every quad moved up by as much. `lift` is whole, so a glyph
/// snaps to the same pixels it would in the full buffer.
fn paint_lifted(p: &Painter<'_>, pixels: &mut [u32], region: [u32; 4], lift: f32) {
    let Painter { blend, bd, stride_px, quads, atlas } = *p;
    if stride_px == 0 {
        return;
    }
    let height = pixels.len() / stride_px;
    let (rx0, ry0) = (region[0] as f32, region[1] as f32);
    let (rx1, ry1) = (rx0 + region[2] as f32, ry0 + region[3] as f32);
    for q in quads {
        if q.rect.max[0] <= rx0
            || q.rect.min[0] >= rx1
            || q.rect.max[1] - lift <= ry0
            || q.rect.min[1] - lift >= ry1
        {
            continue;
        }
        let mut q = *q;
        if lift != 0.0 {
            q.rect.min[1] -= lift;
            q.rect.max[1] -= lift;
            q.clip.min[1] -= lift;
            q.clip.max[1] -= lift;
        }
        // Intersect the scissor with the repaint region, so a quad straddling
        // the edge cannot write outside it.
        q.clip = stein_layout::Rect {
            min: [q.clip.min[0].max(rx0), q.clip.min[1].max(ry0)],
            max: [q.clip.max[0].min(rx1), q.clip.max[1].min(ry1)],
        };
        if q.is_textured() {
            blend_glyph(blend, bd, pixels, stride_px, height, &q, atlas);
        } else if q.is_image() {
            blend_image(bd, pixels, stride_px, height, &q, &atlas.images);
        } else {
            fill_quad(blend, bd, pixels, stride_px, height, &q);
        }
    }
}

/// Paint one frame in full. The render loop goes through `paint_region`;
/// this is the reference a partial repaint is checked against.
pub fn paint(
    blend: Blend,
    pixels: &mut [u32],
    stride_px: usize,
    quads: &[Quad],
    atlas: &Atlas<'_>,
) {
    if stride_px == 0 {
        return;
    }
    let height = pixels.len() / stride_px;
    let region = [0, 0, stride_px as u32, height as u32];
    paint_region(blend, pixels, stride_px, quads, atlas, region);
}

/// Sample the coverage atlas and blend the quad's colour over what is there.
///
/// The glyph is drawn at its rasterized size, so the mapping is one texel per
/// pixel and no filtering is needed. Scaling would need one, which is a reason to
/// rasterize per size rather than scale a cached bitmap.
fn blend_glyph(
    blend: Blend,
    bd: Backdrop,
    pixels: &mut [u32],
    stride_px: usize,
    height: usize,
    q: &Quad,
    atlas: &Atlas<'_>,
) {
    let aw = atlas.size[0] as i32;
    let ah = atlas.size[1] as i32;
    if aw <= 0 || ah <= 0 {
        return;
    }

    // Snap the destination to whole pixels and map one texel to one pixel.
    //
    // The glyph was rasterized at exactly this size, so an integer mapping is
    // exact. Sampling at a fractional offset resamples an already-antialiased
    // coverage map with a nearest-neighbour filter, which is what was eating the
    // antialiasing and smearing stems by up to a pixel.
    // Half up, not half away from zero, so moving a quad by whole pixels,
    // as a band does, moves its glyph by as many.
    let dx = (q.rect.min[0] + 0.5).floor() as i32;
    let dy = (q.rect.min[1] + 0.5).floor() as i32;
    let sx = (q.uv[0] * aw as f32).round() as i32;
    let sy = (q.uv[1] * ah as f32).round() as i32;
    let gw = (q.uv[2] * aw as f32).round() as i32;
    let gh = (q.uv[3] * ah as f32).round() as i32;

    // Clip conservatively: ceil the near edge, floor the far one, so a glyph
    // never bleeds a pixel past its scissor. Then clip to the atlas as well, so
    // the rows below are plain slices with no per texel bounds check.
    let l = dx
        .max(q.clip.min[0].ceil() as i32)
        .max(0)
        .max(dx - sx);
    let t = dy
        .max(q.clip.min[1].ceil() as i32)
        .max(0)
        .max(dy - sy);
    let r = (dx + gw)
        .min(q.clip.max[0].floor() as i32)
        .min(stride_px as i32)
        .min(dx - sx + aw);
    let b = (dy + gh)
        .min(q.clip.max[1].floor() as i32)
        .min(height as i32)
        .min(dy - sy + ah);
    if l >= r || t >= b {
        return;
    }

    let span = Span {
        dst: t as usize * stride_px + l as usize,
        dst_stride: stride_px,
        src: (sy + (t - dy)) as usize * aw as usize + (sx + (l - dx)) as usize,
        src_stride: aw as usize,
        width: (r - l) as usize,
        rows: (b - t) as usize,
    };
    match bd {
        Backdrop::Opaque => blend::blend_span(blend, pixels, atlas.pixels, span, q.color),
        Backdrop::Clear => blend::blend_span_argb(blend, pixels, atlas.pixels, span, q.color),
    }
}

/// The pixels whose centres fall in `[lo, hi)`, the GPU's coverage rule.
fn centres(lo: f32, hi: f32, limit: usize) -> (usize, usize) {
    let a = (lo - 0.5).ceil().max(0.0) as usize;
    let b = ((hi - 0.5).ceil().max(0.0) as usize).min(limit);
    (a, b)
}

/// Which texel of `n` a pixel centre `p` lands on, nearest, as the GPU's
/// nearest clamp sampler picks it.
#[inline]
fn texel(p: f32, origin: f32, extent: f32, uv0: f32, uvw: f32, n: u32) -> usize {
    let u = uv0 + (p - origin) / extent * uvw;
    ((u * n as f32).floor().max(0.0) as usize).min(n as usize - 1)
}

/// Stretch a patch of the image atlas over the quad, nearest sampled,
/// tinted by the colour and blended by the texel's alpha times the
/// colour's. What quad.frag does for an image quad.
fn blend_image(bd: Backdrop, pixels: &mut [u32], stride_px: usize, height: usize, q: &Quad, img: &Images<'_>) {
    if !img.is_usable() {
        return;
    }
    let [iw, ih] = img.size;
    let lo = [q.rect.min[0].max(q.clip.min[0]), q.rect.min[1].max(q.clip.min[1])];
    let hi = [q.rect.max[0].min(q.clip.max[0]), q.rect.max[1].min(q.clip.max[1])];
    let (x0, x1) = centres(lo[0], hi[0], stride_px);
    let (y0, y1) = centres(lo[1], hi[1], height);
    let ext = [
        (q.rect.max[0] - q.rect.min[0]).max(1e-6),
        (q.rect.max[1] - q.rect.min[1]).max(1e-6),
    ];
    let tint = Tint::of(q.color);
    for y in y0..y1 {
        let ty = texel(y as f32 + 0.5, q.rect.min[1], ext[1], q.uv[1], q.uv[3], ih);
        let row = &img.pixels[ty * iw as usize * 4..(ty + 1) * iw as usize * 4];
        let dst = &mut pixels[y * stride_px..y * stride_px + x1.max(x0)];
        for (x, d) in dst.iter_mut().enumerate().skip(x0) {
            let tx = texel(x as f32 + 0.5, q.rect.min[0], ext[0], q.uv[0], q.uv[2], iw);
            *d = tint.over(bd, *d, &row[tx * 4..tx * 4 + 4]);
        }
    }
}

/// A quad colour as a multiplier on RGBA texels.
struct Tint {
    rgb: [u32; 3],
    a: u32,
}

impl Tint {
    fn of(c: u32) -> Self {
        let a = c >> 24;
        Self {
            rgb: [(c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff],
            // 0 and 255 both opaque, as for fills.
            a: if a == 0 { 255 } else { a },
        }
    }

    /// Blend one RGBA texel over `dst`, XRGB or premultiplied ARGB.
    #[inline]
    fn over(&self, bd: Backdrop, dst: u32, t: &[u8]) -> u32 {
        let mul = |c: u8, m: u32| (u32::from(c) * m + 127) / 255;
        let a = mul(t[3], self.a);
        let fg = (mul(t[0], self.rgb[0]) << 16) | (mul(t[1], self.rgb[1]) << 8) | mul(t[2], self.rgb[2]);
        match bd {
            Backdrop::Opaque => blend::blend_px(dst, a as u8, fg),
            Backdrop::Clear => blend::blend_px_argb(dst, a as u8, fg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(w: usize, h: usize) -> Vec<u32> {
        vec![0; w * h]
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> stein_layout::Rect {
        stein_layout::Rect {
            min: [x, y],
            max: [x + w, y + h],
        }
    }

    const NO_CLIP: stein_layout::Rect = stein_layout::Rect {
        min: [f32::MIN, f32::MIN],
        max: [f32::MAX, f32::MAX],
    };

    /// A solid quad with no meaningful scissor, for the geometry tests.
    fn q(x: f32, y: f32, w: f32, h: f32, color: u32) -> Quad {
        Quad::solid(rect(x, y, w, h), NO_CLIP, color)
    }

    /// An atlas the solid-fill tests never sample.
    fn no_atlas() -> Atlas<'static> {
        Atlas {
            pixels: &[],
            size: [0, 0],
            images: Images::NONE,
        }
    }

    #[test]
    fn paint_fills_exact_rect() {
        let (w, h) = (8, 4);
        let mut px = buf(w, h);
        paint(
            Blend::detect(),
            &mut px,
            w,
            &[q(2.0, 1.0, 3.0, 2.0, 0x00ff_0000)],
            &no_atlas(),
        );
        for y in 0..h {
            for x in 0..w {
                let inside = (2..5).contains(&x) && (1..3).contains(&y);
                let expect = if inside { 0x00ff_0000 } else { 0 };
                assert_eq!(px[y * w + x], expect, "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn paint_clips_right_and_bottom() {
        let (w, h) = (4, 4);
        let mut px = buf(w, h);
        paint(
            Blend::detect(),
            &mut px,
            w,
            &[q(2.0, 2.0, 100.0, 100.0, 0x0000_ff00)],
            &no_atlas(),
        );
        assert_eq!(px[2 * w + 3], 0x0000_ff00);
        assert_eq!(px[3 * w + 3], 0x0000_ff00);
        assert_eq!(px[w + 3], 0);
        assert_eq!(px[3 * w + 1], 0);
    }

    #[test]
    fn paint_drops_negative_and_empty_rects() {
        let (w, h) = (4, 4);
        let mut px = buf(w, h);
        paint(
            Blend::detect(),
            &mut px,
            w,
            &[
                q(-10.0, -10.0, 5.0, 5.0, 0x00ff_ffff),
                q(1.0, 1.0, 0.0, 5.0, 0x00ff_ffff),
            ],
            &no_atlas(),
        );
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn paint_later_rects_overwrite() {
        let (w, h) = (4, 4);
        let mut px = buf(w, h);
        paint(
            Blend::detect(),
            &mut px,
            w,
            &[q(0.0, 0.0, 4.0, 4.0, 1), q(0.0, 0.0, 2.0, 2.0, 2)],
            &no_atlas(),
        );
        assert_eq!(px[0], 2);
        assert_eq!(px[3], 1);
    }

    #[test]
    fn a_translucent_fill_blends_and_opaque_ones_do_not() {
        let (w, h) = (300, 2);
        let mut px = vec![0x0000_00ff; w * h];
        let half = q(0.0, 0.0, 300.0, 1.0, 0x80ff_0000);
        let solid = q(0.0, 1.0, 300.0, 1.0, 0xffff_0000);
        paint(Blend::detect(), &mut px, w, &[half, solid], &no_atlas());
        let want = blend::blend_px(0x0000_00ff, 0x80, 0x00ff_0000);
        assert!(px[..w].iter().all(|&p| p == want), "{:#x} vs {want:#x}", px[0]);
        assert_eq!(want >> 16 & 0xff, 0x80);
        assert!(px[w..].iter().all(|&p| p == 0xffff_0000));
        let mut scalar = vec![0x0000_00ff; w * h];
        paint(Blend::Scalar, &mut scalar, w, &[half, solid], &no_atlas());
        assert_eq!(px, scalar);
    }

    /// Over a clear backdrop what is not painted stays transparent, an
    /// opaque fill is opaque whatever its top byte, and a translucent one
    /// leaves premultiplied colour with its alpha.
    #[test]
    fn a_clear_backdrop_paints_premultiplied_argb() {
        let (w, h) = (300, 3);
        let mut px = vec![0x1234_5678; w * h];
        let half = q(0.0, 0.0, 300.0, 1.0, 0x80ff_ffff);
        let solid = q(0.0, 1.0, 300.0, 1.0, 0x0000_ff00);
        let regions = [[0, 0, w as u32, h as u32]];
        for blend in [Blend::Scalar, Blend::detect()] {
            let quads = [half, solid];
            paint_regions_on(blend, Backdrop::Clear, &mut px, w, &quads, &no_atlas(), &regions);
            assert!(px[..w].iter().all(|&p| p == 0x8080_8080), "{:#x}", px[0]);
            assert!(px[w..2 * w].iter().all(|&p| p == 0xff00_ff00), "{:#x}", px[w]);
            assert!(px[2 * w..].iter().all(|&p| p == 0), "{:#x}", px[2 * w]);
        }
    }

    #[test]
    fn paint_zero_stride_is_noop() {
        let mut px = buf(4, 4);
        paint(Blend::detect(), &mut px, 0, &[q(0.0, 0.0, 4.0, 4.0, 9)], &no_atlas());
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn paint_scissors_to_the_clip_rect() {
        let (w, h) = (8, 8);
        let mut px = buf(w, h);
        let quad = Quad::solid(rect(0.0, 0.0, 8.0, 8.0), rect(1.0, 1.0, 2.0, 2.0), 7);
        paint(Blend::detect(), &mut px, w, &[quad], &no_atlas());
        for y in 0..h {
            for x in 0..w {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                assert_eq!(px[y * w + x], if inside { 7 } else { 0 }, "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn paint_drops_a_quad_scissored_entirely_away() {
        let (w, h) = (4, 4);
        let mut px = buf(w, h);
        let quad = Quad::solid(rect(0.0, 0.0, 4.0, 4.0), rect(10.0, 10.0, 10.0, 10.0), 5);
        paint(Blend::detect(), &mut px, w, &[quad], &no_atlas());
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn a_glyph_blends_coverage_rather_than_filling() {
        // 2x2 atlas: opaque, half, empty, empty.
        let atlas_px = [255u8, 128, 0, 0];
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [2, 2],
            images: Images::NONE,
        };
        let (w, h) = (2, 2);
        let mut px = buf(w, h);
        // Full-coverage white over black is white; half coverage is mid grey.
        let quad = Quad::textured(
            rect(0.0, 0.0, 2.0, 2.0),
            NO_CLIP,
            0x00ff_ffff,
            [0.0, 0.0, 1.0, 1.0],
        );
        paint(Blend::detect(), &mut px, w, &[quad], &atlas);
        assert_eq!(
            px[0], 0x00ff_ffff,
            "coverage 255 writes the colour outright"
        );
        assert_eq!(px[1], 0x0080_8080, "coverage 128 blends halfway");
        assert_eq!(px[2], 0, "coverage 0 leaves the background alone");
    }

    #[test]
    fn a_clipped_glyph_samples_the_right_part_of_the_atlas() {
        // Left column opaque, right column empty.
        let atlas_px = [255u8, 0, 255, 0];
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [2, 2],
            images: Images::NONE,
        };
        let (w, h) = (2, 2);
        let mut px = buf(w, h);
        // Scissor away the left column: the surviving pixels must sample the
        // atlas column that lines up with them, not the glyph's origin.
        let quad = Quad::textured(
            rect(0.0, 0.0, 2.0, 2.0),
            rect(1.0, 0.0, 1.0, 2.0),
            0x00ff_ffff,
            [0.0, 0.0, 1.0, 1.0],
        );
        paint(Blend::detect(), &mut px, w, &[quad], &atlas);
        assert_eq!(px[0], 0, "scissored away");
        assert_eq!(px[1], 0, "right atlas column is empty coverage");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn a_full_coverage_texel_writes_the_exact_text_colour() {
        // Rounding in the blend must land on the colour itself at a = 255,
        // otherwise solid stems come out a shade off.
        let atlas_px = [255u8];
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [1, 1],
            images: Images::NONE,
        };
        let mut px = vec![0u32; 1];
        let quad = Quad::textured(
            stein_layout::Rect {
                min: [0.0, 0.0],
                max: [1.0, 1.0],
            },
            stein_layout::Rect {
                min: [0.0, 0.0],
                max: [1.0, 1.0],
            },
            0x00d8_d8e4,
            [0.0, 0.0, 1.0, 1.0],
        );
        paint(Blend::detect(), &mut px, 1, &[quad], &atlas);
        assert_eq!(px[0], 0x00d8_d8e4);
    }
}

#[cfg(test)]
mod blend_paths {
    use super::*;

    /// xorshift64, reproducible without a crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn f(&mut self, lo: f32, hi: f32) -> f32 {
            let u = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
            lo + u * (hi - lo)
        }
    }

    /// A screen of glyph quads at random positions, sizes, clips and
    /// colours over a random atlas, painted once by each path. Geometry,
    /// scissoring, atlas edges and the row tails all go through here.
    #[test]
    fn simd_and_scalar_paint_identical_frames() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let (w, h) = (97usize, 41usize);
        let (aw, ah) = (32u32, 16u32);
        let atlas_px: Vec<u8> = (0..aw * ah).map(|_| rng.next() as u8).collect();
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [aw, ah],
            images: Images::NONE,
        };
        let mut quads = Vec::new();
        for _ in 0..400 {
            let gw = rng.f(1.0, 14.0).round();
            let gh = rng.f(1.0, 12.0).round();
            let u = rng.f(0.0, (aw as f32 - gw).max(0.0)).round();
            let v = rng.f(0.0, (ah as f32 - gh).max(0.0)).round();
            let x = rng.f(-6.0, w as f32 + 2.0);
            let y = rng.f(-6.0, h as f32 + 2.0);
            let clip = stein_layout::Rect {
                min: [rng.f(-2.0, 10.0), rng.f(-2.0, 10.0)],
                max: [rng.f(60.0, 110.0), rng.f(25.0, 50.0)],
            };
            let color = rng.next() as u32 & 0x00ff_ffff;
            quads.push(Quad::textured(
                stein_layout::Rect {
                    min: [x, y],
                    max: [x + gw, y + gh],
                },
                clip,
                color,
                [u / aw as f32, v / ah as f32, gw / aw as f32, gh / ah as f32],
            ));
        }
        let bg: Vec<u32> = (0..w * h).map(|_| rng.next() as u32).collect();
        let mut want = bg.clone();
        let mut got = bg;
        paint(Blend::Scalar, &mut want, w, &quads, &atlas);
        paint(Blend::detect(), &mut got, w, &quads, &atlas);
        for (i, (a, b)) in want.iter().zip(&got).enumerate() {
            assert_eq!(a, b, "pixel {} {}", i % w, i / w);
        }
    }

    /// A tall frame of glyphs, fills and images painted in bands matches
    /// it painted whole, pixel for pixel, whatever straddles a band edge.
    #[test]
    fn bands_paint_what_one_pass_paints() {
        let rect = |x: f32, y: f32, w: f32, h: f32| stein_layout::Rect {
            min: [x, y],
            max: [x + w, y + h],
        };
        let mut rng = Rng(0x5eed_0f_ba4d5);
        let (w, h) = (131usize, 917usize);
        let (aw, ah) = (32u32, 16u32);
        let atlas_px: Vec<u8> = (0..aw * ah).map(|_| rng.next() as u8).collect();
        let (iw, ih) = (8u32, 8u32);
        let image_px: Vec<u8> = (0..iw * ih * 4).map(|_| rng.next() as u8).collect();
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [aw, ah],
            images: Images {
                pixels: &image_px,
                size: [iw, ih],
                version: 1,
            },
        };
        let mut quads = Vec::new();
        for i in 0..900 {
            let (qw, qh) = (rng.f(1.0, 30.0), rng.f(1.0, 40.0));
            let x = rng.f(-6.0, w as f32 + 2.0);
            // Every fourth on a half pixel, the case rounding gets wrong.
            let y = match i % 4 {
                0 => rng.f(0.0, h as f32).floor() + 0.5,
                _ => rng.f(-6.0, h as f32 + 2.0),
            };
            let r = rect(x, y, qw, qh);
            let clip = rect(
                rng.f(-2.0, 20.0),
                rng.f(-2.0, 300.0),
                200.0,
                rng.f(200.0, 900.0),
            );
            let color = rng.next() as u32 & 0x00ff_ffff;
            let quad = match i % 3 {
                0 => {
                    let (gw, gh) = (qw.round().min(14.0), qh.round().min(12.0));
                    let r = rect(x, y, gw, gh);
                    Quad::textured(r, clip, color, [0.0, 0.0, gw / aw as f32, gh / ah as f32])
                }
                1 => Quad::image(r, clip, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]),
                _ => Quad::solid(r, clip, color),
            };
            quads.push(quad);
        }
        let region = [0, 0, w as u32, h as u32];
        let mut want = vec![7u32; w * h];
        clear_region(&mut want, w, region, BG);
        paint_region(Blend::detect(), &mut want, w, &quads, &atlas, region);
        for bands in [2, 3, 7, 8] {
            let mut got = vec![9u32; w * h];
            let (blend, bd, quads, atlas) = (Blend::detect(), Backdrop::Opaque, &quads[..], &atlas);
            paint_banded(&Painter { blend, bd, stride_px: w, quads, atlas }, &mut got, region, bands);
            for (i, (a, b)) in want.iter().zip(&got).enumerate() {
                assert_eq!(a, b, "{bands} bands, pixel {} {}", i % w, i / w);
            }
        }
    }

    /// A glyph whose uv reaches past the atlas must clip to the atlas rather
    /// than index outside it, on both paths, and paint the same thing.
    #[test]
    fn a_glyph_overhanging_the_atlas_is_clipped_to_it() {
        let atlas_px = [255u8; 4];
        let atlas = Atlas {
            pixels: &atlas_px,
            size: [2, 2],
            images: Images::NONE,
        };
        let quad = Quad::textured(
            stein_layout::Rect {
                min: [0.0, 0.0],
                max: [4.0, 4.0],
            },
            stein_layout::Rect {
                min: [0.0, 0.0],
                max: [4.0, 4.0],
            },
            0x00ff_ffff,
            [0.5, 0.5, 2.0, 2.0],
        );
        for blend in [Blend::Scalar, Blend::detect()] {
            let mut px = vec![0u32; 16];
            paint(blend, &mut px, 4, &[quad], &atlas);
            // Only the one atlas texel at (1,1) exists for this mapping.
            let lit: Vec<usize> = (0..16).filter(|&i| px[i] != 0).collect();
            assert_eq!(lit, vec![0]);
            assert_eq!(px[0], 0x00ff_ffff);
        }
    }
}

#[cfg(test)]
mod partial_repaint {
    use super::*;
    use stein_layout::Rect;

    #[test]
    fn no_damage_means_the_whole_surface() {
        // `None` has to mean "repaint everything", not "repaint nothing", or a
        // fresh buffer would be presented holding garbage.
        assert_eq!(clamp_region(None, 800, 600), [0, 0, 800, 600]);
    }

    #[test]
    fn a_region_is_grown_and_clamped_to_the_surface() {
        let d = Rect {
            min: [10.4, 20.6],
            max: [30.2, 40.1],
        };
        let r = clamp_region(Some(d), 800, 600);
        // Grown by a pixel each side, since float geometry rounds and a seam of
        // stale pixels at the edge is exactly what this must not leave.
        assert_eq!(r, [9, 19, 23, 23]);

        // Entirely off-surface clamps to empty rather than wrapping.
        let off = Rect {
            min: [900.0, 700.0],
            max: [950.0, 750.0],
        };
        let r = clamp_region(Some(off), 800, 600);
        assert_eq!(r[2], 0);
        assert_eq!(r[3], 0);
    }
}

#[cfg(test)]
mod multi_region {
    use super::*;
    use stein_layout::Rect;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect {
            min: [x, y],
            max: [x + w, y + h],
        }
    }

    #[test]
    fn each_rect_becomes_its_own_region() {
        let mut d = Damage::new();
        d.add(r(10.0, 10.0, 20.0, 20.0));
        d.add(r(500.0, 400.0, 20.0, 20.0));
        let mut out = [[0u32; 4]; 8];
        assert_eq!(clamp_regions(&d, 800, 600, &mut out), 2);
        // Grown by a pixel each side, as the single-rect path does.
        assert_eq!(out[0], [9, 9, 22, 22]);
        assert_eq!(out[1], [499, 399, 22, 22]);
    }

    #[test]
    fn a_region_off_the_surface_is_dropped_rather_than_reported_empty() {
        // Handing the compositor a zero-sized damage rect is a lie about what
        // changed, and some compositors take it as "nothing".
        let mut d = Damage::new();
        d.add(r(2000.0, 2000.0, 10.0, 10.0));
        d.add(r(5.0, 5.0, 10.0, 10.0));
        let mut out = [[0u32; 4]; 8];
        let n = clamp_regions(&d, 800, 600, &mut out);
        assert_eq!(n, 1, "the off-surface rect was reported");
        assert_eq!(out[0], [4, 4, 12, 12]);
    }

    #[test]
    fn no_damage_is_no_regions() {
        let mut out = [[0u32; 4]; 8];
        assert_eq!(clamp_regions(&Damage::new(), 800, 600, &mut out), 0);
    }

    #[test]
    fn more_rects_than_the_output_holds_are_truncated_rather_than_overflowing() {
        let mut d = Damage::new();
        for i in 0..8 {
            d.add(r(i as f32 * 90.0, 0.0, 10.0, 10.0));
        }
        let mut out = [[0u32; 4]; 3];
        assert_eq!(clamp_regions(&d, 800, 600, &mut out), 3);
    }

    #[test]
    fn painting_several_regions_covers_each_of_them() {
        let mut px = [0u32; 40 * 20];
        let atlas = Atlas {
            pixels: &[],
            size: [0, 0],
            images: Images::NONE,
        };
        let clip = Rect {
            min: [0.0, 0.0],
            max: [40.0, 20.0],
        };
        let quads = [
            Quad::solid(r(0.0, 0.0, 4.0, 4.0), clip, 0xffff_0000),
            Quad::solid(r(30.0, 14.0, 4.0, 4.0), clip, 0xff00_ff00),
        ];
        let regions = [[0, 0, 6, 6], [28, 12, 8, 8]];
        paint_regions(Blend::detect(), &mut px, 40, &quads, &atlas, &regions);
        assert_eq!(px[0], 0xffff_0000, "the first region was not painted");
        assert_eq!(px[15 * 40 + 31], 0xff00_ff00, "the second was not painted");
        // Between them is untouched, which is the point of not using one box.
        assert_eq!(
            px[8 * 40 + 20],
            0,
            "a pixel outside every region was painted"
        );
    }
}

#[cfg(test)]
mod images {
    use super::*;
    use stein_layout::Rect;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect {
            min: [x, y],
            max: [x + w, y + h],
        }
    }

    const ALL: Rect = Rect {
        min: [f32::MIN, f32::MIN],
        max: [f32::MAX, f32::MAX],
    };

    /// 2x2 RGBA: red, green / blue, clear.
    const PX: [u8; 16] = [
        255, 0, 0, 255, 0, 255, 0, 255, //
        0, 0, 255, 255, 9, 9, 9, 0,
    ];

    fn atlas(px: &[u8]) -> Atlas<'_> {
        Atlas {
            pixels: &[],
            size: [0, 0],
            images: Images {
                pixels: px,
                size: [2, 2],
                version: 1,
            },
        }
    }

    fn draw(quads: &[Quad], w: usize, h: usize, px: &[u8]) -> Vec<u32> {
        let mut out = vec![0x0011_1111; w * h];
        paint(Blend::Scalar, &mut out, w, quads, &atlas(px));
        out
    }

    #[test]
    fn one_to_one_copies_texels_and_skips_clear_ones() {
        let q = Quad::image(r(1.0, 1.0, 2.0, 2.0), ALL, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]);
        let out = draw(&[q], 4, 4, &PX);
        assert_eq!(out[4 + 1], 0x00ff_0000);
        assert_eq!(out[4 + 2], 0x0000_ff00);
        assert_eq!(out[8 + 1], 0x0000_00ff);
        assert_eq!(out[8 + 2], 0x0011_1111, "alpha 0 leaves what was there");
        assert_eq!(out[0], 0x0011_1111, "outside the rect");
    }

    #[test]
    fn a_whole_number_scale_repeats_each_texel() {
        let q = Quad::image(r(0.0, 0.0, 4.0, 4.0), ALL, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]);
        let out = draw(&[q], 4, 4, &PX);
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert_eq!(out[y * 4 + x], 0x00ff_0000, "{x},{y}");
        }
        assert_eq!(out[4 * 2 + 2], 0x0011_1111);
        assert_eq!(out[4 * 3], 0x0000_00ff);
    }

    #[test]
    fn a_patch_of_the_atlas_samples_only_that_patch() {
        // The right column only: green over blue.
        let q = Quad::image(r(0.0, 0.0, 1.0, 2.0), ALL, 0xffff_ffff, [0.5, 0.0, 0.5, 1.0]);
        let out = draw(&[q], 1, 2, &PX);
        assert_eq!(out, vec![0x0000_ff00, 0x0011_1111]);
    }

    #[test]
    fn tint_and_colour_alpha_multiply() {
        let full = [255u8, 255, 255, 255].repeat(4);
        let tinted = Quad::image(r(0.0, 0.0, 1.0, 1.0), ALL, 0xff80_4020, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(draw(&[tinted], 1, 1, &full)[0], 0x0080_4020);
        let half = Quad::image(r(0.0, 0.0, 1.0, 1.0), ALL, 0x80ff_ffff, [0.0, 0.0, 1.0, 1.0]);
        let got = draw(&[half], 1, 1, &full)[0];
        assert_eq!(got, blend::blend_px(0x0011_1111, 0x80, 0x00ff_ffff));
    }

    #[test]
    fn the_clip_bounds_it() {
        let q = Quad::image(r(0.0, 0.0, 4.0, 4.0), r(1.0, 1.0, 1.0, 1.0), 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]);
        let out = draw(&[q], 4, 4, &PX);
        let lit: Vec<usize> = (0..16).filter(|&i| out[i] != 0x0011_1111).collect();
        assert_eq!(lit, vec![5]);
    }

    #[test]
    fn a_short_atlas_draws_nothing() {
        let q = Quad::image(r(0.0, 0.0, 2.0, 2.0), ALL, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]);
        let out = draw(&[q], 2, 2, &PX[..8]);
        assert!(out.iter().all(|&p| p == 0x0011_1111));
    }

    #[test]
    fn a_glyph_is_not_an_image() {
        let q = Quad::image(r(0.0, 0.0, 1.0, 1.0), ALL, 0, [0.0, 0.0, 1.0, 1.0]);
        assert!(q.is_image() && !q.is_textured());
        let g = Quad::textured(r(0.0, 0.0, 1.0, 1.0), ALL, 0, [0.0, 0.0, 1.0, 1.0]);
        assert!(g.is_textured() && !g.is_image());
    }
}
