//! Greedy word wrapping for monospaced text.
//!
//! Prose panels need this: help text, tooltips and a details pane all hold
//! paragraphs that have to reflow when their column resizes. Source views do
//! not, which is the reason wrapping is a separate iterator rather than a mode
//! on the line list. A wrapped line has no stable identity across a resize, so
//! anything that needs one, a breakpoint, a cost annotation, a jump target,
//! must not wrap.
//!
//! Monospaced, so a column is a character and fitting is counting. No shaping,
//! no locale-aware break opportunities: a break happens at a space, at a tab or
//! at a newline, and a word longer than the line is cut rather than allowed to
//! overhang. That is wrong for languages that do not space their words, and the
//! fix there is the same as everywhere else in this crate, a real shaper.
//!
//! Cost is linear in the text and the iterator holds only a cursor, so wrapping
//! a paragraph every frame is cheap. Wrapping a megabyte every frame is not:
//! the iterator has no way to seek to line N without walking, so a large
//! document should be wrapped once into caller-owned line offsets.

/// A tab advances to the next multiple of this. Four, because source code in
/// this stack is four, and a wrapped view that disagrees with the editor is
/// worse than one that picks a side.
pub const TAB_WIDTH: usize = 4;

/// Wrapped lines of `s`, each at most `cols` characters wide.
///
/// Hard newlines are honoured, including empty lines, which a paragraph
/// separator depends on. Trailing spaces at a soft break are dropped; a run of
/// spaces inside a line is not.
pub struct Wrap<'a> {
    s: &'a str,
    cols: usize,
    /// Byte offset of the next line's first character.
    at: usize,
    /// A hard newline at the very end still produces the empty line after it,
    /// which is what makes `wrap` and `lines` agree on trailing structure.
    pending_empty: bool,
}

pub fn wrap(s: &str, cols: usize) -> Wrap<'_> {
    Wrap {
        s,
        cols,
        at: 0,
        pending_empty: false,
    }
}

/// How many lines `s` wraps to. Walks the text, so cache it rather than
/// calling it per frame on anything large.
pub fn wrap_count(s: &str, cols: usize) -> usize {
    wrap(s, cols).count()
}

/// Width in columns of `s`, expanding tabs to the next stop.
///
/// Not a character count: a tab is one character and up to `tab_width`
/// columns, which is exactly the discrepancy that makes a source view's
/// cursor drift if it counts characters.
pub fn columns(s: &str, tab_width: usize) -> usize {
    let w = if tab_width == 0 { 1 } else { tab_width };
    let mut n = 0;
    for c in s.chars() {
        n = if c == '\t' { (n / w + 1) * w } else { n + 1 };
    }
    n
}

impl<'a> Iterator for Wrap<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.pending_empty {
            self.pending_empty = false;
            return Some("");
        }
        if self.at >= self.s.len() {
            return None;
        }
        let rest = self.s.get(self.at..)?;
        let (line, consumed, hard_end) = take_line(rest, self.cols);
        self.at += consumed;
        // Only a hard break that ends the text owes a final empty line.
        self.pending_empty = hard_end && self.at >= self.s.len();
        Some(line)
    }
}

/// One line off the front of `rest`. Returns the line, the bytes to skip past
/// it including any separator, and whether the break was a newline.
fn take_line(rest: &str, cols: usize) -> (&str, usize, bool) {
    let width = if cols == 0 { 1 } else { cols };
    // Last byte offset at which a soft break is allowed, and the offset just
    // past the character that would make the line too long.
    let mut soft: Option<(usize, usize)> = None;
    let mut n = 0;
    for (i, c) in rest.char_indices() {
        if c == '\n' {
            return (slice(rest, 0, i), i + 1, true);
        }
        // A space right at the edge is the break, not the next line's start.
        if n == width && (c == ' ' || c == '\t') {
            return (slice(rest, 0, i), i + 1, false);
        }
        if n == width {
            return match soft {
                // `end` skips the space itself, `cut` keeps it out of the line.
                Some((cut, end)) => (slice(rest, 0, cut), end, false),
                None => (slice(rest, 0, i), i, false),
            };
        }
        if c == ' ' || c == '\t' {
            soft = Some((i, i + c.len_utf8()));
        }
        n += 1;
    }
    (rest, rest.len(), false)
}

