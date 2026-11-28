//! Columns: a header strip, resolved pixel widths, sort and resize.
//!
//! The table owns column geometry and nothing else. It does not open a scroll
//! area and does not virtualize, because both already exist and composing them
//! is the caller's business. What it guarantees is that a header cell and the
//! body cells beneath it are the same width, which is the only part that is
//! awkward to get right by hand.
//!
//! Widths resolve every frame from the previous frame's solved outer rect, the
//! same one frame lag the rest of the crate accepts. A dragged divider pins the
//! column to its left at a pixel width, stored in that column's own state, so
//! the number of columns is not bounded by how many floats fit in one state
//! slot.
//!
//! Dividers are floating boxes. They straddle the column boundary rather than
//! sitting between columns, so a grab region wider than a hairline costs no
//! layout width and the columns still sum to the full extent.

use stein_layout::{Align, Axis, BoxSpec, Rect, SizeSpec};
use stein_theme::Theme;
use stein_ui::key::{child, hash_str, indexed, seeded};
use stein_ui::{Response, Ui};

use crate::WidgetError;
use crate::util::{fmax, fmin, load, store};

/// Refuses rather than grows, like every other capacity in the stack. Sixteen
/// is already past the point where a table is readable.
pub const MAX_COLUMNS: usize = 16;

/// How a column claims horizontal space before any user resize.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnWidth {
    /// Fixed pixels. Never redistributed on resize.
    Px(f32),
    /// A weighted share of whatever the fixed columns leave over.
    Grow(f32),
}

#[derive(Clone, Copy, Debug)]
pub struct Column<'a> {
    pub title: &'a str,
    pub width: ColumnWidth,
    /// Floor in pixels, enforced against both the grow split and a drag.
    pub min: f32,
    /// Clicking this header cell cycles the sort.
    pub sortable: bool,
    /// Cross axis alignment hint. The table does not draw text, so this is for
    /// the caller to read back when it draws its own.
    pub align: Align,
}

impl<'a> Column<'a> {
    pub const fn new(title: &'a str, width: ColumnWidth) -> Self {
        Column {
            title,
            width,
            min: 24.0,
            sortable: true,
            align: Align::Start,
        }
    }
    pub const fn min(mut self, v: f32) -> Self {
        self.min = v;
        self
    }
    pub const fn fixed(mut self) -> Self {
        self.sortable = false;
        self
    }
    pub const fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
}

/// Which column orders the rows, and in which direction. `column` is an index
/// into the caller's slice; `None` is the unsorted state the table starts in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub column: Option<u8>,
    pub descending: bool,
}

