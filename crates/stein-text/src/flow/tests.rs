use std::string::String;
use std::vec::Vec;

use super::*;
use crate::shape::{Shaper, advance_of};
use crate::test_font;
use crate::wrap::wrap;

/// One glyph per character, clustered like harfrust's default level: a
/// combining mark joins its base's cluster and takes no advance.
fn run(text: &str, adv: impl Fn(char) -> f32) -> Vec<ShapedGlyph> {
    let mut out: Vec<ShapedGlyph> = Vec::new();
    for (at, ch) in text.char_indices() {
        let mark = ('\u{300}'..='\u{36f}').contains(&ch);
        let cluster = match out.last() {
            Some(g) if mark => g.cluster,
            _ => at as u32,
        };
        out.push(ShapedGlyph {
            glyph_id: ch as u32,
            cluster,
            x_advance: if mark { 0.0 } else { adv(ch) },
            ..ShapedGlyph::default()
        });
    }
    out
}

fn flags(text: &str, r: &[ShapedGlyph]) -> Vec<u8> {
    let mut f = Vec::new();
    classify(text, r, &mut f);
    f
}

fn unit(_: char) -> f32 {
    1.0
}

/// The text a left to right line draws.
fn shown<'t>(text: &'t str, r: &[ShapedGlyph], l: &Line) -> &'t str {
    if l.start == l.end {
        return "";
    }
    let from = r[l.start as usize].cluster as usize;
    let to = r
        .get(l.end as usize)
        .map_or(text.len(), |g| g.cluster as usize);
    &text[from..to]
}

fn lines(text: &str, width: f32) -> Vec<String> {
    let r = run(text, unit);
    flow(&r, &flags(text, &r), width)
        .map(|l| String::from(shown(text, &r, &l)))
        .collect()
}

/// Words of lowercase letters, one space apart, from a fixed seed.
fn words(seed: u32, count: usize) -> String {
    let mut x = seed;
    let mut s = String::new();
    for i in 0..count {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = 1 + (x >> 16) as usize % 11;
        if i > 0 {
            s.push(' ');
        }
        for j in 0..len {
            s.push((b'a' + ((x >> 8) as usize + j) as u8 % 26) as char);
        }
    }
    s
}

#[test]
fn unit_advances_break_where_the_column_wrap_does() {
    for seed in 1..20 {
        let text = words(seed, 40);
        for cols in 1..30 {
            let want: Vec<&str> = wrap(&text, cols).collect();
            assert_eq!(lines(&text, cols as f32), want, "seed={seed} cols={cols}");
        }
    }
}

#[test]
fn a_line_fits_unless_it_is_one_cluster() {
    let adv = |c: char| 3.0 + (c as u32 % 7) as f32 * 1.25;
    for seed in 1..10 {
        let text = words(seed, 60);
        let r = run(&text, adv);
        for w in [0.0, 5.0, 17.5, 40.0, 93.0, 250.0] {
            fits(&text, &r, w);
        }
    }
}

fn fits(text: &str, r: &[ShapedGlyph], w: f32) {
    for l in flow(r, &flags(text, r), w) {
        let g = l.glyphs(r);
        assert!(
            (advance_of(g) - l.width).abs() < 1e-3,
            "width is the drawn advance"
        );
        let one = g.iter().all(|x| x.cluster == g[0].cluster);
        assert!(l.width <= w + EPS || one, "w={w} {l:?} in {text:?}");
    }
}

/// The first glyph at or after `k` that is not a space.
fn past_spaces(text: &str, r: &[ShapedGlyph], mut k: usize) -> usize {
    let hangs = |g: &ShapedGlyph| text[g.cluster as usize..].chars().next().is_some_and(space);
    while r.get(k).is_some_and(hangs) {
        k += 1;
    }
    k
}

#[test]
fn lines_tile_the_run_with_only_spaces_between() {
    let text = "the   quick brown-fox jumps\tover  the lazy dog  ";
    let r = run(text, unit);
    for w in 1..20 {
        let mut next = 0usize;
        for l in flow(&r, &flags(text, &r), w as f32) {
            let gap = &r[next..l.start as usize];
            assert!(gap.is_empty(), "w={w} a line starts where the last ended");
            let rest = text[l.text as usize..].chars().next();
            assert!(
                rest.is_none_or(|c| !space(c)) || l.start == 0,
                "w={w} {l:?}"
            );
            next = past_spaces(text, &r, l.end as usize);
        }
        assert_eq!(
            next,
            r.len(),
            "w={w} nothing after the last line but spaces"
        );
    }
}

#[test]
fn spaces_hang_past_the_end() {
    let text = "aa   bb";
    let r = run(text, unit);
    let got: Vec<Line> = flow(&r, &flags(text, &r), 3.0).collect();
    assert_eq!(lines(text, 3.0), ["aa", "bb"]);
    assert_eq!(got[0].width, 2.0);
    assert_eq!(got[1].text, 5);
}