#[inline]
fn slice(s: &str, from: usize, to: usize) -> &str {
    s.get(from..to).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn v(s: &str, cols: usize) -> Vec<&str> {
        wrap(s, cols).collect()
    }

    #[test]
    fn text_that_fits_is_one_line() {
        assert_eq!(v("hello", 10), ["hello"]);
        assert_eq!(v("hello", 5), ["hello"]);
    }

    #[test]
    fn words_break_at_spaces_and_the_space_is_dropped() {
        assert_eq!(v("the quick brown fox", 10), ["the quick", "brown fox"]);
        // No line may carry a trailing space, which would render as a ragged
        // right edge against a highlighted background.
        for line in v("the quick brown fox jumps over", 9) {
            assert!(!line.ends_with(' '), "trailing space in {line:?}");
        }
    }

    #[test]
    fn no_line_exceeds_the_column_budget() {
        let text = "core::ptr::drop_in_place allocates a Vec of u8 and then \
                    immediately drops it again";
        for cols in 1..40 {
            for line in v(text, cols) {
                assert!(
                    line.chars().count() <= cols.max(1),
                    "cols={cols} line={line:?}"
                );
            }
        }
    }

    #[test]
    fn a_word_longer_than_the_line_is_cut_rather_than_overhanging() {
        let got = v("supercalifragilistic", 6);
        assert_eq!(got, ["superc", "alifra", "gilist", "ic"]);
    }

    #[test]
    fn wrapping_loses_no_characters_except_the_break_spaces() {
        // Words wider than the budget get hard cut, so a line boundary is not
        // necessarily a word boundary. What must hold at every width is that no
        // non-space character is dropped or reordered.
        let text = "one two three four five six seven eight nine ten";
        let want: std::string::String = text.chars().filter(|c| *c != ' ').collect();
        for cols in 1..25 {
            let got: std::string::String = v(text, cols)
                .concat()
                .chars()
                .filter(|c| *c != ' ')
                .collect();
            assert_eq!(got, want, "cols={cols}");
        }
    }

    #[test]
    fn hard_newlines_are_honoured_including_empty_ones() {
        assert_eq!(v("a\n\nb", 20), ["a", "", "b"]);
        assert_eq!(v("para one\n\npara two", 20), ["para one", "", "para two"]);
        // A paragraph break survives wrapping the paragraphs themselves.
        assert_eq!(v("aa bb\n\ncc dd", 3), ["aa", "bb", "", "cc", "dd"]);
    }

    #[test]
    fn a_trailing_newline_produces_the_line_after_it() {
        assert_eq!(v("a\n", 10), ["a", ""]);
        assert_eq!(v("a\nb", 10), ["a", "b"]);
        assert_eq!(v("", 10), Vec::<&str>::new());
    }

    #[test]
    fn a_zero_column_budget_still_terminates() {
        // One character per line rather than an infinite loop of empties.
        let got = v("abc", 0);
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn multi_byte_text_is_never_split_mid_codepoint() {
        let s = "\u{e9}\u{e9}\u{e9} \u{e9}\u{e9}\u{e9}\u{e9}\u{e9}";
        for cols in 1..10 {
            let lines = v(s, cols);
            let mut total = 0;
            for line in &lines {
                assert!(line.chars().all(|c| c == '\u{e9}' || c == ' '));
                total += line.chars().count();
            }
            assert!(total > 0, "cols={cols} produced nothing");
        }
    }

    #[test]
    fn wrap_count_agrees_with_the_iterator() {
        let text = "the quick brown fox jumps over the lazy dog\n\nand then stops";
        for cols in 1..30 {
            assert_eq!(wrap_count(text, cols), v(text, cols).len(), "cols={cols}");
        }
    }

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(columns("\t", 4), 4);
        assert_eq!(columns("a\t", 4), 4);
        assert_eq!(columns("abc\t", 4), 4);
        assert_eq!(columns("abcd\t", 4), 8);
        assert_eq!(columns("\t\t", 4), 8);
        assert_eq!(columns("ab", 4), 2);
        // A zero width would divide by zero, so it falls back to one.
        assert_eq!(columns("\ta", 0), 2);
    }

    #[test]
    fn a_word_that_fills_the_line_leaves_no_space_behind() {
        assert_eq!(v("hello world", 5), ["hello", "world"]);
        assert_eq!(v("ab cd ef", 2), ["ab", "cd", "ef"]);
        assert_eq!(v("a b", 1), ["a", "b"]);
    }

    #[test]
    fn a_tab_is_a_break_opportunity() {
        assert_eq!(v("aa\tbb", 3), ["aa", "bb"]);
    }

    #[test]
    fn wrapping_is_stable_when_rerun_on_its_own_output() {
        // A line that already fits must not be re-broken, which is what keeps a
        // resize from shuffling text that did not need to move.
        let text = "the quick brown fox";
        let once = v(text, 10);
        for line in &once {
            assert_eq!(v(line, 10), [*line], "{line:?} was re-broken");
        }
    }
}
