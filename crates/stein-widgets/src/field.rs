//! A single line text input.
//!
//! Two layers. The editing core is pure functions over a caller owned byte
//! buffer and a [`Cursor`], testable without a Ui. The widget wraps it with
//! focus, pointer placement and a horizontal scroll that keeps the caret in
//! view, and emits a bordered box with a clipped content box inside.
//!
//! The crate knows no fonts, so measurement is the caller's. `stops[i]` is
//! the x offset of char boundary `i` in the text as it was before this
//! frame's edit, which is what pointer hits resolve against. The caller draws
//! glyphs, caret and selection after `end_frame` from its own measurement of
//! the edited text.
//!
//! Text is UTF-8 by contract. Every offset the core produces lands on a char
//! boundary, and nothing indexes out of bounds even if the contract breaks.

use stein_layout::{Axis, BoxSpec, Rect, SizeSpec};
use stein_theme::Theme;
use stein_ui::draw::Shape;
use stein_ui::key::{child, hash_str};
use stein_ui::{Input, KeyEvent, Mods, Response, Ui, WidgetState, keys};

use crate::WidgetError;
use crate::util::{clampf, fmax, fmin, load, store};

/// Keypad Enter, which submits like Return.
const KP_ENTER: u32 = 0xff8d;

/// Caret and selection anchor as byte offsets, both on char boundaries. The
/// selection is the range between them, empty when they meet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub caret: u32,
    pub anchor: u32,
}

impl Cursor {
    pub const fn at(pos: u32) -> Cursor {
        Cursor {
            caret: pos,
            anchor: pos,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.caret == self.anchor
    }
    /// Selection start, the lower of the two offsets.
    pub fn start(&self) -> usize {
        self.caret.min(self.anchor) as usize
    }
    /// Selection end, the higher of the two offsets.
    pub fn end(&self) -> usize {
        self.caret.max(self.anchor) as usize
    }
    /// Clamped into `bytes` and moved back onto a char boundary, for a cursor
    /// that outlived a change the caller made to the text.
    pub fn snapped(self, bytes: &[u8]) -> Cursor {
        Cursor {
            caret: snap(bytes, self.caret as usize) as u32,
            anchor: snap(bytes, self.anchor as usize) as u32,
        }
    }
    fn pack(self) -> u64 {
        u64::from(self.caret) | (u64::from(self.anchor) << 32)
    }
    fn unpack(packed: u64) -> Cursor {
        Cursor {
            caret: packed as u32,
            anchor: (packed >> 32) as u32,
        }
    }
}

/// What one frame of [`edit`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    /// The text changed.
    pub changed: bool,
    /// The caret or the anchor ended somewhere else.
    pub moved: bool,
    /// Return or keypad Enter.
    pub submitted: bool,
    /// Escape.
    pub cancelled: bool,
}

// Byte level UTF-8 plumbing.

#[inline]
fn is_continuation(byte: u8) -> bool {
    byte & 0xc0 == 0x80
}

/// `pos` clamped into `bytes`, then moved back to the char it falls inside.
fn snap(bytes: &[u8], pos: usize) -> usize {
    let mut at = pos.min(bytes.len());
    while at > 0 && at < bytes.len() && is_continuation(bytes[at]) {
        at -= 1;
    }
    at
}

fn prev_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut at = pos.min(bytes.len()).saturating_sub(1);
    while at > 0 && is_continuation(bytes[at]) {
        at -= 1;
    }
    at
}

fn next_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut at = (pos + 1).min(bytes.len());
    while at < bytes.len() && is_continuation(bytes[at]) {
        at += 1;
    }
    at
}

