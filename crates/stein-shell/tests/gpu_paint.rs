//! The GPU painter against the software painter, pixel for pixel. Runs on
//! llvmpipe with validation on, or on the device STEIN_GPU_ADAPTER indexes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_gpu::{Adapter, GpuError};
use stein_layout::Rect;
use stein_shell::Blend;
use stein_shell::gpu::{GpuPaint, PaintError};
use stein_shell::paint::{Atlas, Backdrop, Images, paint_regions, paint_regions_on};
use stein_ui::draw::Quad;

fn adapter() -> Adapter {
    match std::env::var("STEIN_GPU_ADAPTER")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(i) => Adapter::Index(i),
        None => Adapter::Software,
    }
}

fn open() -> Option<GpuPaint> {
    match GpuPaint::open(adapter()) {
        Ok(g) => {
            assert!(
                g.gpu().caps().validation,
                "run inside `nix develop` for the validation layer"
            );
            Some(g)
        }
        Err(PaintError::Gpu(GpuError::NoAdapter)) => {
            eprintln!("skipped: no llvmpipe on this machine");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

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

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.next() as usize % from.len()]
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        min: [x, y],
        max: [x + w, y + h],
    }
}

const NO_CLIP: Rect = Rect {
    min: [f32::MIN, f32::MIN],
    max: [f32::MAX, f32::MAX],
};

fn clip(rng: &mut Rng, w: f32, h: f32) -> Rect {
    if rng.next().is_multiple_of(4) {
        return NO_CLIP;
    }
    let (x, y) = (rng.f(-5.0, w * 0.6), rng.f(-5.0, h * 0.6));
    rect(x, y, rng.f(0.0, w), rng.f(0.0, h))
}

fn glyph(rng: &mut Rng, w: f32, h: f32, atlas: [u32; 2]) -> Quad {
    let (aw, ah) = (atlas[0] as f32, atlas[1] as f32);
    let gw = rng.f(1.0, 14.0).round();
    let gh = rng.f(1.0, 12.0).round();
    let u = rng.f(0.0, (aw - gw).max(0.0)).round();
    let v = rng.f(0.0, (ah - gh).max(0.0)).round();
    let (x, y) = (rng.f(-8.0, w + 2.0), rng.f(-8.0, h + 2.0));
    // Glyph colour alpha is ignored by both painters, so any top byte goes.
    let color = rng.next() as u32;
    Quad::textured(
        rect(x, y, gw, gh),
        clip(rng, w, h),
        color,
        [u / aw, v / ah, gw / aw, gh / ah],
    )
}

fn solid(rng: &mut Rng, w: f32, h: f32) -> Quad {
    let (x, y) = (rng.f(-20.0, w), rng.f(-20.0, h));
    let size = [rng.f(0.0, w * 0.5), rng.f(0.0, h * 0.5)];
    // Zero area, opaque old style XRGB, opaque, and translucent.
    let size = if rng.next().is_multiple_of(16) {
        [0.0, size[1]]
    } else {
        size
    };
    let translucent = (rng.next() % 254 + 1) as u32;
    let alpha = rng.pick(&[0u32, 0xff, translucent]);
    let color = (rng.next() as u32 & 0x00ff_ffff) | alpha << 24;
    Quad::solid(rect(x, y, size[0], size[1]), clip(rng, w, h), color)
}

/// A frame of glyphs and fills in random order over a random atlas.
fn scene(seed: u64, w: u32, h: u32, n: usize) -> (Vec<Quad>, Vec<u8>, [u32; 2]) {
    let mut rng = Rng(seed);
    let size = [64u32, 32];
    // Runs of empty and full coverage as well as everything between.
    let atlas: Vec<u8> = (0..size[0] * size[1])
        .map(|_| match rng.next() % 4 {
            0 => 0,
            1 => 255,
            _ => rng.next() as u8,
        })
        .collect();
    let (fw, fh) = (w as f32, h as f32);
    let quads = (0..n)
        .map(|_| match rng.next() % 3 {
            0 => solid(&mut rng, fw, fh),
            _ => glyph(&mut rng, fw, fh, size),
        })
        .collect();
    (quads, atlas, size)
}

/// Per channel differences, the top byte only where it is alpha.
fn diff(bd: Backdrop, a: u32, b: u32) -> u8 {
    let shifts: &[u32] = match bd {
        Backdrop::Opaque => &[0, 8, 16],
        Backdrop::Clear => &[0, 8, 16, 24],
    };
    shifts
        .iter()
        .map(|s| ((a >> s) as u8).abs_diff((b >> s) as u8))
        .max()
        .unwrap_or(0)
}

