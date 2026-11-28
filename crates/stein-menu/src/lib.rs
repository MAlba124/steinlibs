//! Context menus as plain data: levels of rows, a cursor, where each
//! level sits, and the moves between them.
//!
//! ```text
//! open ──► fill rows ──► place ──► hover, press, key ──► Outcome
//!                                     └──► OpenSub ──► fill the next level ──► place_sub
//! paint ──► Paint::Rect, Paint::Text ──► the app's quads and text
//! ```
//!
//! A menu is a stack of up to [`LEVELS`] levels, the root first and each
//! submenu hanging from a row of the one before. A level holds its rows
//! and their text in fixed tables, so opening, moving and painting a menu
//! allocate nothing. Rows carry the app's own command type; the menu never
//! runs anything, it answers with an [`Outcome`] and the app acts.
//!
//! Text is the app's: it measures a label for placement and draws the runs
//! [`Menu::paint`] hands it, proportional or on a grid alike. Levels the
//! app places itself, such as popup surfaces of their own, work too: set
//! [`Level::rect`] and hand the pointer over in the same space.

use stein_layout::Rect;
use stein_ui::keys;

mod paint;

pub use paint::{Paint, Style};

/// Levels open at once, the root included. Tray menus nest this deep.
pub const LEVELS: usize = 8;
/// Rows a level holds, enough for a network applet's list. Past this,
/// pushes are dropped.
pub const ROWS: usize = 64;
/// Bytes of label and hint text a level holds.
pub const TEXT: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Item,
    /// Opens a submenu, which the app fills when asked.
    Sub,
    Sep,
}

/// What shows in the column before the label when a row is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    None,
    Check,
    Radio,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Span {
    at: u16,
    len: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row<C> {
    pub kind: Kind,
    pub cmd: C,
    pub mark: Mark,
    /// The mark shows.
    pub on: bool,
    /// A disabled row shows dim and the cursor passes over it.
    pub enabled: bool,
    label: Span,
    hint: Span,
}

impl<C: Copy> Row<C> {
    /// Whether the cursor may rest here.
    pub fn live(&self) -> bool {
        self.kind != Kind::Sep && self.enabled
    }

    pub fn disabled(&mut self) -> &mut Row<C> {
        self.enabled = false;
        self
    }
}

/// Sizes a level is laid out with, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub row: f32,
    pub sep: f32,
    /// Above the first row and below the last, and the rows' inset from
    /// the edge.
    pub pad: f32,
    /// The mark column before the label.
    pub mark: f32,
    /// Between the label and a hint.
    pub gap: f32,
    /// After the hint or the submenu arrow.
    pub end: f32,
    pub min_w: f32,
    pub max_w: f32,
}

impl Metrics {
    /// Metrics for rows `row` tall with text `em` wide per character: a
    /// separator half a row, the columns a few characters.
    pub fn new(row: f32, em: f32) -> Metrics {
        Metrics {
            row,
            sep: (row * 0.5).round(),
            pad: 4.0,
            mark: em * 2.0,
            gap: em * 3.0,
            end: em * 1.5,
            min_w: 0.0,
            max_w: f32::MAX,
        }
    }
}

/// One box of rows.
#[derive(Clone, Copy, Debug)]
pub struct Level<C> {
    rows: [Row<C>; ROWS],
    len: usize,
    text: [u8; TEXT],
    used: usize,
    /// The row the pointer or the keyboard is on, never a separator.
    pub at: Option<usize>,
    /// Where it sits, set by placing it.
    pub rect: Rect,
    /// The row of the level before that this one hangs from.
    pub from: Option<usize>,
}

