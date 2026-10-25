//! Tokenizer, namespaces and entity decoding together on real feeds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_xml::ns::{Binding, Names, declares};
use stein_xml::{Attr, Event, Reader, Span, decode, entity};

const ATOM: &[u8] = b"http://www.w3.org/2005/Atom";
const DC: &[u8] = b"http://purl.org/dc/elements/1.1/";

#[derive(Default, Debug, PartialEq)]
struct Item {
    title: String,
    date: String,
    link: String,
}

/// Drive the whole stack over a document like an application would.
fn items(doc: &[u8], item_uri: &[u8], item_local: &[u8]) -> Vec<Item> {
    let mut stack = vec![Span::EMPTY; 64];
    let mut attrs = vec![Attr::default(); 32];
    let mut binds = vec![Binding::default(); 32];
    let mut names = Names::new(&mut binds);
    let mut r = Reader::new(doc, &mut stack, &mut attrs);

    let mut c = Collect {
        cur: None,
        field: None,
        out: Vec::new(),
        buf: vec![0u8; 64 * 1024],
    };
    let want = (item_uri, item_local);
    loop {
        let e = r.read().expect("the document parses");
        if e == Event::Eof {
            return c.out;
        }
        step(&mut c, doc, &r, &mut names, want, e);
    }
}

/// Collector state, kept apart from the borrowed parser state.
struct Collect {
    cur: Option<Item>,
    field: Option<&'static str>,
    out: Vec<Item>,
    buf: Vec<u8>,
}

/// One event. Free fn for the nesting lint.
fn step(
    c: &mut Collect,
    doc: &[u8],
    r: &Reader<'_>,
    names: &mut Names<'_>,
    want: (&[u8], &[u8]),
    e: Event,
) {
    match e {
        Event::Open(name) => open(c, doc, r, names, want, name),
        Event::Close(name) => close(c, doc, r, names, want, name),
        Event::Text(t) | Event::Cdata(t) => text(c, doc, t),
        _ => {}
    }
}

fn open(
    c: &mut Collect,
    doc: &[u8],
    r: &Reader<'_>,
    names: &mut Names<'_>,
    want: (&[u8], &[u8]),
    name: Span,
) {
    let depth = r.depth() as u16;
    // Record declarations before resolving the name that may use them.
    for a in r.attrs() {
        declare(names, doc, *a, depth);
    }
    if names.matches(doc, name, want.0, want.1) {
        c.cur = Some(Item::default());
        return;
    }
    c.field = which(names, doc, name);
    if c.field == Some("link") {
        link_attr(c, doc, r);
    }
}

fn declare(names: &mut Names<'_>, doc: &[u8], a: Attr, depth: u16) {
    let Some(prefix) = declares(doc, a.name) else {
        return;
    };
    names
        .push(Binding::new(prefix, a.value, depth))
        .expect("room for the bindings");
}

/// Atom puts the link in an attribute.
fn link_attr(c: &mut Collect, doc: &[u8], r: &Reader<'_>) {
    let Some(href) = attr(doc, r.attrs(), b"href") else {
        return;
    };
    let s = text_of(doc, href, &mut c.buf);
    if let Some(item) = c.cur.as_mut() {
        item.link = s;
    }
}

fn close(
    c: &mut Collect,
    doc: &[u8],
    r: &Reader<'_>,
    names: &mut Names<'_>,
    want: (&[u8], &[u8]),
    name: Span,
) {
    names.pop_to(r.depth() as u16);
    if names.matches(doc, name, want.0, want.1)
        && let Some(item) = c.cur.take()
    {
        c.out.push(item);
    }
    c.field = None;
}

fn text(c: &mut Collect, doc: &[u8], t: Span) {
    let Some(f) = c.field else { return };
    if c.cur.is_none() {
        return;
    }
    let s = text_of(doc, t, &mut c.buf);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(item) = c.cur.as_mut() else { return };
    match f {
        "title" => item.title.push_str(trimmed),
        "date" => item.date.push_str(trimmed),
        "link" if item.link.is_empty() => item.link.push_str(trimmed),
        _ => {}
    }
}

