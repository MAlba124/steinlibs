//! Fitting text into a width it does not fit into.
//!
//! A label that is dropped because it is too long tells you nothing. A label
//! that is elided tells you most of what you wanted. For a flame graph over a
//! real profile that is the difference between a readable picture and a wall of
//! anonymous rectangles: demangled Rust and C++ symbols routinely run past a
//! hundred characters and almost never fit their bar.
//!
//! Monospace only, like the rest of this crate, so "how many characters fit" is
//! a division rather than a measurement loop.

/// Which part of the string to keep when it has to be cut.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Elide {
    /// Keep the head. Conventional, and right for paths and prose.
    #[default]
    End,
    /// Keep the tail. Right for anything whose identity is at the end.
    Start,
    /// Keep both ends and drop the middle.
    ///
    /// The default for symbols. A demangled name carries its noise at the
    /// front, `<alloc::boxed::Box<dyn core::ops::function::FnOnce...`, and its
    /// meaning at the back, `::call_once`. Keeping only the head is the one
    /// choice that reliably shows nothing useful.
    Middle,
}

/// The character written where text was removed. A single glyph, because two
/// dots of budget is two characters of symbol.
pub const ELLIPSIS: char = '\u{2026}';

/// Fewer than this many characters cannot say anything, so nothing is drawn.
/// Four is one character, an ellipsis, and two more.
pub const MIN_USEFUL: usize = 4;

/// Fit `s` into `max_chars`, writing the result into `out`.
///
/// Returns false when the budget is too small to be worth drawing, leaving
/// `out` untouched. Operates on characters, not bytes, so a multi-byte symbol
/// cannot be split mid-codepoint.
pub fn elide_into(s: &str, max_chars: usize, mode: Elide, out: &mut dyn core::fmt::Write) -> bool {
    let len = s.chars().count();
    if len <= max_chars {
        return out.write_str(s).is_ok();
    }
    if max_chars < MIN_USEFUL {
        return false;
    }
    // One character of the budget pays for the ellipsis.
    let keep = max_chars - 1;
    let ok = match mode {
        Elide::End => write_range(s, 0, keep, out).and_then(|_| out.write_char(ELLIPSIS).ok()),
        Elide::Start => out
            .write_char(ELLIPSIS)
            .ok()
            .and_then(|_| write_range(s, len - keep, len, out)),
        Elide::Middle => {
            // Weighted to the tail, which is where a symbol's name lives.
            let tail = (keep * 2) / 3;
            let head = keep - tail;
            write_range(s, 0, head, out)
                .and_then(|_| out.write_char(ELLIPSIS).ok())
                .and_then(|_| write_range(s, len - tail, len, out))
        }
    };
    ok.is_some()
}

fn write_range(s: &str, from: usize, to: usize, out: &mut dyn core::fmt::Write) -> Option<()> {
    for c in s.chars().skip(from).take(to.saturating_sub(from)) {
        out.write_char(c).ok()?;
    }
    Some(())
}

/// How many characters fit in `width_px` at a fixed `advance`.
#[inline]
pub fn fits(width_px: f32, advance: f32) -> usize {
    if advance <= 0.0 || width_px <= 0.0 {
        return 0;
    }
    (width_px / advance) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    fn run(s: &str, n: usize, mode: Elide) -> Option<String> {
        let mut out = String::new();
        elide_into(s, n, mode, &mut out).then_some(out)
    }

    #[test]
    fn text_that_fits_is_untouched() {
        assert_eq!(
            run("call_once", 9, Elide::Middle).as_deref(),
            Some("call_once")
        );
        assert_eq!(
            run("call_once", 40, Elide::End).as_deref(),
            Some("call_once")
        );
    }

    #[test]
    fn every_mode_respects_the_budget() {
        let long = "core::ptr::drop_in_place<alloc::vec::Vec<u8>>";
        for mode in [Elide::End, Elide::Start, Elide::Middle] {
            for n in MIN_USEFUL..30 {
                let got = run(long, n, mode).expect("budget is usable");
                assert_eq!(got.chars().count(), n, "{mode:?} at {n} produced {:?}", got);
            }
        }
    }

    #[test]
    fn middle_keeps_the_end_where_the_name_is() {
        let sym = "<alloc::boxed::Box<dyn core::ops::function::FnOnce<()>>>::call_once";
        let got = run(sym, 24, Elide::Middle).expect("usable");
        assert!(got.contains(ELLIPSIS));
        assert!(
            got.ends_with("call_once"),
            "the informative end was cut: {got}"
        );
        assert!(got.starts_with('<'), "lost the head entirely: {got}");
    }

    #[test]
    fn start_and_end_keep_the_side_they_name() {
        let s = "abcdefghijklmnop";
        assert_eq!(run(s, 6, Elide::End).as_deref(), Some("abcde\u{2026}"));
        assert_eq!(run(s, 6, Elide::Start).as_deref(), Some("\u{2026}lmnop"));
    }

    #[test]
    fn a_hopeless_budget_draws_nothing() {
        for n in 0..MIN_USEFUL {
            assert_eq!(run("something_long", n, Elide::Middle), None, "n={n}");
        }
    }

    #[test]
    fn multi_byte_text_is_never_split_mid_codepoint() {
        // Every one of these is multi-byte, so a byte-wise cut would produce
        // invalid UTF-8 rather than a shorter string.
        let s = "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}";
        for mode in [Elide::End, Elide::Start, Elide::Middle] {
            let got = run(s, 5, mode).expect("usable");
            assert_eq!(got.chars().count(), 5, "{mode:?}");
            assert!(got.chars().all(|c| c == '\u{e9}' || c == ELLIPSIS));
        }
    }

    #[test]
    fn fits_divides_by_the_advance() {
        assert_eq!(fits(90.0, 9.0), 10);
        assert_eq!(fits(89.0, 9.0), 9);
        assert_eq!(fits(0.0, 9.0), 0);
        assert_eq!(fits(90.0, 0.0), 0, "a zero advance must not divide");
    }
}
