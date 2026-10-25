use super::*;
use crate::{ROOT, Span};
use std::vec;
use std::vec::Vec;

mod corpus;
mod diff;

fn nodes_for(src: &[u8]) -> Vec<Node> {
    vec![Node::EMPTY; count(src).unwrap_or(0).max(1)]
}

fn fail(src: &str) -> ErrorKind {
    let mut nodes = [Node::EMPTY; 64];
    match parse(src.as_bytes(), &mut nodes) {
        Ok(_) => panic!("{src:?} parsed"),
        Err(e) => e.kind,
    }
}

#[test]
fn scalars() {
    for (src, kind) in [
        ("null", Kind::Null),
        ("true", Kind::True),
        (" false ", Kind::False),
        ("-12.5e+3", Kind::Number),
        ("\"x\"", Kind::String),
    ] {
        let mut nodes = [Node::EMPTY; 1];
        let doc = parse(src.as_bytes(), &mut nodes).unwrap();
        assert_eq!(doc.kind(ROOT), Some(kind), "{src}");
    }
}

#[test]
fn an_object_reads_back() {
    let src = br#"{"num": 3, "name": "1:web", "focused": true, "rect": {"x": -4, "w": 1920},
        "tags": ["a", "b\/c"], "none": null, "f": 0.25}"#;
    let mut nodes = nodes_for(src);
    let d = parse(src, &mut nodes).unwrap();
    assert_eq!(d.get_i64(ROOT, "num"), Some(3));
    assert_eq!(d.get_str(ROOT, "name"), Some("1:web"));
    assert_eq!(d.get_bool(ROOT, "focused"), Some(true));
    let rect = d.get(ROOT, "rect").unwrap();
    assert_eq!(d.get_i64(rect, "x"), Some(-4));
    assert_eq!(d.get_i64(rect, "w"), Some(1920));
    let tags = d.get(ROOT, "tags").unwrap();
    assert_eq!(d.len(tags), 2);
    let second = d.index(tags, 1).unwrap();
    assert_eq!(d.str(second), None, "escaped, so not borrowed");
    assert!(d.text(second).unwrap().eq_str("b/c"));
    assert!(d.is_null(d.get(ROOT, "none").unwrap()));
    assert_eq!(d.f64(d.get(ROOT, "f").unwrap()), Some(0.25));
    assert_eq!(d.len(ROOT), 7);
    assert_eq!(d.get(ROOT, "missing"), None);
}