/// The char starting at `pos`, or U+FFFD when the bytes there are broken.
fn char_at(bytes: &[u8], pos: usize) -> char {
    let width = match bytes.get(pos).copied().unwrap_or(0) {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xff => 4,
        _ => 1,
    };
    let end = (pos + width).min(bytes.len());
    bytes
        .get(pos..end)
        .and_then(|slice| core::str::from_utf8(slice).ok())
        .and_then(|text| text.chars().next())
        .unwrap_or('\u{fffd}')
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Start of the word before `pos`, skipping any separators first.
fn word_left(bytes: &[u8], pos: usize) -> usize {
    let mut at = pos.min(bytes.len());
    while at > 0 && !is_word(char_at(bytes, prev_boundary(bytes, at))) {
        at = prev_boundary(bytes, at);
    }
    while at > 0 && is_word(char_at(bytes, prev_boundary(bytes, at))) {
        at = prev_boundary(bytes, at);
    }
    at
}

/// End of the word after `pos`, skipping any separators first.
fn word_right(bytes: &[u8], pos: usize) -> usize {
    let mut at = pos.min(bytes.len());
    while at < bytes.len() && !is_word(char_at(bytes, at)) {
        at = next_boundary(bytes, at);
    }
    while at < bytes.len() && is_word(char_at(bytes, at)) {
        at = next_boundary(bytes, at);
    }
    at
}

/// Byte offset of char boundary `index`, or the length past the last one.
pub fn boundary_to_byte(bytes: &[u8], index: usize) -> usize {
    let mut seen = 0;
    for (pos, byte) in bytes.iter().enumerate() {
        if is_continuation(*byte) {
            continue;
        }
        if seen == index {
            return pos;
        }
        seen += 1;
    }
    bytes.len()
}

/// Char boundary index of byte offset `byte`, snapped back onto a boundary.
pub fn byte_to_boundary(bytes: &[u8], byte: usize) -> usize {
    let at = snap(bytes, byte);
    bytes[..at]
        .iter()
        .filter(|byte| !is_continuation(**byte))
        .count()
}

/// Boundary index nearest `x`, where `stops[i]` is the x offset of char
/// boundary `i`, ascending. A tie goes to the later boundary.
pub fn caret_at(stops: &[f32], x: f32) -> usize {
    let Some(last) = stops.len().checked_sub(1) else {
        return 0;
    };
    let after = stops.partition_point(|stop| *stop < x);
    if after == 0 {
        return 0;
    }
    if after > last {
        return last;
    }
    if x - stops[after - 1] < stops[after] - x {
        after - 1
    } else {
        after
    }
}

// Editing.

/// The selected bytes, for a copy.
pub fn selection(text: &[u8], len: usize, cursor: Cursor) -> &[u8] {
    let bytes = &text[..len.min(text.len())];
    let start = cursor.start().min(bytes.len());
    &bytes[start..cursor.end().min(bytes.len())]
}

fn remove_range(text: &mut [u8], len: &mut usize, start: usize, end: usize) {
    text.copy_within(end..*len, start);
    *len -= end - start;
}

/// Removes the selection and collapses the cursor onto its start. Returns
/// whether anything was removed. A cut is a copy of [`selection`] and this.
pub fn delete_selection(text: &mut [u8], len: &mut usize, cursor: &mut Cursor) -> bool {
    *len = (*len).min(text.len());
    *cursor = cursor.snapped(&text[..*len]);
    if cursor.is_empty() {
        return false;
    }
    let start = cursor.start();
    remove_range(text, len, start, cursor.end());
    *cursor = Cursor::at(start as u32);
    true
}

fn keep_char(ch: &char) -> bool {
    !ch.is_control()
}

/// Bytes of `typed` that fit in `room`, stopping before the first char that
/// would not, so a char is never split.
fn fitting_size(typed: &str, room: usize) -> usize {
    let mut size = 0;
    for ch in typed.chars().filter(keep_char) {
        if size + ch.len_utf8() > room {
            break;
        }
        size += ch.len_utf8();
    }
    size
}

/// Replaces the selection with `typed`, as typing and paste do. Control
/// chars are dropped, since the field is one line. What does not fit the
/// buffer is cut at a char boundary. Returns whether the text changed.
pub fn insert(text: &mut [u8], len: &mut usize, cursor: &mut Cursor, typed: &str) -> bool {
    let removed = delete_selection(text, len, cursor);
    let size = fitting_size(typed, text.len() - *len);
    if size == 0 {
        return removed;
    }
    let at = cursor.caret as usize;
    text.copy_within(at..*len, at + size);
    let mut write = at;
    for ch in typed.chars().filter(keep_char) {
        let width = ch.len_utf8();
        if write + width > at + size {
            break;
        }
        ch.encode_utf8(&mut text[write..write + width]);
        write += width;
    }
    *len += size;
    *cursor = Cursor::at((at + size) as u32);
    true
}

/// Applies this frame's typed text, then its keys, to the text and cursor.
///
/// Text goes first because the two arrive separated and typing then
/// correcting within one frame is the common case. Releases are ignored and
/// repeats count.
pub fn edit(text: &mut [u8], len: &mut usize, cursor: &mut Cursor, input: &Input) -> Edit {
    *len = (*len).min(text.len());
    *cursor = cursor.snapped(&text[..*len]);
    let before = *cursor;
    let mut out = Edit::default();
    let typed = input.text();
    if !typed.is_empty() {
        out.changed |= insert(text, len, cursor, typed);
    }
    for key in input.keys() {
        apply_key(text, len, cursor, *key, &mut out);
    }
    out.moved = *cursor != before;
    out
}

fn apply_key(text: &mut [u8], len: &mut usize, cursor: &mut Cursor, key: KeyEvent, out: &mut Edit) {
    if !key.pressed {
        return;
    }
    let mods = key.mods;
    match key.sym {
        keys::RETURN | KP_ENTER => out.submitted = true,
        keys::ESCAPE => out.cancelled = true,
        keys::BACKSPACE => out.changed |= erase(text, len, cursor, false, mods.ctrl),
        keys::DELETE => out.changed |= erase(text, len, cursor, true, mods.ctrl),
        keys::A if mods.ctrl => {
            *cursor = Cursor {
                caret: *len as u32,
                anchor: 0,
            }
        }
        sym => step(&text[..*len], cursor, sym, mods),
    }
}

/// Removes the selection, or one char or word toward `forward`.
fn erase(text: &mut [u8], len: &mut usize, cursor: &mut Cursor, forward: bool, word: bool) -> bool {
    if delete_selection(text, len, cursor) {
        return true;
    }
    let bytes = &text[..*len];
    let caret = cursor.caret as usize;
    let (start, end) = match (forward, word) {
        (true, true) => (caret, word_right(bytes, caret)),
        (true, false) => (caret, next_boundary(bytes, caret)),
        (false, true) => (word_left(bytes, caret), caret),
        (false, false) => (prev_boundary(bytes, caret), caret),
    };
    if start >= end {
        return false;
    }
    remove_range(text, len, start, end);
    *cursor = Cursor::at(start as u32);
    true
}

/// Caret movement. Without shift the selection collapses onto the caret,
/// and a plain arrow over a selection lands on the selection's edge.
fn step(bytes: &[u8], cursor: &mut Cursor, sym: u32, mods: Mods) {
    let caret = cursor.caret as usize;
    let collapse = !mods.shift && !cursor.is_empty();
    let target = match sym {
        keys::LEFT if mods.ctrl => word_left(bytes, caret),
        keys::LEFT if collapse => cursor.start(),
        keys::LEFT => prev_boundary(bytes, caret),
        keys::RIGHT if mods.ctrl => word_right(bytes, caret),
        keys::RIGHT if collapse => cursor.end(),
        keys::RIGHT => next_boundary(bytes, caret),
        keys::HOME => 0,
        keys::END => bytes.len(),
        _ => return,
    };
    cursor.caret = target as u32;
    if !mods.shift {
        cursor.anchor = cursor.caret;
    }
}

// The widget.

/// Colours and metrics. Alpha lives in the top byte, zero alpha never paints.
/// The caret and selection colours are for the caller, which draws both.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldStyle {
    pub fill: u32,
    pub focused_fill: u32,
    pub border: u32,
    pub focused_border: u32,
    pub caret: u32,
    pub selection: u32,
    pub border_width: f32,
    /// Outer corner radius. The fill's corners are this less the border.
    pub radius: f32,
    /// Space between the border and the content box.
    pub pad: f32,
    pub height: f32,
    /// Width the caret is kept visible by when scrolling.
    pub caret_width: f32,
}

