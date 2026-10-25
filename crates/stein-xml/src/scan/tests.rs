#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

/// Events as transcript lines.
fn read(src: &[u8]) -> Result<Vec<String>, XmlError> {
    read_with(src, 32, 16)
}

fn read_with(src: &[u8], depth: usize, attrs: usize) -> Result<Vec<String>, XmlError> {
    let mut stack = std::vec![Span::EMPTY; depth];
    let mut table = std::vec![Attr::default(); attrs];
    let mut r = Reader::new(src, &mut stack, &mut table);
    let mut out = Vec::new();
    loop {
        let e = r.read()?;
        if e == Event::Eof {
            return Ok(out);
        }
        out.push(show(&r, e));
    }
}

fn show(r: &Reader<'_>, e: Event) -> String {
    let s = |sp: Span| String::from_utf8_lossy(sp.bytes(r.src())).into_owned();
    match e {
        Event::Open(n) => {
            let mut line = format!("<{}", s(n));
            for a in r.attrs() {
                line.push_str(&format!(" {}={}", s(a.name), s(a.value)));
            }
            line.push('>');
            line
        }
        Event::Close(n) => format!("</{}>", s(n)),
        Event::Text(t) => format!("text {:?}", s(t)),
        Event::Cdata(t) => format!("cdata {:?}", s(t)),
        Event::Comment(t) => format!("comment {:?}", s(t)),
        Event::Pi(t) => format!("pi {:?}", s(t)),
        Event::Decl(t) => format!("decl {:?}", s(t)),
        Event::Doctype(t) => format!("doctype {:?}", s(t)),
        Event::Eof => "eof".to_string(),
    }
}

