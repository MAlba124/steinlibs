use super::*;
use crate::paint::{Atlas, Images, paint};
use stein_layout::Rect;
use stein_ui::draw::Shape;

const BG: u32 = 0x0000_0000;
const WHITE: u32 = 0xffff_ffff;
const NO_CLIP: Rect = Rect {
    min: [-1e6, -1e6],
    max: [1e6, 1e6],
};

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        min: [x, y],
        max: [x + w, y + h],
    }
}

fn no_atlas() -> Atlas<'static> {
    Atlas {
        pixels: &[],
        size: [0, 0],
        images: Images::NONE,
    }
}

fn draw(quads: &[Quad], w: usize, h: usize) -> Vec<u32> {
    let mut px = vec![BG; w * h];
    paint(Blend::Scalar, &mut px, w, quads, &no_atlas());
    px
}

/// Blue channel as coverage, for white drawn over black.
fn cov(px: u32) -> f32 {
    (px & 0xff) as f32 / 255.0
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() % 10_000) as f32 / 10_000.0 * (hi - lo)
    }
}

/// Every pixel in the bounds evaluated on its own, no scan, no span fill.
fn brute(bd: Backdrop, px: &mut [u32], w: usize, h: usize, q: &Quad) {
    let Some([x0, y0, x1, y1]) = bounds(q, w, h) else {
        return;
    };
    let outline = Outline::of(q);
    let a = match q.color >> 24 {
        0 => 255.0,
        a => a as f32,
    };
    for y in y0..y1 {
        for x in x0..x1 {
            let d = outline.distance([x as f32 + 0.5, y as f32 + 0.5]);
            put(bd, &mut px[y * w + x], alpha_byte(outline.coverage(d), a), q.color);
        }
    }
}

#[test]
fn the_row_scan_paints_what_every_pixel_on_its_own_paints() {
    let (w, h) = (96usize, 64usize);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..400 {
        let r = rect(rng.f(-10.0, 80.0), rng.f(-10.0, 50.0), rng.f(0.0, 60.0), rng.f(0.0, 40.0));
        let clip = if rng.next().is_multiple_of(3) {
            rect(rng.f(0.0, 60.0), rng.f(0.0, 40.0), rng.f(0.0, 60.0), rng.f(0.0, 40.0))
        } else {
            NO_CLIP
        };
        let radius = [0, 1, 2, 3].map(|_| (rng.next() % 40) as u8);
        let stroke = match rng.next() % 3 {
            0 => (rng.next() % 6) as u8 + 1,
            _ => 0,
        };
        let alpha = [0u32, 0xff, 0x80, (rng.next() % 254 + 1) as u32][(rng.next() % 4) as usize];
        let color = (rng.next() as u32 & 0x00ff_ffff) | alpha << 24;
        let q = Quad::solid(r, clip, color).shaped(Shape { radius, stroke });
        if q.shape.is_none() {
            continue;
        }
        for bd in [Backdrop::Opaque, Backdrop::Clear] {
            let garbage: Vec<u32> = (0..w * h).map(|_| rng.next() as u32).collect();
            let mut scanned = garbage.clone();
            let mut each = garbage;
            fill_shaped(Blend::Scalar, bd, &mut scanned, w, h, &q);
            brute(bd, &mut each, w, h, &q);
            assert_eq!(scanned, each, "case {case} {bd:?}: {q:?}");
        }
    }
}

#[test]
fn simd_and_scalar_paint_the_same_shapes() {
    let quads: Vec<Quad> = (0..12)
        .map(|i| {
            let f = i as f32;
            Quad::solid(rect(3.3 * f, 2.1 * f, 30.0, 20.0), NO_CLIP, 0x80ff_8000 | i)
                .shaped(Shape::round(i as u8 * 2).stroked((i % 3) as u8))
        })
        .collect();
    let (w, h) = (80, 60);
    let mut scalar = vec![0x0012_3456; w * h];
    let mut simd = scalar.clone();
    paint(Blend::Scalar, &mut scalar, w, &quads, &no_atlas());
    paint(Blend::detect(), &mut simd, w, &quads, &no_atlas());
    assert_eq!(scalar, simd);
}

#[test]
fn a_rounded_rect_fills_its_middle_and_softens_its_corners() {
    let (w, h) = (24, 24);
    let q = Quad::solid(rect(2.0, 2.0, 20.0, 20.0), NO_CLIP, WHITE).shaped(Shape::round(6));
    let px = draw(&[q], w, h);
    assert_eq!(px[12 * w + 12], WHITE, "middle");
    assert_eq!(px[2 * w + 12], WHITE, "middle of the top edge");
    assert_eq!(px[2 * w + 2], BG, "the corner pixel is outside the arc");
    assert_eq!(px[w + 12], BG, "nothing above the rect");
    // Partial coverage along the arc.
    let partial = (0..w * h).filter(|&i| cov(px[i]) > 0.0 && cov(px[i]) < 1.0).count();
    assert!(partial > 8, "{partial} partial pixels");
}

