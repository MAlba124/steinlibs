//! A clipping scroll container with a real scrollbar.
//!
//! Everything interaction needs from geometry, the viewport rect, the content
//! extent and the track span, is read at begin time from the previous frame's
//! solve, which `end_frame` records into `WidgetState` for every key that
//! opted in by asking for state. That is the same one frame lag hit testing
//! already accepts, and it is why there is no post frame phase here. A frame
//! emitted against geometry the solve then contradicted, a resize, is always
//! damaged, and a damaged present is followed by another build, which reads
//! the corrected geometry and settles.
//!
//! The bar is sized in parent fractions, so the thumb tracks the solved
//! track extent in the same frame. Only the visible fraction itself is a
//! frame behind.

use stein_layout::{Axis, BoxSpec, Rect, SizeSpec};
use stein_theme::Theme;
use stein_ui::Ui;
use stein_ui::key::{child, hash_str};

use crate::WidgetError;
use crate::util::{axis_index, axis_of, clampf, cross_of, fmax, fmin, load, sized, store};

/// Bar metrics and fills. Alpha lives in the top byte, zero alpha never paints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollStyle {
    pub bar_size: f32,
    /// Floor on the thumb in pixels, so a huge document keeps a grabbable thumb.
    pub min_thumb: f32,
    /// Pixels per wheel unit. Positive wheel moves content toward the start.
    pub wheel_step: f32,
    pub track_fill: u32,
    pub thumb_fill: u32,
    pub thumb_hot_fill: u32,
}

impl Default for ScrollStyle {
    fn default() -> Self {
        ScrollStyle::from(&Theme::default())
    }
}

impl From<&Theme> for ScrollStyle {
    fn from(t: &Theme) -> Self {
        ScrollStyle {
            bar_size: t.size.scrollbar,
            min_thumb: t.size.min_thumb,
            wheel_step: 30.0,
            track_fill: t.pane,
            thumb_fill: t.faint,
            thumb_hot_fill: t.dim,
        }
    }
}

pub struct ScrollArea {
    key: u64,
    a: usize,
    style: ScrollStyle,
    offset: f32,
    view_len: f32,
    content_len: f32,
    view_rect: Rect,
    viewport_index: u32,
    track_index: u32,
    thumb_index: u32,
}

/// Flag bits in `user[0]` of the main key's state. `user[1]` holds the
/// offset at the start of a thumb drag, as f32 bits.
const HELD_THUMB: u64 = 1;
const HELD_TRACK: u64 = 2;

fn k_vp(key: u64) -> u64 {
    child(key, hash_str("scroll.viewport"))
}
fn k_track(key: u64) -> u64 {
    child(key, hash_str("scroll.track"))
}
fn k_space(key: u64) -> u64 {
    child(key, hash_str("scroll.space"))
}
fn k_thumb(key: u64) -> u64 {
    child(key, hash_str("scroll.thumb"))
}