#[test]
fn an_indent_is_drawn() {
    let r = run("  aa", unit);
    let l: Vec<Line> = flow(&r, &flags("  aa", &r), 10.0).collect();
    assert_eq!(
        l,
        [Line {
            start: 0,
            end: 4,
            text: 0,
            width: 4.0
        }]
    );
}

#[test]
fn a_hyphen_between_letters_breaks_after() {
    assert_eq!(lines("well-known thing", 6.0), ["well-", "known", "thing"]);
}

#[test]
fn a_minus_sign_stays_with_its_number() {
    assert_eq!(lines("x -5", 3.0), ["x", "-5"]);
    assert_eq!(lines("x --y", 3.0), ["x", "--y"]);
}

#[test]
fn a_no_break_space_glues() {
    assert_eq!(lines("x a\u{a0}b", 3.0), ["x", "a\u{a0}b"]);
    assert_eq!(lines("x a\u{2060}b", 3.0), ["x", "a\u{2060}b"]);
}

#[test]
fn a_zero_width_space_breaks() {
    assert_eq!(lines("abc\u{200b}def", 4.0), ["abc\u{200b}", "def"]);
}

#[test]
fn ideographs_break_anywhere_but_before_closing_punctuation() {
    let text = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8}\u{3002}";
    assert_eq!(
        lines(text, 4.0),
        [
            "\u{65e5}\u{672c}\u{8a9e}\u{306e}",
            "\u{30c6}\u{30ad}\u{30b9}",
            "\u{30c8}\u{3002}"
        ]
    );
    // An opening bracket keeps what it opens.
    assert_eq!(
        lines("\u{65e5}\u{300c}\u{672c}\u{8a9e}", 2.0),
        ["\u{65e5}", "\u{300c}\u{672c}", "\u{8a9e}"]
    );
}

#[test]
fn a_mark_stays_with_its_base_when_a_word_is_cut() {
    assert_eq!(lines("xe\u{301}y", 1.0), ["x", "e\u{301}", "y"]);
}

#[test]
fn a_ligature_is_never_cut() {
    // "fi" as one glyph under cluster 0, then "x".
    let text = "fix";
    let r = [
        ShapedGlyph {
            cluster: 0,
            x_advance: 2.0,
            ..ShapedGlyph::default()
        },
        ShapedGlyph {
            cluster: 2,
            x_advance: 1.0,
            ..ShapedGlyph::default()
        },
    ];
    let got: Vec<(u32, u32)> = flow(&r, &flags(text, &r), 0.5)
        .map(|l| (l.start, l.end))
        .collect();
    assert_eq!(got, [(0, 1), (1, 2)]);
}

#[test]
fn any_width_terminates() {
    let text = "ab cd";
    let r = run(text, unit);
    for w in [0.0, -3.0, f32::NAN, f32::INFINITY] {
        let n = flow(&r, &flags(text, &r), w).take(100).count();
        assert!((1..=4).contains(&n), "w={w} gave {n} lines");
    }
    assert_eq!(lines(text, 0.0), ["a", "b", "c", "d"]);
}

#[test]
fn an_empty_paragraph_is_one_empty_line() {
    let got: Vec<Line> = flow(&[], &flags("", &[]), 100.0).collect();
    assert_eq!(got, [Line::default()]);
    assert_eq!(flow_count(&[], &[], 100.0), 1);
    assert_eq!(lines("   ", 100.0), [""]);
}

#[test]
fn right_to_left_breaks_from_the_logical_start() {
    let text = "abc def";
    let mut r = run(text, unit);
    r.reverse();
    let got: Vec<Line> = flow(&r, &flags(text, &r), 3.0).collect();
    assert_eq!(got.len(), 2);
    let first: Vec<u32> = got[0].glyphs(&r).iter().map(|g| g.cluster).collect();
    assert_eq!(first, [2, 1, 0], "the first line is the slice's tail");
    assert_eq!((got[0].start, got[0].end, got[0].text), (4, 7, 0));
    assert_eq!((got[1].start, got[1].end, got[1].text), (0, 3, 4));
}

#[test]
fn classify_marks_clusters_spaces_and_breaks() {
    let text = "ab e\u{301}-f";
    let r = run(text, unit);
    assert_eq!(flags(text, &r), [0, 0, HANGS, BREAK, CONT, 0, BREAK]);
    // Right to left: the continuing glyph is the later one in logical order.
    let text = "xe\u{301}";
    let mut r = run(text, unit);
    r.reverse();
    assert_eq!(flags(text, &r), [CONT, 0, 0]);
    let mut out = vec![9; 40];
    classify("", &[], &mut out);
    assert!(out.is_empty());
}

#[test]
fn short_flags_shorten_the_run() {
    let text = "ab cd";
    let r = run(text, unit);
    let f = flags(text, &r);
    let got: Vec<Line> = flow(&r, &f[..2], 100.0).collect();
    assert_eq!(
        got,
        [Line {
            start: 0,
            end: 2,
            text: 0,
            width: 2.0
        }]
    );
}

