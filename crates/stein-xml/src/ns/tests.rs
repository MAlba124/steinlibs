#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::vec::Vec;

const ATOM: &[u8] = b"http://www.w3.org/2005/Atom";
const DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const CONTENT: &[u8] = b"http://purl.org/rss/1.0/modules/content/";

fn table(n: usize) -> Vec<Binding> {
    std::vec![Binding::default(); n]
}

fn nth_span(src: &[u8], needle: &[u8], nth: usize) -> Span {
    let at = src
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .nth(nth)
        .unwrap();
    Span::of(src, at, at + needle.len())
}

fn span(src: &[u8], needle: &[u8]) -> Span {
    nth_span(src, needle, 0)
}

/// Binding with spans into `src`. Empty args give empty spans.
fn bind(src: &[u8], prefix: &[u8], uri: &[u8], depth: u16) -> Binding {
    let p = if prefix.is_empty() {
        Span::EMPTY
    } else {
        span(src, prefix)
    };
    let u = if uri.is_empty() {
        Span::EMPTY
    } else {
        span(src, uri)
    };
    Binding::new(p, u, depth)
}

#[test]
fn a_prefix_resolves_to_the_uri_it_was_bound_to() {
    let doc = b"<rss xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><item/></rss>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();

    assert_eq!(ns.uri(doc, b"dc"), Some(span(doc, DC)));
    assert_eq!(ns.uri_bytes(doc, b"dc"), Some(DC));
    assert_eq!(ns.len(), 1);
}

#[test]
fn an_unbound_prefix_resolves_to_none() {
    let doc = b"<rss xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><media:thumbnail/></rss>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();

    assert_eq!(ns.uri(doc, b"media"), None);
    assert_eq!(ns.resolve(doc, b"media"), None);
}

#[test]
fn an_inner_redeclaration_shadows_the_outer_one() {
    let doc = b"<a xmlns:p=\"http://www.w3.org/2005/Atom\"><b xmlns:p=\"http://purl.org/dc/elements/1.1/\"/></a>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"p", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"p", DC, 1)).unwrap();

    assert_eq!(ns.uri_bytes(doc, b"p"), Some(DC));
}

#[test]
fn popping_the_inner_scope_brings_the_outer_binding_back() {
    let doc = b"<a xmlns:p=\"http://www.w3.org/2005/Atom\"><b xmlns:p=\"http://purl.org/dc/elements/1.1/\"/></a>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"p", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"p", DC, 1)).unwrap();
    ns.pop_to(0);

    assert_eq!(ns.len(), 1);
    assert_eq!(ns.uri_bytes(doc, b"p"), Some(ATOM));
}

#[test]
fn the_default_namespace_answers_the_empty_prefix() {
    let doc = b"<feed xmlns=\"http://www.w3.org/2005/Atom\"><title/></feed>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"", ATOM, 0)).unwrap();

    assert_eq!(ns.uri_bytes(doc, b""), Some(ATOM));
    assert!(ns.matches(doc, span(doc, b"title"), ATOM, b"title"));
}

#[test]
fn an_empty_xmlns_undeclares_the_default_namespace() {
    let doc = b"<feed xmlns=\"http://www.w3.org/2005/Atom\"><div xmlns=\"\"><note/></div></feed>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"", b"", 1)).unwrap();

    // Shadows, the outer default must not leak through.
    assert_eq!(ns.uri(doc, b""), None);
    assert!(!ns.matches(doc, span(doc, b"note"), ATOM, b"note"));
    assert!(ns.matches(doc, span(doc, b"note"), b"", b"note"));

    ns.pop_to(0);
    assert_eq!(ns.uri_bytes(doc, b""), Some(ATOM));
}

#[test]
fn an_unprefixed_name_with_no_default_is_in_no_namespace() {
    let doc = b"<rss><channel><title/></channel></rss>";
    let mut t = table(4);
    let ns = Names::new(&mut t);

    assert_eq!(ns.uri(doc, b""), None);
    assert!(ns.matches(doc, span(doc, b"title"), b"", b"title"));
    assert!(!ns.matches(doc, span(doc, b"title"), ATOM, b"title"));
}

#[test]
fn several_bindings_at_one_depth_all_resolve() {
    let doc = b"<rss xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\" xmlns=\"http://www.w3.org/2005/Atom\"/>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 2)).unwrap();
    ns.push(bind(doc, b"content", CONTENT, 2)).unwrap();
    ns.push(bind(doc, b"", ATOM, 2)).unwrap();

    assert_eq!(ns.len(), 3);
    assert_eq!(ns.uri_bytes(doc, b"dc"), Some(DC));
    assert_eq!(ns.uri_bytes(doc, b"content"), Some(CONTENT));
    assert_eq!(ns.uri_bytes(doc, b""), Some(ATOM));
}

#[test]
fn pop_to_leaves_shallower_bindings_alone() {
    let doc = b"<a xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><b><c xmlns:content=\"http://purl.org/rss/1.0/modules/content/\"/></b></a>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 1)).unwrap();
    ns.push(bind(doc, b"content", CONTENT, 3)).unwrap();
    ns.pop_to(2);

    assert_eq!(ns.len(), 1);
    assert_eq!(ns.uri_bytes(doc, b"content"), None);
    assert_eq!(ns.uri_bytes(doc, b"dc"), Some(DC));
}

