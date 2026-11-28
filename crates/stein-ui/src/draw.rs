//! The draw list, and the escape hatch that keeps it usable at scale.
//!
//! Every visible box becomes a [`Quad`] carrying its own scissor rect, because
//! layout already computed one per box and a flat list is what a GPU backend wants
//! to batch. Painter order is emission order.
//!
//! # Why there is an escape hatch
//!
//! A box per visual element does not survive contact with real data. A flame graph
//! over a few million samples has far more rectangles than you can emit as layout
//! boxes at frame rate, and it does not need layout: the geometry comes from the
//! data, not from a constraint solve.
//!
//! So a widget can instead reserve a rect and push quads into this list directly,
//! doing its own culling against the clip. The cost of not designing this in from
//! the start is a renderer rewrite, because it decides who owns the primitive
//! buffer: shared between the widget layer and application code, not owned by the
//! widget layer.

use stein_layout::Rect;

/// One rectangle, scissored to `clip`.
///
/// Solid fills, glyphs and images are the same primitive. A glyph is a quad
/// whose `uv` names a patch of the coverage atlas, multiplied by `color`; an
/// image names a patch of the RGBA image atlas instead, tinted by `color`;
/// everything else has a zero-area `uv` and is a flat fill. One primitive
/// means one pipeline, one vertex format and one batch, which is what the
/// backend wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub rect: Rect,
    pub clip: Rect,
    pub color: u32,
    /// Source patch as u, v, w, h in normalised texture space.
    pub uv: [f32; 4],
    /// `uv` is in the image atlas rather than the coverage atlas.
    pub image: bool,
    /// Rounded corners and stroke. [`Shape::NONE`] is a plain rect, which keeps
    /// the exact pixel snapping solid quads always had. Glyphs ignore it.
    pub shape: Shape,
}

/// The outline of a solid or image quad.
///
/// Radii are whole pixels per corner, top left, top right, bottom right,
/// bottom left. Each one is clamped to half the shorter side when painted, so
/// 255 makes a pill or a circle of anything up to 510 pixels. A nonzero
/// `stroke` paints only a ring that wide just inside the outline, its inner
/// corners rounded by the radius less the stroke. Any shape other than
/// [`Shape::NONE`] is antialiased on every edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Shape {
    pub radius: [u8; 4],
    pub stroke: u8,
}

impl Shape {
    pub const NONE: Shape = Shape {
        radius: [0; 4],
        stroke: 0,
    };

    /// The same radius on every corner.
    pub const fn round(radius: u8) -> Shape {
        Shape {
            radius: [radius; 4],
            stroke: 0,
        }
    }

    /// As round as the quad allows, a pill or a circle.
    pub const PILL: Shape = Shape::round(u8::MAX);

    pub const fn corners(radius: [u8; 4]) -> Shape {
        Shape { radius, stroke: 0 }
    }

    /// Only the ring `width` wide inside the outline.
    pub const fn stroked(mut self, width: u8) -> Shape {
        self.stroke = width;
        self
    }

    #[inline]
    pub const fn is_none(&self) -> bool {
        self.stroke == 0 && u32::from_le_bytes(self.radius) == 0
    }
}

impl Quad {
    pub const fn solid(rect: Rect, clip: Rect, color: u32) -> Self {
        Quad {
            rect,
            clip,
            color,
            uv: [0.0; 4],
            image: false,
            shape: Shape::NONE,
        }
    }

    /// `uv` is u, v, w, h in normalised texture space, matching what
    /// `stein-text` emits.
    pub const fn textured(rect: Rect, clip: Rect, color: u32, uv: [f32; 4]) -> Self {
        Quad {
            rect,
            clip,
            color,
            uv,
            image: false,
            shape: Shape::NONE,
        }
    }

    /// A patch of the image atlas stretched over `rect`, sampled nearest.
    /// `color` tints it, `0xffff_ffff` leaves it as it is.
    pub const fn image(rect: Rect, clip: Rect, color: u32, uv: [f32; 4]) -> Self {
        Quad {
            rect,
            clip,
            color,
            uv,
            image: true,
            shape: Shape::NONE,
        }
    }

    /// The same quad with `shape` as its outline.
    #[inline]
    pub const fn shaped(mut self, shape: Shape) -> Self {
        self.shape = shape;
        self
    }