impl ScrollArea {
    /// Opens the outer box and the clipped viewport. Emit content into it,
    /// then close with [`ScrollArea::end`].
    ///
    /// The content extent comes from the solve. Spacer boxes, the ones a
    /// virtualized list emits for its offscreen rows, count toward it, so no
    /// caller has to declare a content size.
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        axis: Axis,
        size: [SizeSpec; 2],
        style: ScrollStyle,
    ) -> Result<ScrollArea, WidgetError> {
        if style.bar_size <= 0.0 {
            return Err(WidgetError::BarSizeNotPositive);
        }
        let mut sa = ScrollArea {
            key,
            a: axis_index(axis),
            style,
            offset: 0.0,
            view_len: 0.0,
            content_len: 0.0,
            view_rect: Rect::default(),
            viewport_index: 0,
            track_index: 0,
            thumb_index: 0,
        };
        sa.read_geometry(ui)?;
        sa.interact(ui)?;
        sa.emit_open(ui, size)?;
        Ok(sa)
    }

    /// Asking for the viewport key's state is what opts it into geometry
    /// recording, so this doubles as the subscription.
    fn read_geometry(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let vp = load(ui, k_vp(self.key))?;
        self.view_rect = vp.last_rect;
        self.view_len = fmax(self.view_rect.max[self.a] - self.view_rect.min[self.a], 0.0);
        self.content_len = fmax(vp.last_content[self.a], 0.0);
        Ok(())
    }

    fn max_scroll_of(&self) -> f32 {
        fmax(self.content_len - self.view_len, 0.0)
    }

    /// Thumb extent as a fraction of the track. The track spans the same
    /// range as the viewport by construction, which is what converts the
    /// pixel floor into a fraction.
    fn thumb_frac(&self) -> f32 {
        if self.content_len <= self.view_len || self.content_len <= 0.0 {
            return 1.0;
        }
        let mut f = self.view_len / self.content_len;
        if self.view_len > 0.0 {
            f = fmax(f, self.style.min_thumb / self.view_len);
        }
        fmin(f, 1.0)
    }

    fn interact(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let input = ui.input();
        let max_scroll = self.max_scroll_of();
        let mut st = load(ui, self.key)?;
        if st.is_fresh(ui.frame_index()) {
            // Nothing is sized or hoverable until the first solve lands, so
            // the frame that reads it is owed now.
            ui.request_repaint();
        }
        self.offset = st.scroll[self.a];
        if self.view_rect.contains(input.pointer) {
            self.offset -= input.wheel[self.a] * self.style.wheel_step;
        }
        let movable = self.view_len * (1.0 - self.thumb_frac());
        let held = ui.peek_active(k_thumb(self.key));
        if held && st.user[0] & HELD_THUMB == 0 {
            st.user[1] = u64::from(self.offset.to_bits());
        }
        if held && movable > 0.0 {
            // The offset anchored at the press plus the drag total, so a long
            // drag cannot accumulate per frame rounding.
            let anchor = f32::from_bits(st.user[1] as u32);
            let total = ui.peek_drag(k_thumb(self.key)).total[self.a];
            self.offset = anchor + total * (max_scroll / movable);
        }
        self.offset = clampf(self.offset, 0.0, max_scroll);
        let track_held = ui.peek_active(k_track(self.key)) || ui.peek_active(k_space(self.key));
        if track_held && st.user[0] & HELD_TRACK == 0 {
            self.page(input.pointer[self.a], movable, max_scroll);
        }
        self.offset = clampf(self.offset, 0.0, max_scroll);
        st.user[0] = u64::from(held) * HELD_THUMB + u64::from(track_held) * HELD_TRACK;
        st.scroll[self.a] = self.offset;
        store(ui, self.key, st)
    }

    /// One page per press, toward the click relative to the thumb. The caller
    /// gates on the press edge, and counts the lead spacer as track since it
    /// is a hit target covering the track above the thumb.
    fn page(&mut self, p: f32, movable: f32, max_scroll: f32) {
        if max_scroll <= 0.0 {
            return;
        }
        let thumb_len = self.view_len * self.thumb_frac();
        let thumb_start = self.view_rect.min[self.a] + movable * self.offset / max_scroll;
        if p < thumb_start {
            self.offset -= self.view_len;
        } else if p > thumb_start + thumb_len {
            self.offset += self.view_len;
        }
    }

    fn emit_open(&mut self, ui: &mut Ui, size: [SizeSpec; 2]) -> Result<(), WidgetError> {
        ui.open(
            self.key,
            BoxSpec::container(cross_of(self.a)).w(size[0]).h(size[1]),
            0,
        )?;
        let mut off = [0.0, 0.0];
        off[self.a] = self.offset;
        ui.open(
            k_vp(self.key),
            BoxSpec::container(axis_of(self.a))
                .w(SizeSpec::grow())
                .h(SizeSpec::grow())
                .clip()
                .scroll(off),
            0,
        )?;
        self.viewport_index = ui.last_index();
        Ok(())
    }

    /// Closes the viewport, emits the bar, closes the outer box.
    pub fn end(mut self, ui: &mut Ui) -> Result<ScrollArea, WidgetError> {
        ui.close()?;
        self.emit_bar(ui)?;
        ui.close()?;
        Ok(self)
    }

    /// Bar parts are fractions of the track, so they follow the solved track
    /// in the same frame. Only the visible fraction lags by one.
    fn emit_bar(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let frac = self.thumb_frac();
        let max_scroll = self.max_scroll_of();
        let lead = if max_scroll > 0.0 {
            (1.0 - frac) * self.offset / max_scroll
        } else {
            0.0
        };
        let ts = sized(self.a, SizeSpec::grow(), SizeSpec::px(self.style.bar_size));
        ui.open(
            k_track(self.key),
            BoxSpec::container(axis_of(self.a)).w(ts[0]).h(ts[1]),
            self.style.track_fill,
        )?;
        self.track_index = ui.last_index();
        let sp = sized(self.a, SizeSpec::fraction(lead), SizeSpec::grow());
        ui.leaf(k_space(self.key), BoxSpec::leaf().w(sp[0]).h(sp[1]), 0)?;
        let hot = ui.peek_hovered(k_thumb(self.key)) || ui.peek_active(k_thumb(self.key));
        let fill = if hot {
            self.style.thumb_hot_fill
        } else {
            self.style.thumb_fill
        };
        let th = sized(self.a, SizeSpec::fraction(frac), SizeSpec::grow());
        ui.leaf(k_thumb(self.key), BoxSpec::leaf().w(th[0]).h(th[1]), fill)?;
        self.thumb_index = ui.last_index();
        ui.close()?;
        Ok(())
    }

    /// Clamped offset applied to this frame's viewport.
    pub fn offset(&self) -> f32 {
        self.offset
    }
    /// Viewport extent along the scroll axis, from the previous frame's solve.
    pub fn view_len(&self) -> f32 {
        self.view_len
    }
    /// Content extent along the scroll axis, from the previous frame's solve.
    pub fn content_len(&self) -> f32 {
        self.content_len
    }
    pub fn max_scroll(&self) -> f32 {
        self.max_scroll_of()
    }
    pub fn key(&self) -> u64 {
        self.key
    }
    pub fn viewport_index(&self) -> u32 {
        self.viewport_index
    }
    /// Valid after [`ScrollArea::end`].
    pub fn track_index(&self) -> u32 {
        self.track_index
    }
    /// Valid after [`ScrollArea::end`].
    pub fn thumb_index(&self) -> u32 {
        self.thumb_index
    }
}