#[test]
fn pop_to_keeps_bindings_declared_at_that_depth() {
    let doc = b"<a xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 2)).unwrap();
    ns.pop_to(2);

    assert_eq!(ns.len(), 1);
    ns.pop_to(1);
    assert_eq!(ns.len(), 0);
}

#[test]
fn pop_to_on_an_empty_table_does_nothing() {
    let mut t = table(2);
    let mut ns = Names::new(&mut t);
    ns.pop_to(0);
    ns.pop_to(7);

    assert!(ns.is_empty());
}

#[test]
fn clear_drops_every_binding() {
    let doc = b"<a xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();
    ns.clear();

    assert!(ns.is_empty());
    assert_eq!(ns.uri(doc, b"dc"), None);
}

#[test]
fn pushing_past_capacity_reports_too_many_bindings() {
    let doc = b"<a xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\" xmlns=\"http://www.w3.org/2005/Atom\"/>";
    let mut t = table(2);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();
    ns.push(bind(doc, b"content", CONTENT, 0)).unwrap();

    let third = bind(doc, b"", ATOM, 0);
    let err = ns.push(third).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TooManyBindings);

    assert_eq!(ns.capacity(), 2);
    assert_eq!(ns.len(), 2);
    assert_eq!(ns.uri_bytes(doc, b"dc"), Some(DC));
    assert_eq!(ns.uri_bytes(doc, b"content"), Some(CONTENT));
    assert_eq!(ns.uri(doc, b""), None);
}

#[test]
fn a_table_with_no_room_refuses_the_first_binding() {
    let doc = b"<a xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>";
    let mut t = table(0);
    let mut ns = Names::new(&mut t);
    let b = bind(doc, b"dc", DC, 0);

    let err = ns.push(b).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TooManyBindings);
    assert_eq!(err.at, b.prefix.at);
}

#[test]
fn the_xml_prefix_resolves_without_a_declaration() {
    let doc = b"<feed><entry xml:lang=\"en\"/></feed>";
    let mut t = table(2);
    let ns = Names::new(&mut t);

    assert_eq!(ns.resolve(doc, b"xml"), Some(Uri::Builtin(XML_URI)));
    assert_eq!(ns.uri_bytes(doc, b"xml"), Some(XML_URI));
    assert_eq!(ns.uri(doc, b"xml"), None);
    assert!(ns.matches(doc, span(doc, b"xml:lang"), XML_URI, b"lang"));
}

#[test]
fn a_declaration_cannot_rebind_the_xml_prefix() {
    let doc = b"<a xmlns:xml=\"http://purl.org/dc/elements/1.1/\"/>";
    let mut t = table(2);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"xml", DC, 0)).unwrap();

    assert_eq!(ns.uri_bytes(doc, b"xml"), Some(XML_URI));
}

#[test]
fn the_xmlns_prefix_never_resolves_as_a_name_prefix() {
    let doc = b"<a xmlns:xmlns=\"http://purl.org/dc/elements/1.1/\"><xmlns:oops/></a>";
    let mut t = table(2);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"xmlns", DC, 0)).unwrap();

    assert_eq!(ns.resolve(doc, b"xmlns"), None);
    assert!(!ns.matches(doc, span(doc, b"xmlns:oops"), DC, b"oops"));
}

#[test]
fn a_name_without_a_colon_is_all_local() {
    let doc = b"<title/>";
    let name = span(doc, b"title");
    let (prefix, local) = split(doc, name);

    assert!(prefix.is_empty());
    assert_eq!(prefix.at, name.at);
    assert_eq!(local, name);
}

#[test]
fn a_name_with_one_colon_splits_at_it() {
    let doc = b"<dc:date>2026</dc:date>";
    let (prefix, local) = split(doc, span(doc, b"dc:date"));

    assert_eq!(prefix.bytes(doc), b"dc");
    assert_eq!(local.bytes(doc), b"date");
    assert_eq!(prefix, span(doc, b"dc"));
    assert_eq!(local, span(doc, b"date"));
}

#[test]
fn a_leading_colon_leaves_the_name_whole() {
    let doc = b"<:title/>";
    let name = span(doc, b":title");
    let (prefix, local) = split(doc, name);

    assert!(prefix.is_empty());
    assert_eq!(local, name);
    assert_eq!(local.bytes(doc), b":title");
}

#[test]
fn a_trailing_colon_leaves_the_name_whole() {
    let doc = b"<dc:/>";
    let name = span(doc, b"dc:");
    let (prefix, local) = split(doc, name);

    assert!(prefix.is_empty());
    assert_eq!(local, name);
    assert_eq!(local.bytes(doc), b"dc:");
}

#[test]
fn a_second_colon_stays_in_the_local_part() {
    let doc = b"<dc:a:b/>";
    let (prefix, local) = split(doc, span(doc, b"dc:a:b"));

    assert_eq!(prefix.bytes(doc), b"dc");
    assert_eq!(local.bytes(doc), b"a:b");
}

