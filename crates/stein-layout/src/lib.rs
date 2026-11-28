//! UI layout as linear scans over a table.
//!
//! Boxes are emitted in depth-first preorder, which buys two properties the whole
//! design rests on:
//!
//! - a parent always precedes its descendants, so a forward scan sees parents first
//!   and a reverse scan sees children first;
//! - a box's subtree is a contiguous range, so `subtree_end` is all the structure
//!   needed to walk direct children (first child is `i + 1`, next sibling is
//!   `subtree_end[child]`).
//!
//! Between them, the dependency direction of every layout pass is free. No recursion,
//! no child lists, no pointer chasing. Solving is five scans per axis over flat
//! columns, and the axes are independent.
//!
//! Text is not a dependency. Measured extents arrive in the `measured` column, filled
//! by whoever knows how to measure, so this crate stays a pure transform: boxes in,
//! rects out, no platform, no font, no allocator.
//!
//! ```
//! use stein_layout::{Axis, BoxSpec, BoxTable, Insets, LayoutStorage, Options, SizeSpec};
//!
//! const N: usize = 8;
//! let mut storage = LayoutStorage {
//!     key: &mut [0u64; N],
//!     parent: &mut [0u32; N],
//!     subtree_end: &mut [0u32; N],
//!     size: &mut [[SizeSpec::px(0.0); 2]; N],
//!     axis: &mut [Axis::X; N],
//!     measured: &mut [[0.0f32; 2]; N],
//!     pad: &mut [Insets::ZERO; N],
//!     opts: &mut [Options::default(); N],
//!     computed: &mut [[0.0f32; 2]; N],
//!     content: &mut [[0.0f32; 2]; N],
//!     rect: &mut [Default::default(); N],
//!     clip: &mut [Default::default(); N],
//!     stack: &mut [0u32; N],
//! };
//! let mut t = BoxTable::new(storage).expect("columns agree");
//!
//! // A row of two panes: a 200px sidebar and a body taking the rest.
//! t.open(1, BoxSpec::container(Axis::X).fill()).expect("fits");
//! t.leaf(2, BoxSpec::leaf().w(SizeSpec::px(200.0)).h(SizeSpec::grow())).expect("fits");
//! t.leaf(3, BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::grow())).expect("fits");
//! t.close().expect("balanced");
//!
//! t.solve([800.0, 600.0]);
//! assert_eq!(t.rect(1).expect("live").width(), 200.0);
//! assert_eq!(t.rect(2).expect("live").width(), 600.0);
//! ```

#![no_std]

#[cfg(test)]
extern crate std;

/// `core` lacks `f32::max`/`min` without a libm dependency, and these are two lines.
#[inline]
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
#[inline]
fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

pub const X: usize = 0;
pub const Y: usize = 1;

/// The axis a container arranges its children along. The other axis is the cross
/// axis, where children stack rather than accumulate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    X,
    Y,
}