#[test]
fn break_rules() {
    assert!(allowed(Some('a'), ' ', 'b'));
    assert!(
        !allowed(Some('a'), 'b', ' '),
        "spaces hang, never start a line"
    );
    assert!(!allowed(Some('a'), 'b', 'c'));
    assert!(allowed(Some('a'), '-', 'b'));
    assert!(!allowed(Some(' '), '-', '5'));
    assert!(!allowed(Some('a'), '-', ' '));
    assert!(allowed(Some('a'), '\u{2014}', 'b'));
    assert!(!allowed(Some('a'), ' ', '\u{a0}'));
    assert!(!allowed(Some('a'), '\u{65e5}', '\u{3001}'));
    assert!(!allowed(None, '(', '\u{65e5}'));
    assert!(allowed(None, 'a', '\u{65e5}'));
}

const PARAGRAPH: &str = "Shaping gives every glyph its own advance, so a line \
    holds as many words as their real widths allow. Kerning pulls AV and To \
    together, na\u{ef}ve caf\u{e9}s keep their accents, and a \
    long-hyphenated-compound breaks after its hyphens when it has to.";

#[test]
fn a_shaped_paragraph_fits_and_keeps_its_words() {
    let Some(bytes) = test_font::load_proportional() else {
        return;
    };
    let mut s = Shaper::new(&bytes, 16.0).expect("shaper");
    let mut r = Vec::new();
    s.shape(PARAGRAPH, &mut r);
    let widest = PARAGRAPH
        .split([' ', '-'])
        .map(|w| {
            let mut g = Vec::new();
            s.shape(w, &mut g);
            advance_of(&g)
        })
        .fold(0.0f32, f32::max);
    let mut w = widest.ceil() + 1.0;
    while w < 900.0 {
        let mut words = String::new();
        let mut last = 0.0f32;
        for l in flow(&r, &flags(PARAGRAPH, &r), w) {
            let g = l.glyphs(&r);
            assert!((advance_of(g) - l.width).abs() < 1e-3);
            assert!(l.width <= w + EPS, "w={w} {l:?}");
            words.push_str(shown(PARAGRAPH, &r, &l));
            words.push(' ');
            last = l.width;
        }
        assert!(last > 0.0);
        let want: Vec<&str> = PARAGRAPH
            .split([' ', '-'])
            .filter(|w| !w.is_empty())
            .collect();
        let got: Vec<&str> = words.split([' ', '-']).filter(|w| !w.is_empty()).collect();
        assert_eq!(got, want, "w={w} no word was cut");
        w += 7.5;
    }
}

#[test]
fn shaped_lines_fill_greedily() {
    let Some(bytes) = test_font::load_proportional() else {
        return;
    };
    let mut s = Shaper::new(&bytes, 16.0).expect("shaper");
    let mut r = Vec::new();
    s.shape(PARAGRAPH, &mut r);
    let w = 220.0;
    let got: Vec<Line> = flow(&r, &flags(PARAGRAPH, &r), w).collect();
    // Each line but the last would overflow with the next word on it.
    let mut g = Vec::new();
    for pair in got.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let next = shown(PARAGRAPH, &r, &b);
        let word = next.split([' ', '-']).next().unwrap_or("");
        let from = a.text as usize;
        let upto = b.text as usize + word.len();
        s.shape(&PARAGRAPH[from..upto], &mut g);
        assert!(
            advance_of(&g) > w,
            "{:?} could have taken {word:?}",
            shown(PARAGRAPH, &r, &a)
        );
    }
}

#[test]
fn shaped_hebrew_breaks_from_its_start() {
    let Some(bytes) = test_font::load_proportional() else {
        return;
    };
    let mut s = Shaper::new(&bytes, 16.0).expect("shaper");
    let text = "\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd} \u{5d8}\u{5d5}\u{5d1}";
    let mut r = Vec::new();
    s.shape(text, &mut r);
    if r.first().map(|g| g.cluster) == Some(0) {
        // The font did not take the run as right to left.
        return;
    }
    let one = advance_of(&r) / 2.0;
    let got: Vec<Line> = flow(&r, &flags(text, &r), one).collect();
    assert!(got.len() >= 2);
    assert_eq!(got[0].text, 0);
    assert_eq!(
        got[0].end as usize,
        r.len(),
        "the first line is drawn rightmost in the slice"
    );
    for l in &got {
        assert!(l.width <= one + EPS);
    }
}

#[test]
fn the_ascii_table_agrees_with_the_rules() {
    // Every ASCII pair, after every ASCII byte, a non-ASCII lead byte and
    // the text's start.
    let befores: Vec<Option<u8>> = (0u8..128).chain([0xc3]).map(Some).chain([None]).collect();
    for i in 0..befores.len() * 128 * 128 {
        let (b, p, n) = (
            befores[i / (128 * 128)],
            (i / 128 % 128) as u8,
            (i % 128) as u8,
        );
        let Some(fast) = ascii_allowed(b, p, n) else {
            continue;
        };
        let before = b.filter(|&x| x < 0x80).map(char::from);
        let slow = allowed(before, char::from(p), char::from(n));
        assert_eq!(fast, slow, "{b:?} {p} {n}");
    }
}
