//! A scrollable, selectable block of monospaced lines.
//!
//! Unlike the virtualized list this emits no box per line. A hundred thousand
//! lines cost four boxes: the outer, the clipped viewport, and a bar per axis.
//! Everything inside is drawn by the caller as quads, the same arrangement the
//! flame graph uses, because a line of text has no interaction of its own that
//! a box would pay for. What the view owns is the mapping between pixels and
//! (line, column) and the selection built on top of it.
//!
//! The content extent is known exactly from the line count and the widest
//! line, so this does not need the solve-derived extent the scroll area reads
//! back. That is why it carries its own viewport rather than nesting in one:
//! the numbers are better and the two-axis case stays simple.
//!
//! Columns are characters. Tabs are the caller's problem, because whether a
//! tab is four columns or eight is a property of the document, not the view;
//! pass an already-expanded width to `max_columns` and expand when drawing.

use stein_layout::{Axis, BoxSpec, Rect, SizeSpec};
use stein_theme::Theme;
use stein_ui::key::{child, hash_str};
use stein_ui::{Ui, keys};

use crate::WidgetError;
use crate::scroll::ScrollStyle;
use crate::util::{clampf, fmax, fmin, load, store};

/// Cell metrics from the atlas: `MonoAtlas::line_height` and `cell_advance`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextMetrics {
    pub line_h: f32,
    pub advance: f32,
}

/// A position in the document. `col` may sit one past the end of its line,
/// which is where a caret lives after the last character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub line: u32,
    pub col: u32,
}

impl Caret {
    fn pack(self) -> u64 {
        (u64::from(self.line) << 32) | u64::from(self.col)
    }
    fn unpack(v: u64) -> Caret {
        Caret {
            line: (v >> 32) as u32,
            col: v as u32,
        }
    }
}

/// Lines `first..end` intersect the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineRange {
    pub first: u32,
    pub end: u32,
}

impl LineRange {
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.first)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.first
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextViewStyle {
    /// Width of the line-number gutter in characters. Zero hides it.
    pub gutter_cols: u8,
    /// Space between the gutter and the first text column, in characters.
    pub gutter_gap: u8,
    pub bar: ScrollStyle,
    pub gutter_fill: u32,
    pub background: u32,
    /// Lines per wheel notch. Three is the platform convention everywhere.
    pub wheel_lines: f32,
    /// Keep this many lines visible above and below when scrolling to a line.
    pub reveal_margin: u32,
    pub selectable: bool,
    /// Emit a horizontal bar and allow horizontal scrolling.
    ///
    /// Off for wrapped text, where no line is ever wider than the view and the
    /// bar would be permanent dead space. Bars are in flow rather than
    /// overlaid, so this cannot be decided from the content: a bar that
    /// appeared on overflow would change the height, which changes the
    /// overflow. The caller knows whether it wraps, so the caller decides.
    pub horizontal: bool,
}

impl Default for TextViewStyle {
    fn default() -> Self {
        TextViewStyle::from(&Theme::default())
    }
}

impl From<&Theme> for TextViewStyle {
    fn from(t: &Theme) -> Self {
        TextViewStyle {
            gutter_cols: 0,
            gutter_gap: 1,
            bar: ScrollStyle::from(t),
            gutter_fill: t.bg,
            background: t.pane,
            wheel_lines: 3.0,
            reveal_margin: 3,
            selectable: true,
            horizontal: true,
        }
    }
}

/// Up to three rectangles covering a selection: the tail of its first line,
/// the whole block between, and the head of its last line. A caller pushes
/// whichever are non-empty before it draws the glyphs.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectionRects {
    pub rects: [Rect; 3],
    pub count: u8,
}

impl SelectionRects {
    pub fn as_slice(&self) -> &[Rect] {
        let n = (self.count as usize).min(3);
        &self.rects[..n]
    }
    fn push(&mut self, r: Rect) {
        if r.max[0] > r.min[0] && r.max[1] > r.min[1] && (self.count as usize) < 3 {
            self.rects[self.count as usize] = r;
            self.count += 1;
        }
    }
}

/// `user[0]` and `user[1]` hold the selection anchor and head as packed
/// carets. `user[2]` is a flag word.
const DRAGGING: u64 = 1;
const HAS_SEL: u64 = 2;