    /// Samples the coverage atlas, a glyph.
    #[inline]
    pub fn is_textured(&self) -> bool {
        !self.image && self.uv[2] > 0.0 && self.uv[3] > 0.0
    }

    /// Samples the image atlas.
    #[inline]
    pub fn is_image(&self) -> bool {
        self.image && self.uv[2] > 0.0 && self.uv[3] > 0.0
    }
}

impl Quad {
    /// Whether the scissor leaves anything to draw. A culled quad never reaches
    /// the backend.
    #[inline]
    pub fn visible(&self) -> bool {
        let w = fmin(self.rect.max[0], self.clip.max[0]) - fmax(self.rect.min[0], self.clip.min[0]);
        let h = fmin(self.rect.max[1], self.clip.max[1]) - fmax(self.rect.min[1], self.clip.min[1]);
        w > 0.0 && h > 0.0
    }
}

#[inline]
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
#[inline]
fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

/// A rect a widget reserved to paint itself, handed back once layout has solved.
///
/// The application reads these after `end_frame`, culls against `clip`, and pushes
/// quads. `key` identifies which widget asked, since one frame may contain several.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomRegion {
    pub key: u64,
    pub rect: Rect,
    pub clip: Rect,
}

/// Fixed-capacity quad buffer over caller storage.
pub struct DrawList<'a> {
    quads: &'a mut [Quad],
    len: u32,
    /// Quads that did not fit. Silently dropping them would make a capacity bug
    /// look like a rendering bug.
    dropped: u32,
}

impl<'a> DrawList<'a> {
    pub fn new(quads: &'a mut [Quad]) -> Self {
        DrawList {
            quads,
            len: 0,
            dropped: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.dropped = 0;
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn capacity(&self) -> usize {
        self.quads.len()
    }

    /// How many quads were refused this frame. Nonzero means the buffer is too
    /// small and the frame on screen is incomplete.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Append, culling anything the scissor removes entirely. Returns false when
    /// the quad was refused for want of space, which the caller may want to know
    /// about and which [`DrawList::dropped`] also records.
    pub fn push(&mut self, q: Quad) -> bool {
        if !q.visible() {
            return true;
        }
        if self.len as usize >= self.quads.len() {
            self.dropped += 1;
            return false;
        }
        self.quads[self.len as usize] = q;
        self.len += 1;
        true
    }

    pub fn quads(&self) -> &[Quad] {
        &self.quads[..self.len as usize]
    }
}

/// How many separate regions a frame can report before they start merging.
///
/// Eight covers what a real frame does: a cursor somewhere, a line of output
/// somewhere else, a hovered row, a tooltip. Past that the saving from finer
/// tracking is smaller than the cost of tracking it.
pub const MAX_DAMAGE_RECTS: usize = 8;

/// How much bigger than its parts a union may be before it is not worth
/// merging.
///
/// Glyphs along one line of text are separate quads with gaps between them, so
/// they never touch. Their union is barely bigger than the sum, which is the
/// case this threshold exists to catch. Two quads at opposite corners produce
/// a union many times their sum and stay apart.
const MERGE_RATIO: f32 = 1.6;

/// The regions that differ between two frames.
///
/// A single bounding rect is what this replaces, and it was the wrong shape
/// for anything that changes in two places at once: a terminal with a cursor
/// at the top and new output at the bottom reported the whole screen, so
/// partial repaint bought nothing. Several rects keep those two apart.
///
/// Empty means the frames are identical and nothing has to be presented, which
/// is what lets an idle window cost nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Damage {
    rects: [Rect; MAX_DAMAGE_RECTS],
    len: u8,
    coalesced: bool,
}

impl Damage {
    pub fn new() -> Damage {
        Damage::default()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// The regions to repaint.
    ///
    /// They can still overlap, but only where folding them would cost more than
    /// the double paint does: a row highlight crossing a column highlight is
    /// two thin rects whose union is the whole square.
    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len()]
    }

    /// True when the list filled up and regions were merged, so the reported
    /// area is larger than what actually changed. Still correct, just coarser.
    pub fn coalesced(&self) -> bool {
        self.coalesced
    }

    /// One rect covering everything, for a caller that cannot take a list.
    pub fn bounds(&self) -> Option<Rect> {
        let mut acc: Option<Rect> = None;
        for r in self.rects() {
            acc = Some(union(acc, *r));
        }
        acc
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.coalesced = false;
    }

