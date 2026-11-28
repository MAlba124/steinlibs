#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::vec::Vec;

/// Parse onto vectors sized by [`count`], which is also what every caller
/// does, so a wrong count shows up here rather than in an app.
fn read(src: &str) -> Result<(Vec<Entry>, Vec<Span>), IniError> {
    let c = count(src.as_bytes());
    let mut entries = std::vec![
        Entry {
            section: 0,
            key: Span::EMPTY,
            value: Span::EMPTY,
            line: 0,
            flags: 0
        };
        c.entries
    ];
    let mut sections = std::vec![Span::EMPTY; c.sections];
    let ini = parse(
        src.as_bytes(),
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    )?;
    let n = (ini.entries().len(), ini.sections().count());
    entries.truncate(n.0);
    sections.truncate(n.1);
    Ok((entries, sections))
}

/// The same, but handing back a usable `Ini`. The arrays have to outlive it,
/// so the body runs inside a closure.
fn with<T>(src: &str, f: impl FnOnce(&Ini<'_>) -> T) -> T {
    let c = count(src.as_bytes());
    let mut entries = std::vec![
        Entry {
            section: 0,
            key: Span::EMPTY,
            value: Span::EMPTY,
            line: 0,
            flags: 0
        };
        c.entries.max(1)
    ];
    let mut sections = std::vec![Span::EMPTY; c.sections.max(1)];
    let ini = parse(
        src.as_bytes(),
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    )
    .expect("parses");
    f(&ini)
}

fn err(src: &str) -> IniError {
    read(src).expect_err("should not parse")
}

#[test]
fn a_pair_is_split_at_the_first_equals() {
    with("a = b=c=d", |i| {
        assert_eq!(i.get("", "a"), Some(&b"b=c=d"[..]));
    });
}

#[test]
fn whitespace_around_a_key_and_value_is_not_part_of_either() {
    with("   key   =   value   ", |i| {
        assert_eq!(i.get("", "key"), Some(&b"value"[..]));
    });
}

#[test]
fn a_tab_separated_pair_parses() {
    with("key\t=\tvalue", |i| {
        assert_eq!(i.get("", "key"), Some(&b"value"[..]));
    });
}

#[test]
fn an_empty_value_is_empty_and_present() {
    with("key =", |i| {
        assert_eq!(i.get("", "key"), Some(&b""[..]));
        // Set to nothing is not the same as absent, which is what a flag-
        // shaped setting turns on.
        assert!(i.has("", "key"));
        assert!(!i.has("", "other"));
    });
}

#[test]
fn a_blank_file_has_nothing_in_it() {
    let (e, s) = read("").unwrap();
    assert!(e.is_empty() && s.is_empty());
    let (e, s) = read("\n\n   \n\t\n").unwrap();
    assert!(e.is_empty() && s.is_empty());
}

#[test]
fn a_last_line_without_a_newline_still_counts() {
    with("a = 1\nb = 2", |i| {
        assert_eq!(i.get("", "b"), Some(&b"2"[..]));
    });
}

#[test]
fn crlf_endings_leave_no_carriage_return_on_the_value() {
    with("a = 1\r\n[s]\r\nb = 2\r\n", |i| {
        assert_eq!(i.get("", "a"), Some(&b"1"[..]));
        assert_eq!(i.get("s", "b"), Some(&b"2"[..]));
    });
}

// Comments.

#[test]
fn a_hash_or_semicolon_line_is_a_comment() {
    with("# one\n; two\n   # indented\na = 1", |i| {
        assert_eq!(i.entries().len(), 1);
        assert_eq!(i.get("", "a"), Some(&b"1"[..]));
    });
}

/// The reason this dialect gives up trailing comments. A colour is the single
/// most common thing in a terminal config and `#` is how everyone writes one.
#[test]
fn a_hash_inside_a_value_is_a_colour_not_a_comment() {
    with("fg = #d3d7cf", |i| {
        assert_eq!(i.get("", "fg"), Some(&b"#d3d7cf"[..]));
    });
}

#[test]
fn a_semicolon_inside_a_value_is_kept() {
    with("cmd = echo a; echo b", |i| {
        assert_eq!(i.get("", "cmd"), Some(&b"echo a; echo b"[..]));
    });
}

#[test]
fn a_comment_marker_after_a_value_is_part_of_the_value() {
    with("a = 1 # not a comment", |i| {
        assert_eq!(i.get("", "a"), Some(&b"1 # not a comment"[..]));
    });
}

// Sections.

#[test]
fn pairs_land_in_the_section_above_them() {
    with("r = 0\n[one]\na = 1\n[two]\nb = 2", |i| {
        assert_eq!(i.get("", "r"), Some(&b"0"[..]));
        assert_eq!(i.get("one", "a"), Some(&b"1"[..]));
        assert_eq!(i.get("two", "b"), Some(&b"2"[..]));
        assert_eq!(i.get("one", "b"), None);
        assert_eq!(i.get("two", "a"), None);
    });
}

#[test]
fn a_section_named_twice_continues_the_first() {
    with("[c]\na = 1\n[d]\nx = 9\n[c]\nb = 2", |i| {
        assert_eq!(i.sections().count(), 2);
        assert_eq!(i.get("c", "a"), Some(&b"1"[..]));
        assert_eq!(i.get("c", "b"), Some(&b"2"[..]));
    });
}

#[test]
fn a_section_header_may_be_spaced_out() {
    with("  [  colours  ]  \nfg = 1", |i| {
        assert_eq!(i.get("colours", "fg"), Some(&b"1"[..]));
    });
}

#[test]
fn section_and_key_names_ignore_ascii_case() {
    with("[Colours]\nForeGround = 1", |i| {
        assert_eq!(i.get("colours", "foreground"), Some(&b"1"[..]));
        assert_eq!(i.get("COLOURS", "FOREGROUND"), Some(&b"1"[..]));
    });
}

#[test]
fn the_root_section_is_the_empty_name() {
    with("a = 1\n[s]\nb = 2", |i| {
        assert_eq!(i.section_of(""), Some(Entry::ROOT));
        assert!(i.has_section(""));
        assert!(i.has_section("s"));
        assert!(!i.has_section("nope"));
    });
}

#[test]
fn a_file_with_no_root_pairs_has_no_root_section() {
    with("[s]\nb = 2", |i| {
        assert!(!i.has_section(""));
    });
}

#[test]
fn sections_come_back_in_the_order_written() {
    with("[b]\nx=1\n[a]\ny=2\n[c]\nz=3", |i| {
        let got: Vec<&[u8]> = i.sections().collect();
        assert_eq!(got, std::vec![&b"b"[..], &b"a"[..], &b"c"[..]]);
    });
}

#[test]
fn a_bracket_after_the_first_byte_is_an_ordinary_key() {
    with("a[0] = x", |i| {
        assert_eq!(i.get("", "a[0]"), Some(&b"x"[..]));
    });
}

#[test]
fn a_bracket_inside_a_value_is_kept() {
    with("a = [not a section]", |i| {
        assert_eq!(i.get("", "a"), Some(&b"[not a section]"[..]));
    });
}

// Repeats.

#[test]
fn a_repeated_key_reads_as_the_last_one() {
    with("a = 1\na = 2\na = 3", |i| {
        assert_eq!(i.get("", "a"), Some(&b"3"[..]));
        assert_eq!(i.entry("", "a").unwrap().line, 3);
    });
}

#[test]
fn every_value_of_a_repeated_key_is_kept_in_order() {
    with("bind = a\nbind = b\nother = x\nBIND = c", |i| {
        let got: Vec<&[u8]> = i.all("", "bind").collect();
        assert_eq!(got, std::vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    });
}

#[test]
fn all_is_empty_for_a_missing_key_or_section() {
    with("a = 1", |i| {
        assert_eq!(i.all("", "nope").count(), 0);
        assert_eq!(i.all("nope", "a").count(), 0);
    });
}

#[test]
fn a_section_yields_its_own_pairs_only() {
    with(
        "r = 0\n[one]\na = 1\nb = 2\n[two]\nc = 3\n[one]\nd = 4",
        |i| {
            let got: Vec<&[u8]> = i.in_section("one").map(|e| i.bytes(e.key)).collect();
            assert_eq!(got, std::vec![&b"a"[..], &b"b"[..], &b"d"[..]]);
            let root: Vec<&[u8]> = i.in_section("").map(|e| i.bytes(e.key)).collect();
            assert_eq!(root, std::vec![&b"r"[..]]);
        },
    );
}

// Quoting and escapes.

#[test]
fn quotes_preserve_the_whitespace_inside_them() {
    with("p = \"  two  \"", |i| {
        assert_eq!(i.get("", "p"), Some(&b"  two  "[..]));
        assert!(i.entry("", "p").unwrap().flags & Entry::QUOTED != 0);
    });
}

#[test]
fn an_empty_quoted_value_is_empty() {
    with("p = \"\"", |i| {
        assert_eq!(i.get("", "p"), Some(&b""[..]));
        assert!(i.entry("", "p").unwrap().flags & Entry::QUOTED != 0);
    });
}

#[test]
fn a_quote_that_does_not_start_a_value_is_literal() {
    with("p = say \"hi\"", |i| {
        assert_eq!(i.get("", "p"), Some(&b"say \"hi\""[..]));
        assert_eq!(i.entry("", "p").unwrap().flags, 0);
    });
}

#[test]
fn an_unquoted_value_keeps_its_backslashes() {
    // A Windows path needs no doubling, which is the point of the rule.
    with("p = C:\\Users\\me", |i| {
        assert_eq!(i.get("", "p"), Some(&b"C:\\Users\\me"[..]));
        assert!(!i.entry("", "p").unwrap().needs_unescape());
    });
}

#[test]
fn the_escaped_flag_is_only_set_when_there_is_an_escape() {
    with("a = \"plain\"\nb = \"with\\ttab\"", |i| {
        assert!(!i.entry("", "a").unwrap().needs_unescape());
        assert!(i.entry("", "b").unwrap().needs_unescape());
    });
}

#[test]
fn every_escape_decodes() {
    with(r#"p = "a\\b\"c\nd\te\rf\0g""#, |i| {
        let raw = i.get("", "p").unwrap();
        let mut out = [0u8; 32];
        let n = unescape(raw, &mut out).unwrap();
        assert_eq!(&out[..n], b"a\\b\"c\nd\te\rf\0g");
        assert_eq!(unescape_len(raw), n);
    });
}

#[test]
fn a_quoted_value_ending_in_an_escaped_quote_decodes() {
    with(r#"p = "a\"""#, |i| {
        let raw = i.get("", "p").unwrap();
        let mut out = [0u8; 8];
        let n = unescape(raw, &mut out).unwrap();
        assert_eq!(&out[..n], b"a\"");
    });
}