#[test]
fn coverage_adds_up_to_the_area() {
    let (w, h) = (64, 48);
    let r = 9.0f32;
    let q = Quad::solid(rect(4.25, 3.5, 50.0, 36.0), NO_CLIP, WHITE).shaped(Shape::round(r as u8));
    let px = draw(&[q], w, h);
    let total: f32 = px.iter().map(|&p| cov(p)).sum();
    let area = 50.0 * 36.0 - (4.0 - std::f32::consts::PI) * r * r;
    assert!((total - area).abs() < area * 0.005, "{total} against {area}");
}

#[test]
fn a_ring_leaves_its_hole_alone_and_covers_the_band() {
    let (w, h) = (40, 40);
    let q = Quad::solid(rect(5.0, 5.0, 30.0, 30.0), NO_CLIP, WHITE).shaped(Shape::round(8).stroked(2));
    let px = draw(&[q], w, h);
    assert_eq!(px[20 * w + 20], BG, "hole");
    assert_eq!(px[5 * w + 20], WHITE, "top band");
    assert_eq!(px[6 * w + 20], WHITE, "top band, second row");
    assert_eq!(px[7 * w + 20], BG, "past the stroke");
    let total: f32 = px.iter().map(|&p| cov(p)).sum();
    let rounded = |side: f32, r: f32| side * side - (4.0 - std::f32::consts::PI) * r * r;
    let band = rounded(30.0, 8.0) - rounded(26.0, 6.0);
    assert!((total - band).abs() < band * 0.02, "{total} against {band}");
}

#[test]
fn a_pill_is_a_circle_on_a_square_and_mirrors_both_ways() {
    let (w, h) = (32, 32);
    let q = Quad::solid(rect(6.0, 6.0, 20.0, 20.0), NO_CLIP, WHITE).shaped(Shape::PILL);
    let px = draw(&[q], w, h);
    for y in 0..h {
        for x in 0..w {
            assert_eq!(px[y * w + x], px[y * w + (w - 1 - x)], "{x},{y} mirrored in x");
            assert_eq!(px[y * w + x], px[(h - 1 - y) * w + x], "{x},{y} mirrored in y");
        }
    }
    let total: f32 = px.iter().map(|&p| cov(p)).sum();
    let area = std::f32::consts::PI * 100.0;
    assert!((total - area).abs() < area * 0.01, "{total} against {area}");
}

#[test]
fn each_corner_takes_its_own_radius() {
    let (w, h) = (24, 24);
    let q = Quad::solid(rect(2.0, 2.0, 20.0, 20.0), NO_CLIP, WHITE).shaped(Shape::corners([10, 0, 0, 0]));
    let px = draw(&[q], w, h);
    assert_eq!(px[2 * w + 2], BG, "top left is round");
    assert_eq!(px[2 * w + 21], WHITE, "top right is square");
    assert_eq!(px[21 * w + 21], WHITE, "bottom right is square");
    assert_eq!(px[21 * w + 2], WHITE, "bottom left is square");
}

#[test]
fn a_shape_stays_inside_its_clip() {
    let (w, h) = (32, 32);
    let clip = rect(10.0, 10.0, 8.0, 8.0);
    let q = Quad::solid(rect(4.0, 4.0, 24.0, 24.0), clip, WHITE).shaped(Shape::round(4));
    let px = draw(&[q], w, h);
    for y in 0..h {
        for x in 0..w {
            let inside = (10..18).contains(&x) && (10..18).contains(&y);
            assert_eq!(px[y * w + x], if inside { WHITE } else { BG }, "{x},{y}");
        }
    }
}

#[test]
fn a_translucent_shape_blends_its_middle_like_a_plain_fill() {
    let (w, h) = (20, 20);
    let color = 0x80ff_0000;
    let shaped = Quad::solid(rect(0.0, 0.0, 20.0, 20.0), NO_CLIP, color).shaped(Shape::round(5));
    let plain = Quad::solid(rect(0.0, 0.0, 20.0, 20.0), NO_CLIP, color);
    let a = draw(&[shaped], w, h);
    let b = draw(&[plain], w, h);
    assert_eq!(a[10 * w + 10], b[10 * w + 10]);
    assert_ne!(a[0], b[0], "the corner is cut");
}

#[test]
fn a_rounded_image_drops_the_texels_outside_its_corners() {
    let texels = [0xffu8; 4 * 4 * 4];
    let atlas = Atlas {
        pixels: &[],
        size: [0, 0],
        images: Images {
            pixels: &texels,
            size: [4, 4],
            version: 1,
        },
    };
    let (w, h) = (24, 24);
    let q = Quad::image(rect(2.0, 2.0, 20.0, 20.0), NO_CLIP, WHITE, [0.0, 0.0, 1.0, 1.0]).shaped(Shape::round(8));
    let mut px = vec![BG; w * h];
    paint(Blend::Scalar, &mut px, w, &[q], &atlas);
    assert_eq!(px[12 * w + 12] & 0x00ff_ffff, 0x00ff_ffff, "middle");
    assert_eq!(px[2 * w + 2], BG, "corner");
    let partial = px.iter().filter(|&&p| cov(p) > 0.0 && cov(p) < 1.0).count();
    assert!(partial > 8, "{partial} partial pixels");
}