impl Default for FieldStyle {
    fn default() -> Self {
        FieldStyle::from(&Theme::default())
    }
}

impl From<&Theme> for FieldStyle {
    fn from(theme: &Theme) -> Self {
        FieldStyle {
            fill: theme.pane,
            focused_fill: theme.pane,
            border: theme.border,
            focused_border: theme.accent,
            caret: theme.cursor,
            selection: theme.selection,
            border_width: 1.0,
            radius: 4.0,
            pad: 6.0,
            height: 26.0,
            caret_width: 1.0,
        }
    }
}

/// What the field did this frame, and where the caller draws.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Field {
    /// Content rect from the previous solve. For this frame's, read
    /// `ui.geometry_of(content_index)` after `end_frame`.
    pub content: Rect,
    /// Clip of the content box from the previous solve.
    pub clip: Rect,
    pub content_index: u32,
    /// Horizontal scroll. Text x of boundary `i` is `content.min[0] + stops[i] - scroll`.
    pub scroll: f32,
    /// After this frame's edit, in the edited text.
    pub cursor: Cursor,
    pub focused: bool,
    pub changed: bool,
    pub moved: bool,
    pub submitted: bool,
    pub cancelled: bool,
    /// Merged over the outer, fill and content boxes.
    pub response: Response,
}