impl Axis {
    #[inline]
    fn index(self) -> usize {
        match self {
            Axis::X => X,
            Axis::Y => Y,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Rect {
    pub fn width(&self) -> f32 {
        self.max[X] - self.min[X]
    }
    pub fn height(&self) -> f32 {
        self.max[Y] - self.min[Y]
    }
    pub fn contains(&self, p: [f32; 2]) -> bool {
        p[X] >= self.min[X] && p[X] < self.max[X] && p[Y] >= self.min[Y] && p[Y] < self.max[Y]
    }
}

/// How a box's extent on one axis is determined.
///
/// The variants are grouped by which pass resolves them, which is not an
/// implementation detail: it is the reason the passes can be linear.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    /// Known immediately. Pass 1.
    Px(f32),
    /// Taken from the `measured` column, filled by the caller. Pass 1.
    Measured,
    /// A fraction of the parent's final extent. Pass 2, since the parent is already
    /// resolved by then, unless the parent is `ChildrenSum`, which is a cycle: see
    /// the note on [`Size::ChildrenSum`].
    ParentFraction(f32),
    /// Sum of children along the layout axis, largest child across it. Pass 3, in
    /// reverse, where children are already resolved.
    ChildrenSum,
    /// An equal share of whatever the parent has left over. Pass 4.
    Grow,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SizeSpec {
    pub kind: Size,
    /// How hard this box resists shrinking when its siblings overflow the parent.
    /// 0 gives way first, 1 never gives way. Clamped into range on use.
    pub strictness: f32,
    /// Hard floor. Wins over everything, including the overflow solver, so a
    /// container too small for its children's minimums overflows rather than
    /// crushing them. Same trade as a strict child, and for the same reason.
    pub min: f32,
    /// Hard ceiling. `f32::INFINITY` for none.
    pub max: f32,
}

impl SizeSpec {
    pub const fn new(kind: Size, strictness: f32) -> Self {
        SizeSpec {
            kind,
            strictness,
            min: 0.0,
            max: f32::INFINITY,
        }
    }
    pub const fn px(v: f32) -> Self {
        SizeSpec::new(Size::Px(v), 1.0)
    }
    pub const fn measured() -> Self {
        SizeSpec::new(Size::Measured, 1.0)
    }
    pub const fn fraction(f: f32) -> Self {
        SizeSpec::new(Size::ParentFraction(f), 0.0)
    }
    pub const fn children_sum() -> Self {
        SizeSpec::new(Size::ChildrenSum, 0.0)
    }
    pub const fn grow() -> Self {
        SizeSpec::new(Size::Grow, 0.0)
    }
    /// Same size, but it gives way before its siblings do.
    pub const fn soft(mut self) -> Self {
        self.strictness = 0.0;
        self
    }
    pub const fn strict(mut self) -> Self {
        self.strictness = 1.0;
        self
    }
    pub const fn at_least(mut self, v: f32) -> Self {
        self.min = v;
        self
    }
    pub const fn at_most(mut self, v: f32) -> Self {
        self.max = v;
        self
    }

    #[inline]
    fn clamp(&self, v: f32) -> f32 {
        fmin(fmax(v, fmax(self.min, 0.0)), self.max)
    }
}

/// Where children sit in leftover space.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

impl Align {
    #[inline]
    fn offset(self, slack: f32) -> f32 {
        let slack = fmax(slack, 0.0);
        match self {
            Align::Start => 0.0,
            Align::Center => slack * 0.5,
            Align::End => slack,
        }
    }
}

/// Whether a box takes part in its parent's flow.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Placement {
    #[default]
    Flow,
    /// Positioned at an offset from the parent's content origin and excluded from
    /// the parent's sizing entirely. Tooltips, popups, menus. A floating box also
    /// escapes its ancestors' clip rects, which is the whole point of one.
    ///
    /// Paint order is still emission order, so overlays must be emitted late.
    Floating([f32; 2]),
    /// Positioned and sized like [`Placement::Floating`], but clipped by its
    /// ancestors like a box in the flow. Decoration laid over a widget, a
    /// switch's knob or a badge on a card, that has to scroll away with it.
    Absolute([f32; 2]),
}

impl Placement {
    /// Where it sits against the parent's content origin, for a box out of
    /// the flow.
    #[inline]
    fn offset(self) -> Option<[f32; 2]> {
        match self {
            Placement::Flow => None,
            Placement::Floating(at) | Placement::Absolute(at) => Some(at),
        }
    }
}

/// Per-box options that do not affect sizing, only placement and visibility.
///
/// One column rather than five. They are read by the positioning and clipping
/// passes, which run together at the end.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Options {
    /// Cross-axis alignment of this container's children.
    pub align: Align,
    /// Main-axis distribution of leftover space. Irrelevant when a child grows,
    /// because then there is no leftover.
    pub justify: Align,
    /// Confine this box's subtree to its own rect.
    pub clip: bool,
    /// Shift children by this much. Owned by the caller, since scroll position is
    /// persistent state and layout is stateless.
    pub scroll: [f32; 2],
    pub placement: Placement,
    /// Invisible to [`BoxTable::hit`], so the pointer reaches the box under
    /// it. For labels and decoration inside something clickable.
    pub passive: bool,
}

/// Space a container holds back from its children.
///
/// One struct rather than separate columns because all four fields are read by the
/// same three passes, always together.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    /// Inside the box, before the first child, per axis.
    pub min: [f32; 2],
    /// Inside the box, after the last child, per axis.
    pub max: [f32; 2],
    /// Between adjacent children, along the layout axis only.
    pub gap: f32,
}

impl Insets {
    pub const ZERO: Insets = Insets {
        min: [0.0, 0.0],
        max: [0.0, 0.0],
        gap: 0.0,
    };
    pub const fn all(v: f32) -> Insets {
        Insets {
            min: [v, v],
            max: [v, v],
            gap: 0.0,
        }
    }
}

