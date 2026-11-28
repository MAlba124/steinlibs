//! Anti-aliased scanline rasterization of a glyph outline into 8-bit coverage.
//!
//! skrifa hands out scaled outlines through the [`OutlinePen`] callback trait and
//! stops there, so the fill itself lives here. The method is signed-area
//! accumulation: every flattened edge deposits a small amount of signed area into
//! the cells it crosses, then a prefix sum along each row turns those deltas into
//! coverage. No edge list to sort, no scanline active list, one pass over the
//! edges and one over the pixels.
//!
//! The prefix sum is per row rather than over the whole buffer. A closed contour
//! contributes zero net signed height to any horizontal band, so a flat sum would
//! also work, but only while nothing is clipped. Resetting at each row keeps a
//! clipped contour from bleeding into the rows below it.
//!
//! Two buffers are held across calls and only ever grow: the flattened edge table
//! and the accumulation grid.

use skrifa::outline::OutlinePen;

/// Maximum distance in pixels between a curve and its polyline approximation.
const FLATTEN_TOLERANCE: f32 = 0.1;

/// Upper bound on subdivisions per curve, so a degenerate control polygon cannot
/// turn one glyph into an unbounded amount of work.
const MAX_SUBDIVISIONS: u32 = 64;

/// A flattened straight edge in outline space, y up, pixels.
#[derive(Copy, Clone, Debug)]
struct Edge {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

/// Integer pixel bounds of a drawn outline.
///
/// `left` is the offset from the pen origin to the first covered column and
/// `top` is the height of the first covered row above the baseline. Together
/// they are the bearing pair a renderer needs to place the bitmap.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Collects an outline, then fills it.
pub struct Rasterizer {
    edges: Vec<Edge>,
    accum: Vec<f32>,
    start: [f32; 2],
    cur: [f32; 2],
    min: [f32; 2],
    max: [f32; 2],
    open: bool,
}

impl Default for Rasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Rasterizer {
    #[must_use]
    pub fn new() -> Self {
        let mut r = Self {
            edges: Vec::new(),
            accum: Vec::new(),
            start: [0.0; 2],
            cur: [0.0; 2],
            min: [0.0; 2],
            max: [0.0; 2],
            open: false,
        };
        r.reset();
        r
    }

    /// Drops the collected outline. Capacity survives.
    pub fn reset(&mut self) {
        self.edges.clear();
        self.start = [0.0; 2];
        self.cur = [0.0; 2];
        self.min = [f32::INFINITY; 2];
        self.max = [f32::NEG_INFINITY; 2];
        self.open = false;
    }

    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Capacities of the two retained buffers, as `[edges, accumulator]`.
    ///
    /// Exposed so callers can assert that a warm path stopped growing them.
    #[must_use]
    pub fn capacities(&self) -> [usize; 2] {
        [self.edges.capacity(), self.accum.capacity()]
    }

    /// Integer bounds of everything drawn since the last [`Rasterizer::reset`].
    ///
    /// A glyph with no contours, such as space, reports all zeros.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        let finite = self.min.iter().chain(&self.max).all(|v| v.is_finite());
        if self.edges.is_empty() || !finite {
            return Bounds::default();
        }
        let x0 = self.min[0].floor();
        let x1 = self.max[0].ceil();
        let y0 = self.min[1].floor();
        let y1 = self.max[1].ceil();
        let w = x1 - x0;
        let h = y1 - y0;
        if w <= 0.0 || h <= 0.0 {
            return Bounds::default();
        }
        Bounds {
            left: x0 as i32,
            top: y1 as i32,
            width: w as u32,
            height: h as u32,
        }
    }

