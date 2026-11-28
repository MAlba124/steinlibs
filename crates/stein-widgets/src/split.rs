//! Two panes and a draggable divider.
//!
//! The split fraction is persistent state. The minimum pane size is enforced
//! in pixels against the splitter's own extent, read at begin time from the
//! previous frame's solve. The first pane is a parent fraction, so it keeps
//! its proportion through a resize in the same frame, and only the minimum
//! clamp is a frame behind.
//!
//! # A pane runs across the split
//!
//! Each pane is a container on the **cross** axis, so a `Y` splitter hands back
//! panes that lay their children out left to right. That is the right default
//! for the common case of one child, and for a row of them, but it means a
//! caller that wants a bar above its content has to open its own `Y` container
//! inside the pane. Emitting both straight into the pane puts them side by
//! side, and the second one covers the first: the symptom is a bar that draws
//! but does not respond to clicks past whatever the content does not overlap.

use stein_layout::{Axis, BoxSpec, SizeSpec};
use stein_theme::Theme;
use stein_ui::key::{child, hash_str};
use stein_ui::{Response, Ui};

use crate::WidgetError;
use crate::util::{axis_index, axis_of, clampf, cross_of, fmax, load, sized, store};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitterStyle {
    pub divider: f32,
    pub min_pane: f32,
    pub divider_fill: u32,
    pub divider_hot_fill: u32,
}

impl Default for SplitterStyle {
    fn default() -> Self {
        SplitterStyle::from(&Theme::default())
    }
}

impl From<&Theme> for SplitterStyle {
    fn from(t: &Theme) -> Self {
        SplitterStyle {
            divider: t.size.divider,
            min_pane: 48.0,
            divider_fill: t.bg,
            divider_hot_fill: t.accent,
        }
    }
}

pub struct Splitter {
    key: u64,
    a: usize,
    style: SplitterStyle,
    f: f32,
    total: f32,
    outer_index: u32,
    pane_a_index: u32,
    divider_index: u32,
    pane_b_index: u32,
}

/// Flag bit in `user[0]`. `user[1]` holds the fraction at the start of a
/// drag, as f32 bits.
const HELD: u64 = 1;

fn k_div(key: u64) -> u64 {
    child(key, hash_str("split.divider"))
}
fn k_pane_a(key: u64) -> u64 {
    child(key, hash_str("split.pane_a"))
}
fn k_pane_b(key: u64) -> u64 {
    child(key, hash_str("split.pane_b"))
}

/// When the box is too small for both minimums the middle is the least wrong
/// answer, and the honest overflow stays visible.
fn clamped_fraction(f: f32, lo: f32, hi: f32) -> f32 {
    if lo > hi {
        return 0.5;
    }
    clampf(f, lo, hi)
}

