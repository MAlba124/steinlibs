//! Rounded and stroked quads in the software painter.
//!
//! ```text
//! Quad with a Shape ──► Outline ──► distance at each pixel centre ──► coverage ──► blend
//! ```
//!
//! The coverage of a pixel is how far its centre is inside the outline,
//! clamped to one pixel: `clamp(0.5 - d, 0, 1)`. A ring is the outline less
//! the outline moved in by the stroke. shaders/quad.frag in stein-gpu does
//! the same arithmetic in the same order, so both painters agree up to the
//! rounding of a blend.
//!
//! A filled shape is convex, so along a row its pixels go outside, partial,
//! full, partial, outside. Each row is scanned in from both ends until a full
//! pixel, and the run between is one span fill. A ring's hole is convex too,
//! so the same scan stops at the hole and skips it. Only edge pixels pay for
//! a distance.

use crate::blend::{self, Blend};
use crate::paint::{Backdrop, fill_span};
use stein_ui::draw::Quad;

/// A shaped quad's outline, ready to evaluate at pixel centres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Outline {
    centre: [f32; 2],
    half: [f32; 2],
    /// Top left, top right, bottom right, bottom left, clamped to half the
    /// shorter side.
    radius: [f32; 4],
    stroke: f32,
}

impl Outline {
    pub(crate) fn of(q: &Quad) -> Outline {
        let half = [
            (q.rect.max[0] - q.rect.min[0]).max(0.0) * 0.5,
            (q.rect.max[1] - q.rect.min[1]).max(0.0) * 0.5,
        ];
        let limit = half[0].min(half[1]);
        Outline {
            centre: [
                (q.rect.min[0] + q.rect.max[0]) * 0.5,
                (q.rect.min[1] + q.rect.max[1]) * 0.5,
            ],
            half,
            radius: q.shape.radius.map(|r| f32::from(r).min(limit)),
            stroke: f32::from(q.shape.stroke),
        }
    }

    /// Signed distance from the outline, negative inside.
    #[inline]
    pub(crate) fn distance(&self, p: [f32; 2]) -> f32 {
        let dx = p[0] - self.centre[0];
        let dy = p[1] - self.centre[1];
        let r = match (dx < 0.0, dy < 0.0) {
            (true, true) => self.radius[0],
            (false, true) => self.radius[1],
            (false, false) => self.radius[2],
            (true, false) => self.radius[3],
        };
        let ax = dx.abs() - self.half[0] + r;
        let ay = dy.abs() - self.half[1] + r;
        let ox = ax.max(0.0);
        let oy = ay.max(0.0);
        (ox * ox + oy * oy).sqrt() + ax.max(ay).min(0.0) - r
    }

    /// Coverage of a pixel whose centre is `d` from the outline, 0 to 1.
    #[inline]
    pub(crate) fn coverage(&self, d: f32) -> f32 {
        let outer = (0.5 - d).clamp(0.0, 1.0);
        if self.stroke > 0.0 {
            outer - (0.5 - d - self.stroke).clamp(0.0, 1.0)
        } else {
            outer
        }
    }

    /// Whether a scan in from the edge can stop: the pixel is fully covered,
    /// or for a ring, fully inside its hole.
    #[inline]
    fn inside(&self, d: f32) -> bool {
        d <= -0.5 - self.stroke
    }
}

/// Coverage times an alpha of 0 to 255, rounded to a byte as quad.frag does.
#[inline]
pub(crate) fn alpha_byte(coverage: f32, alpha: f32) -> u8 {
    (coverage * alpha + 0.5) as u8
}

/// The pixels a shaped quad can touch: `[floor(min), ceil(max))` of its
/// rect, the pixels whose centres the clip holds, and the buffer. x0 y0 x1 y1.
pub(crate) fn bounds(q: &Quad, width: usize, height: usize) -> Option<[usize; 4]> {
    let x0 = q.rect.min[0].floor().max((q.clip.min[0] - 0.5).ceil()).max(0.0);
    let y0 = q.rect.min[1].floor().max((q.clip.min[1] - 0.5).ceil()).max(0.0);
    let x1 = q.rect.max[0].ceil().min((q.clip.max[0] - 0.5).ceil()).min(width as f32);
    let y1 = q.rect.max[1].ceil().min((q.clip.max[1] - 0.5).ceil()).min(height as f32);
    if x0 < x1 && y0 < y1 {
        Some([x0 as usize, y0 as usize, x1 as usize, y1 as usize])
    } else {
        None
    }
}

/// A solid quad with a shape. The colour's alpha, 0 and 255 both opaque,
/// scales every pixel's coverage.
pub(crate) fn fill_shaped(blend: Blend, bd: Backdrop, pixels: &mut [u32], stride_px: usize, height: usize, q: &Quad) {
    let Some([x0, y0, x1, y1]) = bounds(q, stride_px, height) else {
        return;
    };
    let outline = Outline::of(q);
    let a = match q.color >> 24 {
        0 => 255.0,
        a => a as f32,
    };
    let ring = outline.stroke > 0.0;
    for y in y0..y1 {
        let cy = y as f32 + 0.5;
        let row = &mut pixels[y * stride_px..y * stride_px + x1];
        let mut lo = x0;
        while lo < x1 {
            let d = outline.distance([lo as f32 + 0.5, cy]);
            if outline.inside(d) {
                break;
            }
            put(bd, &mut row[lo], alpha_byte(outline.coverage(d), a), q.color);
            lo += 1;
        }
        let mut hi = x1;
        while hi > lo {
            let d = outline.distance([(hi - 1) as f32 + 0.5, cy]);
            if outline.inside(d) {
                break;
            }
            put(bd, &mut row[hi - 1], alpha_byte(outline.coverage(d), a), q.color);
            hi -= 1;
        }
        if !ring && lo < hi {
            fill_span(blend, bd, &mut row[lo..hi], q.color);
        }
    }
}

/// One edge pixel. Full coverage writes the colour as a plain fill does.
#[inline]
fn put(bd: Backdrop, px: &mut u32, cov: u8, color: u32) {
    *px = match (cov, bd) {
        (0, _) => return,
        (0xff, _) => bd.solid(color),
        (_, Backdrop::Opaque) => blend::blend_px(*px, cov, color),
        (_, Backdrop::Clear) => blend::blend_px_argb(*px, cov, color),
    };
}

#[cfg(test)]
mod tests;