impl Sort {
    fn pack(self) -> u64 {
        match self.column {
            None => 0,
            Some(c) => u64::from(c) + 1 + (u64::from(self.descending) << 8),
        }
    }
    fn unpack(v: u64) -> Sort {
        let idx = v & 0xff;
        Sort {
            column: if idx == 0 {
                None
            } else {
                Some((idx - 1) as u8)
            },
            descending: v & (1 << 8) != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableStyle {
    pub header_h: f32,
    pub row_h: f32,
    /// Grab width of a divider. Split evenly across the boundary it straddles.
    pub divider: f32,
    pub header_fill: u32,
    pub header_hot_fill: u32,
    pub divider_fill: u32,
    pub divider_hot_fill: u32,
    /// Space between the edge of a cell and its content, for the caller to
    /// apply when it draws. Also insets the header cells.
    pub cell_pad: f32,
    pub resizable: bool,
}

impl Default for TableStyle {
    fn default() -> Self {
        TableStyle::from(&Theme::default())
    }
}

impl From<&Theme> for TableStyle {
    fn from(t: &Theme) -> Self {
        TableStyle {
            header_h: 24.0,
            row_h: 20.0,
            divider: 6.0,
            header_fill: t.pane,
            header_hot_fill: t.hover,
            divider_fill: t.pane,
            divider_hot_fill: t.faint,
            cell_pad: 6.0,
            resizable: true,
        }
    }
}

/// `user[0]` of a column's state holds its pinned width as f32 bits, zero
/// meaning unpinned. `user[1]` holds the width at the start of a drag, and
/// `user[2]` is the held flag, so the anchor is captured once on the press
/// edge rather than chased every frame.
const HELD: u64 = 1;

pub struct Table<'a> {
    key: u64,
    cols: &'a [Column<'a>],
    widths: [f32; MAX_COLUMNS],
    style: TableStyle,
    sort: Sort,
    sort_changed: bool,
    avail: f32,
    header_index: u32,
    head_index: [u32; MAX_COLUMNS],
    outer_index: u32,
    row_open: bool,
}

/// One key per column, used both for the header cell and for that column's
/// persisted width. They are the same thing on purpose: the header cell is the
/// only box whose solved width *is* the column width, so subscribing it to
/// geometry is what lets a drag anchor on the real width rather than on a spec
/// the solve may have overridden.
fn k_col(key: u64, col: usize) -> u64 {
    indexed(key, hash_str("table.col"), col as u64)
}
fn k_div(key: u64, col: usize) -> u64 {
    indexed(key, hash_str("table.div"), col as u64)
}
fn k_header_row(key: u64) -> u64 {
    child(key, hash_str("table.header"))
}

impl<'a> Table<'a> {
    /// Opens the outer box and emits the header. Emit the body after this and
    /// close with [`Table::end`].
    pub fn begin(
        ui: &mut Ui,
        key: u64,
        cols: &'a [Column<'a>],
        size: [SizeSpec; 2],
        style: TableStyle,
    ) -> Result<Table<'a>, WidgetError> {
        if cols.is_empty() || cols.len() > MAX_COLUMNS {
            return Err(WidgetError::ColumnCountOutOfRange);
        }
        if style.header_h <= 0.0 || style.row_h <= 0.0 {
            return Err(WidgetError::RowHeightNotPositive);
        }
        if style.divider <= 0.0 {
            return Err(WidgetError::DividerNotPositive);
        }
        if cols.iter().any(|c| c.min < 0.0) {
            return Err(WidgetError::MinPaneNegative);
        }
        let mut t = Table {
            key,
            cols,
            widths: [0.0; MAX_COLUMNS],
            style,
            sort: Sort::default(),
            sort_changed: false,
            avail: 0.0,
            header_index: 0,
            head_index: [0; MAX_COLUMNS],
            outer_index: 0,
            row_open: false,
        };
        t.read_geometry(ui)?;
        t.drag_dividers(ui)?;
        t.resolve_widths(ui)?;
        t.emit_open(ui, size)?;
        t.emit_header(ui)?;
        Ok(t)
    }

    fn read_geometry(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let st = load(ui, self.key)?;
        if st.is_fresh(ui.frame_index()) {
            // Columns are sized blind until the first solve lands, so the frame
            // that reads it is owed now.
            ui.request_repaint();
        }
        self.avail = fmax(st.last_rect.max[0] - st.last_rect.min[0], 0.0);
        self.sort = Sort::unpack(st.user[0]);
        Ok(())
    }

    /// A divider moves the boundary to its right, which is the same as setting
    /// the width of the column to its left. The anchor is the width at the
    /// press, so a long drag cannot accumulate per frame rounding.
    fn drag_dividers(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        if !self.style.resizable {
            return Ok(());
        }
        // The last column has no divider: it absorbs the remainder, and pinning
        // it would let the row stop covering the table.
        for i in 0..self.cols.len().saturating_sub(1) {
            self.drag_one(ui, i)?;
        }
        Ok(())
    }

    fn drag_one(&mut self, ui: &mut Ui, i: usize) -> Result<(), WidgetError> {
        let held = ui.peek_active(k_div(self.key, i));
        let mut st = load(ui, k_col(self.key, i))?;
        let was_held = st.user[2] & HELD != 0;
        st.user[2] = u64::from(held) * HELD;
        if !held {
            return store(ui, k_col(self.key, i), st);
        }
        if !was_held {
            // Unpinned columns anchor on their solved width, so the first
            // pixel of the drag does not snap the column to its spec.
            let solved = fmax(st.last_rect.max[0] - st.last_rect.min[0], 0.0);
            let anchor = pinned_of(st.user[0]).unwrap_or(solved);
            st.user[1] = u64::from(anchor.to_bits());
        }
        let anchor = f32::from_bits(st.user[1] as u32);
        let total = ui.peek_drag(k_div(self.key, i)).total[0];
        let w = fmax(anchor + total, self.cols[i].min);
        // A zero width would read back as unpinned, and a column that narrow is
        // unusable anyway.
        st.user[0] = u64::from(fmax(w, 1.0).to_bits());
        store(ui, k_col(self.key, i), st)
    }

    /// Pinned columns take their pixels first, the rest split what is left by
    /// weight. Every column is then floored at its minimum, which can overflow
    /// the table; that is the honest answer, and the header clips it.
    fn resolve_widths(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let mut pinned = [0.0f32; MAX_COLUMNS];
        let (fixed, weight) = self.measure_fixed(ui, &mut pinned)?;
        let cols = self.cols;
        let spare = fmax(self.avail - fixed, 0.0);
        let n = self.cols.len();
        for (i, w) in self.widths.iter_mut().enumerate().take(n) {
            *w = width_of(pinned[i], cols[i], spare, weight);
        }
        Ok(())
    }

    /// Total pixels the fixed columns claim and total weight the rest carry.
    fn measure_fixed(
        &self,
        ui: &mut Ui,
        pinned: &mut [f32; MAX_COLUMNS],
    ) -> Result<(f32, f32), WidgetError> {
        let mut fixed = 0.0;
        let mut weight = 0.0;
        for (i, c) in self.cols.iter().enumerate() {
            let st = load(ui, k_col(self.key, i))?;
            // A pin overrides the spec, and a Px column is a pin the caller
            // wrote, so both leave the grow pool alone.
            let w = pinned_of(st.user[0]).or(px_of(c.width));
            pinned[i] = w.map_or(0.0, |v| fmax(v, c.min));
            fixed += pinned[i];
            weight += w.map_or(grow_of(c.width), |_| 0.0);
        }
        Ok((fixed, weight))
    }

    fn emit_open(&mut self, ui: &mut Ui, size: [SizeSpec; 2]) -> Result<(), WidgetError> {
        ui.open(
            self.key,
            BoxSpec::container(Axis::Y).w(size[0]).h(size[1]).clip(),
            0,
        )?;
        self.outer_index = ui.last_index();
        Ok(())
    }

    /// Header cells in flow, then the dividers floating on top of the
    /// boundaries between them. Painter order is emission order, so the grab
    /// regions have to come last to be visible over the cells.
    fn emit_header(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        ui.open(
            k_header_row(self.key),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::px(self.style.header_h))
                .clip(),
            self.style.header_fill,
        )?;
        self.header_index = ui.last_index();
        let mut before = self.sort;
        for i in 0..self.cols.len() {
            let hot = ui.peek_hovered(k_col(self.key, i)) && self.cols[i].sortable;
            let fill = pick(hot, self.style.header_hot_fill, self.style.header_fill);
            let r = ui.leaf(
                k_col(self.key, i),
                BoxSpec::leaf()
                    .w(SizeSpec::px(self.widths[i]))
                    .h(SizeSpec::grow()),
                fill,
            )?;
            self.head_index[i] = ui.last_index();
            let sorts = r.clicked && self.cols[i].sortable;
            before = cycle_if(sorts, before, i as u8);
        }
        if self.style.resizable {
            self.emit_dividers(ui)?;
        }
        ui.close()?;
        if before != self.sort {
            self.sort = before;
            self.sort_changed = true;
            let mut st = load(ui, self.key)?;
            st.user[0] = before.pack();
            store(ui, self.key, st)?;
            // The rows below are about to be built in the old order, because
            // the caller reads the sort after this returns. Next frame is the
            // one that shows the new order, so it has to happen.
            ui.request_repaint();
        }
        Ok(())
    }

    fn emit_dividers(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        let half = self.style.divider * 0.5;
        let mut x = 0.0;
        for i in 0..self.cols.len().saturating_sub(1) {
            x += self.widths[i];
            let hot = ui.peek_hovered(k_div(self.key, i)) || ui.peek_active(k_div(self.key, i));
            let fill = pick(hot, self.style.divider_hot_fill, self.style.divider_fill);
            ui.leaf(
                k_div(self.key, i),
                BoxSpec::leaf()
                    .w(SizeSpec::px(self.style.divider))
                    .h(SizeSpec::grow())
                    .floating([x - half, 0.0]),
                fill,
            )?;
        }
        Ok(())
    }

    /// Opens one body row. Emit cells with [`Table::cell`], then
    /// [`Table::end_row`]. `id` is the caller's stable row id, so selection
    /// and hover survive a re-sort.
    pub fn row(&mut self, ui: &mut Ui, id: u64, fill: u32) -> Result<Response, WidgetError> {
        if self.row_open {
            return Err(WidgetError::RowAlreadyOpen);
        }
        let r = ui.open(
            self.row_key(id),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::px(self.style.row_h)),
            fill,
        )?;
        self.row_open = true;
        Ok(r)
    }

    /// One cell, as a leaf. Rich content goes on top: read the rect back with
    /// `ui.last_rect(table.cell_key(id, col))` and draw into it, the same way
    /// text and inline bars are drawn everywhere else in the stack.
    pub fn cell(
        &self,
        ui: &mut Ui,
        id: u64,
        col: usize,
        fill: u32,
    ) -> Result<Response, WidgetError> {
        if col >= self.cols.len() {
            return Err(WidgetError::ColumnOutOfRange);
        }
        let r = ui.leaf(
            self.cell_key(id, col),
            BoxSpec::leaf()
                .w(SizeSpec::px(self.widths[col]))
                .h(SizeSpec::grow()),
            fill,
        )?;
        Ok(r)
    }

    pub fn end_row(&mut self, ui: &mut Ui) -> Result<(), WidgetError> {
        if !self.row_open {
            return Err(WidgetError::NoRowOpen);
        }
        ui.close()?;
        self.row_open = false;
        Ok(())
    }

    /// Closes the outer box.
    pub fn end(self, ui: &mut Ui) -> Result<Table<'a>, WidgetError> {
        if self.row_open {
            return Err(WidgetError::RowAlreadyOpen);
        }
        ui.close()?;
        Ok(self)
    }

    pub fn row_key(&self, id: u64) -> u64 {
        seeded(self.key, hash_str("table.row"), id)
    }
    pub fn cell_key(&self, id: u64, col: usize) -> u64 {
        indexed(self.row_key(id), hash_str("table.cell"), col as u64)
    }
    /// Resolved pixel widths, one per column, in the caller's order.
    pub fn widths(&self) -> &[f32] {
        &self.widths[..self.cols.len()]
    }
    /// The content rect of a cell, its rect inset by `cell_pad` on both sides
    /// and clipped. Zero-width when the text would have nowhere to go.
    pub fn cell_content(&self, rect: Rect) -> Rect {
        let p = self.style.cell_pad;
        let x0 = rect.min[0] + p;
        let x1 = fmax(rect.max[0] - p, x0);
        Rect {
            min: [x0, rect.min[1]],
            max: [x1, rect.max[1]],
        }
    }
    /// Where a cell's content starts horizontally, honouring the column's
    /// alignment for a string of `text_w` pixels.
    pub fn align_x(&self, col: usize, rect: Rect, text_w: f32) -> f32 {
        let c = self.cell_content(rect);
        let space = fmax(c.max[0] - c.min[0] - text_w, 0.0);
        let a = self.cols.get(col).map_or(Align::Start, |c| c.align);
        match a {
            Align::Start => c.min[0],
            Align::Center => c.min[0] + space * 0.5,
            Align::End => c.min[0] + space,
        }
    }
    pub fn sort(&self) -> Sort {
        self.sort
    }
    /// True on the frame a header click changed the ordering. The rows built
    /// this frame are still in the old order; the repaint this schedules is
    /// what shows the new one.
    pub fn sort_changed(&self) -> bool {
        self.sort_changed
    }
    pub fn style(&self) -> TableStyle {
        self.style
    }
    pub fn columns(&self) -> &'a [Column<'a>] {
        self.cols
    }
    pub fn key(&self) -> u64 {
        self.key
    }
    pub fn header_index(&self) -> u32 {
        self.header_index
    }
    /// Box index of a header cell, for painting its title and sort arrow.
    pub fn head_index(&self, col: usize) -> Option<u32> {
        if col < self.cols.len() {
            Some(self.head_index[col])
        } else {
            None
        }
    }
    pub fn outer_index(&self) -> u32 {
        self.outer_index
    }
    /// Sum of the resolved widths. Larger than the table when minimums
    /// overflow it, which the caller can use to offer horizontal scrolling.
    pub fn total_width(&self) -> f32 {
        let mut t = 0.0;
        for w in self.widths() {
            t += w;
        }
        t
    }
    /// Resets every pinned width back to its spec.
    pub fn reset_widths(&self, ui: &mut Ui) -> Result<(), WidgetError> {
        for i in 0..self.cols.len() {
            let mut st = load(ui, k_col(self.key, i))?;
            st.user[0] = 0;
            store(ui, k_col(self.key, i), st)?;
        }
        Ok(())
    }
    /// Sets the sort directly, for restoring a persisted one or driving it from
    /// a keyboard shortcut.
    pub fn set_sort(&mut self, ui: &mut Ui, sort: Sort) -> Result<(), WidgetError> {
        let mut st = load(ui, self.key)?;
        st.user[0] = sort.pack();
        self.sort = sort;
        store(ui, self.key, st)
    }
}