    /// Fold in another frame's damage, for a caller carrying it across frames
    /// that were built but never presented.
    pub fn absorb(&mut self, other: &Damage) {
        for r in other.rects() {
            self.add(*r);
        }
        self.coalesced |= other.coalesced;
    }

    pub fn add(&mut self, r: Rect) {
        if r.max[0] <= r.min[0] || r.max[1] <= r.min[1] {
            return;
        }
        if self.rects().iter().any(|e| covers(*e, r)) {
            return;
        }
        if let Some(i) = cheapest(self, r, MERGE_RATIO) {
            self.rects[i] = union(Some(self.rects[i]), r);
            collapse(self, i);
            return;
        }
        if self.len() < MAX_DAMAGE_RECTS {
            self.rects[self.len()] = r;
            self.len += 1;
            return;
        }
        // Full. Whichever region grows least absorbs it, and the report says
        // it is now coarser than the truth.
        if let Some(i) = cheapest(self, r, f32::INFINITY) {
            self.rects[i] = union(Some(self.rects[i]), r);
            self.coalesced = true;
            collapse(self, i);
        }
    }
}

/// Index of the region that merging `r` into costs least, if any is within
/// `ratio` of the combined area of the two. A free function because the lint
/// counts the impl block as a level of nesting.
fn cheapest(d: &Damage, r: Rect, ratio: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, e) in d.rects().iter().enumerate() {
        let merged = area(union(Some(*e), r));
        let grew = merged - area(*e);
        let fits = merged <= (area(*e) + area(r)) * ratio;
        if fits && best.is_none_or(|(_, b)| grew < b) {
            best = Some((i, grew));
        }
    }
    best.map(|(i, _)| i)
}

/// A region that just grew may now sit on top of another one. Both get painted,
/// so the shared pixels are done twice and the total area stops describing the
/// work. Folding those together fixes both, but only when the union is worth it
/// by the same rule a merge uses: crossing bars overlap and must stay apart.
///
/// `keep` is the region that grew. Bounded by `MAX_DAMAGE_RECTS`, so restarting
/// the scan after each fold is cheaper than reasoning about what it invalidated.
fn collapse(d: &mut Damage, keep: usize) {
    let mut i = keep;
    let mut j = 0;
    while j < d.len() {
        if j == i || !worth_folding(d.rects[i], d.rects[j]) {
            j += 1;
            continue;
        }
        d.rects[i] = union(Some(d.rects[i]), d.rects[j]);
        let last = d.len() - 1;
        d.rects[j] = d.rects[last];
        d.len -= 1;
        // The grown region may have been the one moved into slot j.
        if i == last {
            i = j;
        }
        j = 0;
    }
}

fn worth_folding(a: Rect, b: Rect) -> bool {
    overlaps(a, b) && area(union(Some(a), b)) <= (area(a) + area(b)) * MERGE_RATIO
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.min[0] < b.max[0] && b.min[0] < a.max[0] && a.min[1] < b.max[1] && b.min[1] < a.max[1]
}

fn area(r: Rect) -> f32 {
    fmax(r.max[0] - r.min[0], 0.0) * fmax(r.max[1] - r.min[1], 0.0)
}

fn covers(outer: Rect, inner: Rect) -> bool {
    outer.min[0] <= inner.min[0]
        && outer.min[1] <= inner.min[1]
        && outer.max[0] >= inner.max[0]
        && outer.max[1] >= inner.max[1]
}

/// The regions that differ between two frames.
///
/// Quads are compared in order, because order is painter order and a
/// reordering is a visual change even when the set is the same.
pub fn damage(prev: &[Quad], next: &[Quad]) -> Damage {
    let mut acc = Damage::new();
    let common = if prev.len() < next.len() {
        prev.len()
    } else {
        next.len()
    };

    for i in 0..common {
        if prev[i] != next[i] {
            acc.add(visible_rect(prev[i]));
            acc.add(visible_rect(next[i]));
        }
    }
    // Whichever list is longer contributes its tail: those quads either appeared
    // or went away, and both need repainting.
    for q in prev.iter().skip(common) {
        acc.add(visible_rect(*q));
    }
    for q in next.iter().skip(common) {
        acc.add(visible_rect(*q));
    }
    acc
}