#[test]
fn an_empty_name_splits_into_two_empties() {
    let doc = b"<a/>";
    let (prefix, local) = split(doc, Span::of(doc, 1, 1));

    assert!(prefix.is_empty());
    assert!(local.is_empty());
}

#[test]
fn a_degenerate_name_matches_nothing_real() {
    let doc = b"<feed xmlns=\"http://www.w3.org/2005/Atom\"><:title/></feed>";
    let mut t = table(2);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"", ATOM, 0)).unwrap();

    assert!(!ns.matches(doc, span(doc, b":title"), ATOM, b"title"));
}

#[test]
fn declares_recognises_xmlns_attributes() {
    let doc = b"<feed xmlns=\"x\" xmlns:dc=\"y\" href=\"z\"/>";

    let default = declares(doc, span(doc, b"xmlns")).unwrap();
    assert!(default.is_empty());

    let dc = declares(doc, span(doc, b"xmlns:dc")).unwrap();
    assert_eq!(dc.bytes(doc), b"dc");

    assert_eq!(declares(doc, span(doc, b"href")), None);
}

#[test]
fn matches_is_true_for_one_uri_under_two_prefixes() {
    let doc = b"<rss xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:a=\"http://www.w3.org/2005/Atom\"><atom:link/><a:link/></rss>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(Binding::new(span(doc, b"atom"), nth_span(doc, ATOM, 0), 0))
        .unwrap();
    ns.push(Binding::new(span(doc, b"a"), nth_span(doc, ATOM, 1), 0))
        .unwrap();

    assert!(ns.matches(doc, span(doc, b"atom:link"), ATOM, b"link"));
    assert!(ns.matches(doc, span(doc, b"a:link"), ATOM, b"link"));
}

#[test]
fn matches_is_false_for_the_same_local_in_another_namespace() {
    let doc = b"<rss xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title/></rss>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();

    assert!(ns.matches(doc, span(doc, b"dc:title"), DC, b"title"));
    assert!(!ns.matches(doc, span(doc, b"dc:title"), ATOM, b"title"));
    assert!(!ns.matches(doc, span(doc, b"dc:title"), DC, b"date"));
}

#[test]
fn matches_is_false_for_an_unbound_prefix() {
    let doc = b"<rss><media:title/></rss>";
    let mut t = table(2);
    let ns = Names::new(&mut t);

    assert!(!ns.matches(doc, span(doc, b"media:title"), b"", b"title"));
    assert!(!ns.matches(doc, span(doc, b"media:title"), ATOM, b"title"));
}

#[test]
fn resolve_name_gives_the_namespace_and_the_local_part_at_once() {
    let doc = b"<feed xmlns=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:date/></feed>";
    let mut t = table(4);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();

    let (uri, local) = ns.resolve_name(doc, span(doc, b"dc:date"));
    assert_eq!(uri.map(|u| u.bytes(doc)), Some(DC));
    assert_eq!(local.bytes(doc), b"date");

    let (uri, local) = ns.resolve_name(doc, span(doc, b"feed"));
    assert_eq!(uri.map(|u| u.bytes(doc)), Some(ATOM));
    assert_eq!(local.bytes(doc), b"feed");
}

#[test]
fn an_atom_entry_resolves_its_default_and_its_dc_prefix() {
    let doc: &[u8] = b"<feed xmlns=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
<entry><title>Post</title><dc:date>2026-09-22</dc:date><dc:title>Meta</dc:title></entry></feed>";
    let mut t = table(8);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"dc", DC, 0)).unwrap();

    assert!(ns.matches(doc, span(doc, b"feed"), ATOM, b"feed"));
    assert!(ns.matches(doc, span(doc, b"title"), ATOM, b"title"));
    assert!(ns.matches(doc, span(doc, b"dc:date"), DC, b"date"));

    assert!(ns.matches(doc, span(doc, b"dc:title"), DC, b"title"));
    assert!(!ns.matches(doc, span(doc, b"dc:title"), ATOM, b"title"));
    assert!(!ns.matches(doc, span(doc, b"title"), DC, b"title"));
}

#[test]
fn an_rss_channel_carrying_atom_under_a_prefix_still_matches_atom() {
    let doc: &[u8] = b"<rss xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\">\
<channel><title>Feed</title><atom:link/><content:encoded/></channel></rss>";
    let mut t = table(8);
    let mut ns = Names::new(&mut t);
    ns.push(bind(doc, b"atom", ATOM, 0)).unwrap();
    ns.push(bind(doc, b"content", CONTENT, 0)).unwrap();

    assert!(ns.matches(doc, span(doc, b"atom:link"), ATOM, b"link"));
    assert!(ns.matches(doc, span(doc, b"content:encoded"), CONTENT, b"encoded"));
    // RSS is in no namespace.
    assert!(ns.matches(doc, span(doc, b"channel"), b"", b"channel"));
    assert!(ns.matches(doc, span(doc, b"title"), b"", b"title"));
}