/// A pinned width out of a state word, `None` when unpinned. Zero is the
/// sentinel because a zero-wide column is meaningless and `0.0f32` is all-zero
/// bits, so an untouched slot reads as unpinned without a second flag.
#[inline]
fn pinned_of(word: u64) -> Option<f32> {
    if word == 0 {
        None
    } else {
        Some(f32::from_bits(word as u32))
    }
}

#[inline]
fn px_of(w: ColumnWidth) -> Option<f32> {
    match w {
        ColumnWidth::Px(p) => Some(p),
        ColumnWidth::Grow(_) => None,
    }
}

#[inline]
fn grow_of(w: ColumnWidth) -> f32 {
    match w {
        ColumnWidth::Grow(g) => fmax(g, 0.0),
        ColumnWidth::Px(_) => 0.0,
    }
}

/// Final width of one column. A free function so the two branches sit at the
/// top of a body rather than three levels inside the loop that calls it.
fn width_of(pinned: f32, c: Column<'_>, spare: f32, weight: f32) -> f32 {
    if pinned > 0.0 {
        return pinned;
    }
    let share = if weight > 0.0 {
        spare * grow_of(c.width) / weight
    } else {
        0.0
    };
    fmax(share, c.min)
}

#[inline]
fn pick(hot: bool, on: u32, off: u32) -> u32 {
    if hot { on } else { off }
}

