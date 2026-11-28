//! A menu as boxes and runs of text, for the app to turn into its own
//! quads and glyphs.

use stein_layout::{Align, Rect};

use crate::{Kind, Mark, Menu, Metrics, text_box};

/// One thing to draw, in order, later over earlier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint<'a> {
    Rect {
        rect: Rect,
        colour: u32,
    },
    /// `text` inside `rect`, a row high, at its start or its end.
    Text {
        text: &'a str,
        rect: Rect,
        colour: u32,
        align: Align,
    },
}

/// Colours a menu is painted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub shadow: u32,
    pub border: u32,
    pub bg: u32,
    /// Behind the lit row.
    pub lit: u32,
    pub text: u32,
    pub hint: u32,
    /// A hint on the lit row.
    pub hint_lit: u32,
    pub disabled: u32,
    pub mark: u32,
    pub rule: u32,
}

impl From<&stein_theme::Theme> for Style {
    fn from(t: &stein_theme::Theme) -> Style {
        Style {
            shadow: t.shadow,
            border: t.border,
            bg: t.menu,
            lit: t.selection,
            text: t.fg,
            hint: t.faint,
            hint_lit: t.dim,
            disabled: t.faint,
            mark: t.accent,
            rule: t.border,
        }
    }
}

impl<C: Copy + Default> Menu<C> {
    /// Every open level: a soft shadow, the box, then its rows.
    pub fn paint(&self, m: &Metrics, s: &Style, out: &mut dyn FnMut(Paint<'_>)) {
        for i in 0..self.levels().len() {
            self.paint_level(i, m, s, out);
        }
    }

    /// One level alone, for an app that gives each level a surface.
    pub fn paint_level(&self, i: usize, m: &Metrics, s: &Style, out: &mut dyn FnMut(Paint<'_>)) {
        let Some(l) = self.levels().get(i) else {
            return;
        };
        let r = l.rect;
        let grow = |dx0: f32, dy0: f32, dx1: f32, dy1: f32| Rect {
            min: [r.min[0] + dx0, r.min[1] + dy0],
            max: [r.max[0] + dx1, r.max[1] + dy1],
        };
        for (rect, colour) in [
            (grow(3.0, 4.0, 3.0, 5.0), s.shadow),
            (r, s.border),
            (grow(1.0, 1.0, -1.0, -1.0), s.bg),
        ] {
            out(Paint::Rect { rect, colour });
        }
        let lit = self.lit(i);
        for row in 0..l.len() {
            self.paint_row(i, row, lit == Some(row), m, s, out);
        }
    }

    fn paint_row(
        &self,
        i: usize,
        row: usize,
        lit: bool,
        m: &Metrics,
        s: &Style,
        out: &mut dyn FnMut(Paint<'_>),
    ) {
        let l = &self.levels()[i];
        let r = l.rows()[row];
        let b = l.row_rect(row, m);
        if r.kind == Kind::Sep {
            let y = ((b.min[1] + b.max[1]) * 0.5).round();
            let rect = Rect {
                min: [b.min[0] + 2.0, y],
                max: [b.max[0] - 2.0, y + 1.0],
            };
            return out(Paint::Rect {
                rect,
                colour: s.rule,
            });
        }
        if lit {
            out(Paint::Rect {
                rect: b,
                colour: s.lit,
            });
        }
        let text = if r.enabled { s.text } else { s.disabled };
        let mark = match (r.mark, r.on) {
            (Mark::Check, true) => "\u{2713}",
            (Mark::Radio, true) => "\u{2022}",
            _ => "",
        };
        let label_x = b.min[0] + m.mark;
        let end = b.max[0] - m.end + m.pad;
        let right = match r.kind {
            Kind::Sub => "\u{203a}",
            _ => l.hint(row),
        };
        let right_colour = match (r.kind, lit, r.enabled) {
            (_, _, false) => s.disabled,
            (Kind::Sub, _, _) => text,
            (_, true, _) => s.hint_lit,
            _ => s.hint,
        };
        let runs = [
            (mark, text_box(b, b.min[0], label_x), s.mark, Align::Center),
            (l.label(row), text_box(b, label_x, end), text, Align::Start),
            (right, text_box(b, label_x, end), right_colour, Align::End),
        ];
        for (t, rect, colour, align) in runs.into_iter().filter(|r| !r.0.is_empty()) {
            out(Paint::Text {
                text: t,
                rect,
                colour,
                align,
            });
        }
    }
}
