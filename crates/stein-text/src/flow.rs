//! Greedy line breaking for proportional text.
//!
//! ```text
//! paragraph ──► Shaper ──► run ──► classify ──► flags
//!                           └──────────┬──────────┘
//!                          flow at a width ──► lines ──► place, one per line
//! ```
//!
//! The sibling of [`crate::wrap`] for text whose glyphs have their own
//! widths. It works on a run that is already shaped, so the advances are the
//! ones the text will be drawn with, kerning and ligatures included, and a
//! line is a slice of that run handed straight to [`crate::MonoAtlas::place`].
//! Nothing is shaped twice.
//!
//! Where a line may break depends on the text alone, so [`classify`] reads
//! it once into a flag byte per glyph, and [`flow`] at any width reads
//! only advances and flags. A resize flows again without looking at a
//! character.
//!
//! Breaks fall between clusters, never inside one, so a base keeps its marks
//! and a ligature stays whole. Where a break may go:
//!
//! - after a run of spaces, which hang past the line's end and are not drawn
//!   or counted in its width
//! - after a hyphen or a dash between a letter or digit and another
//! - after a zero width space
//! - between ideographs, kana and fullwidth forms, except before closing
//!   punctuation and after opening punctuation
//!
//! No-break spaces and word joiners glue. A word wider than the line is cut at
//! the last cluster that fits, and a line always takes at least one cluster,
//! so any width terminates.
//!
//! One paragraph per call: split on newlines first, since the shaper takes a
//! line without them. A run whose clusters descend is right to left and is
//! broken from its logical start, so its first line is the tail of the slice.
//! A run that mixes directions breaks in the order it is stored.
//!
//! The iterator holds a cursor and nothing else and allocates nothing.

use crate::shape::ShapedGlyph;

/// Slack for advances that were summed in a different order.
const EPS: f32 = 1e-3;

/// Flag: this glyph continues the cluster before it.
pub const CONT: u8 = 1;
/// Flag: this cluster is a breakable space, hanging at a line's end.
pub const HANGS: u8 = 2;
/// Flag: a line may start at this cluster.
pub const BREAK: u8 = 4;

/// One line of a paragraph.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Line {
    /// Glyphs to draw, `start..end` of the run in its stored order. Spaces
    /// hanging at the line's end are left out.
    pub start: u32,
    pub end: u32,
    /// Byte offset in the text of the line's first cluster.
    pub text: u32,
    /// Advance of the glyphs to draw.
    pub width: f32,
}

impl Line {
    #[must_use]
    pub fn glyphs<'r>(&self, run: &'r [ShapedGlyph]) -> &'r [ShapedGlyph] {
        run.get(self.start as usize..self.end as usize)
            .unwrap_or(&[])
    }
}

/// Whether `run` is right to left: its clusters descend.
fn descends(run: &[ShapedGlyph]) -> bool {
    match (run.first(), run.last()) {
        (Some(a), Some(b)) => a.cluster > b.cluster,
        _ => false,
    }
}

/// Flags for each glyph of `run`, shaped from `text`, into `out` by the
/// run's index. `out` is cleared first and keeps its capacity.
pub fn classify(text: &str, run: &[ShapedGlyph], out: &mut Vec<u8>) {
    out.clear();
    out.resize(run.len(), 0);
    let n = run.len();
    let rtl = descends(run);
    let at = |k: usize| if rtl { n - 1 - k } else { k };
    let mut prev = u32::MAX;
    for k in 0..n {
        let i = at(k);
        let c = run[i].cluster;
        if k > 0 && c == prev {
            out[i] = CONT;
            continue;
        }
        let mut f = if is_space(text, c) { HANGS } else { 0 };
        if k > 0 && breaks_between(text, prev.max(c) as usize) {
            f |= BREAK;
        }
        out[i] = f;
        prev = c;
    }
}

/// Whether a line may start at byte `at` of `text`.
fn breaks_between(text: &str, at: usize) -> bool {
    let b = text.as_bytes();
    if let (Some(&p), Some(&n)) = (b.get(at.wrapping_sub(1)), b.get(at))
        && let Some(yes) = ascii_allowed(b.get(at.wrapping_sub(2)).copied(), p, n)
    {
        return yes;
    }
    let (Some(head), Some(tail)) = (text.get(..at), text.get(at..)) else {
        return false;
    };
    let mut back = head.chars().rev();
    let (Some(prev), Some(next)) = (back.next(), tail.chars().next()) else {
        return false;
    };
    allowed(back.next(), prev, next)
}

/// Whether the cluster at byte `at` is a breakable space.
fn is_space(text: &str, at: u32) -> bool {
    let at = at as usize;
    match text.as_bytes().get(at) {
        Some(&b) if b < 0x80 => ASCII[usize::from(b)] == SPACE,
        Some(_) => text
            .get(at..)
            .and_then(|s| s.chars().next())
            .is_some_and(space),
        None => false,
    }
}