#[test]
fn subtrees_are_contiguous_and_siblings_one_jump() {
    let src = br#"[[1, [2, 3]], {"k": [4]}, 5]"#;
    let mut nodes = nodes_for(src);
    let d = parse(src, &mut nodes).unwrap();
    let top: Vec<u32> = d.children(ROOT).collect();
    assert_eq!(top.len(), 3);
    assert_eq!(d.node(top[0]).unwrap().end, top[1]);
    assert_eq!(d.node(top[1]).unwrap().end, top[2]);
    assert_eq!(d.i64(top[2]), Some(5));
    assert_eq!(d.node(ROOT).unwrap().end as usize, d.nodes().len());
    assert_eq!(d.raw(top[1]), br#"{"k": [4]}"#);
    assert_eq!(
        d.node(ROOT).unwrap().span,
        Span {
            at: 0,
            len: src.len() as u32
        }
    );
}

#[test]
fn empty_containers() {
    let src = b"{\"a\":[],\"b\":{},\"c\":[ ]}";
    let mut nodes = nodes_for(src);
    let d = parse(src, &mut nodes).unwrap();
    assert_eq!(d.len(d.get(ROOT, "a").unwrap()), 0);
    assert_eq!(d.len(d.get(ROOT, "b").unwrap()), 0);
    assert_eq!(d.len(d.get(ROOT, "c").unwrap()), 0);
}

#[test]
fn numbers_follow_the_grammar() {
    for ok in [
        "0",
        "-0",
        "10",
        "1.5",
        "1e9",
        "1E-2",
        "-0.0e+0",
        "123456789012",
    ] {
        let mut nodes = [Node::EMPTY; 1];
        let d = parse(ok.as_bytes(), &mut nodes).unwrap();
        assert_eq!(d.number(ROOT), Some(ok));
    }
    for bad in ["-", "1.", ".5", "1e", "1e+", "+1", "--1"] {
        let k = fail(bad);
        assert!(
            matches!(k, ErrorKind::BadNumber | ErrorKind::BadByte),
            "{bad}: {k:?}"
        );
    }
    // Leading zeros stop the number and leave a stray digit.
    assert_eq!(fail("01"), ErrorKind::Trailing);
    let mut nodes = [Node::EMPTY; 1];
    let d = parse(b"9223372036854775808", &mut nodes).unwrap();
    assert_eq!(d.i64(ROOT), None, "past i64");
    assert_eq!(d.u64(ROOT), Some(9_223_372_036_854_775_808));
    assert_eq!(d.i64(0), None);
}

#[test]
fn malformed_documents_fail_where_they_break() {
    let mut nodes = [Node::EMPTY; 16];
    let e = parse(br#"{"a": 1,}"#, &mut nodes).unwrap_err();
    assert_eq!(
        e,
        JsonError {
            kind: ErrorKind::BadKey,
            at: 8
        }
    );
    assert_eq!(fail(r#"{"a" 1}"#), ErrorKind::BadByte);
    assert_eq!(fail(r#"{1: 2}"#), ErrorKind::BadKey);
    assert_eq!(fail("[1 2]"), ErrorKind::BadByte);
    assert_eq!(fail("[1,]"), ErrorKind::BadByte);
    assert_eq!(fail("[1"), ErrorKind::Truncated);
    assert_eq!(fail("\"abc"), ErrorKind::Truncated);
    assert_eq!(fail("tru"), ErrorKind::Truncated);
    assert_eq!(fail("trve"), ErrorKind::BadLiteral);
    assert_eq!(fail("1 2"), ErrorKind::Trailing);
    assert_eq!(fail("]"), ErrorKind::BadByte);
    assert_eq!(fail(""), ErrorKind::Truncated);
    assert_eq!(fail("\"a\u{1}b\""), ErrorKind::Control);
    assert_eq!(fail(r#""\x""#), ErrorKind::BadEscape);
    assert_eq!(fail(r#""\ud800""#), ErrorKind::BadEscape);
}

#[test]
fn strings_must_be_utf8() {
    let mut nodes = [Node::EMPTY; 1];
    let e = parse(b"\"\xff\"", &mut nodes).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Utf8);
}

#[test]
fn nesting_is_bounded() {
    let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
    let mut nodes = nodes_for(ok.as_bytes());
    assert!(parse(ok.as_bytes(), &mut nodes).is_ok());
    let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
    assert_eq!(count(deep.as_bytes()).unwrap_err().kind, ErrorKind::TooDeep);
}

#[test]
fn too_few_nodes_is_an_error_not_a_panic() {
    let src = b"[1,2,3]";
    assert_eq!(count(src), Ok(4));
    let mut nodes = [Node::EMPTY; 3];
    assert_eq!(
        parse(src, &mut nodes).unwrap_err().kind,
        ErrorKind::TooManyNodes
    );
}

#[test]
fn a_vector_is_storage_and_the_doc_is_the_document() {
    let mut nodes = Vec::new();
    let d = parse_vec(br#"{"a":[1,2,3],"b":"x"}"#, &mut nodes).unwrap();
    assert_eq!(d.nodes().len(), 8);
    assert_eq!(d.get_str(ROOT, "b"), Some("x"));
    let stored = nodes.len();
    assert!(stored >= 8);
    let d = parse_vec(b"[true]", &mut nodes).unwrap();
    assert_eq!(d.nodes().len(), 2);
    assert_eq!(nodes.len(), stored, "not shrunk");
    assert_eq!(
        parse_vec(b"[1,", &mut nodes).unwrap_err().kind,
        ErrorKind::Truncated
    );
}

#[test]
fn token_arrays_decode_to_u32s() {
    let src =
        br#"{"data":[0,7,12,4294967295,3],"bad":[1,-2],"frac":[1.5],"big":[4294967296],"s":["1"]}"#;
    let mut nodes = Vec::new();
    let d = parse_vec(src, &mut nodes).unwrap();
    let mut out = Vec::new();
    let data = d.get(ROOT, "data").unwrap();
    assert_eq!(d.u32s(data, &mut out), Some(5));
    assert_eq!(out, [0, 7, 12, u32::MAX, 3]);
    let mut fixed = [0u32; 5];
    assert_eq!(d.u32s_into(data, &mut fixed), Some(5));
    assert_eq!(fixed, [0, 7, 12, u32::MAX, 3]);
    assert_eq!(d.u32s_into(data, &mut fixed[..4]), None, "too short");
    for key in ["bad", "frac", "big", "s"] {
        let i = d.get(ROOT, key).unwrap();
        assert_eq!(d.u32s(i, &mut out), None, "{key}");
    }
    assert_eq!(d.u32s(ROOT, &mut out), None, "not an array");
    let empty = parse_vec(b"[]", &mut nodes).unwrap();
    assert_eq!(empty.u32s(ROOT, &mut out), Some(0));
    assert!(out.is_empty());
}

/// xorshift64, reproducible without a crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Every corruption of a real document either parses or fails cleanly,
/// and whatever parses reads back without a panic.
#[test]
fn mutated_documents_never_panic() {
    let base = br#"{"id":94,"type":"workspace","nodes":[{"name":"a\"b\u00e9","rect":{"x":0,"y":-1.5e3}}],"ok":true,"n":null}"#;
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut nodes = vec![Node::EMPTY; 64];
    let mut buf = [0u8; 256];
    for _ in 0..20_000 {
        let mut src = base.to_vec();
        for _ in 0..(rng.next() % 4 + 1) {
            let i = rng.next() as usize % src.len();
            match rng.next() % 3 {
                0 => src[i] = rng.next() as u8,
                1 => {
                    src.remove(i);
                }
                _ => {
                    let alphabet = b"{}[]\",:\\u0";
                    src.insert(i, alphabet[rng.next() as usize % alphabet.len()]);
                }
            }
        }
        if let Ok(d) = parse(&src, &mut nodes) {
            for i in 0..d.nodes().len() as u32 {
                let _ = (d.i64(i), d.f64(i), d.raw(i), d.len(i));
                if let Some(t) = d.text(i) {
                    let _ = t.get(&mut buf);
                }
            }
        }
    }
}
