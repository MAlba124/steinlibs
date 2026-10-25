#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::string::String;
use std::vec::Vec;

/// Decode into an input-sized buffer, asserting it never grows.
fn dec(raw: &str) -> Vec<u8> {
    let mut out = std::vec![0u8; raw.len()];
    let n = decode(raw.as_bytes(), &mut out).unwrap();
    assert!(n <= raw.len(), "decoding {raw:?} grew the text");
    out.truncate(n);
    out
}

fn want(raw: &str, expect: &str) {
    let got = String::from_utf8(dec(raw)).unwrap();
    assert_eq!(got, expect, "decoding {raw:?}");
}

fn err(raw: &str) -> XmlError {
    let mut out = std::vec![0u8; raw.len()];
    decode(raw.as_bytes(), &mut out).unwrap_err()
}

fn bad(raw: &str) {
    assert_eq!(err(raw).kind, ErrorKind::BadReference, "decoding {raw:?}");
}

#[test]
fn text_with_no_references_passes_through_unchanged() {
    want("", "");
    want("plain text", "plain text");
    want("a <b> c \n d", "a <b> c \n d");
    want("caf\u{e9} na\u{ef}ve", "caf\u{e9} na\u{ef}ve");
}

#[test]
fn the_five_predefined_entities_decode() {
    want("&amp;", "&");
    want("&lt;", "<");
    want("&gt;", ">");
    want("&quot;", "\"");
    want("&apos;", "'");
    want("&lt;a href=&quot;x&quot;&gt;", "<a href=\"x\">");
}

#[test]
fn decimal_character_references_decode() {
    want("&#65;", "A");
    want("&#38;", "&");
    want("&#8212;", "\u{2014}");
    want("&#128512;", "\u{1F600}");
    want("&#0000065;", "A");
}

#[test]
fn hex_character_references_decode() {
    want("&#x41;", "A");
    want("&#x26;", "&");
    want("&#x2014;", "\u{2014}");
    want("&#x1F600;", "\u{1F600}");
    want("&#x00000041;", "A");
}

#[test]
fn character_references_encode_at_every_utf8_length() {
    // UTF-8 width boundaries.
    want("&#x7F;", "\u{7F}");
    want("&#x80;", "\u{80}");
    want("&#x7FF;", "\u{7FF}");
    want("&#x800;", "\u{800}");
    want("&#xFFFF;", "\u{FFFF}");
    want("&#x10000;", "\u{10000}");
    want("&#x10FFFF;", "\u{10FFFF}");
    assert_eq!(dec("&#x7F;").len(), 1);
    assert_eq!(dec("&#x80;").len(), 2);
    assert_eq!(dec("&#x800;").len(), 3);
    assert_eq!(dec("&#x10000;").len(), 4);
}

#[test]
fn the_x_marker_and_the_hex_digits_may_be_either_case() {
    want("&#X41;", "A");
    want("&#x41;", "A");
    want("&#xabcd;", "\u{ABCD}");
    want("&#xABCD;", "\u{ABCD}");
    want("&#XaBcD;", "\u{ABCD}");
}

#[test]
fn a_bare_ampersand_in_a_url_survives() {
    want("http://x/?a=1&b=2", "http://x/?a=1&b=2");
    want("Tom & Jerry", "Tom & Jerry");
    want("R&D report", "R&D report");
    want("a && b", "a && b");
    want("&=1", "&=1");
}

#[test]
fn an_ampersand_at_the_very_end_of_the_input_survives() {
    want("&", "&");
    want("x?a=1&", "x?a=1&");
}

#[test]
fn a_reference_with_no_terminator_is_text() {
    want("&amp", "&amp");
    want("text &lt", "text &lt");
    want("&#65", "&#65");
    want("&#x41", "&#x41");
}

#[test]
fn text_ending_in_an_ampersand_word_is_kept() {
    want("R&D", "R&D");
    want("Tom&Jerry", "Tom&Jerry");
}

#[test]
fn a_terminated_reference_that_is_unknown_is_still_an_error() {
    bad("R&D;");
    bad("&nosuchthing;");
    bad("&#xZZ;");
}