    /// Fills the collected outline into `dst`, placing the top left of the glyph
    /// bitmap at (`dst_x`, `dst_y`) in a buffer of `dst_stride` bytes per row.
    ///
    /// `b` must come from [`Rasterizer::bounds`] on the same collected outline.
    /// Pixels are written, not blended, so the destination rectangle is assumed
    /// to be freshly allocated.
    pub fn fill(&mut self, b: Bounds, dst: &mut [u8], dst_stride: usize, dst_x: u32, dst_y: u32) {
        let w = b.width as usize;
        let h = b.height as usize;
        if w == 0 || h == 0 {
            return;
        }
        // Two guard columns absorb the clamped writes of edges that graze the
        // left border and the area split that runs one cell past the right one.
        let stride = w + 2;
        let need = stride * h;
        if self.accum.len() < need {
            self.accum.resize(need, 0.0);
        }
        self.accum[..need].fill(0.0);

        let left = b.left as f32;
        let top = b.top as f32;
        for e in &self.edges {
            accumulate(
                &mut self.accum,
                stride,
                h,
                e.x0 - left,
                top - e.y0,
                e.x1 - left,
                top - e.y1,
            );
        }
        resolve(
            &self.accum,
            stride,
            w,
            h,
            dst,
            dst_stride,
            dst_x as usize,
            dst_y as usize,
        );
    }

    fn touch(&mut self, x: f32, y: f32) {
        if x < self.min[0] {
            self.min[0] = x;
        }
        if x > self.max[0] {
            self.max[0] = x;
        }
        if y < self.min[1] {
            self.min[1] = y;
        }
        if y > self.max[1] {
            self.max[1] = y;
        }
    }

    fn push_edge(&mut self, x: f32, y: f32) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.edges.push(Edge {
            x0: self.cur[0],
            y0: self.cur[1],
            x1: x,
            y1: y,
        });
        self.cur = [x, y];
        self.touch(x, y);
    }

    fn close_subpath(&mut self) {
        if self.open {
            let [sx, sy] = self.start;
            self.push_edge(sx, sy);
            self.open = false;
        }
    }
}