/// Lines of one shaped paragraph, each at most `width` wide where a break
/// allows.
pub struct Flow<'a> {
    run: &'a [ShapedGlyph],
    flags: &'a [u8],
    width: f32,
    rtl: bool,
    /// Logical index of the next line's first glyph.
    at: usize,
    /// An empty paragraph still owes its one empty line.
    blank: bool,
}

/// Break `run` into lines at most `width` wide, `flags` from [`classify`].
///
/// A paragraph always has one line at least, so an empty one keeps its
/// height.
#[must_use]
pub fn flow<'a>(run: &'a [ShapedGlyph], flags: &'a [u8], width: f32) -> Flow<'a> {
    let n = run.len().min(flags.len());
    let run = &run[..n];
    Flow {
        run,
        flags: &flags[..n],
        width,
        rtl: descends(run),
        at: 0,
        blank: n == 0,
    }
}

/// How many lines `run` breaks into at `width`.
#[must_use]
pub fn flow_count(run: &[ShapedGlyph], flags: &[u8], width: f32) -> usize {
    flow(run, flags, width).count()
}

/// Logical glyph `k` of an `n` glyph run.
#[inline]
fn at<const RTL: bool>(n: usize, k: usize) -> usize {
    if RTL { n - 1 - k } else { k }
}

/// The cluster from logical glyph `k`: where the next one starts, and its
/// advance.
#[inline]
fn cluster<const RTL: bool>(run: &[ShapedGlyph], flags: &[u8], k: usize) -> (usize, f32) {
    let n = run.len();
    let mut adv = run[at::<RTL>(n, k)].x_advance;
    let mut j = k + 1;
    while j < n && flags[at::<RTL>(n, j)] & CONT != 0 {
        adv += run[at::<RTL>(n, j)].x_advance;
        j += 1;
    }
    (j, adv)
}

/// One line from logical glyph `k0`, and where the next starts. The
/// direction is a parameter so the index map is not a branch.
fn scan<const RTL: bool>(
    run: &[ShapedGlyph],
    flags: &[u8],
    k0: usize,
    width: f32,
) -> (Line, usize) {
    let n = run.len();
    let limit = width + EPS;
    // Pen with hanging spaces, the end and width of what is drawn, and the
    // last break as next start, drawn end and drawn width, `k0` for none.
    let mut pen = 0.0f32;
    let (mut ink, mut ink_w) = (k0, 0.0f32);
    let (mut bk, mut be, mut bw) = (k0, k0, 0.0f32);
    let mut k = k0;
    while k < n {
        let f = flags[at::<RTL>(n, k)];
        let (j, adv) = cluster::<RTL>(run, flags, k);
        let hangs = f & HANGS != 0;
        let brk = f & BREAK != 0 && k > k0;
        (bk, be, bw) = pick(brk, (k, ink, ink_w), (bk, be, bw));
        if !hangs && k > k0 && pen + adv > limit {
            // No break seen: cut the word before this cluster.
            let (next, end, w) = pick(bk > k0, (bk, be, bw), (k, ink, ink_w));
            return (line::<RTL>(run, k0, end, w), next);
        }
        pen += adv;
        (ink, ink_w) = pick(hangs, (ink, ink_w), (j, pen));
        k = j;
    }
    (line::<RTL>(run, k0, ink, ink_w), n)
}

#[inline]
fn pick<T>(first: bool, a: T, b: T) -> T {
    if first { a } else { b }
}

/// Logical glyphs `k0..k1` as a line.
fn line<const RTL: bool>(run: &[ShapedGlyph], k0: usize, k1: usize, width: f32) -> Line {
    let n = run.len();
    let (start, end) = if RTL { (n - k1, n - k0) } else { (k0, k1) };
    Line {
        start: start as u32,
        end: end as u32,
        text: run[at::<RTL>(n, k0)].cluster,
        width,
    }
}

impl Iterator for Flow<'_> {
    type Item = Line;

    fn next(&mut self) -> Option<Line> {
        if self.blank {
            self.blank = false;
            return Some(Line::default());
        }
        if self.at >= self.run.len() {
            return None;
        }
        let (line, next) = if self.rtl {
            scan::<true>(self.run, self.flags, self.at, self.width)
        } else {
            scan::<false>(self.run, self.flags, self.at, self.width)
        };
        self.at = next;
        Some(line)
    }
}

/// Whether a line may break between `prev` and `next`, `before` being the
/// character ahead of `prev`.
#[must_use]
pub fn allowed(before: Option<char>, prev: char, next: char) -> bool {
    if glue(prev) || glue(next) || space(next) {
        return false;
    }
    if space(prev) || prev == '\u{200b}' {
        return true;
    }
    if closes(next) || opens(prev) {
        return false;
    }
    if matches!(prev, '-' | '\u{2010}' | '\u{2013}' | '\u{2014}') {
        // A word's hyphen, not a minus sign or a dash standing alone.
        let word = |c: char| c.is_alphanumeric() || ('\u{300}'..='\u{36f}').contains(&c);
        return before.is_some_and(word) && next.is_alphanumeric();
    }
    ideographic(prev) || ideographic(next)
}