/// Flag bits in `user[1]`. `user[0]` is the packed cursor.
const WAS_ACTIVE: u64 = 1;
const WAS_DOWN: u64 = 2;

fn k_inner(key: u64) -> u64 {
    child(key, hash_str("field.inner"))
}
fn k_content(key: u64) -> u64 {
    child(key, hash_str("field.content"))
}

fn validate(text: &[u8], len: usize, style: &FieldStyle) -> Result<(), WidgetError> {
    if style.height <= 0.0 {
        return Err(WidgetError::FieldHeightNotPositive);
    }
    if style.border_width < 0.0 || style.pad < 0.0 || style.radius < 0.0 {
        return Err(WidgetError::FieldInsetNegative);
    }
    if len > text.len() {
        return Err(WidgetError::FieldLengthOutOfRange);
    }
    Ok(())
}

/// A single line text field.
///
/// A press inside focuses it and places the caret, shift keeps the anchor,
/// and dragging extends the selection. A press anywhere else drops its
/// focus. While focused it runs [`edit`] on this frame's input. Escape only
/// reports `cancelled`, so whether that also blurs is the caller's call.
pub fn field(
    ui: &mut Ui,
    key: u64,
    width: SizeSpec,
    text: &mut [u8],
    len: &mut usize,
    stops: &[f32],
    style: FieldStyle,
) -> Result<Field, WidgetError> {
    validate(text, *len, &style)?;
    let geometry = load(ui, k_content(key))?;
    let mut st = load(ui, key)?;
    if st.is_fresh(ui.frame_index()) {
        // Nothing is hit testable until the first solve lands.
        ui.request_repaint();
    }
    let start = Cursor::unpack(st.user[0]).snapped(&text[..*len]);
    let mut cursor = start;
    let hit = PointerHit {
        key,
        content: geometry.last_rect,
        scroll: st.scroll[0],
        stops,
    };
    pointer(ui, &hit, &text[..*len], &mut st, &mut cursor);
    let focused = ui.focusable(key);
    let mut result = Edit::default();
    if focused {
        result = edit(text, len, &mut cursor, &ui.input());
    }
    let view_w = geometry.last_rect.width();
    let caret_x = stop_of(
        stops,
        byte_to_boundary(&text[..*len], cursor.caret as usize),
    );
    let text_w = stops.last().copied().unwrap_or(0.0);
    st.scroll[0] = reveal(st.scroll[0], caret_x, text_w, view_w, style.caret_width);
    st.user[0] = cursor.pack();
    store(ui, key, st)?;
    if result.changed || cursor != start {
        // The caller remeasures from the edited text, which the next frame reads.
        ui.request_repaint();
    }
    let (response, content_index) = emit(ui, key, width, &style, focused)?;
    Ok(Field {
        content: geometry.last_rect,
        clip: geometry.last_clip,
        content_index,
        scroll: st.scroll[0],
        cursor,
        focused,
        changed: result.changed,
        moved: cursor != start,
        submitted: result.submitted,
        cancelled: result.cancelled,
        response,
    })
}

/// What a pointer hit resolves against, all from the previous frame.
struct PointerHit<'s> {
    key: u64,
    content: Rect,
    scroll: f32,
    stops: &'s [f32],
}