/// What a box is, at emission time.
#[derive(Clone, Copy, Debug)]
pub struct BoxSpec {
    pub size: [SizeSpec; 2],
    pub axis: Axis,
    pub measured: [f32; 2],
    pub pad: Insets,
    pub opts: Options,
}

const DEFAULT_OPTS: Options = Options {
    align: Align::Start,
    justify: Align::Start,
    clip: false,
    scroll: [0.0, 0.0],
    placement: Placement::Flow,
    passive: false,
};

impl BoxSpec {
    /// A container arranging children along `axis`, sized to fit them.
    pub const fn container(axis: Axis) -> Self {
        BoxSpec {
            size: [SizeSpec::children_sum(), SizeSpec::children_sum()],
            axis,
            measured: [0.0, 0.0],
            pad: Insets::ZERO,
            opts: DEFAULT_OPTS,
        }
    }
    /// A childless box. The axis is irrelevant but has to be something.
    pub const fn leaf() -> Self {
        BoxSpec {
            size: [SizeSpec::px(0.0), SizeSpec::px(0.0)],
            axis: Axis::X,
            measured: [0.0, 0.0],
            pad: Insets::ZERO,
            opts: DEFAULT_OPTS,
        }
    }
    /// Cross-axis alignment of this box's children.
    pub const fn align(mut self, a: Align) -> Self {
        self.opts.align = a;
        self
    }
    /// Main-axis distribution of leftover space.
    pub const fn justify(mut self, a: Align) -> Self {
        self.opts.justify = a;
        self
    }
    /// Let the pointer through to whatever is under this box.
    pub const fn passive(mut self) -> Self {
        self.opts.passive = true;
        self
    }
    /// Confine this subtree to this box's rect.
    pub const fn clip(mut self) -> Self {
        self.opts.clip = true;
        self
    }
    /// Shift children. Combine with `clip` for a scroll region; read
    /// [`BoxTable::content`] to know how far scrolling can go.
    pub const fn scroll(mut self, offset: [f32; 2]) -> Self {
        self.opts.scroll = offset;
        self
    }
    /// Take this box out of its parent's flow and place it at `offset` from the
    /// parent's content origin.
    pub const fn floating(mut self, offset: [f32; 2]) -> Self {
        self.opts.placement = Placement::Floating(offset);
        self
    }
    /// Place this box at `offset` from the parent's content origin, out of the
    /// flow, still clipped by its ancestors.
    pub const fn absolute(mut self, offset: [f32; 2]) -> Self {
        self.opts.placement = Placement::Absolute(offset);
        self
    }
    /// Equal padding on all four sides.
    pub const fn pad(mut self, v: f32) -> Self {
        let gap = self.pad.gap;
        self.pad = Insets::all(v);
        self.pad.gap = gap;
        self
    }
    pub const fn pad_xy(mut self, x: f32, y: f32) -> Self {
        self.pad.min = [x, y];
        self.pad.max = [x, y];
        self
    }
    /// Space between adjacent children, along the layout axis.
    pub const fn gap(mut self, g: f32) -> Self {
        self.pad.gap = g;
        self
    }
    pub const fn w(mut self, s: SizeSpec) -> Self {
        self.size[X] = s;
        self
    }
    pub const fn h(mut self, s: SizeSpec) -> Self {
        self.size[Y] = s;
        self
    }
    /// Fill the parent on both axes.
    pub const fn fill(mut self) -> Self {
        self.size = [SizeSpec::grow(), SizeSpec::grow()];
        self
    }
    /// Size to the measured extent on both axes, for text and images.
    pub const fn measured(mut self, w: f32, h: f32) -> Self {
        self.size = [SizeSpec::measured(), SizeSpec::measured()];
        self.measured = [w, h];
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Every column must describe the same number of box slots.
    LengthMismatch,
    /// Emitted more boxes than the table has slots.
    TooManyBoxes,
    /// Nested deeper than the open-box stack allows.
    TooDeep,
    /// `close` without a matching `open`.
    Unbalanced,
    /// `solve` called with boxes still open.
    StillOpen,
}

/// Borrowed columns, one per column, all the same length except `stack`, which
/// bounds nesting depth rather than box count.
pub struct LayoutStorage<'a> {
    pub key: &'a mut [u64],
    pub parent: &'a mut [u32],
    pub subtree_end: &'a mut [u32],
    pub size: &'a mut [[SizeSpec; 2]],
    pub axis: &'a mut [Axis],
    pub measured: &'a mut [[f32; 2]],
    pub pad: &'a mut [Insets],
    pub opts: &'a mut [Options],
    pub computed: &'a mut [[f32; 2]],
    /// Extent the children actually occupy, padding included. Differs from
    /// `computed` whenever a box is sized independently of its content, which is
    /// exactly the scroll case: content larger than the box is how far it scrolls.
    pub content: &'a mut [[f32; 2]],
    pub rect: &'a mut [Rect],
    /// Visible region of each box after its ancestors' clip rects are applied.
    pub clip: &'a mut [Rect],
    pub stack: &'a mut [u32],
}