#[test]
fn an_unknown_named_entity_is_an_error() {
    bad("&nosuchthing;");
    bad("&AMP;");
    bad("&foo;");
    bad("&#;");
    bad("&#x;");
}

#[test]
fn a_malformed_numeric_reference_is_an_error() {
    bad("&#xZZ;");
    bad("&#12a;");
    bad("&#999999999;");
    bad("&#x7FFFFFFF;");
}

#[test]
fn surrogates_are_rejected() {
    bad("&#xD800;");
    bad("&#xDC00;");
    bad("&#xDFFF;");
    bad("&#55296;");
    want("&#xD7FF;", "\u{D7FF}");
    want("&#xE000;", "\u{E000}");
}

#[test]
fn codepoints_above_the_unicode_ceiling_are_rejected() {
    bad("&#x110000;");
    bad("&#1114112;");
    bad("&#x7FFFFFFF;");
    want("&#x10FFFF;", "\u{10FFFF}");
}

#[test]
fn a_nul_reference_is_rejected() {
    bad("&#0;");
    bad("&#x0;");
    bad("&#00000;");
}

#[test]
fn the_illegal_control_range_is_rejected() {
    bad("&#1;");
    bad("&#8;");
    bad("&#11;");
    bad("&#12;");
    bad("&#14;");
    bad("&#31;");
    bad("&#x1F;");
    // Allowed controls and the first char above them.
    want("&#9;", "\t");
    want("&#10;", "\n");
    want("&#13;", "\r");
    want("&#32;", " ");
}

#[test]
fn a_reference_may_be_immediately_followed_by_another() {
    want("&amp;&lt;", "&<");
    want("&#65;&#66;&#67;", "ABC");
    want("&lt;&lt;&lt;", "<<<");
    // Produced `&` is not re-read.
    want("&amp;amp;", "&amp;");
    want("&amp;#65;", "&#65;");
}

#[test]
fn a_reference_at_the_very_start_and_the_very_end_decodes() {
    want("&lt;middle&gt;", "<middle>");
    want("&amp;", "&");
    want("x&amp;", "x&");
    want("&amp;x", "&x");
}

#[test]
fn an_output_buffer_the_size_of_the_input_is_enough() {
    // Widest char from the shortest source.
    let raw = "&#x10000;".as_bytes();
    let mut out = std::vec![0u8; raw.len()];
    assert_eq!(decode(raw, &mut out).unwrap(), 4);

    let raw = "&#x80;&#x800;&#x10000;A".as_bytes();
    let mut out = std::vec![0u8; raw.len()];
    let n = decode(raw, &mut out).unwrap();
    assert_eq!(&out[..n], "\u{80}\u{800}\u{10000}A".as_bytes());
}

#[test]
fn a_too_small_output_buffer_errors_without_writing_past_the_end() {
    let raw = b"hello world";
    let guard = 0xAA;
    for room in 0..raw.len() {
        let mut backing = std::vec![guard; raw.len() + 8];
        let e = decode(raw, &mut backing[..room]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadReference);
        assert!(
            backing[room..].iter().all(|b| *b == guard),
            "wrote past the end with room {room}"
        );
    }
}

#[test]
fn a_too_small_output_buffer_errors_on_the_character_too() {
    // Run fits, expanded char does not.
    let raw = "ab&#x10000;".as_bytes();
    let guard = 0xAA;
    let mut backing = std::vec![guard; 16];
    let e = decode(raw, &mut backing[..5]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::BadReference);
    assert!(backing[5..].iter().all(|b| *b == guard));
    assert_eq!(&backing[..2], b"ab");
}

#[test]
fn the_named_table_is_sorted_and_has_no_duplicates() {
    for pair in NAMED.windows(2) {
        let (a, _) = pair[0];
        let (b, _) = pair[1];
        assert!(a.as_bytes() < b.as_bytes(), "{a:?} is not before {b:?}");
    }
    assert!(!NAMED.is_empty());
}

