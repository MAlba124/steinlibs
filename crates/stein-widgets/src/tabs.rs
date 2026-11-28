//! A row of tabs with one selected.
//!
//! Selection is a caller supplied stable id, not a position, so it survives
//! tabs being reordered or closed. The id lives in the state's `user` words,
//! which stein-ui treats as opaque, so nothing upstream can corrupt it.
//!
//! # A tab that does not fit is still reachable
//!
//! More tabs than the bar is wide is the normal case, not the edge case: the
//! bar shrinks with its pane and the tab set grows with the data. An
//! overflowing row that simply clips is the worst outcome, because the tabs
//! past the edge draw as if they were there and silently ignore clicks. So the
//! bar scrolls: the strip sizes to its children inside a clipped viewport, the
//! wheel moves it, and selecting a tab that sits outside brings it in.

use stein_layout::{Axis, BoxSpec, SizeSpec};
use stein_theme::Theme;
use stein_ui::key::{hash_str, seeded};
use stein_ui::{Response, Ui};

use crate::WidgetError;
use crate::util::{clampf, fmax, fmin, load, store};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabsStyle {
    pub height: f32,
    pub pad: f32,
    pub gap: f32,
    pub bar_fill: u32,
    pub tab_fill: u32,
    pub tab_hot_fill: u32,
    pub tab_active_fill: u32,
    /// Clip the strip and scroll it rather than letting it overflow.
    pub scrollable: bool,
    /// Pixels per wheel notch over the bar.
    pub wheel_step: f32,
}

impl Default for TabsStyle {
    fn default() -> Self {
        TabsStyle::from(&Theme::default())
    }
}

impl From<&Theme> for TabsStyle {
    fn from(t: &Theme) -> Self {
        TabsStyle {
            height: 28.0,
            pad: 4.0,
            gap: 4.0,
            bar_fill: t.bg,
            tab_fill: t.bg,
            tab_hot_fill: t.hover,
            tab_active_fill: t.pane,
            scrollable: true,
            wheel_step: 60.0,
        }
    }
}

/// What one tab reported back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabResponse {
    /// Selected after this frame's clicks are applied.
    pub active: bool,
    pub clicked: bool,
    /// Box index, for painting a label after `end_frame`.
    pub index: u32,
    pub response: Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabsResult {
    /// None only when no tabs were emitted.
    pub active: Option<u64>,
    /// Selection changed this frame, by click or fallback.
    pub changed: bool,
}

pub struct Tabs {
    key: u64,
    style: TabsStyle,
    selected: Option<u64>,
    first_seen: Option<u64>,
    selected_seen: bool,
    changed: bool,
    /// Left edge of the next tab within the strip, in strip coordinates.
    cursor: f32,
    /// Span of the selected tab within the strip, for revealing it.
    active_span: Option<[f32; 2]>,
    offset: f32,
    view_w: f32,
    /// The selection the offset was last brought to, if any.
    revealed: Option<u64>,
}

/// `user[2]` holds the scroll offset as f32 bits. `user[3]` holds the id the
/// bar last scrolled to, plus one, so zero means it has never revealed one.
const OFFSET: usize = 2;
const REVEALED: usize = 3;

fn strip_key(key: u64) -> u64 {
    seeded(key, hash_str("tabs.strip"), 0)
}

/// `user[0]` is the selected id, `user[1]` nonzero means one is stored.
const SELECTED_ID: usize = 0;
const HAS_SELECTION: usize = 1;

fn tab_key(key: u64, id: u64) -> u64 {
    seeded(key, hash_str("tabs.tab"), id)
}