const NO_PARENT: u32 = u32::MAX;

/// Owns the storage struct rather than borrowing it, so a `BoxTable` can be a
/// field of something larger without that thing becoming self-referential.
/// `LayoutStorage` is itself only borrowed slices, so this moves nothing real.
pub struct BoxTable<'a> {
    s: LayoutStorage<'a>,
    len: u32,
    depth: u32,
}

impl<'a> BoxTable<'a> {
    pub fn new(s: LayoutStorage<'a>) -> Result<Self, LayoutError> {
        let n = s.key.len();
        if s.parent.len() != n
            || s.subtree_end.len() != n
            || s.size.len() != n
            || s.axis.len() != n
            || s.measured.len() != n
            || s.pad.len() != n
            || s.opts.len() != n
            || s.computed.len() != n
            || s.content.len() != n
            || s.rect.len() != n
            || s.clip.len() != n
            || s.stack.is_empty()
        {
            return Err(LayoutError::LengthMismatch);
        }
        Ok(BoxTable {
            s,
            len: 0,
            depth: 0,
        })
    }

    /// Start a frame. Cheap: the columns are overwritten as boxes are emitted, so
    /// there is nothing to clear.
    pub fn reset(&mut self) {
        self.len = 0;
        self.depth = 0;
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn capacity(&self) -> usize {
        self.s.key.len()
    }

    /// Emit a box and descend into it. Must be matched by [`BoxTable::close`].
    pub fn open(&mut self, key: u64, spec: BoxSpec) -> Result<u32, LayoutError> {
        let i = self.push(key, spec)?;
        if self.depth as usize >= self.s.stack.len() {
            return Err(LayoutError::TooDeep);
        }
        self.s.stack[self.depth as usize] = i;
        self.depth += 1;
        Ok(i)
    }

    /// Emit a box with no children.
    pub fn leaf(&mut self, key: u64, spec: BoxSpec) -> Result<u32, LayoutError> {
        let i = self.push(key, spec)?;
        self.s.subtree_end[i as usize] = i + 1;
        Ok(i)
    }

    pub fn close(&mut self) -> Result<(), LayoutError> {
        if self.depth == 0 {
            return Err(LayoutError::Unbalanced);
        }
        self.depth -= 1;
        let i = self.s.stack[self.depth as usize];
        // Everything emitted since `open` is this box's subtree, by construction.
        self.s.subtree_end[i as usize] = self.len;
        Ok(())
    }

    fn push(&mut self, key: u64, spec: BoxSpec) -> Result<u32, LayoutError> {
        let i = self.len;
        if i as usize >= self.s.key.len() {
            return Err(LayoutError::TooManyBoxes);
        }
        let parent = if self.depth == 0 {
            NO_PARENT
        } else {
            self.s.stack[self.depth as usize - 1]
        };
        let u = i as usize;
        self.s.key[u] = key;
        self.s.parent[u] = parent;
        self.s.subtree_end[u] = i + 1;
        self.s.size[u] = spec.size;
        self.s.axis[u] = spec.axis;
        self.s.measured[u] = spec.measured;
        self.s.pad[u] = spec.pad;
        self.s.opts[u] = spec.opts;
        self.s.computed[u] = [0.0, 0.0];
        self.s.content[u] = [0.0, 0.0];
        self.s.rect[u] = Rect::default();
        self.s.clip[u] = Rect::default();
        self.len = i + 1;
        Ok(i)
    }

    pub fn key(&self, i: u32) -> Option<u64> {
        self.s.key.get(i as usize).copied().filter(|_| i < self.len)
    }

    pub fn rect(&self, i: u32) -> Option<Rect> {
        if i >= self.len {
            return None;
        }
        Some(self.s.rect[i as usize])
    }

    /// Rects in emission order, which is painter's order: a box is drawn before its
    /// children, and earlier siblings before later ones.
    pub fn rects(&self) -> impl Iterator<Item = (u64, Rect)> + '_ {
        let n = self.len as usize;
        self.s.key[..n]
            .iter()
            .copied()
            .zip(self.s.rect[..n].iter().copied())
    }