pub struct TextView {
    key: u64,
    style: TextViewStyle,
    m: TextMetrics,
    lines: u32,
    max_cols: u32,
    offset: [f32; 2],
    view: Rect,
    visible: LineRange,
    first_col: u32,
    anchor: Caret,
    head: Caret,
    has_sel: bool,
    viewport_index: u32,
    gutter_w: f32,
}

fn k_vp(key: u64) -> u64 {
    child(key, hash_str("text.viewport"))
}
fn k_bar(key: u64, a: usize) -> u64 {
    child(
        key,
        hash_str(if a == 0 { "text.barx" } else { "text.bary" }),
    )
}
fn k_thumb(key: u64, a: usize) -> u64 {
    child(key, hash_str(if a == 0 { "text.thx" } else { "text.thy" }))
}

impl TextView {
    /// Opens the outer box and the clipped viewport. Draw into
    /// [`TextView::viewport`] after `end_frame`, then close with
    /// [`TextView::end`].
    ///
    /// `max_columns` is the width of the widest line, in columns. It only sets
    /// how far horizontal scrolling can go, so an over-estimate is harmless
    /// and a caller that has not measured every line can pass a bound.
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        size: [SizeSpec; 2],
        line_count: u32,
        max_columns: u32,
        m: TextMetrics,
        style: TextViewStyle,
    ) -> Result<TextView, WidgetError> {
        if m.line_h <= 0.0 || m.advance <= 0.0 {
            return Err(WidgetError::TextMetricsNotPositive);
        }
        if style.bar.bar_size <= 0.0 {
            return Err(WidgetError::BarSizeNotPositive);
        }
        let gutter_w = if style.gutter_cols == 0 {
            0.0
        } else {
            (f32::from(style.gutter_cols) + f32::from(style.gutter_gap)) * m.advance
        };
        let mut t = TextView {
            key,
            style,
            m,
            lines: line_count,
            max_cols: max_columns,
            offset: [0.0, 0.0],
            view: Rect::default(),
            visible: LineRange::default(),
            first_col: 0,
            anchor: Caret::default(),
            head: Caret::default(),
            has_sel: false,
            viewport_index: 0,
            gutter_w,
        };
        t.read_geometry(ui)?;
        t.interact(ui)?;
        t.emit_open(ui, size)?;
        Ok(t)
    }

    fn read_geometry(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let st = load(ui, k_vp(self.key))?;
        if st.is_fresh(ui.frame_index()) {
            // Nothing is scrollable or hit-testable until the first solve, so
            // the frame that reads it is owed now.
            ui.request_repaint();
        }
        self.view = st.last_rect;
        Ok(())
    }

    fn view_size(&self) -> [f32; 2] {
        [
            fmax(self.view.max[0] - self.view.min[0] - self.gutter_w, 0.0),
            fmax(self.view.max[1] - self.view.min[1], 0.0),
        ]
    }

    /// Exact, unlike a solve-derived extent, because a monospaced document's
    /// size is two multiplications.
    fn content(&self) -> [f32; 2] {
        let w = if self.style.horizontal {
            self.max_cols as f32 * self.m.advance
        } else {
            0.0
        };
        [w, self.lines as f32 * self.m.line_h]
    }

    fn max_scroll(&self) -> [f32; 2] {
        let v = self.view_size();
        let c = self.content();
        [fmax(c[0] - v[0], 0.0), fmax(c[1] - v[1], 0.0)]
    }

    fn interact(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let input = ui.input();
        let mut st = load(ui, self.key)?;
        self.offset = st.scroll;
        self.anchor = Caret::unpack(st.user[0]);
        self.head = Caret::unpack(st.user[1]);
        self.has_sel = st.user[2] & HAS_SEL != 0;
        let inside = self.view.contains(input.pointer);
        if inside {
            self.offset[1] -= input.wheel[1] / stein_ui::WHEEL_NOTCH * self.style.wheel_lines * self.m.line_h;
            self.offset[0] -= input.wheel[0] / stein_ui::WHEEL_NOTCH * self.style.wheel_lines * self.m.advance;
        }
        self.drag_thumbs(ui, &mut st)?;
        if self.style.selectable {
            self.select(ui, &mut st, inside)?;
        }
        if ui.keys_for(self.key) {
            self.keys(ui);
        }
        let max = self.max_scroll();
        self.offset[0] = clampf(self.offset[0], 0.0, max[0]);
        self.offset[1] = clampf(self.offset[1], 0.0, max[1]);
        self.resolve_visible();
        st.scroll = self.offset;
        st.user[0] = self.anchor.pack();
        st.user[1] = self.head.pack();
        // Only the selection bit is recomputed here. The two drag flags were
        // written by the phases above and must survive.
        st.user[2] = (st.user[2] & !HAS_SEL) | (u64::from(self.has_sel) * HAS_SEL);
        store(ui, self.key, st)
    }

    fn drag_thumbs(
        &mut self,
        ui: &mut Ui,
        st: &mut stein_ui::WidgetState,
    ) -> Result<(), WidgetError> {
        let held_y = ui.peek_active(k_thumb(self.key, 1));
        let held = ui.peek_active(k_thumb(self.key, 0)) || held_y;
        // One anchor slot is enough because only one thumb can be held.
        let a = usize::from(held_y);
        let fresh_press = held && st.user[2] & DRAGGING == 0;
        // Captured before it is used, so the first frame of a drag anchors on
        // where the view actually was rather than on the previous drag's value.
        st.user[3] = if fresh_press {
            u64::from(self.offset[a].to_bits())
        } else {
            st.user[3]
        };
        st.user[2] = (st.user[2] & !DRAGGING) | (u64::from(held) * DRAGGING);
        let max = self.max_scroll()[a];
        let movable = self.view_size()[a] * (1.0 - self.thumb_frac(a));
        if !held || movable <= 0.0 || max <= 0.0 {
            return Ok(());
        }
        // Anchored on the offset at the press, so a long drag cannot
        // accumulate per-frame rounding.
        let anchor = f32::from_bits(st.user[3] as u32);
        let total = ui.peek_drag(k_thumb(self.key, a)).total[a];
        self.offset[a] = anchor + total * (max / movable);
        Ok(())
    }

    fn select(
        &mut self,
        ui: &mut Ui,
        st: &mut stein_ui::WidgetState,
        inside: bool,
    ) -> Result<(), WidgetError> {
        let input = ui.input();
        let on_text = inside && input.pointer[0] >= self.view.min[0] + self.gutter_w;
        let pressing = ui.peek_active(k_vp(self.key)) && !st.is_fresh(ui.frame_index());
        let continuing = was_dragging(st);
        set_was_dragging(st, pressing);
        // A fresh press moves both ends. Continuing one only moves the head,
        // which is what lets a drag grow the selection.
        let c = self.caret_at(input.pointer);
        let anchor = if continuing { self.anchor } else { c };
        if pressing {
            self.anchor = anchor;
            self.head = c;
            self.has_sel = true;
            ui.focus(Some(self.key));
        }
        // A press elsewhere clears the selection, which is what makes a second
        // click in empty space feel like a deselect rather than a no-op.
        if input.pointer_down && !on_text && !pressing {
            self.has_sel = false;
        }
        Ok(())
    }

    fn keys(&mut self, ui: &mut Ui) {
        let page = self.page_lines();
        let input = ui.input();
        for k in input.keys() {
            self.key_event(*k, page);
        }
    }

    fn key_event(&mut self, k: stein_ui::KeyEvent, page: u32) {
        if !k.pressed {
            return;
        }
        // Ctrl+A over the whole document, which every text surface offers.
        if k.sym == keys::A && k.mods.ctrl {
            self.select_all();
            return;
        }
        let moved = match k.sym {
            keys::UP => self.step_line(-1),
            keys::DOWN => self.step_line(1),
            keys::LEFT => self.step_col(-1),
            keys::RIGHT => self.step_col(1),
            keys::PAGE_UP => self.step_line(-(page as i64)),
            keys::PAGE_DOWN => self.step_line(page as i64),
            keys::HOME => self.set_col(0),
            keys::END => self.set_col(self.max_cols),
            _ => false,
        };
        if !moved {
            return;
        }
        // Without shift the selection collapses onto the caret, which is what
        // makes an arrow key a move rather than an extend.
        self.anchor = if k.mods.shift { self.anchor } else { self.head };
        self.has_sel = true;
        self.reveal_head();
    }

    fn select_all(&mut self) {
        self.anchor = Caret { line: 0, col: 0 };
        self.head = Caret {
            line: self.lines.saturating_sub(1),
            col: self.max_cols,
        };
        self.has_sel = true;
    }

    fn page_lines(&self) -> u32 {
        let n = (self.view_size()[1] / self.m.line_h) as u32;
        n.max(1)
    }

    fn step_line(&mut self, by: i64) -> bool {
        let last = self.lines.saturating_sub(1);
        let want = (i64::from(self.head.line) + by).clamp(0, i64::from(last));
        let next = want as u32;
        let changed = next != self.head.line;
        self.head.line = next;
        changed
    }

    fn step_col(&mut self, by: i64) -> bool {
        let want = (i64::from(self.head.col) + by).clamp(0, i64::from(self.max_cols));
        let next = want as u32;
        let changed = next != self.head.col;
        self.head.col = next;
        changed
    }

    fn set_col(&mut self, col: u32) -> bool {
        let changed = self.head.col != col;
        self.head.col = col;
        changed
    }

    /// Scrolls the minimum distance that puts the head back in view, keeping
    /// the configured margin. Moving the caret should not recentre the page.
    fn reveal_head(&mut self) {
        let v = self.view_size();
        let margin = f32::from(self.style.reveal_margin as u16) * self.m.line_h;
        let top = self.head.line as f32 * self.m.line_h - margin;
        let bottom = (self.head.line + 1) as f32 * self.m.line_h + margin;
        self.offset[1] = fmin(self.offset[1], top);
        self.offset[1] = fmax(self.offset[1], bottom - v[1]);
        let left = self.head.col as f32 * self.m.advance;
        self.offset[0] = fmin(self.offset[0], left);
        self.offset[0] = fmax(self.offset[0], left + self.m.advance - v[0]);
        self.offset[0] = fmax(self.offset[0], 0.0);
        self.offset[1] = fmax(self.offset[1], 0.0);
    }

    fn resolve_visible(&mut self) {
        let v = self.view_size();
        if v[1] <= 0.0 || self.lines == 0 {
            self.visible = LineRange::default();
            self.first_col = 0;
            return;
        }
        // Truncating casts are floor for non-negative values, and core has no
        // f32 floor without libm.
        let first = ((self.offset[1] / self.m.line_h) as u32).min(self.lines);
        let stop = self.offset[1] + v[1];
        let mut end = (stop / self.m.line_h) as u32;
        if (end as f32) * self.m.line_h < stop {
            end += 1;
        }
        self.visible = LineRange {
            first,
            end: end.clamp(first, self.lines),
        };
        self.first_col = (self.offset[0] / self.m.advance) as u32;
    }

    fn thumb_frac(&self, a: usize) -> f32 {
        let v = self.view_size()[a];
        let c = self.content()[a];
        if c <= v || c <= 0.0 {
            return 1.0;
        }
        let mut f = v / c;
        if v > 0.0 {
            f = fmax(f, self.style.bar.min_thumb / v);
        }
        fmin(f, 1.0)
    }

    fn emit_open(&mut self, ui: &mut Ui, size: [SizeSpec; 2]) -> Result<(), WidgetError> {
        ui.open(
            self.key,
            BoxSpec::container(Axis::Y).w(size[0]).h(size[1]),
            0,
        )?;
        ui.open(
            child(self.key, hash_str("text.row")),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::grow()),
            0,
        )?;
        ui.leaf(
            k_vp(self.key),
            BoxSpec::leaf()
                .w(SizeSpec::grow())
                .h(SizeSpec::grow())
                .clip(),
            self.style.background,
        )?;
        self.viewport_index = ui.last_index();
        // A focusable surface, so arrow keys reach it once clicked.
        ui.focusable(self.key);
        Ok(())
    }

    /// Emits the two bars and closes. Bars sit outside the clipped viewport,
    /// so they are never scrolled by their own offsets.
    pub fn end(mut self, ui: &mut Ui) -> Result<TextView, WidgetError> {
        self.emit_bar(ui, 1)?;
        ui.close()?;
        if self.style.horizontal {
            self.emit_bar(ui, 0)?;
        }
        ui.close()?;
        Ok(self)
    }

    fn emit_bar(&mut self, ui: &mut Ui, a: usize) -> Result<(), WidgetError> {
        let frac = self.thumb_frac(a);
        let max = self.max_scroll()[a];
        let lead = if max > 0.0 {
            (1.0 - frac) * self.offset[a] / max
        } else {
            0.0
        };
        let bar = self.style.bar.bar_size;
        let (bw, bh) = if a == 0 {
            (SizeSpec::grow(), SizeSpec::px(bar))
        } else {
            (SizeSpec::px(bar), SizeSpec::grow())
        };
        let axis = if a == 0 { Axis::X } else { Axis::Y };
        ui.open(
            k_bar(self.key, a),
            BoxSpec::container(axis).w(bw).h(bh),
            self.style.bar.track_fill,
        )?;
        let (lw, lh) = if a == 0 {
            (SizeSpec::fraction(lead), SizeSpec::grow())
        } else {
            (SizeSpec::grow(), SizeSpec::fraction(lead))
        };
        ui.leaf(
            child(k_bar(self.key, a), hash_str("text.space")),
            BoxSpec::leaf().w(lw).h(lh),
            0,
        )?;
        let hot = ui.peek_hovered(k_thumb(self.key, a)) || ui.peek_active(k_thumb(self.key, a));
        let fill = if hot {
            self.style.bar.thumb_hot_fill
        } else {
            self.style.bar.thumb_fill
        };
        let (tw, th) = if a == 0 {
            (SizeSpec::fraction(frac), SizeSpec::grow())
        } else {
            (SizeSpec::grow(), SizeSpec::fraction(frac))
        };
        ui.leaf(k_thumb(self.key, a), BoxSpec::leaf().w(tw).h(th), fill)?;
        ui.close()?;
        Ok(())
    }

    // Geometry the caller draws against. All absolute, all after the solve.

    /// Rect of the viewport, including the gutter. Clip every quad to this.
    pub fn viewport(&self) -> Rect {
        self.view
    }
    /// Rect of the text area, gutter excluded.
    pub fn text_area(&self) -> Rect {
        Rect {
            min: [self.view.min[0] + self.gutter_w, self.view.min[1]],
            max: self.view.max,
        }
    }
    pub fn gutter(&self) -> Rect {
        Rect {
            min: self.view.min,
            max: [self.view.min[0] + self.gutter_w, self.view.max[1]],
        }
    }
    pub fn gutter_width(&self) -> f32 {
        self.gutter_w
    }
    pub fn visible_lines(&self) -> LineRange {
        self.visible
    }
    /// First fully or partly visible column, for slicing a line before drawing.
    pub fn first_column(&self) -> u32 {
        self.first_col
    }
    /// Full-width rect of line `i`, for a row highlight.
    pub fn line_rect(&self, i: u32) -> Rect {
        let y = self.view.min[1] + i as f32 * self.m.line_h - self.offset[1];
        Rect {
            min: [self.view.min[0] + self.gutter_w, y],
            max: [self.view.max[0], y + self.m.line_h],
        }
    }
    pub fn gutter_rect(&self, i: u32) -> Rect {
        let y = self.view.min[1] + i as f32 * self.m.line_h - self.offset[1];
        Rect {
            min: [self.view.min[0], y],
            max: [self.view.min[0] + self.gutter_w, y + self.m.line_h],
        }
    }
    /// Where column 0 of line `i` starts. Add `col * advance` for any other.
    pub fn line_origin(&self, i: u32) -> [f32; 2] {
        [
            self.view.min[0] + self.gutter_w - self.offset[0],
            self.view.min[1] + i as f32 * self.m.line_h - self.offset[1],
        ]
    }
    /// Which (line, column) a point falls on. Clamped into the document, so a
    /// drag past the bottom edge selects to the end rather than nothing.
    pub fn caret_at(&self, p: [f32; 2]) -> Caret {
        let last = self.lines.saturating_sub(1);
        let dy = p[1] - self.view.min[1] + self.offset[1];
        let line = if dy <= 0.0 {
            0
        } else {
            ((dy / self.m.line_h) as u32).min(last)
        };
        let dx = p[0] - self.view.min[0] - self.gutter_w + self.offset[0];
        // Rounding rather than flooring, so a click in the right half of a cell
        // puts the caret after it.
        let col = if dx <= 0.0 {
            0
        } else {
            (((dx / self.m.advance) + 0.5) as u32).min(self.max_cols)
        };
        Caret { line, col }
    }
    pub fn offset(&self) -> [f32; 2] {
        self.offset
    }
    pub fn metrics(&self) -> TextMetrics {
        self.m
    }
    pub fn key(&self) -> u64 {
        self.key
    }
    pub fn viewport_index(&self) -> u32 {
        self.viewport_index
    }
    pub fn line_count(&self) -> u32 {
        self.lines
    }

    /// Selection in document order, or `None` when it is empty.
    pub fn selection(&self) -> Option<(Caret, Caret)> {
        if !self.has_sel || self.anchor == self.head {
            return None;
        }
        Some(if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        })
    }
    pub fn caret(&self) -> Caret {
        self.head
    }

    /// Up to three rects covering the selection, already clipped to the
    /// viewport, for the caller to push before the glyphs.
    pub fn selection_rects(&self) -> SelectionRects {
        let mut out = SelectionRects::default();
        let Some((a, b)) = self.selection() else {
            return out;
        };
        let text = self.text_area();
        let lh = self.m.line_h;
        let (x0, y0) = (self.x_of(a.col), self.y_of(a.line));
        let (x1, y1) = (self.x_of(b.col), self.y_of(b.line));
        if a.line == b.line {
            out.push(clip_to(rect(x0, y0, x1, y0 + lh), text));
            return out;
        }
        out.push(clip_to(rect(x0, y0, text.max[0], y0 + lh), text));
        // The block between is only there when a whole line lies inside.
        let mid = rect(text.min[0], self.y_of(a.line + 1), text.max[0], y1);
        out.push(clip_to(mid, text));
        out.push(clip_to(rect(text.min[0], y1, x1, y1 + lh), text));
        out
    }

    fn x_of(&self, col: u32) -> f32 {
        self.view.min[0] + self.gutter_w + col as f32 * self.m.advance - self.offset[0]
    }
    fn y_of(&self, line: u32) -> f32 {
        self.view.min[1] + line as f32 * self.m.line_h - self.offset[1]
    }

    /// Scrolls so `line` is visible, keeping the style's margin. Takes effect
    /// on the next frame, which is the same lag as every other geometry read.
    pub fn scroll_to_line(&self, ui: &mut Ui, line: u32) -> Result<(), WidgetError> {
        let v = self.view_size();
        let margin = f32::from(self.style.reveal_margin as u16) * self.m.line_h;
        let top = line as f32 * self.m.line_h - margin;
        let bottom = (line + 1) as f32 * self.m.line_h + margin;
        let mut st = load(ui, self.key)?;
        let mut y = st.scroll[1];
        y = fmin(y, top);
        y = fmax(y, bottom - v[1]);
        st.scroll[1] = clampf(y, 0.0, self.max_scroll()[1]);
        store(ui, self.key, st)?;
        ui.request_repaint();
        Ok(())
    }

    /// Puts the caret on a line without moving the view, for a search hit or a
    /// jump from another panel. Pair with [`TextView::scroll_to_line`].
    pub fn set_caret(&self, ui: &mut Ui, c: Caret) -> Result<(), WidgetError> {
        let mut st = load(ui, self.key)?;
        st.user[0] = c.pack();
        st.user[1] = c.pack();
        st.user[2] |= HAS_SEL;
        store(ui, self.key, st)
    }

    pub fn clear_selection(&self, ui: &mut Ui) -> Result<(), WidgetError> {
        let mut st = load(ui, self.key)?;
        st.user[2] &= !HAS_SEL;
        store(ui, self.key, st)
    }
}

#[inline]
fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect {
        min: [x0, y0],
        max: [x1, y1],
    }
}

/// Clamps `r` into `to`. An empty result is what `SelectionRects::push` drops,
/// so a line scrolled out of view costs nothing rather than painting offscreen.
fn clip_to(r: Rect, to: Rect) -> Rect {
    Rect {
        min: [
            clampf(r.min[0], to.min[0], to.max[0]),
            clampf(r.min[1], to.min[1], to.max[1]),
        ],
        max: [
            clampf(r.max[0], to.min[0], to.max[0]),
            clampf(r.max[1], to.min[1], to.max[1]),
        ],
    }
}

/// Bit 2 of the flag word, distinct from the thumb's `DRAGGING`, tracking
/// whether the previous frame was mid text-drag. Without it, every frame of a
/// held press would reset the anchor and the selection could never grow.
const TEXT_DRAG: u64 = 4;

fn was_dragging(st: &stein_ui::WidgetState) -> bool {
    st.user[2] & TEXT_DRAG != 0
}
fn set_was_dragging(st: &mut stein_ui::WidgetState, v: bool) {
    st.user[2] = (st.user[2] & !TEXT_DRAG) | (u64::from(v) * TEXT_DRAG);
}