impl Tabs {
    /// Opens the bar. Emit tabs with [`Tabs::item`], close with [`Tabs::end`].
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        width: SizeSpec,
        style: TabsStyle,
    ) -> Result<Tabs, WidgetError> {
        if style.height <= 0.0 {
            return Err(WidgetError::TabHeightNotPositive);
        }
        let mut st = load(ui, key)?;
        let selected = if st.user[HAS_SELECTION] != 0 {
            Some(st.user[SELECTED_ID])
        } else {
            None
        };
        let view = st.last_rect;
        let view_w = fmax(view.max[0] - view.min[0], 0.0);
        let mut offset = f32::from_bits(st.user[OFFSET] as u32);
        if !offset.is_finite() {
            offset = 0.0;
        }
        if style.scrollable && view.contains(ui.input().pointer) {
            offset -= ui.input().wheel[1] / stein_ui::WHEEL_NOTCH * style.wheel_step;
        }
        st.user[OFFSET] = u64::from(fmax(offset, 0.0).to_bits());
        store(ui, key, st)?;
        // `scroll` rather than a floating strip: a floating box deliberately
        // starts its clip over from the viewport so a popup cannot be clipped
        // away, which for a scrolled strip means the tabs escape the bar.
        ui.open(
            key,
            BoxSpec::container(Axis::X)
                .w(width)
                .h(SizeSpec::px(style.height))
                .clip()
                .scroll([fmax(offset, 0.0), 0.0]),
            style.bar_fill,
        )?;
        // The strip sizes to its tabs, so overflow is real rather than
        // squeezed, and the clip above is what makes scrolling meaningful.
        let strip_w = if style.scrollable {
            SizeSpec::children_sum()
        } else {
            SizeSpec::grow()
        };
        ui.open(
            strip_key(key),
            BoxSpec::container(Axis::X)
                .w(strip_w)
                .h(SizeSpec::grow())
                .pad_xy(style.pad, style.pad)
                .gap(style.gap),
            0,
        )?;
        Ok(Tabs {
            key,
            style,
            selected,
            first_seen: None,
            selected_seen: false,
            changed: false,
            cursor: style.pad,
            active_span: None,
            offset: fmax(offset, 0.0),
            view_w,
            revealed: match st.user[REVEALED] {
                0 => None,
                v => Some(v - 1),
            },
        })
    }

    /// One tab. `width` is a pre measured extent, this crate never measures
    /// text. A click updates the stored selection and shows next frame.
    pub fn item(&mut self, ui: &mut Ui, id: u64, width: f32) -> Result<TabResponse, WidgetError> {
        let k = tab_key(self.key, id);
        if self.first_seen.is_none() {
            self.first_seen = Some(id);
        }
        let was_active = self.selected == Some(id);
        if was_active {
            self.selected_seen = true;
        }
        let fill = if was_active {
            self.style.tab_active_fill
        } else if ui.peek_hovered(k) || ui.peek_active(k) {
            self.style.tab_hot_fill
        } else {
            self.style.tab_fill
        };
        let r = ui.leaf(
            k,
            BoxSpec::leaf().w(SizeSpec::px(width)).h(SizeSpec::grow()),
            fill,
        )?;
        let index = ui.last_index();
        // Positions are accumulated from the widths the caller supplied, which
        // is the only measurement this crate has.
        let x0 = self.cursor;
        self.cursor += width + self.style.gap;
        if self.selected == Some(id) {
            self.active_span = Some([x0, x0 + width]);
        }
        if r.clicked && !was_active {
            self.select(ui, id)?;
            self.active_span = Some([x0, x0 + width]);
        }
        Ok(TabResponse {
            active: self.selected == Some(id),
            clicked: r.clicked,
            index,
            response: r,
        })
    }

    /// Keeps the offset inside the strip, and on a selection change scrolls the
    /// least distance that brings the new tab fully into view.
    ///
    /// Only on a change. Revealing every frame would pin the bar to whatever is
    /// selected and the user could never scroll past it to reach anything else,
    /// which is exactly the tab they are trying to get to.
    fn reveal_active(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        if !self.style.scrollable {
            return Ok(());
        }
        let strip_w = self.cursor + self.style.pad;
        let max = fmax(strip_w - self.view_w, 0.0);
        let mut offset = clampf(self.offset, 0.0, max);
        // A selection the bar has not scrolled to yet, however it was set: a
        // click, a shortcut, or a layout restored from disk.
        let owed = self.selected.is_some() && self.selected != self.revealed;
        if let (true, Some([x0, x1])) = (owed, self.active_span) {
            offset = fmin(offset, x0);
            offset = fmax(offset, x1 - self.view_w);
            offset = clampf(offset, 0.0, max);
        }
        let mark = match self.selected {
            Some(id) if owed => id + 1,
            _ => u64::from(self.revealed.is_some()) * (self.revealed.unwrap_or(0) + 1),
        };
        let mut st = load(ui, self.key)?;
        let settled = offset == self.offset && st.user[REVEALED] == mark;
        st.user[OFFSET] = u64::from(offset.to_bits());
        st.user[REVEALED] = mark;
        store(ui, self.key, st)?;
        if settled {
            return Ok(());
        }
        // The strip was already emitted at the old offset, so the frame that
        // shows the corrected one has to be asked for.
        self.offset = offset;
        ui.request_repaint();
        Ok(())
    }

    /// How far the strip is scrolled, in pixels.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Total width the tabs need, whether or not the bar is that wide.
    pub fn strip_width(&self) -> f32 {
        self.cursor + self.style.pad
    }

    fn select(&mut self, ui: &mut Ui, id: u64) -> Result<(), WidgetError> {
        self.selected = Some(id);
        self.selected_seen = true;
        self.changed = true;
        let mut st = load(ui, self.key)?;
        st.user[SELECTED_ID] = id;
        st.user[HAS_SELECTION] = 1;
        store(ui, self.key, st)?;
        // The selected tab was already painted with its old fill, so the
        // frame that shows the change has to be requested.
        ui.request_repaint();
        Ok(())
    }

    /// Closes the bar. When the stored selection was not among this frame's
    /// tabs it falls back to the first one, which is what closing the active
    /// tab should do.
    pub fn end(mut self, ui: &mut Ui) -> Result<TabsResult, WidgetError> {
        ui.close()?;
        self.reveal_active(ui)?;
        ui.close()?;
        if !self.selected_seen {
            match self.first_seen {
                Some(fid) => self.select(ui, fid)?,
                None => self.selected = None,
            }
        }
        Ok(TabsResult {
            active: self.selected,
            changed: self.changed,
        })
    }
}