impl<C: Copy + Default> Level<C> {
    fn empty() -> Level<C> {
        Level {
            rows: [Row {
                kind: Kind::Sep,
                cmd: C::default(),
                mark: Mark::None,
                on: false,
                enabled: true,
                label: Span::default(),
                hint: Span::default(),
            }; ROWS],
            len: 0,
            text: [0; TEXT],
            used: 0,
            at: None,
            rect: Rect::default(),
            from: None,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.used = 0;
        self.at = None;
        self.from = None;
    }

    /// Text into the level's table, cut at a character boundary when the
    /// table is full.
    fn put(&mut self, s: &str) -> Span {
        let room = TEXT - self.used;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        let at = self.used;
        self.text[at..at + n].copy_from_slice(&s.as_bytes()[..n]);
        self.used += n;
        Span {
            at: at as u16,
            len: n as u16,
        }
    }

    fn text_of(&self, s: Span) -> &str {
        let (a, b) = (s.at as usize, s.at as usize + s.len as usize);
        core::str::from_utf8(&self.text[a..b]).unwrap_or("")
    }

    /// Add a row. When the level is full the row goes to a scratch slot
    /// that is never shown, so a builder never has to check.
    pub fn push(&mut self, kind: Kind, label: &str, hint: &str, cmd: C) -> &mut Row<C> {
        let (label, hint) = (self.put(label), self.put(hint));
        let i = self.len.min(ROWS - 1);
        if self.len < ROWS {
            self.len += 1;
        }
        self.rows[i] = Row {
            kind,
            cmd,
            mark: Mark::None,
            on: false,
            enabled: true,
            label,
            hint,
        };
        &mut self.rows[i]
    }

    pub fn item(&mut self, label: &str, hint: &str, cmd: C) -> &mut Row<C> {
        self.push(Kind::Item, label, hint, cmd)
    }

    /// An item with a tick when `on`.
    pub fn check(&mut self, label: &str, cmd: C, on: bool) -> &mut Row<C> {
        let r = self.push(Kind::Item, label, "", cmd);
        r.mark = Mark::Check;
        r.on = on;
        r
    }

    /// An item with a dot when `on`, one of a group.
    pub fn radio(&mut self, label: &str, cmd: C, on: bool) -> &mut Row<C> {
        let r = self.push(Kind::Item, label, "", cmd);
        r.mark = Mark::Radio;
        r.on = on;
        r
    }

    /// A row that opens a submenu; `cmd` tells the app which.
    pub fn sub(&mut self, label: &str, cmd: C) -> &mut Row<C> {
        self.push(Kind::Sub, label, "", cmd)
    }

    pub fn sep(&mut self) {
        self.push(Kind::Sep, "", "", C::default());
    }

    pub fn rows(&self) -> &[Row<C>] {
        &self.rows[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn label(&self, i: usize) -> &str {
        self.rows().get(i).map_or("", |r| self.text_of(r.label))
    }

    pub fn hint(&self, i: usize) -> &str {
        self.rows().get(i).map_or("", |r| self.text_of(r.hint))
    }

    /// Move the cursor `d` live rows. Stops at the ends, or wraps.
    pub fn step(&mut self, d: isize, wrap: bool) {
        let n = self.len as isize;
        if !self.rows().iter().any(Row::live) {
            return;
        }
        let from = match self.at {
            Some(i) => i as isize,
            None if d > 0 => -1,
            None => n,
        };
        let step = d.signum();
        let at = |i: isize| match wrap {
            true => Some(i.rem_euclid(n)),
            false => (0..n).contains(&i).then_some(i),
        };
        let next = (1..=n)
            .map_while(|k| at(from + step * k))
            .find(|&i| self.rows[i as usize].live());
        if let Some(i) = next {
            self.at = Some(i as usize);
        }
    }

    fn row_h(&self, i: usize, m: &Metrics) -> f32 {
        match self.rows[i].kind {
            Kind::Sep => m.sep,
            _ => m.row,
        }
    }

    /// The width and height the rows need.
    pub fn size(&self, m: &Metrics, measure: &mut dyn FnMut(&str) -> f32) -> [f32; 2] {
        let (mut label, mut hint, mut sub) = (0.0f32, 0.0f32, false);
        for (i, r) in self.rows().iter().enumerate() {
            label = label.max(measure(self.label(i)));
            hint = hint.max(measure(self.hint(i)));
            sub |= r.kind == Kind::Sub;
        }
        let right = match (hint > 0.0, sub) {
            (true, _) => m.gap + hint,
            (false, true) => m.gap,
            (false, false) => 0.0,
        };
        let w = (2.0 * m.pad + m.mark + label + right + m.end).clamp(m.min_w, m.max_w);
        let h = (0..self.len).map(|i| self.row_h(i, m)).sum::<f32>() + 2.0 * m.pad;
        [w, h]
    }

    /// Row `i`'s box, inset from the level's edges.
    pub fn row_rect(&self, i: usize, m: &Metrics) -> Rect {
        let y0 = self.rect.min[1] + m.pad + (0..i).map(|j| self.row_h(j, m)).sum::<f32>();
        let y1 = y0 + if i < self.len { self.row_h(i, m) } else { 0.0 };
        Rect {
            min: [self.rect.min[0] + m.pad, y0],
            max: [self.rect.max[0] - m.pad, y1],
        }
    }

    /// The live row under a point.
    pub fn row_at(&self, p: [f32; 2], m: &Metrics) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        (0..self.len)
            .find(|&i| {
                let r = self.row_rect(i, m);
                p[1] >= r.min[1] && p[1] < r.max[1]
            })
            .filter(|&i| self.rows[i].live())
    }
}

/// What a move did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome<C> {
    Stay,
    /// Dismissed: Escape on the root, or a press outside every level.
    Close,
    /// Row `row` of level `level` wants its submenu. Fill it with
    /// [`Menu::open_sub`] and place it; a keyboard open then goes in with
    /// [`Menu::enter_sub`].
    OpenSub {
        level: usize,
        row: usize,
        cmd: C,
    },
    /// An item was chosen. The menu stays open until the app closes it.
    Run(C),
}

/// The open levels and which one the keyboard drives.
#[derive(Clone, Copy, Debug)]
pub struct Menu<C> {
    levels: [Level<C>; LEVELS],
    depth: usize,
    focus: usize,
    /// Where the pointer was last seen, and on which level's surface when
    /// levels have their own, so a pointer resting on a row does not take
    /// the cursor back from the keys every frame.
    pointer: Option<(usize, [f32; 2])>,
    /// Up past the last row goes to the first.
    pub wrap: bool,
}

impl<C: Copy + Default> Default for Menu<C> {
    fn default() -> Menu<C> {
        Menu::new()
    }
}

impl<C: Copy + Default> Menu<C> {
    pub fn new() -> Menu<C> {
        Menu {
            levels: [Level::empty(); LEVELS],
            depth: 0,
            focus: 0,
            pointer: None,
            wrap: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.depth > 0
    }

    /// Open empty, and hand over the root to fill.
    pub fn open(&mut self) -> &mut Level<C> {
        self.depth = 1;
        self.focus = 0;
        self.pointer = None;
        self.levels[0].clear();
        &mut self.levels[0]
    }

    pub fn close(&mut self) {
        self.depth = 0;
        self.focus = 0;
    }

    /// The open levels, the root first.
    pub fn levels(&self) -> &[Level<C>] {
        &self.levels[..self.depth]
    }

    pub fn level_mut(&mut self, i: usize) -> Option<&mut Level<C>> {
        self.levels[..self.depth].get_mut(i)
    }

    /// The level the keyboard drives.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// The root, `at` its top left, kept inside `view`: flipped left of
    /// `at` when it would run off the right, pulled up off the bottom.
    pub fn place(
        &mut self,
        at: [f32; 2],
        view: [f32; 2],
        m: &Metrics,
        measure: &mut dyn FnMut(&str) -> f32,
    ) {
        let l = &mut self.levels[0];
        let [w, h] = l.size(m, measure);
        let x = match at[0] + w > view[0] {
            true => (at[0] - w).max(0.0),
            false => at[0],
        };
        let y = at[1].min(view[1] - h).max(0.0);
        l.rect = rect(x, y, w, h);
    }

    /// Clear the level after `level`, hang it from its `row`, and hand it
    /// over to fill. Anything deeper closes.
    pub fn open_sub(&mut self, level: usize, row: usize) -> Option<&mut Level<C>> {
        let next = level + 1;
        if next >= LEVELS || level >= self.depth {
            return None;
        }
        self.depth = next + 1;
        self.focus = self.focus.min(level);
        let l = &mut self.levels[next];
        l.clear();
        l.from = Some(row);
        Some(l)
    }

    /// Place the deepest level beside the row it hangs from: right of its
    /// parent, or left when the right has no room.
    pub fn place_sub(&mut self, view: [f32; 2], m: &Metrics, measure: &mut dyn FnMut(&str) -> f32) {
        let i = self.depth - 1;
        if i == 0 {
            return;
        }
        let parent = self.levels[i - 1];
        let row = parent.row_rect(self.levels[i].from.unwrap_or(0), m);
        let l = &mut self.levels[i];
        let [w, h] = l.size(m, measure);
        let right = parent.rect.max[0] - m.pad * 0.5;
        let x = match right + w > view[0] {
            true => (parent.rect.min[0] - w + m.pad * 0.5).max(0.0),
            false => right,
        };
        let y = (row.min[1] - m.pad).min(view[1] - h).max(0.0);
        l.rect = rect(x, y, w, h);
    }

    /// Give the keyboard to the deepest level, on its first live row.
    pub fn enter_sub(&mut self) {
        self.focus = self.depth.saturating_sub(1);
        let l = &mut self.levels[self.focus];
        l.at = None;
        l.step(1, false);
    }

    /// Row `row` of `level` was chosen.
    pub fn choose(&mut self, level: usize, row: usize) -> Outcome<C> {
        let Some(r) = self.levels().get(level).and_then(|l| l.rows().get(row)) else {
            return Outcome::Stay;
        };
        match (r.kind, r.enabled) {
            (_, false) | (Kind::Sep, _) => Outcome::Stay,
            (Kind::Item, true) => Outcome::Run(r.cmd),
            (Kind::Sub, true) => Outcome::OpenSub {
                level,
                row,
                cmd: r.cmd,
            },
        }
    }

    /// A key. Up and Down move, Home and End jump, Enter and Space choose,
    /// Right opens a submenu, Left and Escape back out one level.
    pub fn key(&mut self, sym: u32) -> Outcome<C> {
        let (wrap, f) = (self.wrap, self.focus);
        match sym {
            keys::DOWN => self.levels[f].step(1, wrap),
            keys::UP => self.levels[f].step(-1, wrap),
            keys::HOME => {
                self.levels[f].at = None;
                self.levels[f].step(1, false);
            }
            keys::END => {
                self.levels[f].at = None;
                self.levels[f].step(-1, false);
            }
            keys::ESCAPE | keys::LEFT if f > 0 => {
                self.depth = f;
                self.focus = f - 1;
            }
            keys::ESCAPE => return Outcome::Close,
            keys::RETURN | keys::SPACE | keys::RIGHT => return self.chosen(sym == keys::RIGHT),
            _ => (),
        }
        Outcome::Stay
    }

    /// The cursor's row chosen. Right only opens submenus.
    fn chosen(&mut self, right: bool) -> Outcome<C> {
        let f = self.focus;
        let Some(row) = self.levels[f].at else {
            return Outcome::Stay;
        };
        let sub = self.levels[f].rows[row].kind == Kind::Sub;
        match (right, sub) {
            (true, false) => Outcome::Stay,
            _ => self.choose(f, row),
        }
    }

    /// The deepest level under a point, and the live row there.
    pub fn hit(&self, p: [f32; 2], m: &Metrics) -> Option<(usize, Option<usize>)> {
        (0..self.depth)
            .rev()
            .find(|&i| self.levels[i].rect.contains(p))
            .map(|i| (i, self.levels[i].row_at(p, m)))
    }

    /// The pointer is at `p`. When it moved, the row under it lights, its
    /// submenu is asked for, and a deeper level hung from another row
    /// closes. Where it only rests, the keys keep the cursor.
    pub fn hover(&mut self, p: [f32; 2], m: &Metrics) -> Outcome<C> {
        if self.pointer.replace((usize::MAX, p)) == Some((usize::MAX, p)) {
            return Outcome::Stay;
        }
        let hit = self.hit(p, m);
        self.hovered(hit)
    }

    /// As [`Menu::hover`], for a level on a surface of its own: `p` is in
    /// that level's space and only it is hit.
    pub fn hover_in(&mut self, level: usize, p: [f32; 2], m: &Metrics) -> Outcome<C> {
        if self.pointer.replace((level, p)) == Some((level, p)) {
            return Outcome::Stay;
        }
        let hit = self.hit_in(level, p, m);
        self.hovered(hit)
    }

    fn hit_in(&self, level: usize, p: [f32; 2], m: &Metrics) -> Option<(usize, Option<usize>)> {
        let l = self.levels().get(level)?;
        l.rect.contains(p).then(|| (level, l.row_at(p, m)))
    }

    fn hovered(&mut self, hit: Option<(usize, Option<usize>)>) -> Outcome<C> {
        let Some((i, Some(row))) = hit else {
            return Outcome::Stay;
        };
        self.levels[i].at = Some(row);
        self.focus = i;
        let r = self.levels[i].rows[row];
        let open_here = self.depth > i + 1 && self.levels[i + 1].from == Some(row);
        match r.kind {
            Kind::Sub if !open_here => Outcome::OpenSub {
                level: i,
                row,
                cmd: r.cmd,
            },
            Kind::Sub => Outcome::Stay,
            _ => {
                self.depth = i + 1;
                Outcome::Stay
            }
        }
    }

    /// A press at `p`: on a live row it chooses, elsewhere in a level it
    /// does nothing, outside every level it closes.
    pub fn press(&mut self, p: [f32; 2], m: &Metrics) -> Outcome<C> {
        let hit = self.hit(p, m);
        self.pressed(hit)
    }

    /// As [`Menu::press`], for a level on a surface of its own.
    pub fn press_in(&mut self, level: usize, p: [f32; 2], m: &Metrics) -> Outcome<C> {
        let hit = self.hit_in(level, p, m);
        self.pressed(hit)
    }

    fn pressed(&mut self, hit: Option<(usize, Option<usize>)>) -> Outcome<C> {
        match hit {
            Some((i, Some(row))) => self.choose(i, row),
            Some((_, None)) => Outcome::Stay,
            None => Outcome::Close,
        }
    }

    /// Which row of level `i` shows lit: the cursor's on the focused
    /// level, and on the levels before it the row the next hangs from.
    pub fn lit(&self, i: usize) -> Option<usize> {
        match i.cmp(&self.focus) {
            core::cmp::Ordering::Equal => self.levels[i].at,
            core::cmp::Ordering::Less => self.levels[i + 1].from,
            core::cmp::Ordering::Greater => self.levels[i].at,
        }
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        min: [x, y],
        max: [x + w, y + h],
    }
}

/// Where a run of row text sits, for [`Paint::Text`].
pub(crate) fn text_box(row: Rect, x0: f32, x1: f32) -> Rect {
    Rect {
        min: [x0, row.min[1]],
        max: [x1.max(x0), row.max[1]],
    }
}

#[cfg(test)]
mod tests;