/// `cycle` when `yes`, otherwise the sort as it stands. A free function so the
/// branch does not sit three levels inside the header loop.
#[inline]
fn cycle_if(yes: bool, cur: Sort, col: u8) -> Sort {
    if yes { cycle(cur, col) } else { cur }
}

/// Ascending, descending, off. Three states rather than two, because a table
/// whose natural order carries meaning has nowhere to go back to otherwise.
fn cycle(cur: Sort, col: u8) -> Sort {
    match cur.column {
        Some(c) if c == col => {
            if cur.descending {
                Sort::default()
            } else {
                Sort {
                    column: Some(col),
                    descending: true,
                }
            }
        }
        _ => Sort {
            column: Some(col),
            descending: false,
        },
    }
}

/// Number of characters that fit in `w` pixels at `advance` per character,
/// for callers eliding cell text. Separate from the widget so a caller can use
/// it before committing to a layout.
pub fn chars_that_fit(w: f32, advance: f32) -> usize {
    if advance <= 0.0 || w <= 0.0 {
        return 0;
    }
    let n = w / advance;
    fmin(n, 4096.0) as usize
}

/// Clamps a column index into range, for a sort read back from persistence
/// after the column set changed.
pub fn clamp_sort(sort: Sort, columns: usize) -> Sort {
    match sort.column {
        Some(c) if (c as usize) < columns => sort,
        Some(_) => Sort::default(),
        None => sort,
    }
}

/// Where the divider between `col` and `col + 1` sits, as an offset from the
/// header's content origin. Exposed for tests and for a caller drawing its own
/// column rules in the body.
pub fn divider_offset(widths: &[f32], col: usize) -> f32 {
    let mut x = 0.0;
    for w in widths.iter().take(col + 1) {
        x += *w;
    }
    x
}