fn ours_active(ui: &Ui, key: u64) -> bool {
    ui.peek_active(key) || ui.peek_active(k_inner(key)) || ui.peek_active(k_content(key))
}

fn ours_hovered(ui: &Ui, key: u64) -> bool {
    ui.peek_hovered(key) || ui.peek_hovered(k_inner(key)) || ui.peek_hovered(k_content(key))
}

/// Press, drag and blur. `bytes` is the text before this frame's edit, the
/// text `stops` measured.
fn pointer(ui: &mut Ui, hit: &PointerHit, bytes: &[u8], st: &mut WidgetState, cursor: &mut Cursor) {
    let input = ui.input();
    let active = ours_active(ui, hit.key) && !st.is_fresh(ui.frame_index());
    let hovered = ours_hovered(ui, hit.key);
    let was_active = st.user[1] & WAS_ACTIVE != 0;
    let was_down = st.user[1] & WAS_DOWN != 0;
    if active {
        let x = input.pointer[0] - hit.content.min[0] + hit.scroll;
        let byte = boundary_to_byte(bytes, caret_at(hit.stops, x)) as u32;
        if !was_active {
            ui.focus(Some(hit.key));
            if !input.mods.shift {
                cursor.anchor = byte;
            }
        }
        cursor.caret = byte;
    } else if input.pointer_down && !was_down && !hovered && ui.is_focused(hit.key) {
        ui.focus(None);
    }
    st.user[1] = u64::from(active) * WAS_ACTIVE + u64::from(input.pointer_down) * WAS_DOWN;
}

fn stop_of(stops: &[f32], boundary: usize) -> f32 {
    let last = stops.len().saturating_sub(1);
    stops.get(boundary.min(last)).copied().unwrap_or(0.0)
}

/// Scroll that moves the least to keep `[caret_x, caret_x + caret_w]` inside
/// a view `view_w` wide, clamped to the text. Unchanged before the first
/// solve, when there is no view to fit.
pub(crate) fn reveal(scroll: f32, caret_x: f32, text_w: f32, view_w: f32, caret_w: f32) -> f32 {
    if view_w <= 0.0 {
        return scroll;
    }
    let mut offset = fmin(scroll, caret_x);
    offset = fmax(offset, caret_x + caret_w - view_w);
    clampf(offset, 0.0, fmax(text_w + caret_w - view_w, 0.0))
}

fn radius_px(radius: f32) -> u8 {
    clampf(radius + 0.5, 0.0, 255.0) as u8
}

/// Border box, fill box inset by the border, clipped content box inset by
/// the pad. Returns the merged response and the content box index.
fn emit(
    ui: &mut Ui,
    key: u64,
    width: SizeSpec,
    style: &FieldStyle,
    focused: bool,
) -> Result<(Response, u32), WidgetError> {
    let (border, fill) = if focused {
        (style.focused_border, style.focused_fill)
    } else {
        (style.border, style.fill)
    };
    let outer = BoxSpec::container(Axis::X)
        .w(width)
        .h(SizeSpec::px(style.height))
        .pad(style.border_width);
    let outer_response = ui.open(key, outer, border)?;
    ui.shape(Shape::round(radius_px(style.radius)));
    let inner = BoxSpec::container(Axis::X)
        .w(SizeSpec::grow())
        .h(SizeSpec::grow())
        .pad(style.pad);
    let inner_response = ui.open(k_inner(key), inner, fill)?;
    ui.shape(Shape::round(radius_px(fmax(
        style.radius - style.border_width,
        0.0,
    ))));
    let content = BoxSpec::leaf()
        .w(SizeSpec::grow())
        .h(SizeSpec::grow())
        .clip();
    let content_response = ui.leaf(k_content(key), content, 0)?;
    let content_index = ui.last_index();
    ui.close()?;
    ui.close()?;
    let response = merge(merge(outer_response, inner_response), content_response);
    Ok((response, content_index))
}

fn merge(first: Response, second: Response) -> Response {
    Response {
        hovered: first.hovered || second.hovered,
        pressed: first.pressed || second.pressed,
        active: first.active || second.active,
        clicked: first.clicked || second.clicked,
        drag: if second.active {
            second.drag
        } else {
            first.drag
        },
    }
}

#[cfg(test)]
mod tests;