    /// Visible region of a box: its ancestors' clip rects intersected down.
    /// Renderers draw box `i` scissored to this.
    pub fn clip(&self, i: u32) -> Option<Rect> {
        if i >= self.len {
            return None;
        }
        Some(self.s.clip[i as usize])
    }

    /// How much room this box's children want, padding included.
    ///
    /// Subtract [`BoxTable::rect`]'s extent to get the scrollable overflow. The
    /// caller owns the scroll offset, so clamping it is the caller's job.
    pub fn content(&self, i: u32) -> Option<[f32; 2]> {
        if i >= self.len {
            return None;
        }
        Some(self.s.content[i as usize])
    }

    /// Topmost box containing `p`. Reverse emission order, so the last thing painted
    /// is the first thing hit.
    ///
    /// A point inside a box but outside its clip rect is not a hit. Without that,
    /// a scrolled-away list row would still swallow clicks.
    pub fn hit(&self, p: [f32; 2]) -> Option<u64> {
        let n = self.len as usize;
        (0..n)
            .rev()
            .find(|&i| !self.s.opts[i].passive && self.s.rect[i].contains(p) && self.s.clip[i].contains(p))
            .map(|i| self.s.key[i])
    }

    /// Index of the first direct child, and the iteration step, expressed as a
    /// closure-free loop in the callers below:
    ///
    /// ```text
    /// let mut c = i + 1;
    /// while c < subtree_end[i] { ...; c = subtree_end[c]; }
    /// ```
    #[inline]
    fn subtree_end(&self, i: u32) -> u32 {
        self.s.subtree_end[i as usize]
    }

    /// A fraction-of-parent child inside a sum-of-children parent is a cycle: the
    /// parent needs the child's size to compute its own, and the child needs the
    /// parent's.
    ///
    /// It is broken by excluding the child from the parent's sum *and* from the
    /// overflow accounting that follows, resolving it against the parent's final
    /// size in pass 3b, and letting it overflow if it must. Counting it in pass 4
    /// after excluding it in pass 3 would penalise it for space it never claimed.
    /// This is the resolution CSS reaches for a percentage width inside a
    /// shrink-to-fit box, and it has the merit of being predictable.
    #[inline]
    fn excluded_from_sum(&self, c: usize, a: usize) -> bool {
        if !matches!(self.s.size[c][a].kind, Size::ParentFraction(_)) {
            return false;
        }
        let p = self.s.parent[c];
        p != NO_PARENT && matches!(self.s.size[p as usize][a].kind, Size::ChildrenSum)
    }

    /// Children the parent does not account for: floating and absolute boxes,
    /// which are placed by hand, and the fraction-of-a-sum-parent cycle above.
    #[inline]
    fn out_of_flow(&self, c: usize, a: usize) -> bool {
        self.s.opts[c].placement.offset().is_some() || self.excluded_from_sum(c, a)
    }

    #[inline]
    fn is_floating(&self, c: usize) -> bool {
        matches!(self.s.opts[c].placement, Placement::Floating(_))
    }

    /// Resolve every box into an absolute rect.
    ///
    /// `viewport` is the extent the root is measured against, so a root of
    /// `ParentFraction(1.0)` or `Grow` fills the window.
    pub fn solve(&mut self, viewport: [f32; 2]) -> Result<(), LayoutError> {
        if self.depth != 0 {
            return Err(LayoutError::StillOpen);
        }
        if self.len == 0 {
            return Ok(());
        }
        for a in [X, Y] {
            self.pass_standalone(a);
            self.pass_parent_fraction(a, viewport[a]);
            self.pass_children_sum(a);
            self.pass_fraction_of_sum(a);
            self.pass_grow_and_violations(a, viewport[a]);
            self.pass_content(a);
            self.pass_position(a);
        }
        // Clipping is rectangular, so it cannot be done per axis and runs once
        // both are placed.
        self.pass_clip(viewport);
        Ok(())
    }

    /// Pass 1. No dependencies at all, so order is irrelevant and this is the pass
    /// that would parallelise if it ever needed to.
    fn pass_standalone(&mut self, a: usize) {
        for i in 0..self.len as usize {
            let spec = self.s.size[i][a];
            self.s.computed[i][a] = match spec.kind {
                Size::Px(v) => spec.clamp(v),
                Size::Measured => spec.clamp(self.s.measured[i][a]),
                // Resolved later, but the floor applies from the start so a box
                // that never gets a chance to grow still honours it.
                _ => fmax(spec.min, 0.0),
            };
        }
    }