impl Splitter {
    /// Opens the outer box and the first pane. Fill pane one, call
    /// [`Splitter::separator`], fill pane two, then [`Splitter::end`].
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        axis: Axis,
        size: [SizeSpec; 2],
        initial: f32,
        style: SplitterStyle,
    ) -> Result<Splitter, WidgetError> {
        if style.divider <= 0.0 {
            return Err(WidgetError::DividerNotPositive);
        }
        if style.min_pane < 0.0 {
            return Err(WidgetError::MinPaneNegative);
        }
        if initial <= 0.0 || initial >= 1.0 {
            return Err(WidgetError::InitialFractionOutOfRange);
        }
        let mut sp = Splitter {
            key,
            a: axis_index(axis),
            style,
            f: initial,
            total: 0.0,
            outer_index: 0,
            pane_a_index: 0,
            divider_index: 0,
            pane_b_index: 0,
        };
        sp.interact(ui, initial)?;
        sp.emit_open(ui, size)
    }

    fn usable(&self) -> f32 {
        self.total - self.style.divider
    }

    fn interact(&mut self, ui: &mut Ui, initial: f32) -> Result<(), WidgetError> {
        let mut st = load(ui, self.key)?;
        self.total = fmax(st.last_rect.max[self.a] - st.last_rect.min[self.a], 0.0);
        let mut f = st.scroll[0];
        if st.is_fresh(ui.frame_index()) {
            // The panes are sized blind until the first solve lands, so the
            // frame that reads it is owed now.
            f = initial;
            ui.request_repaint();
        }
        let held = ui.peek_active(k_div(self.key));
        if held && st.user[0] & HELD == 0 {
            st.user[1] = u64::from(f.to_bits());
        }
        if held && self.usable() > 0.0 {
            let anchor = f32::from_bits(st.user[1] as u32);
            f = anchor + ui.peek_drag(k_div(self.key)).total[self.a] / self.usable();
        }
        st.user[0] = u64::from(held) * HELD;
        if self.usable() > 0.0 {
            let lo = self.style.min_pane / self.usable();
            let hi = 1.0 - self.style.min_pane / self.usable();
            f = clamped_fraction(f, lo, hi);
        }
        self.f = clampf(f, 0.0, 1.0);
        st.scroll[0] = self.f;
        store(ui, self.key, st)
    }

    /// The first pane is a fraction of the whole box, so a resize scales it
    /// proportionally in the same frame. The fraction is rescaled to exclude
    /// the divider, which keeps `f` meaning a share of the usable extent.
    fn emit_open(mut self, ui: &mut Ui, size: [SizeSpec; 2]) -> Result<Splitter, WidgetError> {
        ui.open(
            self.key,
            BoxSpec::container(axis_of(self.a)).w(size[0]).h(size[1]),
            0,
        )?;
        self.outer_index = ui.last_index();
        let a_frac = if self.total > 0.0 {
            self.f * self.usable() / self.total
        } else {
            self.f
        };
        let s = sized(self.a, SizeSpec::fraction(a_frac), SizeSpec::grow());
        ui.open(
            k_pane_a(self.key),
            BoxSpec::container(cross_of(self.a)).w(s[0]).h(s[1]).clip(),
            0,
        )?;
        self.pane_a_index = ui.last_index();
        Ok(self)
    }

    /// Closes pane one, emits the divider, opens pane two.
    pub fn separator(&mut self, ui: &mut Ui) -> Result<Response, WidgetError> {
        ui.close()?;
        let k = k_div(self.key);
        let hot = ui.peek_hovered(k) || ui.peek_active(k);
        let fill = if hot {
            self.style.divider_hot_fill
        } else {
            self.style.divider_fill
        };
        let ds = sized(self.a, SizeSpec::px(self.style.divider), SizeSpec::grow());
        let r = ui.leaf(k, BoxSpec::leaf().w(ds[0]).h(ds[1]), fill)?;
        self.divider_index = ui.last_index();
        let bs = sized(self.a, SizeSpec::grow(), SizeSpec::grow());
        ui.open(
            k_pane_b(self.key),
            BoxSpec::container(cross_of(self.a))
                .w(bs[0])
                .h(bs[1])
                .clip(),
            0,
        )?;
        self.pane_b_index = ui.last_index();
        Ok(r)
    }

    pub fn end(self, ui: &mut Ui) -> Result<Splitter, WidgetError> {
        ui.close()?;
        ui.close()?;
        Ok(self)
    }

    /// Fraction of the usable extent given to the first pane.
    pub fn fraction(&self) -> f32 {
        self.f
    }
    pub fn outer_index(&self) -> u32 {
        self.outer_index
    }
    pub fn pane_a_index(&self) -> u32 {
        self.pane_a_index
    }
    /// Valid after [`Splitter::separator`].
    pub fn divider_index(&self) -> u32 {
        self.divider_index
    }
    /// Valid after [`Splitter::separator`].
    pub fn pane_b_index(&self) -> u32 {
        self.pane_b_index
    }
}
