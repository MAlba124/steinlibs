#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::string::String;
use std::vec::Vec;

/// RFC 3986 5.4 base.
const RFC: &str = "http://a/b/c/d;p?q";

fn ok(src: &str) -> Url<'_> {
    parse(src.as_bytes()).unwrap()
}

fn bad(src: &str) -> UrlError {
    parse(src.as_bytes()).unwrap_err()
}

fn text(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap()
}

fn res(base: &str, reference: &str) -> String {
    let mut buf = [0u8; 1024];
    let n = resolve(base.as_bytes(), reference.as_bytes(), &mut buf).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

fn res_err(base: &str, reference: &str) -> UrlError {
    let mut buf = [0u8; 1024];
    resolve(base.as_bytes(), reference.as_bytes(), &mut buf).unwrap_err()
}

fn decode(src: &str) -> String {
    let mut buf = std::vec![0u8; decoded_len(src.as_bytes())];
    let n = percent_decode(src.as_bytes(), &mut buf).unwrap();
    buf.truncate(n);
    String::from_utf8(buf).unwrap()
}

#[test]
fn an_ordinary_http_url_splits_into_its_pieces() {
    let u = ok("http://example.com/feed.xml");
    assert_eq!(u.scheme(), Scheme::Http);
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(text(u.path()), "/feed.xml");
    assert_eq!(u.port(), 80);
    assert!(!u.has_query());
    assert!(!u.has_fragment());
    assert!(!u.has_userinfo());
}

#[test]
fn an_https_url_with_no_path_has_an_empty_path() {
    let u = ok("https://example.com");
    assert_eq!(u.scheme(), Scheme::Https);
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(text(u.path()), "");
    assert_eq!(u.port(), 443);
}

#[test]
fn a_url_carries_its_query_and_fragment_separately() {
    let u = ok("https://example.com/search?q=rust&page=2#results");
    assert_eq!(text(u.path()), "/search");
    assert_eq!(text(u.query()), "q=rust&page=2");
    assert_eq!(text(u.fragment()), "results");
    assert!(u.has_query());
    assert!(u.has_fragment());
}

#[test]
fn a_query_that_is_present_and_empty_is_not_a_missing_query() {
    let u = ok("https://example.com/p?");
    assert!(u.has_query());
    assert_eq!(text(u.query()), "");
    assert!(!ok("https://example.com/p").has_query());
}

#[test]
fn the_default_port_is_eighty_for_http_and_four_four_three_for_https() {
    assert_eq!(ok("http://example.com/").port(), 80);
    assert_eq!(ok("https://example.com/").port(), 443);
    assert_eq!(ok("http://example.com/").explicit_port(), None);
}

#[test]
fn an_explicit_port_overrides_the_default() {
    let u = ok("https://example.com:8443/feed");
    assert_eq!(u.port(), 8443);
    assert_eq!(u.explicit_port(), Some(8443));
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(text(u.path()), "/feed");
}

#[test]
fn userinfo_before_the_host_does_not_become_the_host() {
    let u = ok("https://evil.com@good.com/");
    assert_eq!(text(u.host()), "good.com");
    assert_eq!(text(u.userinfo()), "evil.com");
    assert!(u.has_userinfo());
    assert!(u.host_eq(b"good.com"));
}

#[test]
fn an_encoded_at_sign_in_the_userinfo_keeps_the_last_at_as_the_split() {
    let u = ok("https://user%40evil.com@good.com/feed");
    assert_eq!(text(u.host()), "good.com");
    assert_eq!(text(u.userinfo()), "user%40evil.com");
    assert_eq!(text(u.path()), "/feed");
}

#[test]
fn a_colon_in_the_userinfo_is_not_a_port() {
    let u = ok("https://user:pass@example.com/x");
    assert_eq!(text(u.userinfo()), "user:pass");
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(u.port(), 443);
    assert_eq!(u.explicit_port(), None);
}

#[test]
fn userinfo_and_a_port_can_both_be_there() {
    let u = ok("http://user:pass@example.com:8080/x");
    assert_eq!(text(u.userinfo()), "user:pass");
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(u.port(), 8080);
    assert_eq!(text(u.authority()), "user:pass@example.com:8080");
}

#[test]
fn an_at_sign_after_the_authority_is_part_of_the_path() {
    let u = ok("https://good.com/path@evil.com");
    assert_eq!(text(u.host()), "good.com");
    assert!(!u.has_userinfo());
    assert_eq!(text(u.path()), "/path@evil.com");
}

#[test]
fn an_ipv6_literal_keeps_its_colons_out_of_the_port() {
    let u = ok("http://[2001:db8::1]/feed");
    assert_eq!(text(u.host()), "2001:db8::1");
    assert!(u.host_is_ipv6());
    assert_eq!(u.port(), 80);
    assert_eq!(text(u.path()), "/feed");
}

#[test]
fn an_ipv6_literal_with_a_port_splits_after_the_bracket() {
    let u = ok("https://[2001:db8::1]:8443/path");
    assert_eq!(text(u.host()), "2001:db8::1");
    assert_eq!(u.port(), 8443);
    assert_eq!(text(u.path()), "/path");
    assert_eq!(text(u.authority()), "[2001:db8::1]:8443");
}

#[test]
fn a_bracket_that_never_closes_is_an_error() {
    assert_eq!(bad("http://[2001:db8::1/feed"), UrlError::BadHost);
    assert_eq!(bad("http://[::1"), UrlError::BadHost);
}

#[test]
fn an_empty_bracket_pair_is_an_error() {
    assert_eq!(bad("http://[]/feed"), UrlError::NoHost);
}

#[test]
fn something_bracketed_that_is_not_an_address_is_an_error() {
    assert_eq!(bad("http://[example.com]/feed"), UrlError::BadHost);
}

#[test]
fn a_bracketed_host_followed_by_junk_is_an_error() {
    assert_eq!(bad("http://[::1]x/feed"), UrlError::BadHost);
}

#[test]
fn an_empty_port_is_an_error() {
    assert_eq!(bad("http://example.com:/feed"), UrlError::BadPort);
}

#[test]
fn a_port_that_is_not_all_digits_is_an_error() {
    assert_eq!(bad("http://example.com:8x80/"), UrlError::BadPort);
    assert_eq!(bad("http://example.com:80:90/"), UrlError::BadPort);
    assert_eq!(bad("http://example.com:-1/"), UrlError::BadPort);
}

#[test]
fn a_port_above_sixty_five_thousand_is_an_error_not_a_truncation() {
    assert_eq!(bad("https://example.com:99999/"), UrlError::BadPort);
    assert_eq!(bad("https://example.com:65536/"), UrlError::BadPort);
    assert_eq!(ok("https://example.com:65535/").port(), 65535);
}

#[test]
fn leading_zeros_in_a_port_are_still_digits() {
    assert_eq!(ok("http://example.com:00080/").port(), 80);
}

#[test]
fn the_scheme_is_read_without_regard_to_case() {
    assert_eq!(ok("HTTP://example.com/").scheme(), Scheme::Http);
    assert_eq!(ok("HtTpS://example.com/").scheme(), Scheme::Https);
    assert_eq!(ok("HtTpS://example.com/").port(), 443);
}

#[test]
fn the_host_compares_without_regard_to_case() {
    let u = ok("http://ExAmPlE.CoM/Feed");
    assert!(u.host_eq(b"example.com"));
    assert!(u.host_eq(b"EXAMPLE.COM"));
    assert!(!u.host_eq(b"example.org"));
    assert_eq!(text(u.host()), "ExAmPlE.CoM");
}

#[test]
fn the_path_keeps_its_case() {
    let u = ok("http://example.com/Feed/Items.XML?A=B#Frag");
    assert_eq!(text(u.path()), "/Feed/Items.XML");
    assert_eq!(text(u.query()), "A=B");
    assert_eq!(text(u.fragment()), "Frag");
}

#[test]
fn an_empty_host_is_an_error() {
    assert_eq!(bad("http:///path"), UrlError::NoHost);
    assert_eq!(bad("http://"), UrlError::NoHost);
    assert_eq!(bad("http://:8080/"), UrlError::NoHost);
    assert_eq!(bad("http://@example.com:/"), UrlError::BadPort);
    assert_eq!(bad("http://user@/path"), UrlError::NoHost);
}

#[test]
fn a_url_with_no_scheme_is_an_error() {
    assert_eq!(bad("//example.com/feed"), UrlError::NoScheme);
    assert_eq!(bad("example.com/feed"), UrlError::NoScheme);
    assert_eq!(bad("/feed.xml"), UrlError::NoScheme);
    assert_eq!(bad(""), UrlError::NoScheme);
}

#[test]
fn a_scheme_that_is_not_http_or_https_is_an_error() {
    assert_eq!(bad("ftp://example.com/f"), UrlError::BadScheme);
    assert_eq!(bad("mailto:someone@example.com"), UrlError::BadScheme);
    assert_eq!(bad("javascript://example.com/x"), UrlError::BadScheme);
    assert_eq!(bad("feed://example.com/rss"), UrlError::BadScheme);
}

#[test]
fn a_url_that_is_only_a_scheme_has_no_host() {
    assert_eq!(bad("http:"), UrlError::NoHost);
    assert_eq!(bad("https:"), UrlError::NoHost);
    assert_eq!(bad("http:/feed"), UrlError::NoHost);
}

#[test]
fn a_host_carrying_a_byte_a_host_may_not_have_is_an_error() {
    assert_eq!(bad("http://exa|mple.com/"), UrlError::BadHost);
    assert_eq!(bad("http://exam\u{e9}ple.com/"), UrlError::BadHost);
    assert_eq!(bad("http://ex<ample.com/"), UrlError::BadHost);
}

#[test]
fn a_control_byte_anywhere_in_a_url_is_refused() {
    assert_eq!(bad("http://example.com/a\r\nGET /b"), UrlError::BadByte);
    assert_eq!(bad("http://example.com/a b"), UrlError::BadByte);
    assert_eq!(bad("http://exam\tple.com/"), UrlError::BadByte);
}

#[test]
fn whitespace_around_a_url_is_trimmed_off() {
    let src = "  \r\n http://example.com/feed \t";
    let u = parse(src.as_bytes()).unwrap();
    assert_eq!(text(u.as_bytes()), "http://example.com/feed");
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(text(u.path()), "/feed");
}

#[test]
fn repeated_slashes_in_a_path_are_kept_as_they_are() {
    let u = ok("http://example.com//a//b//");
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(text(u.path()), "//a//b//");
    assert_eq!(res("http://h/x", "//a//b//"), "http://a//b//");
    assert_eq!(res("http://h/x", "/a//b"), "http://h/a//b");
}

#[test]
fn a_hash_inside_a_query_starts_the_fragment() {
    let u = ok("http://example.com/p?a=1#b=2");
    assert_eq!(text(u.query()), "a=1");
    assert_eq!(text(u.fragment()), "b=2");
}

#[test]
fn a_second_question_mark_stays_inside_the_query() {
    let u = ok("http://example.com/p?a=1?b=2");
    assert_eq!(text(u.query()), "a=1?b=2");
    assert!(!u.has_fragment());
}

#[test]
fn a_question_mark_after_a_hash_stays_inside_the_fragment() {
    let u = ok("http://example.com/p#a?b");
    assert!(!u.has_query());
    assert_eq!(text(u.fragment()), "a?b");
    assert_eq!(text(u.path()), "/p");
}

#[test]
fn a_very_long_url_parses() {
    let mut src = String::from("http://example.com/");
    for i in 0..1000 {
        src.push_str("segment");
        src.push((b'0' + (i % 10) as u8) as char);
        src.push('/');
    }
    src.push_str("?q=x");
    let u = parse(src.as_bytes()).unwrap();
    assert_eq!(text(u.host()), "example.com");
    assert_eq!(
        u.path().len(),
        src.len() - "http://example.com".len() - "?q=x".len()
    );
    assert_eq!(text(u.query()), "q=x");
}

#[test]
fn a_very_long_relative_reference_resolves() {
    let mut reference = String::new();
    for _ in 0..500 {
        reference.push_str("a/../");
    }
    reference.push_str("end");
    let mut buf = [0u8; 4096];
    let n = resolve(b"http://example.com/x/y", reference.as_bytes(), &mut buf).unwrap();
    assert_eq!(text(&buf[..n]), "http://example.com/x/end");
}

/// RFC 3986 5.4.1. `g:h` is refused and tested separately.
#[test]
fn the_rfc_normal_examples_resolve_as_the_rfc_says() {
    let table: &[(&str, &str)] = &[
        ("g", "http://a/b/c/g"),
        ("./g", "http://a/b/c/g"),
        ("g/", "http://a/b/c/g/"),
        ("/g", "http://a/g"),
        ("//g", "http://g"),
        ("?y", "http://a/b/c/d;p?y"),
        ("g?y", "http://a/b/c/g?y"),
        ("#s", "http://a/b/c/d;p?q#s"),
        ("g#s", "http://a/b/c/g#s"),
        ("g?y#s", "http://a/b/c/g?y#s"),
        (";x", "http://a/b/c/;x"),
        ("g;x", "http://a/b/c/g;x"),
        ("g;x?y#s", "http://a/b/c/g;x?y#s"),
        ("", "http://a/b/c/d;p?q"),
        (".", "http://a/b/c/"),
        ("./", "http://a/b/c/"),
        ("..", "http://a/b/"),
        ("../", "http://a/b/"),
        ("../g", "http://a/b/g"),
        ("../..", "http://a/"),
        ("../../", "http://a/"),
        ("../../g", "http://a/g"),
    ];
    for (reference, want) in table {
        assert_eq!(&res(RFC, reference), want, "resolving {reference:?}");
    }
}

#[test]
fn the_rfc_example_with_a_foreign_scheme_is_refused_instead() {
    assert_eq!(res_err(RFC, "g:h"), UrlError::BadScheme);
}

/// RFC 3986 5.4.2. `http:g` uses the non-strict answer.
#[test]
fn the_rfc_abnormal_examples_resolve_as_the_rfc_says() {
    let table: &[(&str, &str)] = &[
        ("../../../g", "http://a/g"),
        ("../../../../g", "http://a/g"),
        ("/./g", "http://a/g"),
        ("/../g", "http://a/g"),
        ("g.", "http://a/b/c/g."),
        (".g", "http://a/b/c/.g"),
        ("g..", "http://a/b/c/g.."),
        ("..g", "http://a/b/c/..g"),
        ("./../g", "http://a/b/g"),
        ("./g/.", "http://a/b/c/g/"),
        ("g/./h", "http://a/b/c/g/h"),
        ("g/../h", "http://a/b/c/h"),
        ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
        ("g;x=1/../y", "http://a/b/c/y"),
        ("g?y/./x", "http://a/b/c/g?y/./x"),
        ("g?y/../x", "http://a/b/c/g?y/../x"),
        ("g#s/./x", "http://a/b/c/g#s/./x"),
        ("g#s/../x", "http://a/b/c/g#s/../x"),
        ("http:g", "http://a/b/c/g"),
    ];
    for (reference, want) in table {
        assert_eq!(&res(RFC, reference), want, "resolving {reference:?}");
    }
}

#[test]
fn an_absolute_reference_replaces_everything() {
    assert_eq!(
        res("http://a/b/c/d?q#f", "https://other.example:8443/x?y#z"),
        "https://other.example:8443/x?y#z"
    );
    assert_eq!(
        res("https://a/b", "http://other.example/x"),
        "http://other.example/x"
    );
}

#[test]
fn a_scheme_relative_reference_keeps_the_scheme_and_takes_the_rest() {
    assert_eq!(
        res("https://a/b/c?q#f", "//other.example/p"),
        "https://other.example/p"
    );
    assert_eq!(
        res("http://a/b/c", "//other.example/p"),
        "http://other.example/p"
    );
    assert_eq!(
        res("https://a/b/c", "//other.example"),
        "https://other.example"
    );
}

#[test]
fn an_absolute_path_reference_keeps_the_authority() {
    assert_eq!(
        res("https://user@a:8443/b/c?q", "/p"),
        "https://user@a:8443/p"
    );
}

#[test]
fn a_relative_path_reference_hangs_off_the_base_directory() {
    assert_eq!(res("http://a/b/c/d", "p"), "http://a/b/c/p");
    assert_eq!(res("http://a/b/c/", "p"), "http://a/b/c/p");
    assert_eq!(res("http://a/b", "p"), "http://a/p");
    assert_eq!(res("http://a", "p"), "http://a/p");
}

#[test]
fn a_dot_dot_reference_climbs_one_directory() {
    assert_eq!(res("http://a/b/c/d", "../p"), "http://a/b/p");
    assert_eq!(res("http://a/b/c/d/", "../p"), "http://a/b/c/p");
}

#[test]
fn an_empty_reference_is_the_base_without_its_fragment() {
    assert_eq!(res("http://a/b/c?q#frag", ""), "http://a/b/c?q");
    assert_eq!(res("http://a/b/c#frag", ""), "http://a/b/c");
}

#[test]
fn a_query_only_reference_replaces_the_query_and_keeps_the_path() {
    assert_eq!(res("http://a/b/c?old#frag", "?x=1"), "http://a/b/c?x=1");
    assert_eq!(res("http://a/b/c?old", "?"), "http://a/b/c?");
}

#[test]
fn a_fragment_only_reference_keeps_everything_else() {
    assert_eq!(res("http://a/b/c?q#old", "#frag"), "http://a/b/c?q#frag");
    assert_eq!(res("http://a/b/c", "#"), "http://a/b/c#");
}

#[test]
fn dot_segments_are_removed_from_the_middle_of_a_path() {
    assert_eq!(res("http://h/base/x", "/a/b/../c"), "http://h/a/c");
    assert_eq!(res("http://h/base/x", "/a/./b"), "http://h/a/b");
    assert_eq!(res("http://h/base/x", "/a/b/./../c"), "http://h/a/c");
}

#[test]
fn a_trailing_dot_segment_leaves_a_trailing_slash() {
    assert_eq!(res("http://h/base/x", "/a/b/.."), "http://h/a/");
    assert_eq!(res("http://h/base/x", "/a/b/."), "http://h/a/b/");
    assert_eq!(res("http://h/base/x", "/a/b/../.."), "http://h/");
}

#[test]
fn dot_dot_cannot_climb_above_the_root() {
    assert_eq!(res("http://h/base/x", "/../x"), "http://h/x");
    assert_eq!(res("http://h/base/x", "/../../../x"), "http://h/x");
    assert_eq!(res("http://h/base/x", "/.."), "http://h/");
    assert_eq!(
        res("http://h/a/b/c/d", "../../../../../../etc/passwd"),
        "http://h/etc/passwd"
    );
}

#[test]
fn a_reference_that_is_nothing_but_dots_lands_on_the_root() {
    assert_eq!(res("http://h/a", ".."), "http://h/");
    assert_eq!(res("http://h/a/b", "../.."), "http://h/");
    assert_eq!(res("http://h/", "."), "http://h/");
}

#[test]
fn an_empty_segment_survives_dot_segment_removal() {
    assert_eq!(res("http://h/x", "/a//../b"), "http://h/a/b");
    assert_eq!(res("http://h/x", "/a//b"), "http://h/a//b");
}

#[test]
fn the_base_path_is_normalised_too() {
    // Deviation. The RFC leaves the base path untouched for an empty ref.
    assert_eq!(res("http://h/a/b/../c", ""), "http://h/a/c");
    assert_eq!(res("http://h/a/b/../c", "?q"), "http://h/a/c?q");
}

#[test]
fn an_output_buffer_of_exactly_the_right_size_is_enough() {
    let want = "http://a/b/c/g";
    let mut buf = [0u8; 14];
    assert_eq!(buf.len(), want.len());
    let n = resolve(RFC.as_bytes(), b"g", &mut buf).unwrap();
    assert_eq!(n, want.len());
    assert_eq!(text(&buf[..n]), want);
}

#[test]
fn an_output_buffer_one_byte_short_writes_nothing() {
    let mut buf = [0xAAu8; 13];
    let got = resolve(RFC.as_bytes(), b"g", &mut buf);
    assert_eq!(got, Err(UrlError::TooLong));
    assert!(buf.iter().all(|b| *b == 0xAA), "the buffer was touched");
}

#[test]
fn an_empty_output_buffer_writes_nothing() {
    let mut buf: [u8; 0] = [];
    assert_eq!(
        resolve(RFC.as_bytes(), b"g", &mut buf),
        Err(UrlError::TooLong)
    );
}

#[test]
fn a_reference_naming_a_scheme_nothing_can_fetch_is_refused() {
    assert_eq!(
        res_err(RFC, "mailto:someone@example.com"),
        UrlError::BadScheme
    );
    assert_eq!(res_err(RFC, "javascript:alert(1)"), UrlError::BadScheme);
    assert_eq!(res_err(RFC, "https:g"), UrlError::NoHost);
}

#[test]
fn a_reference_with_a_malformed_authority_is_refused() {
    assert_eq!(res_err(RFC, "//host:99999/x"), UrlError::BadPort);
    assert_eq!(res_err(RFC, "//[::1/x"), UrlError::BadHost);
    assert_eq!(res_err(RFC, "///x"), UrlError::NoHost);
    assert_eq!(res_err(RFC, "//h/a\r\nb"), UrlError::BadByte);
}

#[test]
fn a_base_that_is_not_an_absolute_url_is_refused() {
    assert_eq!(res_err("/not/absolute", "g"), UrlError::NoScheme);
    assert_eq!(res_err("ftp://h/x", "g"), UrlError::BadScheme);
}

#[test]
fn resolution_copies_the_authority_through_untouched() {
    assert_eq!(
        res("https://[2001:db8::1]:8443/a/b", "c"),
        "https://[2001:db8::1]:8443/a/c"
    );
    assert_eq!(res("http://user%40x@h/a/b", "c"), "http://user%40x@h/a/c");
    // Userinfo survives resolution, callers must check the result too.
    let out = res("http://a/b", "//evil.com@good.com/x");
    assert_eq!(out, "http://evil.com@good.com/x");
    assert_eq!(text(ok(&out).host()), "good.com");
}

#[test]
fn the_path_and_query_are_never_percent_decoded() {
    let u = ok("http://h/a%2Fb/c%20d?x=%26y");
    assert_eq!(text(u.path()), "/a%2Fb/c%20d");
    assert_eq!(text(u.query()), "x=%26y");
    assert_eq!(res("http://h/a/b", "c%2Fd"), "http://h/a/c%2Fd");
    // An encoded dot segment is not a dot segment.
    assert_eq!(res("http://h/a/b", "%2E%2E/c"), "http://h/a/%2E%2E/c");
}

#[test]
fn percent_decoding_turns_escapes_back_into_bytes() {
    assert_eq!(decode("a%20b"), "a b");
    assert_eq!(decode("%2F"), "/");
    assert_eq!(decode("%2f"), "/");
    assert_eq!(decode("caf%C3%A9"), "caf\u{e9}");
    assert_eq!(decode("nothing-to-do"), "nothing-to-do");
    assert_eq!(decode(""), "");
}

#[test]
fn percent_decoding_leaves_a_percent_that_is_not_an_escape_alone() {
    assert_eq!(decode("100%"), "100%");
    assert_eq!(decode("%zz"), "%zz");
    assert_eq!(decode("%4"), "%4");
    assert_eq!(decode("50%%20off"), "50% off");
    assert_eq!(decode("a+b"), "a+b");
}

#[test]
fn percent_decoding_into_a_short_buffer_writes_nothing() {
    let src = b"a%20b";
    assert_eq!(decoded_len(src), 3);
    let mut buf = [0xAAu8; 2];
    assert_eq!(percent_decode(src, &mut buf), Err(UrlError::TooLong));
    assert!(buf.iter().all(|b| *b == 0xAA));
    let mut exact = [0u8; 3];
    assert_eq!(percent_decode(src, &mut exact), Ok(3));
    assert_eq!(text(&exact), "a b");
}

#[test]
fn the_parts_of_a_url_are_offsets_into_the_input() {
    let src = "https://user@example.com:8443/p?q#f";
    let u = parse(src.as_bytes()).unwrap();
    let p = u.parts();
    let piece = |s: Span| text(s.bytes(src.as_bytes()));
    assert_eq!(piece(p.scheme), "https");
    assert_eq!(piece(p.authority), "user@example.com:8443");
    assert_eq!(piece(p.userinfo), "user");
    assert_eq!(piece(p.host), "example.com");
    assert_eq!(piece(p.port), "8443");
    assert_eq!(piece(p.path), "/p");
    assert_eq!(piece(p.query), "q");
    assert_eq!(piece(p.fragment), "f");
    assert_eq!(piece(p.whole), src);
    assert!(p.has(Parts::SCHEME | Parts::AUTHORITY));
    assert!(p.has(Parts::USERINFO));
    assert!(!p.has(Parts::IPV6));
}

#[test]
fn a_url_borrows_and_copies_nothing() {
    let src = Vec::from(&b"http://example.com/feed.xml"[..]);
    let u = parse(&src).unwrap();
    assert!(core::ptr::eq(u.host().as_ptr(), src[7..].as_ptr()));
}

#[test]
fn an_error_says_what_went_wrong() {
    use std::string::ToString;
    assert_eq!(UrlError::BadPort.to_string(), "malformed port");
    assert_eq!(UrlError::NoHost.to_string(), "no host");
}