    /// Pass 2. Forward, because a parent precedes its children in DFS order and is
    /// therefore already resolved, unless it is `ChildrenSum`. Those are left at zero
    /// here and picked up by pass 3b.
    fn pass_parent_fraction(&mut self, a: usize, viewport: f32) {
        for i in 0..self.len as usize {
            let Size::ParentFraction(f) = self.s.size[i][a].kind else {
                continue;
            };
            let p = self.s.parent[i];
            let basis = if p == NO_PARENT {
                viewport
            } else {
                if matches!(self.s.size[p as usize][a].kind, Size::ChildrenSum) {
                    continue;
                }
                self.s.computed[p as usize][a]
            };
            self.s.computed[i][a] = self.s.size[i][a].clamp(basis * f);
        }
    }

    /// Pass 3. Reverse, so every child is resolved before its parent is asked to sum
    /// them. This is the pass that would need recursion in a tree of pointers and
    /// needs nothing here but iterating backwards.
    fn pass_children_sum(&mut self, a: usize) {
        for i in (0..self.len).rev() {
            if !matches!(self.s.size[i as usize][a].kind, Size::ChildrenSum) {
                continue;
            }
            let along = self.s.axis[i as usize].index() == a;
            let pad = self.s.pad[i as usize];
            let end = self.subtree_end(i);
            let mut acc = 0.0f32;
            let mut counted = 0u32;
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                if !self.out_of_flow(cu, a) {
                    counted += 1;
                    if along {
                        acc += self.s.computed[cu][a];
                    } else {
                        acc = fmax(acc, self.s.computed[cu][a]);
                    }
                }
                c = self.subtree_end(c);
            }
            if along && counted > 1 {
                acc += pad.gap * (counted - 1) as f32;
            }
            let total = acc + pad.min[a] + pad.max[a];
            self.s.computed[i as usize][a] = self.s.size[i as usize][a].clamp(total);
        }
    }

    /// Pass 3b. The other half of the cycle break: fraction children whose parent is
    /// `ChildrenSum` now have a parent extent to be a fraction of.
    fn pass_fraction_of_sum(&mut self, a: usize) {
        for i in 0..self.len as usize {
            let Size::ParentFraction(f) = self.s.size[i][a].kind else {
                continue;
            };
            let p = self.s.parent[i];
            if p == NO_PARENT || !matches!(self.s.size[p as usize][a].kind, Size::ChildrenSum) {
                continue;
            }
            // The content box, matching pass 4. Using the full extent here made
            // a fraction of a padded `ChildrenSum` parent answer differently
            // from a fraction of a padded `Px` one, purely because a different
            // pass resolved it.
            let pu = p as usize;
            let pad = self.s.pad[pu];
            let basis = fmax(self.s.computed[pu][a] - pad.min[a] - pad.max[a], 0.0);
            self.s.computed[i][a] = self.s.size[i][a].clamp(basis * f);
        }
    }

    /// Pass 4. Forward, one parent at a time: hand `Grow` children the leftover, then
    /// take back any overflow from whoever resists least.
    fn pass_grow_and_violations(&mut self, a: usize, viewport: f32) {
        for i in 0..self.len {
            let iu = i as usize;
            // The root has no parent to distribute from; the viewport stands in.
            if self.s.parent[iu] == NO_PARENT && matches!(self.s.size[iu][a].kind, Size::Grow) {
                self.s.computed[iu][a] = self.s.size[iu][a].clamp(viewport);
            }
            let end = self.subtree_end(i);
            if end <= i + 1 {
                continue;
            }
            let along = self.s.axis[iu].index() == a;
            let pad = self.s.pad[iu];
            // What is left of the box once its own padding is taken out.
            let inner = fmax(self.s.computed[iu][a] - pad.min[a] - pad.max[a], 0.0);

            // Resolve fraction children here, where the parent is final.
            //
            // Pass 2 can only resolve them against parents whose own size was
            // known by then. A `Grow` parent is not: it gets its size from its
            // own parent in this pass, so pass 2 divided by zero-ish and the
            // child silently collapsed. That made the obvious way to size a
            // scrollbar thumb, a fraction of a growing track, produce nothing.
            //
            // Idempotent for parents that were already resolved, since the
            // basis is the same. `ChildrenSum` parents are excluded: those are
            // a real cycle, broken in pass 3b.
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                if let Size::ParentFraction(f) = self.s.size[cu][a].kind {
                    if !self.excluded_from_sum(cu, a) {
                        self.s.computed[cu][a] = self.s.size[cu][a].clamp(inner * f);
                    }
                }
                c = self.subtree_end(c);
            }

            let mut fixed = 0.0f32;
            let mut grow_count = 0u32;
            let mut counted = 0u32;
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                if self.out_of_flow(cu, a) {
                    // Never claimed space, so it is not taking any from the others.
                } else {
                    counted += 1;
                    if matches!(self.s.size[cu][a].kind, Size::Grow) {
                        grow_count += 1;
                    } else if along {
                        fixed += self.s.computed[cu][a];
                    }
                }
                c = self.subtree_end(c);
            }

            // Gaps are spent before children get a say, on the layout axis only.
            let gaps = if along && counted > 1 {
                pad.gap * (counted - 1) as f32
            } else {
                0.0
            };
            let available = fmax(inner - gaps, 0.0);

            let share = if along {
                fmax(available - fixed, 0.0) / fmax(grow_count as f32, 1.0)
            } else {
                available
            };
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                if matches!(self.s.size[cu][a].kind, Size::Grow) {
                    // A floating box shares with nobody, so it takes the whole
                    // content box rather than a slice of the leftover.
                    let v = if self.out_of_flow(cu, a) {
                        inner
                    } else {
                        share
                    };
                    self.s.computed[cu][a] = self.s.size[cu][a].clamp(v);
                }
                c = self.subtree_end(c);
            }

            if along {
                self.solve_violations(i, end, a, available);
            } else {
                self.clamp_cross_axis(i, end, a, available);
            }
        }
    }

    /// Shrink children proportionally to size times give, where give is
    /// `1 - strictness`. A row of strict children simply overflows, which is the
    /// honest outcome and visible rather than silently clipped.
    fn solve_violations(&mut self, i: u32, end: u32, a: usize, available: f32) {
        let mut total = 0.0f32;
        let mut c = i + 1;
        while c < end {
            let cu = c as usize;
            if !self.out_of_flow(cu, a) {
                total += self.s.computed[cu][a];
            }
            c = self.subtree_end(c);
        }
        let overflow = total - available;
        if overflow <= 0.0 {
            return;
        }
        let mut weight = 0.0f32;
        let mut c = i + 1;
        while c < end {
            let cu = c as usize;
            if !self.out_of_flow(cu, a) {
                weight += self.s.computed[cu][a] * self.give(cu, a);
            }
            c = self.subtree_end(c);
        }
        if weight <= 0.0 {
            return;
        }
        let mut c = i + 1;
        while c < end {
            let cu = c as usize;
            if !self.out_of_flow(cu, a) {
                let share = self.s.computed[cu][a] * self.give(cu, a) / weight;
                let shrunk = self.s.computed[cu][a] - overflow * share;
                // The floor wins. A child that cannot shrink far enough leaves the
                // parent overflowing, same as an all-strict row.
                self.s.computed[cu][a] = fmax(shrunk, fmax(self.s.size[cu][a].min, 0.0));
            }
            c = self.subtree_end(c);
        }
    }

    /// On the cross axis children do not accumulate, so each is clamped on its own,
    /// and only as far as its strictness allows.
    fn clamp_cross_axis(&mut self, i: u32, end: u32, a: usize, available: f32) {
        let mut c = i + 1;
        while c < end {
            let cu = c as usize;
            let v = self.s.computed[cu][a];
            if v > available && !self.out_of_flow(cu, a) {
                let shrunk = v - (v - available) * self.give(cu, a);
                self.s.computed[cu][a] = fmax(shrunk, fmax(self.s.size[cu][a].min, 0.0));
            }
            c = self.subtree_end(c);
        }
    }

    /// How far a box will be pushed, as the complement of its strictness.
    #[inline]
    fn give(&self, c: usize, a: usize) -> f32 {
        1.0 - fmin(fmax(self.s.size[c][a].strictness, 0.0), 1.0)
    }

    /// Pass 4b. Reverse again, now that every size is final, to record how much
    /// room the children actually want.
    ///
    /// Pass 3 already did this for `ChildrenSum` boxes, but only from sizes known
    /// at that point. A `Grow` container's children are not resolved until pass 4,
    /// and a scroll region is exactly a box whose size is decided independently of
    /// its content. `content` minus `computed` is how far it can scroll.
    fn pass_content(&mut self, a: usize) {
        for i in (0..self.len).rev() {
            let iu = i as usize;
            let end = self.subtree_end(i);
            if end <= i + 1 {
                self.s.content[iu][a] = self.s.computed[iu][a];
                continue;
            }
            let along = self.s.axis[iu].index() == a;
            let pad = self.s.pad[iu];
            let mut acc = 0.0f32;
            let mut counted = 0u32;
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                if !self.out_of_flow(cu, a) {
                    counted += 1;
                    if along {
                        acc += self.s.computed[cu][a];
                    } else {
                        acc = fmax(acc, self.s.computed[cu][a]);
                    }
                }
                c = self.subtree_end(c);
            }
            if along && counted > 1 {
                acc += pad.gap * (counted - 1) as f32;
            }
            self.s.content[iu][a] = acc + pad.min[a] + pad.max[a];
        }
    }

    /// Pass 5. Forward, so a parent's absolute origin is final before its children
    /// are placed against it. Relative positions never need storing.
    fn pass_position(&mut self, a: usize) {
        for i in 0..self.len {
            let iu = i as usize;
            // Roots are reached before their children, so placing them here rather
            // than in a pass of their own costs nothing and handles several of them.
            if self.s.parent[iu] == NO_PARENT {
                self.s.rect[iu].min[a] = 0.0;
                self.s.rect[iu].max[a] = self.s.computed[iu][a];
            }
            let end = self.subtree_end(i);
            if end <= i + 1 {
                continue;
            }
            let along = self.s.axis[iu].index() == a;
            let pad = self.s.pad[iu];
            let opts = self.s.opts[iu];
            let content_origin = self.s.rect[iu].min[a] + pad.min[a];
            let inner = fmax(self.s.computed[iu][a] - pad.min[a] - pad.max[a], 0.0);

            // Leftover along the layout axis, which is what justify distributes.
            // Zero whenever a child grows, since growing consumed it.
            let used = fmax(self.s.content[iu][a] - pad.min[a] - pad.max[a], 0.0);
            let justify_off = if along {
                opts.justify.offset(inner - used)
            } else {
                0.0
            };

            let start = content_origin - opts.scroll[a] + justify_off;
            let mut cursor = 0.0f32;
            let mut c = i + 1;
            while c < end {
                let cu = c as usize;
                let extent = self.s.computed[cu][a];
                if let Some(offset) = self.s.opts[cu].placement.offset() {
                    // Placed by hand against the content origin, and it does not
                    // move the cursor for whoever comes next.
                    self.s.rect[cu].min[a] = content_origin + offset[a];
                    self.s.rect[cu].max[a] = content_origin + offset[a] + extent;
                } else {
                    let cross_off = if along {
                        0.0
                    } else {
                        opts.align.offset(inner - extent)
                    };
                    self.s.rect[cu].min[a] = start + cursor + cross_off;
                    self.s.rect[cu].max[a] = self.s.rect[cu].min[a] + extent;
                    if along {
                        cursor += extent + pad.gap;
                    }
                }
                c = self.subtree_end(c);
            }
        }
    }

    /// Pass 6. Both axes at once, since a clip rect is a rectangle rather than two
    /// independent intervals.
    ///
    /// Forward, so a parent's clip is final before its children inherit it. A box
    /// that clips intersects its own rect into what it passes down; a floating box
    /// starts over from the viewport, because a popup that its parent could clip
    /// would be useless.
    fn pass_clip(&mut self, viewport: [f32; 2]) {
        let root_clip = Rect {
            min: [0.0, 0.0],
            max: viewport,
        };
        for i in 0..self.len as usize {
            let p = self.s.parent[i];
            let inherited = if p == NO_PARENT || self.is_floating(i) {
                root_clip
            } else {
                self.s.clip[p as usize]
            };
            self.s.clip[i] = if self.s.opts[i].clip {
                intersect(inherited, self.s.rect[i])
            } else {
                inherited
            };
        }
    }
}

/// Overlap of two rects, empty (min == max) when they do not meet.
fn intersect(a: Rect, b: Rect) -> Rect {
    let min = [fmax(a.min[X], b.min[X]), fmax(a.min[Y], b.min[Y])];
    let max = [fmin(a.max[X], b.max[X]), fmin(a.max[Y], b.max[Y])];
    Rect {
        min,
        max: [fmax(min[X], max[X]), fmax(min[Y], max[Y])],
    }
}

#[cfg(test)]
mod tests;