/// How many real blends land on each pixel: translucent fills, and glyph
/// texels with coverage strictly between 0 and 255. Each blend rounds to 8
/// bits in both painters and may land one level apart, and an earlier
/// difference is carried at most unchanged, so N blends bound the gap at N.
/// Opaque writes are exact in both and would reset it, which this ignores,
/// so the bound is loose there.
fn blend_counts(quads: &[Quad], atlas: &Atlas<'_>, w: u32, h: u32) -> Vec<u8> {
    let mut counts = vec![0u8; (w * h) as usize];
    let mut one = vec![0u32; (w * h) as usize];
    for q in quads {
        let alpha = q.color >> 24;
        let glyph = q.is_textured();
        if !glyph && (alpha == 0 || alpha == 255) {
            continue;
        }
        // White over black shows where the quad blended: a fill leaves
        // anything above zero, a glyph leaves its coverage.
        let mut probe = *q;
        probe.color = alpha << 24 | 0x00ff_ffff;
        one.fill(0);
        stein_shell::paint::paint(Blend::Scalar, &mut one, w as usize, &[probe], atlas);
        tally(&mut counts, &one, glyph);
    }
    counts
}

fn tally(counts: &mut [u8], probe: &[u32], glyph: bool) {
    for (c, &p) in counts.iter_mut().zip(probe) {
        let v = (p & 0xff) as u8;
        let blended = if glyph { v > 0 && v < 255 } else { v > 0 };
        *c = c.saturating_add(u8::from(blended));
    }
}

/// Paint `regions` with both painters over the same garbage and compare,
/// within the blend bound above. Returns how many pixels were not exact.
fn compare(g: &mut GpuPaint, seed: u64, size: [u32; 2], n: usize, regions: &[[u32; 4]]) -> usize {
    compare_on(g, Backdrop::Opaque, seed, size, n, regions)
}

/// [`compare`] over `bd`.
fn compare_on(g: &mut GpuPaint, bd: Backdrop, seed: u64, [w, h]: [u32; 2], n: usize, regions: &[[u32; 4]]) -> usize {
    let (quads, atlas_px, size) = scene(seed, w, h, n);
    let atlas = Atlas {
        pixels: &atlas_px,
        size,
        images: Images::NONE,
    };
    let mut rng = Rng(seed ^ 0x5555);
    let garbage: Vec<u32> = (0..w * h).map(|_| rng.next() as u32).collect();
    let mut soft = garbage.clone();
    let mut hard = garbage;
    paint_regions_on(
        Blend::Scalar,
        bd,
        &mut soft,
        w as usize,
        &quads,
        &atlas,
        regions,
    );
    g.paint_on(bd, &mut hard, w as usize, [w, h], &quads, &atlas, regions)
        .unwrap();
    let counts = blend_counts(&quads, &atlas, w, h);
    let mut inexact = 0;
    for (i, ((s, d), &blends)) in soft.iter().zip(&hard).zip(&counts).enumerate() {
        let off = diff(bd, *s, *d);
        let (x, y) = (i % w as usize, i / w as usize);
        assert!(
            off <= blends.max(1),
            "pixel {x},{y}: software {s:#010x} gpu {d:#010x}, {blends} blends"
        );
        inexact += usize::from(off > 0);
    }
    inexact
}

#[test]
fn full_frames_match_the_software_painter() {
    let Some(mut g) = open() else { return };
    for (seed, size) in [(1, [157, 93]), (2, [64, 64]), (3, [301, 17])] {
        let inexact = compare(&mut g, seed, size, 600, &[[0, 0, size[0], size[1]]]);
        let total = size[0] * size[1];
        eprintln!("{size:?}: {inexact} of {total} pixels not exact");
    }
    assert_eq!(g.gpu().validation_errors(), 0);
}

/// Over a clear backdrop both paint premultiplied ARGB, alpha included.
#[test]
fn a_clear_backdrop_matches_the_software_painter() {
    let Some(mut g) = open() else { return };
    for (seed, size) in [(21, [157, 93]), (22, [64, 64])] {
        compare_on(&mut g, Backdrop::Clear, seed, size, 600, &[[0, 0, size[0], size[1]]]);
    }
    compare_on(&mut g, Backdrop::Clear, 23, [157, 93], 400, &[[10, 10, 30, 20], [0, 88, 157, 5]]);
    assert_eq!(g.gpu().validation_errors(), 0);
}

#[test]
fn partial_regions_leave_the_rest_alone() {
    let Some(mut g) = open() else { return };
    let regions = [[10, 10, 30, 20], [100, 50, 40, 30], [0, 88, 157, 5]];
    compare(&mut g, 7, [157, 93], 400, &regions);
    assert_eq!(g.gpu().validation_errors(), 0);
}

/// More quads than the first ring holds, a bigger atlas, a resize.
#[test]
fn the_painter_grows_with_the_frame() {
    let Some(mut g) = open() else { return };
    compare(&mut g, 11, [200, 120], 20_000, &[[0, 0, 200, 120]]);
    compare(&mut g, 12, [90, 300], 50, &[[0, 0, 90, 300]]);
    assert_eq!(g.gpu().validation_errors(), 0);
}

#[test]
fn a_short_buffer_is_refused() {
    let Some(mut g) = open() else { return };
    let mut px = vec![0u32; 10];
    let atlas = Atlas {
        pixels: &[],
        size: [0, 0],
        images: Images::NONE,
    };
    let r = g.paint(&mut px, 4, [4, 4], &[], &atlas, &[[0, 0, 4, 4]]);
    assert_eq!(r, Err(PaintError::ShortBuffer));
}