#[test]
fn unescaping_into_a_short_buffer_writes_nothing() {
    let raw = br"a\nb";
    let mut out = [0xaau8; 3];
    assert_eq!(unescape_len(raw), 3);
    assert_eq!(unescape(raw, &mut out[..2]), None);
    assert_eq!(out, [0xaa; 3]);
    assert_eq!(unescape(raw, &mut out), Some(3));
    assert_eq!(&out[..], b"a\nb");
}

#[test]
fn a_lone_trailing_backslash_decodes_to_itself() {
    // Unreachable from a parsed file, which rejects it, but `unescape` is
    // public and must not read past the end for a caller's own bytes.
    let mut out = [0u8; 4];
    let n = unescape(br"a\", &mut out).unwrap();
    assert_eq!(&out[..n], br"a\");
}

// Errors.

#[test]
fn a_header_without_a_close_is_refused() {
    assert_eq!(err("[section"), IniError::UnclosedSection { line: 1 });
    assert_eq!(err("a=1\n[oops"), IniError::UnclosedSection { line: 2 });
    assert_eq!(err("["), IniError::UnclosedSection { line: 1 });
}

#[test]
fn a_header_with_no_name_is_refused() {
    assert_eq!(err("[]"), IniError::EmptySection { line: 1 });
    assert_eq!(err("[   ]"), IniError::EmptySection { line: 1 });
}

#[test]
fn a_line_that_is_not_a_pair_is_refused() {
    assert_eq!(err("just some words"), IniError::NotAPair { line: 1 });
    assert_eq!(err("a = 1\n\nbare\n"), IniError::NotAPair { line: 3 });
}

#[test]
fn a_value_with_no_key_is_refused() {
    assert_eq!(err("= 1"), IniError::EmptyKey { line: 1 });
    assert_eq!(err("   =1"), IniError::EmptyKey { line: 1 });
}

#[test]
fn an_unclosed_quote_is_refused() {
    assert_eq!(err("a = \"open"), IniError::UnclosedQuote { line: 1 });
    assert_eq!(err("a = \""), IniError::UnclosedQuote { line: 1 });
}

#[test]
fn an_unknown_escape_is_refused_at_parse_time() {
    assert_eq!(err(r#"a = "b\qc""#), IniError::BadEscape { line: 1 });
    assert_eq!(err(r#"a = "b\""#), IniError::BadEscape { line: 1 });
}

#[test]
fn a_control_byte_is_refused_wherever_it_appears() {
    assert_eq!(err("a\x01b = 1"), IniError::BadByte { line: 1 });
    assert_eq!(err("a = b\x01c"), IniError::BadByte { line: 1 });
    assert_eq!(err("[s\x7fx]\na=1"), IniError::BadByte { line: 1 });
    assert_eq!(err("a = \"b\x00c\""), IniError::BadByte { line: 1 });
}

#[test]
fn an_interior_tab_is_not_a_control_byte() {
    with("a = b\tc", |i| {
        assert_eq!(i.get("", "a"), Some(&b"b\tc"[..]));
    });
}

#[test]
fn the_reported_line_survives_blank_and_comment_lines() {
    assert_eq!(
        err("\n\n# c\n\n; c\n\nbad\n"),
        IniError::NotAPair { line: 7 }
    );
}

#[test]
fn every_error_but_the_size_one_names_a_line() {
    assert_eq!(err("bad").line(), Some(1));
    assert_eq!(IniError::TooLarge.line(), None);
}

#[test]
fn running_out_of_entries_is_reported_with_the_line() {
    let src = b"a=1\nb=2\nc=3";
    let mut entries = [Entry {
        section: 0,
        key: Span::EMPTY,
        value: Span::EMPTY,
        line: 0,
        flags: 0,
    }; 2];
    let mut sections = [Span::EMPTY; 1];
    let got = parse(
        src,
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    );
    assert_eq!(got.err(), Some(IniError::TooManyEntries { line: 3 }));
}

#[test]
fn running_out_of_sections_is_reported_with_the_line() {
    let src = b"[a]\n[b]\n";
    let mut entries = [Entry {
        section: 0,
        key: Span::EMPTY,
        value: Span::EMPTY,
        line: 0,
        flags: 0,
    }; 1];
    let mut sections = [Span::EMPTY; 1];
    let got = parse(
        src,
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    );
    assert_eq!(got.err(), Some(IniError::TooManySections { line: 2 }));
}

#[test]
fn a_repeated_section_does_not_need_a_second_slot() {
    let src = b"[a]\nx=1\n[a]\ny=2\n";
    let mut entries = [Entry {
        section: 0,
        key: Span::EMPTY,
        value: Span::EMPTY,
        line: 0,
        flags: 0,
    }; 2];
    let mut sections = [Span::EMPTY; 1];
    let ini = parse(
        src,
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    )
    .expect("one slot is enough");
    assert_eq!(ini.get("a", "y"), Some(&b"2"[..]));
}

// Counting.

#[test]
fn the_count_is_enough_room_for_the_parse() {
    let src = "# c\n[a]\nx=1\ny=2\n\n[b]\nz=3\n[a]\nw=4";
    let c = count(src.as_bytes());
    assert_eq!(c.entries, 4);
    // Three headers, two distinct. An upper bound, as documented.
    assert_eq!(c.sections, 3);
    let (e, s) = read(src).unwrap();
    assert!(e.len() <= c.entries && s.len() <= c.sections);
    assert_eq!(s.len(), 2);
}

#[test]
fn counting_a_broken_file_does_not_panic() {
    assert_eq!(count(b"[oops\n= 1\n\"\n").entries, 2);
}

// Typed reads.

#[test]
fn numbers_read_in_every_base() {
    let src = "d = 42\nh = 0xFF\nb = 0b1010\no = 0o17\nn = -7\np = +7\nu = 1_000_000";
    with(src, |i| {
        assert_eq!(i.int("", "d"), Some(42));
        assert_eq!(i.int("", "h"), Some(255));
        assert_eq!(i.int("", "b"), Some(10));
        assert_eq!(i.int("", "o"), Some(15));
        assert_eq!(i.int("", "n"), Some(-7));
        assert_eq!(i.int("", "p"), Some(7));
        assert_eq!(i.int("", "u"), Some(1_000_000));
    });
}

#[test]
fn a_number_that_is_not_one_is_none() {
    let src = "a = \nb = 0x\nc = 12ab\nd = -\ne = 9999999999999999999999\nf = hello";
    with(src, |i| {
        for k in ["a", "b", "c", "d", "e", "f"] {
            assert_eq!(i.int("", k), None, "{k} should not be a number");
        }
    });
}

#[test]
fn a_count_refuses_a_negative() {
    with("a = -1\nb = 5", |i| {
        assert_eq!(i.uint("", "a"), None);
        assert_eq!(i.uint("", "b"), Some(5));
    });
}

#[test]
fn flags_read_every_spelling() {
    let src = "a=true\nb=Yes\nc=ON\nd=1\ne=false\nf=No\ng=off\nh=0\ni=maybe";
    with(src, |x| {
        for k in ["a", "b", "c", "d"] {
            assert_eq!(x.bool("", k), Some(true), "{k}");
        }
        for k in ["e", "f", "g", "h"] {
            assert_eq!(x.bool("", k), Some(false), "{k}");
        }
        // A typo is not false. Silently defaulting is how a setting appears to
        // be ignored.
        assert_eq!(x.bool("", "i"), None);
    });
}

#[test]
fn fractions_read() {
    with("a = 0.5\nb = 2\nc = -1.25\nd = nonsense", |i| {
        assert_eq!(i.float("", "a"), Some(0.5));
        assert_eq!(i.float("", "b"), Some(2.0));
        assert_eq!(i.float("", "c"), Some(-1.25));
        assert_eq!(i.float("", "d"), None);
    });
}

#[test]
fn text_refuses_bytes_that_are_not_utf8() {
    let src = b"a = \xff\xfe";
    let mut entries = [Entry {
        section: 0,
        key: Span::EMPTY,
        value: Span::EMPTY,
        line: 0,
        flags: 0,
    }; 1];
    let mut sections = [Span::EMPTY; 1];
    let ini = parse(
        src,
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
    )
    .expect("high bytes are not control bytes");
    assert_eq!(ini.text("", "a"), None);
    assert_eq!(ini.get("", "a"), Some(&b"\xff\xfe"[..]));
}

#[test]
fn text_reads_utf8() {
    with("a = héllo → ok", |i| {
        assert_eq!(i.text("", "a"), Some("héllo → ok"));
    });
}

#[test]
fn a_missing_key_reads_as_nothing_everywhere() {
    with("a = 1", |i| {
        assert_eq!(i.get("", "z"), None);
        assert_eq!(i.text("", "z"), None);
        assert_eq!(i.int("", "z"), None);
        assert_eq!(i.uint("", "z"), None);
        assert_eq!(i.float("", "z"), None);
        assert_eq!(i.bool("", "z"), None);
        assert!(!i.has("", "z"));
    });
}

// Spans.

#[test]
fn a_value_points_into_the_original_bytes() {
    let src = "key = value";
    with(src, |i| {
        let v = i.get("", "key").unwrap();
        let base = src.as_ptr() as usize;
        let at = v.as_ptr() as usize;
        assert!(at >= base && at < base + src.len(), "not a borrow");
        assert_eq!(at - base, 6);
    });
}

#[test]
fn a_span_outside_the_source_reads_empty() {
    let s = Span { at: 900, len: 4 };
    assert_eq!(s.of(b"short"), b"");
    assert_eq!(Span { at: 3, len: 99 }.of(b"short"), b"");
    assert!(Span::EMPTY.is_empty());
}

#[test]
fn a_realistic_file_reads_the_way_it_looks() {
    let src = "\
# steinterm
shell = tmux
scrollback = 4_194_304

[colours]
fg = #d3d7cf
bg = #1e1e1e
dim = 0.5

[keys]
bind = ctrl+shift+c copy
bind = ctrl+shift+v paste
";
    with(src, |i| {
        assert_eq!(i.text("", "shell"), Some("tmux"));
        assert_eq!(i.uint("", "scrollback"), Some(4 << 20));
        assert_eq!(i.text("colours", "fg"), Some("#d3d7cf"));
        assert_eq!(i.float("colours", "dim"), Some(0.5));
        assert_eq!(i.all("keys", "bind").count(), 2);
        let got: Vec<&[u8]> = i.sections().collect();
        assert_eq!(got, std::vec![&b"colours"[..], &b"keys"[..]]);
    });
}

/// [`with`], under the desktop dialect.
fn with_desktop<T>(src: &str, f: impl FnOnce(&Ini<'_>) -> T) -> T {
    let c = count_with(src.as_bytes(), Dialect::DESKTOP);
    let mut entries = std::vec![
        Entry {
            section: 0,
            key: Span::EMPTY,
            value: Span::EMPTY,
            line: 0,
            flags: 0
        };
        c.entries.max(1)
    ];
    let mut sections = std::vec![Span::EMPTY; c.sections.max(1)];
    let ini = parse_with(
        src.as_bytes(),
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
        Dialect::DESKTOP,
    )
    .expect("parses");
    f(&ini)
}

#[test]
fn desktop_dialect_takes_an_unbalanced_quote_verbatim() {
    with_desktop(
        "[Desktop Entry]\nExec=\"/opt/My App/run\" --flag",
        |i| {
            assert_eq!(
                i.get("Desktop Entry", "Exec"),
                Some(&b"\"/opt/My App/run\" --flag"[..])
            );
        },
    );
}

#[test]
fn desktop_dialect_keeps_a_fully_quoted_value_quoted() {
    with_desktop("[s]\nExec=\"/usr/bin/foo\"", |i| {
        assert_eq!(i.get("s", "Exec"), Some(&b"\"/usr/bin/foo\""[..]));
    });
}

#[test]
fn desktop_dialect_leaves_backslashes_alone() {
    with_desktop("[s]\nName=a\\sb\\\\c", |i| {
        let e = i.entry("s", "Name").expect("entry");
        assert!(!e.needs_unescape());
        assert_eq!(i.get("s", "Name"), Some(&b"a\\sb\\\\c"[..]));
    });
}

#[test]
fn desktop_dialect_treats_a_semicolon_line_as_a_pair_error() {
    let src = ";not a comment";
    let c = count_with(src.as_bytes(), Dialect::DESKTOP);
    assert_eq!(c.entries, 1);
    let mut entries = std::vec![
        Entry {
            section: 0,
            key: Span::EMPTY,
            value: Span::EMPTY,
            line: 0,
            flags: 0
        };
        c.entries
    ];
    let mut sections = std::vec![Span::EMPTY; c.sections.max(1)];
    let got = parse_with(
        src.as_bytes(),
        Storage {
            entries: &mut entries,
            sections: &mut sections,
        },
        Dialect::DESKTOP,
    );
    assert!(matches!(got, Err(IniError::NotAPair { line: 1 })));
}

#[test]
fn desktop_dialect_still_takes_hash_comments() {
    with_desktop("# comment\n[s]\nk=v", |i| {
        assert_eq!(i.get("s", "k"), Some(&b"v"[..]));
    });
}

#[test]
fn the_ini_dialect_is_what_parse_uses() {
    let c_ini = count_with("; c\nk=v".as_bytes(), Dialect::INI);
    let c_plain = count("; c\nk=v".as_bytes());
    assert_eq!(c_ini, c_plain);
}