const OTHER: u8 = 0;
const SPACE: u8 = 1;
const HYPHEN: u8 = 2;
const CLOSE: u8 = 3;
const OPEN: u8 = 4;
const ALNUM: u8 = 5;

/// Break classes of ASCII, the same answers [`allowed`] gives.
const ASCII: [u8; 128] = {
    let mut t = [OTHER; 128];
    let mut i = 0;
    while i < 128 {
        let c = i as u8;
        t[i] = match c {
            b' ' | b'\t' | b'\n' => SPACE,
            b'-' => HYPHEN,
            b')' | b']' | b'}' | b',' | b'.' | b':' | b';' | b'!' | b'?' => CLOSE,
            b'(' | b'[' | b'{' => OPEN,
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => ALNUM,
            _ => OTHER,
        };
        i += 1;
    }
    t
};

/// [`allowed`] on bytes, or `None` when one is not ASCII.
#[inline]
fn ascii_allowed(before: Option<u8>, prev: u8, next: u8) -> Option<bool> {
    if prev >= 0x80 || next >= 0x80 {
        return None;
    }
    let (p, n) = (ASCII[usize::from(prev)], ASCII[usize::from(next)]);
    Some(match (p, n) {
        (_, SPACE) => false,
        (SPACE, _) => true,
        (_, CLOSE) | (OPEN, _) => false,
        (HYPHEN, ALNUM) => match before {
            Some(b) if b < 0x80 => ASCII[usize::from(b)] == ALNUM,
            // A letter outside ASCII ahead of the hyphen.
            Some(_) => return None,
            None => false,
        },
        _ => false,
    })
}

/// Breakable spaces. The no-break ones are [`glue`].
fn space(c: char) -> bool {
    matches!(c,
        ' ' | '\t' | '\n' | '\u{1680}' | '\u{3000}' | '\u{205f}'
        | '\u{2000}'..='\u{2006}'
        | '\u{2008}'..='\u{200a}')
}

fn glue(c: char) -> bool {
    matches!(
        c,
        '\u{a0}' | '\u{2007}' | '\u{202f}' | '\u{2060}' | '\u{feff}'
    )
}

/// Scripts written without spaces, where any two characters may break.
fn ideographic(c: char) -> bool {
    matches!(c,
        '\u{2e80}'..='\u{2fff}'
        | '\u{3001}'..='\u{303f}'
        | '\u{3040}'..='\u{30ff}'
        | '\u{3100}'..='\u{31ff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{4e00}'..='\u{9fff}'
        | '\u{f900}'..='\u{faff}'
        | '\u{ff00}'..='\u{ff60}'
        | '\u{20000}'..='\u{3ffff}')
}

/// Never starts a line: closing punctuation, and small kana.
fn closes(c: char) -> bool {
    matches!(
        c,
        ')' | ']'
            | '}'
            | ','
            | '.'
            | ':'
            | ';'
            | '!'
            | '?'
            | '\u{3001}'
            | '\u{3002}'
            | '\u{3009}'
            | '\u{300b}'
            | '\u{300d}'
            | '\u{300f}'
            | '\u{3011}'
            | '\u{3015}'
            | '\u{3017}'
            | '\u{3019}'
            | '\u{301b}'
            | '\u{30fb}'
            | '\u{30fc}'
            | '\u{3005}'
            | '\u{3041}'
            | '\u{3043}'
            | '\u{3045}'
            | '\u{3047}'
            | '\u{3049}'
            | '\u{3063}'
            | '\u{3083}'
            | '\u{3085}'
            | '\u{3087}'
            | '\u{308e}'
            | '\u{30a1}'
            | '\u{30a3}'
            | '\u{30a5}'
            | '\u{30a7}'
            | '\u{30a9}'
            | '\u{30c3}'
            | '\u{30e3}'
            | '\u{30e5}'
            | '\u{30e7}'
            | '\u{30ee}'
            | '\u{30f5}'
            | '\u{30f6}'
            | '\u{ff09}'
            | '\u{ff0c}'
            | '\u{ff0e}'
            | '\u{ff1a}'
            | '\u{ff1b}'
            | '\u{ff1f}'
            | '\u{ff01}'
            | '\u{ff3d}'
            | '\u{ff5d}'
    )
}

/// Never ends a line: opening punctuation.
fn opens(c: char) -> bool {
    matches!(
        c,
        '(' | '['
            | '{'
            | '\u{3008}'
            | '\u{300a}'
            | '\u{300c}'
            | '\u{300e}'
            | '\u{3010}'
            | '\u{3014}'
            | '\u{3016}'
            | '\u{3018}'
            | '\u{301a}'
            | '\u{ff08}'
            | '\u{ff3b}'
            | '\u{ff5b}'
    )
}

#[cfg(test)]
mod tests;