/// RGBA texels with fully clear, fully opaque and translucent alpha.
fn texels(seed: u64, n: usize) -> Vec<u8> {
    let mut rng = Rng(seed);
    (0..n)
        .flat_map(|_| {
            let mid = rng.next() as u8;
            let a = rng.pick(&[0u8, 255, 255, mid]);
            [rng.next() as u8, rng.next() as u8, rng.next() as u8, a]
        })
        .collect()
}

/// Image quads at 1:1 and scaled by whole numbers, some tinted, some
/// clipped, painted by both painters. Each pixel blends one texel once, and
/// the tint multiplies in 8 bits on the CPU and in float on the GPU, so two
/// levels apart is the bound.
fn compare_images(g: &mut GpuPaint, pixels: &[u8], version: u64) {
    let (w, h) = (96u32, 64u32);
    let size = [16u32, 8];
    let images = Images {
        pixels,
        size,
        version,
    };
    let atlas = Atlas {
        pixels: &[],
        size: [0, 0],
        images,
    };
    let half = [0.0, 0.0, 0.5, 1.0];
    let quads = [
        Quad::image(rect(2.0, 2.0, 16.0, 8.0), NO_CLIP, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]),
        Quad::image(rect(20.0, 2.0, 32.0, 16.0), NO_CLIP, 0xffff_ffff, [0.0, 0.0, 1.0, 1.0]),
        Quad::image(rect(2.0, 30.0, 24.0, 24.0), NO_CLIP, 0xff80_c040, half),
        Quad::image(rect(40.0, 30.0, 48.0, 24.0), rect(50.0, 35.0, 20.0, 10.0), 0x80ff_ffff, [0.5, 0.0, 0.5, 1.0]),
        Quad::image(rect(60.0, 2.0, 32.0, 16.0), NO_CLIP, 0, [0.0, 0.0, 1.0, 1.0]),
    ];
    let garbage: Vec<u32> = {
        let mut rng = Rng(version);
        (0..w * h).map(|_| rng.next() as u32).collect()
    };
    let (mut soft, mut hard) = (garbage.clone(), garbage);
    let all = [[0, 0, w, h]];
    paint_regions(Blend::Scalar, &mut soft, w as usize, &quads, &atlas, &all);
    g.paint(&mut hard, w as usize, [w, h], &quads, &atlas, &all)
        .unwrap();
    for (i, (s, d)) in soft.iter().zip(&hard).enumerate() {
        let (x, y) = (i % w as usize, i / w as usize);
        assert!(diff(Backdrop::Opaque, *s, *d) <= 2, "pixel {x},{y}: software {s:#010x} gpu {d:#010x}");
    }
}

#[test]
fn images_match_the_software_painter() {
    let Some(mut g) = open() else { return };
    compare_images(&mut g, &texels(21, 16 * 8), 1);
    // New pixels under a new version must reach the GPU, the old texture
    // would fail the comparison.
    compare_images(&mut g, &texels(22, 16 * 8), 2);
    assert_eq!(g.gpu().validation_errors(), 0);
}

/// A scene's own pass fills the canvas red; the quads then draw over it,
/// kept rather than cleared, and a frame where the scene drew nothing is
/// the background again.
#[test]
fn a_scene_pass_shows_under_the_quads() {
    use stein_gpu::{Attachment, Load, MAX_COLOR, Pass, Store};
    use stein_shell::paint::BG;
    let Some(mut g) = open() else { return };
    let (w, h) = (32u32, 16u32);
    let atlas = Atlas {
        pixels: &[],
        size: [0, 0],
        images: Images::NONE,
    };
    let quad = Quad::solid(rect(0.0, 0.0, 8.0, 8.0), rect(0.0, 0.0, w as f32, h as f32), 0xff00_ff00);
    let all = [[0, 0, w, h]];
    for drew in [true, false] {
        let b = g.begin_paint([w, h]).unwrap().unwrap();
        if drew {
            let ctx = g.ctx(&b);
            let mut color = [None; MAX_COLOR];
            color[0] = Some(Attachment {
                target: ctx.target,
                load: Load::Clear([1.0, 0.0, 0.0, 1.0]),
                store: Store::Store,
            });
            ctx.cmds.begin_pass(Pass {
                color,
                depth: None,
                size: ctx.size,
            });
            ctx.cmds.end_pass();
        }
        let mut px = vec![0u32; (w * h) as usize];
        g.finish_paint(b, drew, &mut px, w as usize, &[quad], &atlas, &all)
            .unwrap();
        assert_eq!(px[0] & 0xff_ffff, 0x00_ff00, "the quad is not over the scene");
        let under = if drew { 0xff_0000 } else { BG };
        assert_eq!(px[((h - 1) * w + w - 1) as usize] & 0xff_ffff, under, "drew {drew}");
    }
    assert_eq!(g.gpu().validation_errors(), 0);
}