fn visible_rect(q: Quad) -> Rect {
    Rect {
        min: [
            fmax(q.rect.min[0], q.clip.min[0]),
            fmax(q.rect.min[1], q.clip.min[1]),
        ],
        max: [
            fmin(q.rect.max[0], q.clip.max[0]),
            fmin(q.rect.max[1], q.clip.max[1]),
        ],
    }
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => Rect {
            min: [fmin(a.min[0], b.min[0]), fmin(a.min[1], b.min[1])],
            max: [fmax(a.max[0], b.max[0]), fmax(a.max[1], b.max[1])],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(x: f32, y: f32, w: f32, h: f32, color: u32) -> Quad {
        Quad::solid(
            Rect {
                min: [x, y],
                max: [x + w, y + h],
            },
            Rect {
                min: [-1e9, -1e9],
                max: [1e9, 1e9],
            },
            color,
        )
    }

    #[test]
    fn a_solid_quad_is_not_textured_and_a_glyph_is() {
        let r = Rect {
            min: [0.0, 0.0],
            max: [4.0, 4.0],
        };
        assert!(!Quad::solid(r, r, 1).is_textured());
        assert!(Quad::textured(r, r, 1, [0.0, 0.0, 0.1, 0.1]).is_textured());
        // A degenerate patch is a fill, not an invisible glyph.
        assert!(!Quad::textured(r, r, 1, [0.5, 0.5, 0.0, 0.0]).is_textured());
    }

    #[test]
    fn pushing_appends_in_order() {
        let mut buf = [q(0.0, 0.0, 1.0, 1.0, 0); 4];
        let mut d = DrawList::new(&mut buf);
        assert!(d.push(q(0.0, 0.0, 10.0, 10.0, 1)));
        assert!(d.push(q(5.0, 5.0, 10.0, 10.0, 2)));
        assert_eq!(d.len(), 2);
        assert_eq!(d.quads()[0].color, 1);
        assert_eq!(d.quads()[1].color, 2);
    }

    #[test]
    fn a_fully_scissored_quad_is_culled_not_stored() {
        let mut buf = [q(0.0, 0.0, 1.0, 1.0, 0); 4];
        let mut d = DrawList::new(&mut buf);
        let mut hidden = q(0.0, 0.0, 10.0, 10.0, 1);
        hidden.clip = Rect {
            min: [100.0, 100.0],
            max: [200.0, 200.0],
        };
        assert!(d.push(hidden), "culling is not a failure");
        assert_eq!(d.len(), 0);
        assert_eq!(d.dropped(), 0, "culled is not dropped");
    }

    #[test]
    fn overflow_is_reported_not_silent() {
        let mut buf = [q(0.0, 0.0, 1.0, 1.0, 0); 2];
        let mut d = DrawList::new(&mut buf);
        assert!(d.push(q(0.0, 0.0, 1.0, 1.0, 1)));
        assert!(d.push(q(0.0, 0.0, 1.0, 1.0, 2)));
        assert!(!d.push(q(0.0, 0.0, 1.0, 1.0, 3)), "must refuse");
        assert_eq!(d.len(), 2, "must not grow");
        assert_eq!(d.dropped(), 1);
    }

    #[test]
    fn clear_resets_the_overflow_counter_too() {
        let mut buf = [q(0.0, 0.0, 1.0, 1.0, 0); 1];
        let mut d = DrawList::new(&mut buf);
        d.push(q(0.0, 0.0, 1.0, 1.0, 1));
        d.push(q(0.0, 0.0, 1.0, 1.0, 2));
        assert_eq!(d.dropped(), 1);
        d.clear();
        assert_eq!(d.dropped(), 0);
        assert!(d.is_empty());
    }

    #[test]
    fn identical_frames_produce_no_damage() {
        let a = [q(0.0, 0.0, 10.0, 10.0, 1), q(5.0, 5.0, 2.0, 2.0, 2)];
        assert!(
            damage(&a, &a).is_empty(),
            "an idle window must cost nothing"
        );
    }

    #[test]
    fn a_changed_colour_damages_that_quad_only() {
        let a = [q(0.0, 0.0, 10.0, 10.0, 1), q(50.0, 50.0, 4.0, 4.0, 2)];
        let b = [q(0.0, 0.0, 10.0, 10.0, 1), q(50.0, 50.0, 4.0, 4.0, 9)];
        let d = damage(&a, &b).bounds().expect("colour changed");
        assert_eq!(d.min, [50.0, 50.0]);
        assert_eq!(d.max, [54.0, 54.0]);
    }

    #[test]
    fn a_moved_quad_damages_both_positions() {
        let a = [q(0.0, 0.0, 4.0, 4.0, 1)];
        let b = [q(20.0, 20.0, 4.0, 4.0, 1)];
        let d = damage(&a, &b).bounds().expect("it moved");
        // The old spot needs repainting as much as the new one.
        assert_eq!(d.min, [0.0, 0.0]);
        assert_eq!(d.max, [24.0, 24.0]);
    }

    #[test]
    fn an_appearing_quad_is_damage() {
        let a = [q(0.0, 0.0, 4.0, 4.0, 1)];
        let b = [q(0.0, 0.0, 4.0, 4.0, 1), q(30.0, 30.0, 2.0, 2.0, 5)];
        let d = damage(&a, &b).bounds().expect("one appeared");
        assert_eq!(d.min, [30.0, 30.0]);
        assert_eq!(d.max, [32.0, 32.0]);
    }

    #[test]
    fn a_disappearing_quad_is_damage() {
        let a = [q(0.0, 0.0, 4.0, 4.0, 1), q(30.0, 30.0, 2.0, 2.0, 5)];
        let b = [q(0.0, 0.0, 4.0, 4.0, 1)];
        let d = damage(&a, &b).bounds().expect("one went away");
        assert_eq!(d.min, [30.0, 30.0]);
        assert_eq!(d.max, [32.0, 32.0]);
    }

    #[test]
    fn reordering_is_damage_even_when_the_set_matches() {
        // Painter order decides what covers what, so a swap is visible.
        let x = q(0.0, 0.0, 10.0, 10.0, 1);
        let y = q(0.0, 0.0, 10.0, 10.0, 2);
        assert!(!damage(&[x, y], &[y, x]).is_empty());
    }

    #[test]
    fn damage_is_measured_after_scissoring() {
        let mut a = q(0.0, 0.0, 1000.0, 1000.0, 1);
        a.clip = Rect {
            min: [0.0, 0.0],
            max: [10.0, 10.0],
        };
        let mut b = a;
        b.color = 2;
        let d = damage(&[a], &[b]).bounds().expect("colour changed");
        // The quad is huge; only the visible 10x10 needs repainting.
        assert_eq!(d.max, [10.0, 10.0]);
    }
}

#[cfg(test)]
mod damage_tests {
    use super::*;
    use std::vec::Vec;

    fn q(x: f32, y: f32, w: f32, h: f32, color: u32) -> Quad {
        Quad::solid(
            Rect {
                min: [x, y],
                max: [x + w, y + h],
            },
            Rect {
                min: [-1e9, -1e9],
                max: [1e9, 1e9],
            },
            color,
        )
    }

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect {
            min: [x, y],
            max: [x + w, y + h],
        }
    }

    /// The case the whole change exists for: a terminal cursor at the top and
    /// new output at the bottom. One bounding rect covers the entire window
    /// and partial repaint buys nothing.
    #[test]
    fn two_changes_at_opposite_ends_stay_apart() {
        let a = [q(0.0, 0.0, 8.0, 16.0, 1), q(0.0, 900.0, 8.0, 16.0, 2)];
        let b = [q(0.0, 0.0, 8.0, 16.0, 9), q(0.0, 900.0, 8.0, 16.0, 9)];
        let d = damage(&a, &b);
        assert_eq!(d.len(), 2, "{:?}", d.rects());
        assert!(!d.coalesced());

        // The old behaviour is still available and still covers everything.
        let bounds = d.bounds().expect("something changed");
        assert_eq!(bounds.min[1], 0.0);
        assert_eq!(bounds.max[1], 916.0);
        // And the two rects together are a fraction of it.
        let total: f32 = d.rects().iter().map(|r| area(*r)).sum();
        assert!(
            total * 20.0 < area(bounds),
            "{total} against {}",
            area(bounds)
        );
    }

    /// Glyphs along one line are separate quads with gaps between them. They
    /// never touch, so merging has to be by cost rather than by adjacency.
    #[test]
    fn glyphs_along_one_line_merge_into_one_region() {
        let mut d = Damage::new();
        for i in 0..40 {
            d.add(r(i as f32 * 10.0, 100.0, 8.0, 16.0));
        }
        assert_eq!(d.len(), 1, "a line of text split into {:?}", d.rects());
        assert!(!d.coalesced(), "it should not have needed to coalesce");
        let only = d.rects()[0];
        assert_eq!(only.min, [0.0, 100.0]);
        assert_eq!(only.max, [398.0, 116.0]);
    }

    #[test]
    fn distant_regions_do_not_merge_until_the_list_is_full() {
        let mut d = Damage::new();
        for i in 0..MAX_DAMAGE_RECTS {
            d.add(r(i as f32 * 500.0, i as f32 * 500.0, 10.0, 10.0));
        }
        assert_eq!(d.len(), MAX_DAMAGE_RECTS);
        assert!(!d.coalesced());

        // One more has to go somewhere, and the report says it is now coarser.
        d.add(r(9000.0, 9000.0, 10.0, 10.0));
        assert_eq!(d.len(), MAX_DAMAGE_RECTS, "the cap was exceeded");
        assert!(d.coalesced(), "coarsening went unreported");
    }

    #[test]
    fn a_region_already_covered_is_not_added_again() {
        let mut d = Damage::new();
        d.add(r(0.0, 0.0, 100.0, 100.0));
        d.add(r(10.0, 10.0, 5.0, 5.0));
        assert_eq!(d.len(), 1);
        assert_eq!(d.rects()[0], r(0.0, 0.0, 100.0, 100.0));
    }

    #[test]
    fn an_empty_region_is_not_damage() {
        let mut d = Damage::new();
        d.add(r(10.0, 10.0, 0.0, 50.0));
        d.add(r(10.0, 10.0, 50.0, 0.0));
        // A fully clipped quad reports min past max.
        d.add(Rect {
            min: [50.0, 50.0],
            max: [10.0, 10.0],
        });
        assert!(d.is_empty(), "{:?}", d.rects());
        assert!(d.bounds().is_none());
    }

    #[test]
    fn every_change_is_inside_the_reported_regions() {
        // The invariant that makes partial repaint correct: nothing that
        // changed may fall outside what gets repainted.
        let a: Vec<Quad> = (0..60)
            .map(|i| {
                q(
                    i as f32 * 37.0 % 900.0,
                    i as f32 * 53.0 % 700.0,
                    9.0,
                    9.0,
                    1,
                )
            })
            .collect();
        let mut b = a.clone();
        for i in (0..60).step_by(7) {
            b[i] = q(
                i as f32 * 37.0 % 900.0,
                i as f32 * 53.0 % 700.0,
                9.0,
                9.0,
                7,
            );
        }
        let d = damage(&a, &b);
        assert!(!d.is_empty());
        for (i, changed) in (0..60).filter(|i| a[*i] != b[*i]).map(|i| (i, a[i].rect)) {
            assert!(
                d.rects().iter().any(|e| covers(*e, changed)),
                "quad {i} at {changed:?} is not inside {:?}",
                d.rects()
            );
        }
    }

    #[test]
    fn absorbing_another_frames_damage_keeps_both() {
        // A frame built but never presented has to carry its damage forward.
        let mut a = Damage::new();
        a.add(r(0.0, 0.0, 10.0, 10.0));
        let mut b = Damage::new();
        b.add(r(500.0, 500.0, 10.0, 10.0));
        a.absorb(&b);
        assert_eq!(a.len(), 2);
        let bounds = a.bounds().expect("not empty");
        assert_eq!(bounds.min, [0.0, 0.0]);
        assert_eq!(bounds.max, [510.0, 510.0]);
    }

    #[test]
    fn absorbing_carries_the_coarsening_flag() {
        let mut a = Damage::new();
        let mut b = Damage::new();
        for i in 0..MAX_DAMAGE_RECTS + 2 {
            b.add(r(i as f32 * 900.0, i as f32 * 900.0, 4.0, 4.0));
        }
        assert!(b.coalesced());
        a.absorb(&b);
        assert!(a.coalesced(), "the caller would think the report was exact");
    }

    #[test]
    fn clearing_puts_it_back_to_empty() {
        let mut d = Damage::new();
        for i in 0..MAX_DAMAGE_RECTS + 3 {
            d.add(r(i as f32 * 900.0, 0.0, 4.0, 4.0));
        }
        d.clear();
        assert!(d.is_empty());
        assert!(!d.coalesced());
        assert!(d.bounds().is_none());
    }

    #[test]
    fn a_whole_screen_redraw_degrades_to_roughly_the_bounding_box() {
        // Thousands of scattered changes cannot be tracked finely, and the
        // honest outcome is one big region rather than eight arbitrary ones.
        let a: Vec<Quad> = (0..2000)
            .map(|i| {
                q(
                    (i % 100) as f32 * 8.0,
                    (i / 100) as f32 * 16.0,
                    8.0,
                    16.0,
                    1,
                )
            })
            .collect();
        let b: Vec<Quad> = a.iter().map(|q| Quad { color: 2, ..*q }).collect();
        let d = damage(&a, &b);
        assert!(!d.is_empty());
        let bounds = d.bounds().expect("everything changed");
        assert_eq!(bounds.min, [0.0, 0.0]);
        assert_eq!(bounds.max, [800.0, 320.0]);
        assert!(d.len() <= MAX_DAMAGE_RECTS);
    }

    /// A region that grows over a neighbour would otherwise leave both in the
    /// list, so the shared pixels get cleared and repainted twice.
    #[test]
    fn a_region_that_grows_over_another_swallows_it() {
        let mut d = Damage::new();
        d.add(r(100.0, 100.0, 20.0, 20.0));
        d.add(r(400.0, 100.0, 20.0, 20.0));
        assert_eq!(d.len(), 2, "these should not have merged on their own");
        // Spanning both pulls the first into the span, which then sits on the
        // second.
        d.add(r(100.0, 100.0, 330.0, 20.0));
        assert_eq!(d.len(), 1, "overlapping regions were left in place");
        assert_eq!(d.rects()[0], r(100.0, 100.0, 330.0, 20.0));
    }

    /// Folding is still refused where it would cost more than the double paint.
    #[test]
    fn a_row_and_a_column_highlight_stay_two_regions() {
        let mut d = Damage::new();
        d.add(r(0.0, 300.0, 800.0, 18.0));
        d.add(r(390.0, 0.0, 18.0, 600.0));
        assert_eq!(d.len(), 2, "crossing bars were merged into the window");
        let painted: f32 = d.rects().iter().map(|x| x.width() * x.height()).sum();
        assert!(painted < 800.0 * 600.0 * 0.1, "painted {painted}");
    }

    #[test]
    fn no_region_ever_contains_another() {
        let mut d = Damage::new();
        // Nested and staggered boxes, the shape a tooltip over a table makes.
        for i in 0..40 {
            let f = i as f32;
            d.add(r(f * 7.0, f * 3.0, 40.0 - f, 30.0));
            d.add(r(600.0 - f * 5.0, 400.0, 12.0, 12.0 + f));
        }
        let nested = pairs(&d).find(|(a, b)| covers(*a, *b));
        assert!(nested.is_none(), "one region contains another: {nested:?}");
    }

    /// Every ordered pair of distinct regions.
    fn pairs(d: &Damage) -> impl Iterator<Item = (Rect, Rect)> + '_ {
        d.rects()
            .iter()
            .enumerate()
            .flat_map(move |(i, a)| {
                d.rects()
                    .iter()
                    .enumerate()
                    .map(move |(j, b)| (i, j, *a, *b))
            })
            .filter(|(i, j, _, _)| i != j)
            .map(|(_, _, a, b)| (a, b))
    }

    /// The collapse pass moves the last region into a freed slot, so the index
    /// it is tracking can be the one that moved. Getting that wrong grows the
    /// wrong region and drops changes.
    #[test]
    fn collapsing_the_last_region_still_covers_every_change() {
        let mut d = Damage::new();
        let boxes = [
            r(10.0, 10.0, 30.0, 30.0),
            r(500.0, 500.0, 30.0, 30.0),
            r(10.0, 12.0, 34.0, 26.0),
            r(495.0, 498.0, 40.0, 36.0),
            r(12.0, 8.0, 26.0, 34.0),
        ];
        for b in boxes {
            d.add(b);
        }
        for b in boxes {
            assert!(
                d.rects().iter().any(|e| covers(*e, b)),
                "{b:?} is not inside {:?}",
                d.rects()
            );
        }
    }
}