impl OutlinePen for Rasterizer {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close_subpath();
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.start = [x, y];
        self.cur = [x, y];
        self.open = true;
        self.touch(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.push_edge(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let [px, py] = self.cur;
        // Second difference of the control polygon. Chord error of a quadratic is
        // |P0 - 2P1 + P2| / 8 and falls as 1/n^2 under uniform subdivision.
        let dx = px - 2.0 * cx0 + x;
        let dy = py - 2.0 * cy0 + y;
        let n = subdivisions(dx.hypot(dy) * 0.125);
        let step = 1.0 / n as f32;
        let mut t = step;
        for _ in 1..n {
            let u = 1.0 - t;
            let a = u * u;
            let b = 2.0 * u * t;
            let c = t * t;
            self.push_edge(a * px + b * cx0 + c * x, a * py + b * cy0 + c * y);
            t += step;
        }
        self.push_edge(x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let [px, py] = self.cur;
        let d0 = (px - 2.0 * cx0 + cx1).hypot(py - 2.0 * cy0 + cy1);
        let d1 = (cx0 - 2.0 * cx1 + x).hypot(cy0 - 2.0 * cy1 + y);
        // Cubic chord error is bounded by 3/4 of the larger second difference.
        let n = subdivisions(d0.max(d1) * 0.75);
        let step = 1.0 / n as f32;
        let mut t = step;
        for _ in 1..n {
            let u = 1.0 - t;
            let a = u * u * u;
            let b = 3.0 * u * u * t;
            let c = 3.0 * u * t * t;
            let d = t * t * t;
            self.push_edge(
                a * px + b * cx0 + c * cx1 + d * x,
                a * py + b * cy0 + c * cy1 + d * y,
            );
            t += step;
        }
        self.push_edge(x, y);
    }

    fn close(&mut self) {
        self.close_subpath();
    }
}

/// Segment count that brings `deviation` pixels of chord error under tolerance.
fn subdivisions(deviation: f32) -> u32 {
    if !deviation.is_finite() || deviation <= FLATTEN_TOLERANCE {
        return 1;
    }
    let n = (deviation / FLATTEN_TOLERANCE).sqrt().ceil();
    if n >= MAX_SUBDIVISIONS as f32 {
        MAX_SUBDIVISIONS
    } else {
        n as u32
    }
}

/// Adds `v` at column `col` of the row starting at `row_base`, clamped into the
/// row's guard columns so an out-of-bounds edge cannot corrupt a neighbour row.
#[inline]
fn add(a: &mut [f32], row_base: usize, stride: usize, col: i32, v: f32) {
    let last = stride - 1;
    let c = if col < 0 { 0 } else { (col as usize).min(last) };
    a[row_base + c] += v;
}

/// Deposits one edge's signed area into the accumulation grid.
///
/// Coordinates are raster space: x right, y down, origin at the bitmap corner.
/// Adapted from the area accumulation in Raph Levien's font-rs.
fn accumulate(a: &mut [f32], stride: usize, h: usize, x0: f32, y0: f32, x1: f32, y1: f32) {
    if !(y0 - y1).is_finite() || (y0 - y1).abs() < f32::EPSILON {
        return;
    }
    let (dir, ax, ay, bx, by) = if y0 < y1 {
        (1.0f32, x0, y0, x1, y1)
    } else {
        (-1.0f32, x1, y1, x0, y0)
    };
    let dxdy = (bx - ax) / (by - ay);
    if !dxdy.is_finite() {
        return;
    }
    let mut x = ax;
    let y_start = if ay < 0.0 {
        x -= ay * dxdy;
        0
    } else {
        ay as usize
    };
    let y_end = (by.ceil().max(0.0) as usize).min(h);
    for y in y_start..y_end {
        let base = y * stride;
        let dy = ((y + 1) as f32).min(by) - (y as f32).max(ay);
        let xnext = x + dxdy * dy;
        let d = dy * dir;
        let (lo, hi) = if x < xnext { (x, xnext) } else { (xnext, x) };
        scatter(a, base, stride, x, xnext, lo, hi, d);
        x = xnext;
    }
}

/// Splits one scanline's worth of an edge across the columns it spans.
#[allow(clippy::too_many_arguments)]
#[inline]
fn scatter(
    a: &mut [f32],
    base: usize,
    stride: usize,
    x: f32,
    xnext: f32,
    lo: f32,
    hi: f32,
    d: f32,
) {
    let lo_floor = lo.floor();
    let lo_i = lo_floor as i32;
    let hi_ceil = hi.ceil();
    let hi_i = hi_ceil as i32;
    if hi_i <= lo_i + 1 {
        // Narrower than a pixel: split by the midpoint's fractional position.
        let xmf = 0.5 * (x + xnext) - lo_floor;
        add(a, base, stride, lo_i, d - d * xmf);
        add(a, base, stride, lo_i + 1, d * xmf);
        return;
    }
    let s = (hi - lo).recip();
    let lo_f = lo - lo_floor;
    let a0 = 0.5 * s * (1.0 - lo_f) * (1.0 - lo_f);
    let hi_f = hi - hi_ceil + 1.0;
    let am = 0.5 * s * hi_f * hi_f;
    add(a, base, stride, lo_i, d * a0);
    if hi_i == lo_i + 2 {
        add(a, base, stride, lo_i + 1, d * (1.0 - a0 - am));
    } else {
        let a1 = s * (1.5 - lo_f);
        add(a, base, stride, lo_i + 1, d * (a1 - a0));
        for xi in lo_i + 2..hi_i - 1 {
            add(a, base, stride, xi, d * s);
        }
        let a2 = a1 + (hi_i - lo_i - 3) as f32 * s;
        add(a, base, stride, hi_i - 1, d * (1.0 - a2 - am));
    }
    add(a, base, stride, hi_i, d * am);
}

/// Prefix sums each row of the accumulation grid into 8-bit coverage.
#[allow(clippy::too_many_arguments)]
fn resolve(
    a: &[f32],
    stride: usize,
    w: usize,
    h: usize,
    dst: &mut [u8],
    dst_stride: usize,
    dst_x: usize,
    dst_y: usize,
) {
    for y in 0..h {
        let src = &a[y * stride..y * stride + w];
        let row = (dst_y + y) * dst_stride + dst_x;
        let Some(out) = dst.get_mut(row..row + w) else {
            return;
        };
        let mut acc = 0.0f32;
        for (o, s) in out.iter_mut().zip(src) {
            acc += *s;
            *o = (acc.abs().min(1.0) * 255.0 + 0.5) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fills a unit-tested shape into a standalone bitmap.
    fn render(r: &mut Rasterizer) -> (Bounds, Vec<u8>) {
        let b = r.bounds();
        let mut px = vec![0u8; (b.width * b.height) as usize];
        r.fill(b, &mut px, b.width as usize, 0, 0);
        (b, px)
    }

    #[test]
    fn an_empty_outline_has_empty_bounds() {
        let r = Rasterizer::new();
        assert_eq!(r.bounds(), Bounds::default());
    }

    #[test]
    fn an_axis_aligned_square_is_solid() {
        let mut r = Rasterizer::new();
        r.move_to(2.0, 2.0);
        r.line_to(10.0, 2.0);
        r.line_to(10.0, 10.0);
        r.line_to(2.0, 10.0);
        r.close();
        let (b, px) = render(&mut r);
        assert_eq!(
            b,
            Bounds {
                left: 2,
                top: 10,
                width: 8,
                height: 8
            }
        );
        assert!(px.iter().all(|&v| v == 255), "interior is not solid");
    }

    #[test]
    fn a_half_covered_column_lands_near_half() {
        let mut r = Rasterizer::new();
        r.move_to(0.0, 0.0);
        r.line_to(0.5, 0.0);
        r.line_to(0.5, 4.0);
        r.line_to(0.0, 4.0);
        r.close();
        let (b, px) = render(&mut r);
        assert_eq!(b.width, 1);
        assert_eq!(b.height, 4);
        for &v in &px {
            assert!(
                (i32::from(v) - 128).abs() <= 2,
                "coverage {v} not near half"
            );
        }
    }

    #[test]
    fn winding_cancels_for_a_counter() {
        // Outer square clockwise, inner square counter-clockwise: a ring.
        let mut r = Rasterizer::new();
        r.move_to(0.0, 0.0);
        r.line_to(0.0, 12.0);
        r.line_to(12.0, 12.0);
        r.line_to(12.0, 0.0);
        r.close();
        r.move_to(4.0, 4.0);
        r.line_to(8.0, 4.0);
        r.line_to(8.0, 8.0);
        r.line_to(4.0, 8.0);
        r.close();
        let (b, px) = render(&mut r);
        assert_eq!(b.width, 12);
        let at = |x: usize, y: usize| px[y * 12 + x];
        assert_eq!(at(1, 1), 255, "outer ring should be filled");
        assert_eq!(at(6, 6), 0, "counter should be empty");
    }

    #[test]
    fn a_quadratic_is_flattened_into_several_edges() {
        let mut r = Rasterizer::new();
        r.move_to(0.0, 0.0);
        r.quad_to(20.0, 40.0, 40.0, 0.0);
        r.close();
        assert!(r.edge_count() > 4, "curve barely subdivided");
    }

    #[test]
    fn buffers_are_reused_across_fills() {
        let mut r = Rasterizer::new();
        for _ in 0..4 {
            r.reset();
            r.move_to(0.0, 0.0);
            r.line_to(16.0, 0.0);
            r.line_to(16.0, 16.0);
            r.close();
            let _ = render(&mut r);
        }
        let edge_cap = r.edges.capacity();
        let accum_cap = r.accum.capacity();
        for _ in 0..200 {
            r.reset();
            r.move_to(0.0, 0.0);
            r.line_to(16.0, 0.0);
            r.line_to(16.0, 16.0);
            r.close();
            let _ = render(&mut r);
        }
        assert_eq!(
            (edge_cap, accum_cap),
            (r.edges.capacity(), r.accum.capacity())
        );
    }
}