#[test]
fn every_named_entity_is_ascii_and_short_enough_to_be_found() {
    for (name, _) in NAMED {
        assert!(!name.is_empty(), "empty name in the table");
        assert!(name.len() <= MAX_BODY, "{name:?} is longer than the scan");
        assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric()),
            "{name:?} has a byte the reference scan would stop on"
        );
    }
}

#[test]
fn every_named_entity_decodes_to_its_character() {
    for (name, ch) in NAMED {
        let mut raw = String::from("&");
        raw.push_str(name);
        raw.push(';');
        let got = String::from_utf8(dec(&raw)).unwrap();
        assert_eq!(got.chars().next(), Some(*ch), "decoding {raw:?}");
        assert_eq!(got.chars().count(), 1, "decoding {raw:?}");
    }
}

#[test]
fn no_named_entity_expands_past_its_own_length() {
    assert_eq!(MAX_EXPANSION, 1);
    for (name, ch) in NAMED {
        assert!(
            ch.len_utf8() <= name.len() + 2,
            "{name:?} expands past its reference"
        );
    }
}

#[test]
fn the_html_entities_real_feeds_use_are_present() {
    let required = [
        "nbsp", "copy", "reg", "trade", "hellip", "mdash", "ndash", "lsquo", "rsquo", "ldquo",
        "rdquo", "bull", "middot", "laquo", "raquo", "deg", "plusmn", "times", "divide", "frac12",
        "eacute", "egrave", "agrave", "ccedil", "uuml", "ouml", "auml", "szlig", "euro", "pound",
        "yen", "cent", "sect", "para", "dagger", "permil", "prime", "ne", "le", "ge", "larr",
        "rarr", "harr", "infin", "alpha", "beta", "gamma", "pi", "mu", "ohm", "amp", "lt", "gt",
        "quot", "apos",
    ];
    for name in required {
        assert!(
            NAMED.iter().any(|(n, _)| *n == name),
            "{name:?} is missing from the table"
        );
    }
    want("a&nbsp;b", "a\u{A0}b");
    want("&copy;&reg;&trade;", "\u{A9}\u{AE}\u{2122}");
    want("wait&hellip;", "wait\u{2026}");
    want("caf&eacute;", "caf\u{e9}");
    want("&frac12; &deg;C", "\u{BD} \u{B0}C");
    want("&euro;9&thinsp;000", "\u{20AC}9\u{2009}000");
}

#[test]
fn needs_decode_agrees_with_whether_decode_changes_anything() {
    let clean = ["", "plain", "a <b> c", "caf\u{e9}", "1 + 1 = 2"];
    for raw in clean {
        assert!(!needs_decode(raw.as_bytes()), "{raw:?}");
        want(raw, raw);
    }
    let dirty = ["&amp;", "a&lt;b", "&#65;", "x&nbsp;y"];
    for raw in dirty {
        assert!(needs_decode(raw.as_bytes()), "{raw:?}");
        assert_ne!(dec(raw).as_slice(), raw.as_bytes(), "{raw:?}");
    }
    // Bare `&` sets the flag but decodes to itself.
    assert!(needs_decode(b"?a=1&b=2"));
    want("?a=1&b=2", "?a=1&b=2");
}

#[test]
fn error_offsets_point_at_the_ampersand() {
    assert_eq!(err("hello &nope; world").at, 6);
    assert_eq!(err("&amp;&nope;").at, 5);
    assert_eq!(err("abc&#xZZ;").at, 3);
    assert_eq!(err("abcd&lt;x&nope;").at, 9);
}

#[test]
fn a_realistic_feed_title_decodes() {
    want(
        "Rust &amp; C++: what&rsquo;s next &#8212; part 2",
        "Rust & C++: what\u{2019}s next \u{2014} part 2",
    );
    want(
        "&lt;title&gt; tags, &ldquo;quoted&rdquo; &amp; linked: /x?a=1&b=2",
        "<title> tags, \u{201C}quoted\u{201D} & linked: /x?a=1&b=2",
    );
}

#[test]
fn a_long_run_after_an_ampersand_is_text_not_a_reference() {
    let raw = "&abcdefghijklmnopqrstuvwxyz and more";
    want(raw, raw);
}