fn kind(src: &[u8]) -> ErrorKind {
    match read(src) {
        Err(e) => e.kind,
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

#[test]
fn an_element_opens_and_closes() {
    assert_eq!(read(b"<a></a>").unwrap(), ["<a>", "</a>"]);
}

#[test]
fn a_self_closing_element_reports_an_open_and_a_close() {
    assert_eq!(read(b"<a/>").unwrap(), ["<a>", "</a>"]);
    assert_eq!(read(b"<a />").unwrap(), ["<a>", "</a>"]);
}

#[test]
fn text_between_elements_is_reported_as_written() {
    assert_eq!(
        read(b"<a>hello world</a>").unwrap(),
        ["<a>", "text \"hello world\"", "</a>"]
    );
}

#[test]
fn references_in_text_are_not_resolved_here() {
    assert_eq!(
        read(b"<a>x &amp; y</a>").unwrap(),
        ["<a>", "text \"x &amp; y\"", "</a>"]
    );
}

#[test]
fn nesting_is_reported_in_document_order() {
    assert_eq!(
        read(b"<a><b>t</b><c/></a>").unwrap(),
        ["<a>", "<b>", "text \"t\"", "</b>", "<c>", "</c>", "</a>"]
    );
}

#[test]
fn attributes_are_read_with_either_quote() {
    assert_eq!(
        read(b"<a x='1' y=\"2\"></a>").unwrap(),
        ["<a x=1 y=2>", "</a>"]
    );
}

#[test]
fn whitespace_around_attributes_is_allowed() {
    assert_eq!(read(b"<a\n  x = '1'\t></a>").unwrap(), ["<a x=1>", "</a>"]);
}

#[test]
fn an_empty_attribute_value_is_fine() {
    assert_eq!(read(b"<a x=''/>").unwrap(), ["<a x=>", "</a>"]);
}

#[test]
fn a_value_may_contain_the_other_quote() {
    assert_eq!(read(b"<a x=\"it's\"/>").unwrap(), ["<a x=it's>", "</a>"]);
}

#[test]
fn a_comment_is_reported_without_its_delimiters() {
    assert_eq!(
        read(b"<a><!-- note --></a>").unwrap(),
        ["<a>", "comment \" note \"", "</a>"]
    );
}

#[test]
fn cdata_is_text_with_no_references() {
    assert_eq!(
        read(b"<a><![CDATA[<b> & </b>]]></a>").unwrap(),
        ["<a>", "cdata \"<b> & </b>\"", "</a>"]
    );
}

#[test]
fn the_declaration_is_told_apart_from_a_processing_instruction() {
    let out = read(b"<?xml version='1.0'?><?php echo ?><a/>").unwrap();
    assert_eq!(out[0], "decl \"xml version='1.0'\"");
    assert_eq!(out[1], "pi \"php echo \"");
}

#[test]
fn a_target_beginning_with_xml_is_not_the_declaration() {
    let out = read(b"<?xmlstylesheet href='x'?><a/>").unwrap();
    assert!(out[0].starts_with("pi "), "got {:?}", out[0]);
}

#[test]
fn a_doctype_is_skipped() {
    assert_eq!(
        read(b"<!DOCTYPE html><a/>").unwrap(),
        ["doctype \" html\"", "<a>", "</a>"]
    );
}

#[test]
fn a_doctype_with_an_internal_subset_is_refused() {
    assert_eq!(
        kind(b"<!DOCTYPE a [<!ENTITY x 'y'>]><a>&x;</a>"),
        ErrorKind::Unsupported
    );
}

#[test]
fn a_close_tag_naming_the_wrong_element_is_an_error() {
    assert_eq!(kind(b"<a><b></a></b>"), ErrorKind::Mismatched);
}

#[test]
fn a_close_tag_with_nothing_open_is_an_error() {
    assert_eq!(kind(b"</a>"), ErrorKind::Unbalanced);
}

#[test]
fn an_unclosed_element_at_the_end_is_an_error() {
    assert_eq!(kind(b"<a><b>text"), ErrorKind::Truncated);
}

#[test]
fn every_truncation_point_is_an_error_rather_than_a_silent_stop() {
    let doc = b"<?xml version='1.0'?><!-- c --><a x='1'><![CDATA[z]]><b/></a>";
    for cut in 1..doc.len() {
        let part = &doc[..cut];
        // Must error or have every open element closed. Never Eof with something open.
        if let Ok(events) = read(part) {
            let closes = events.iter().filter(|e| e.starts_with("</")).count();
            let opens = events
                .iter()
                .filter(|e| e.starts_with('<') && !e.starts_with("</"))
                .count();
            assert_eq!(
                opens,
                closes,
                "cut {cut} left {} open: {events:?}",
                opens - closes
            );
        }
    }
}

#[test]
fn an_unterminated_comment_is_truncated() {
    assert_eq!(kind(b"<a><!-- forever"), ErrorKind::Truncated);
}

#[test]
fn an_unterminated_cdata_is_truncated() {
    assert_eq!(kind(b"<a><![CDATA[forever"), ErrorKind::Truncated);
}

#[test]
fn an_unterminated_attribute_value_is_truncated() {
    assert_eq!(kind(b"<a x='forever"), ErrorKind::Truncated);
}

#[test]
fn an_attribute_without_a_value_is_an_error() {
    assert_eq!(kind(b"<a checked></a>"), ErrorKind::BadAttr);
}

#[test]
fn an_unquoted_attribute_value_is_an_error() {
    assert_eq!(kind(b"<a x=1></a>"), ErrorKind::BadAttr);
}

#[test]
fn the_same_attribute_twice_is_an_error() {
    assert_eq!(kind(b"<a x='1' x='2'/>"), ErrorKind::DuplicateAttr);
}

#[test]
fn a_tag_that_is_not_a_tag_is_an_error() {
    assert_eq!(kind(b"<1a/>"), ErrorKind::BadTag);
    assert_eq!(kind(b"<!bogus>"), ErrorKind::BadTag);
}

#[test]
fn nesting_past_the_stack_is_refused_rather_than_grown() {
    assert_eq!(
        read_with(b"<a><b><c><d/></c></b></a>", 2, 4)
            .unwrap_err()
            .kind,
        ErrorKind::TooDeep
    );
}

#[test]
fn more_attributes_than_the_table_holds_are_refused() {
    assert_eq!(
        read_with(b"<a p='1' q='2' r='3'/>", 8, 2).unwrap_err().kind,
        ErrorKind::TooManyAttrs
    );
}

#[test]
fn the_depth_tracks_the_open_elements() {
    let mut stack = std::vec![Span::EMPTY; 8];
    let mut attrs = std::vec![Attr::default(); 8];
    let mut r = Reader::new(b"<a><b/></a>", &mut stack, &mut attrs);
    assert_eq!(r.depth(), 0);
    r.read().unwrap();
    assert_eq!(r.depth(), 1);
    assert_eq!(r.parent().map(|s| s.bytes(b"<a><b/></a>")), Some(&b"a"[..]));
    r.read().unwrap();
    assert_eq!(
        r.depth(),
        2,
        "a self closing element is open until its close"
    );
    assert_eq!(r.parent().map(|s| s.bytes(b"<a><b/></a>")), Some(&b"b"[..]));
    r.read().unwrap();
    assert_eq!(r.depth(), 1, "the self closing element stayed open");
    r.read().unwrap();
    assert_eq!(r.depth(), 0);
}

#[test]
fn every_span_points_inside_the_document() {
    let doc = b"<?xml version='1.0'?><a x='v'><!-- c --><b>t</b><![CDATA[z]]></a>";
    let mut stack = std::vec![Span::EMPTY; 8];
    let mut attrs = std::vec![Attr::default(); 8];
    let mut r = Reader::new(doc, &mut stack, &mut attrs);
    loop {
        let e = r.read().unwrap();
        if e == Event::Eof {
            break;
        }
        let spans: Vec<Span> = match e {
            Event::Open(s)
            | Event::Close(s)
            | Event::Text(s)
            | Event::Cdata(s)
            | Event::Comment(s)
            | Event::Pi(s)
            | Event::Decl(s)
            | Event::Doctype(s) => std::vec![s],
            Event::Eof => std::vec![],
        };
        for s in spans {
            let end = s.at as usize + s.len as usize;
            assert!(end <= doc.len(), "{s:?} runs past the document");
        }
    }
}

#[test]
fn text_is_borrowed_rather_than_copied() {
    let doc = b"<a>borrowed</a>";
    let mut stack = std::vec![Span::EMPTY; 4];
    let mut attrs = std::vec![Attr::default(); 4];
    let mut r = Reader::new(doc, &mut stack, &mut attrs);
    r.read().unwrap();
    let Event::Text(t) = r.read().unwrap() else {
        panic!("expected text");
    };
    let got = t.bytes(doc);
    assert_eq!(got, b"borrowed");
    assert!(
        core::ptr::eq(got.as_ptr(), doc[3..].as_ptr()),
        "the text was copied"
    );
}

#[test]
fn utf8_content_passes_through_untouched() {
    let doc = "<a t='日本'>ばらばら</a>".as_bytes();
    assert_eq!(
        read(doc).unwrap(),
        ["<a t=日本>", "text \"ばらばら\"", "</a>"]
    );
}

#[test]
fn a_prefixed_name_is_read_whole() {
    assert_eq!(
        read(b"<dc:date>x</dc:date>").unwrap(),
        ["<dc:date>", "text \"x\"", "</dc:date>"]
    );
}

#[test]
fn whitespace_between_elements_is_text() {
    assert_eq!(
        read(b"<a>\n  <b/>\n</a>").unwrap(),
        [
            "<a>",
            "text \"\\n  \"",
            "<b>",
            "</b>",
            "text \"\\n\"",
            "</a>"
        ]
    );
}

#[test]
fn a_real_rss_opening_parses() {
    let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Example &amp; Co</title>
    <link>https://example.com/?a=1&amp;b=2</link>
    <item>
      <title>First post</title>
      <dc:date>2024-01-01</dc:date>
      <description><![CDATA[<p>Hello</p>]]></description>
    </item>
  </channel>
</rss>"#;
    let out = read(doc).unwrap();
    assert!(out.iter().any(|e| e.starts_with("<rss ")), "{out:?}");
    assert!(out.contains(&"<dc:date>".to_string()));
    assert!(out.contains(&"cdata \"<p>Hello</p>\"".to_string()));
    assert!(out.contains(&"text \"Example &amp; Co\"".to_string()));
}

#[test]
fn a_real_atom_opening_parses() {
    let doc = br#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Example</title>
  <entry>
    <title>Post</title>
    <link rel="alternate" href="https://example.com/1"/>
    <updated>2024-01-01T00:00:00Z</updated>
  </entry>
</feed>"#;
    let out = read(doc).unwrap();
    assert!(out.iter().any(|e| e.contains("xmlns=")), "{out:?}");
    assert!(out.iter().any(|e| e.contains("rel=alternate")), "{out:?}");
}

#[test]
fn a_document_with_no_elements_is_not_an_error() {
    assert_eq!(
        read(b"<!-- just a comment -->").unwrap(),
        ["comment \" just a comment \""]
    );
    assert_eq!(read(b"").unwrap(), Vec::<String>::new());
}

#[test]
fn the_events_do_not_depend_on_the_table_sizes() {
    let doc = b"<a x='1' y='2'><b><c/></b></a>";
    let small = read_with(doc, 8, 4).unwrap();
    let large = read_with(doc, 64, 64).unwrap();
    assert_eq!(small, large);
}