/// Field for an element, matched by namespace not prefix.
fn which(names: &Names<'_>, doc: &[u8], name: Span) -> Option<&'static str> {
    for (uri, local, field) in [
        (ATOM, &b"title"[..], "title"),
        (ATOM, &b"updated"[..], "date"),
        (ATOM, &b"link"[..], "link"),
        (DC, &b"date"[..], "date"),
        (&b""[..], &b"title"[..], "title"),
        (&b""[..], &b"pubDate"[..], "date"),
        (&b""[..], &b"link"[..], "link"),
    ] {
        if names.matches(doc, name, uri, local) {
            return Some(field);
        }
    }
    None
}

fn attr(doc: &[u8], attrs: &[Attr], want: &[u8]) -> Option<Span> {
    attrs
        .iter()
        .find(|a| a.name.bytes(doc) == want)
        .map(|a| a.value)
}

fn text_of(doc: &[u8], s: Span, buf: &mut [u8]) -> String {
    let raw = s.bytes(doc);
    if !entity::needs_decode(raw) {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let n = decode(raw, buf).expect("the references decode");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[test]
fn a_real_rss_feed_yields_its_items() {
    let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Channel title</title>
    <item>
      <title>Rust &amp; C++</title>
      <link>https://example.com/1?a=1&amp;b=2</link>
      <dc:date>2024-01-01</dc:date>
    </item>
    <item>
      <title><![CDATA[Raw <b>markup</b>]]></title>
      <link>https://example.com/2</link>
      <pubDate>Mon, 02 Jan 2024 00:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#;
    let got = items(doc, b"", b"item");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].title, "Rust & C++");
    assert_eq!(got[0].link, "https://example.com/1?a=1&b=2");
    assert_eq!(got[0].date, "2024-01-01");
    assert_eq!(got[1].title, "Raw <b>markup</b>");
}

#[test]
fn a_real_atom_feed_yields_its_entries() {
    let doc = br#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Feed title</title>
  <entry>
    <title>First &#8212; part 1</title>
    <link rel="alternate" href="https://example.com/a"/>
    <updated>2024-03-01T10:00:00Z</updated>
  </entry>
  <entry>
    <title>Second</title>
    <link rel="alternate" href="https://example.com/b"/>
    <updated>2024-03-02T10:00:00Z</updated>
  </entry>
</feed>"#;
    let got = items(doc, ATOM, b"entry");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].title, "First \u{2014} part 1");
    assert_eq!(got[0].link, "https://example.com/a");
    assert_eq!(got[0].date, "2024-03-01T10:00:00Z");
    assert_eq!(got[1].title, "Second");
}

#[test]
fn the_prefix_a_publisher_chose_does_not_matter() {
    let with_a = br#"<a:feed xmlns:a="http://www.w3.org/2005/Atom">
  <a:entry><a:title>T</a:title><a:updated>D</a:updated></a:entry>
</a:feed>"#;
    let with_z = br#"<z:feed xmlns:z="http://www.w3.org/2005/Atom">
  <z:entry><z:title>T</z:title><z:updated>D</z:updated></z:entry>
</z:feed>"#;
    let a = items(with_a, ATOM, b"entry");
    let z = items(with_z, ATOM, b"entry");
    assert_eq!(a, z);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].title, "T");
}

#[test]
fn a_default_namespace_reads_the_same_as_a_prefixed_one() {
    let defaulted = br#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>T</title><updated>D</updated></entry>
</feed>"#;
    let prefixed = br#"<a:feed xmlns:a="http://www.w3.org/2005/Atom">
  <a:entry><a:title>T</a:title><a:updated>D</a:updated></a:entry>
</a:feed>"#;
    assert_eq!(
        items(defaulted, ATOM, b"entry"),
        items(prefixed, ATOM, b"entry")
    );
}

#[test]
fn a_local_name_in_another_namespace_is_not_a_match() {
    let doc = br#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:x="urn:other">
  <entry><x:title>wrong</x:title><title>right</title></entry>
</feed>"#;
    let got = items(doc, ATOM, b"entry");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title, "right");
}

#[test]
fn a_binding_goes_out_of_scope_with_its_element() {
    let doc = br#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry xmlns:d="http://purl.org/dc/elements/1.1/">
    <title>one</title><d:date>D1</d:date>
  </entry>
  <entry>
    <title>two</title><d:date>ignored</d:date>
  </entry>
</feed>"#;
    let got = items(doc, ATOM, b"entry");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].date, "D1");
    // `d:` is unbound in the second entry.
    assert_eq!(got[1].date, "", "an out of scope prefix still resolved");
}

#[test]
fn the_html_entities_real_feeds_use_survive() {
    let doc = br#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>caf&eacute; &mdash; what&rsquo;s next&hellip;</title></entry>
</feed>"#;
    let got = items(doc, ATOM, b"entry");
    assert_eq!(
        got[0].title,
        "caf\u{e9} \u{2014} what\u{2019}s next\u{2026}"
    );
}

#[test]
fn a_raw_ampersand_in_a_url_is_tolerated() {
    let doc = br#"<rss version="2.0"><channel><item>
    <title>t</title><link>https://e.com/?a=1&b=2</link>
  </item></channel></rss>"#;
    let got = items(doc, b"", b"item");
    assert_eq!(got[0].link, "https://e.com/?a=1&b=2");
}

#[test]
fn a_binding_on_a_self_closing_element_does_not_leak() {
    let doc = br#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><link xmlns:d="http://purl.org/dc/elements/1.1/" href="x"/><d:date>leaked</d:date></entry>
</feed>"#;
    let got = items(doc, ATOM, b"entry");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].link, "x");
    assert_eq!(got[0].date, "", "the binding outlived its element");
}
