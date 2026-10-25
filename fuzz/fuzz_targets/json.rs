//! stein-json against the byte at a time parser it replaced: count, parse and parse_vec give
//! the oracle's nodes or its error at the same byte, and a slice too short fails at the same
//! node. A document that parses reads back through every reader without a panic, each string
//! decodes into a buffer of its raw length, and written again with `write::string` it parses
//! back to the same text.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_json::{Doc, ErrorKind, Kind, Node, count, oracle, parse, parse_vec, write};

fuzz_target!(|src: &[u8]| {
    let want = oracle::count(src);
    assert_eq!(count(src), want, "count");
    // a node takes at least a byte, so this holds them all
    let cap = src.len() + 1;
    let mut a = vec![Node::EMPTY; cap];
    let mut b = vec![Node::EMPTY; cap];
    let ra = oracle::parse(src, &mut a);
    let rb = parse(src, &mut b).map(|d| d.nodes().len());
    assert_eq!(rb, ra, "parse");
    let mut v = Vec::new();
    let rv = parse_vec(src, &mut v).map(|d| d.nodes().len());
    assert_eq!(rv, ra, "parse_vec");
    let Ok(n) = ra else {
        return;
    };
    assert_eq!(want, Ok(n));
    assert!(a[..n] == b[..n], "nodes");
    assert!(v[..n] == a[..n], "vec nodes");
    short(src, n);
    let doc = parse(src, &mut b);
    assert!(doc.is_ok(), "a second parse");
    if let Ok(doc) = doc {
        read(&doc);
    }
});

/// A slice too short fails at the same node as the oracle, for want of nodes.
fn short(src: &[u8], n: usize) {
    for len in [0, n / 2, n - 1] {
        let mut a = vec![Node::EMPTY; len];
        let mut b = vec![Node::EMPTY; len];
        let ra = oracle::parse(src, &mut a);
        let rb = parse(src, &mut b).map(|d| d.nodes().len());
        assert_eq!(rb, ra, "{len} of {n} nodes");
        assert_eq!(ra.map_err(|e| e.kind), Err(ErrorKind::TooManyNodes));
    }
}

/// Every node through the readers that fit its kind.
fn read(doc: &Doc<'_>) {
    let mut buf = Vec::new();
    for i in 0..doc.nodes().len() as u32 {
        let Some(node) = doc.node(i) else {
            continue;
        };
        match node.kind {
            Kind::String => string(doc, i, &mut buf),
            Kind::Number => {
                assert!(doc.number(i).is_some(), "node {i} has its literal");
                let (_, _, _) = (doc.i64(i), doc.u64(i), doc.f64(i));
            }
            Kind::Array => {
                let len = doc.len(i);
                assert_eq!(doc.children(i).count(), len);
                assert!(doc.index(i, len.saturating_sub(1)).is_some() || len == 0);
                assert!(doc.index(i, len).is_none());
            }
            Kind::Object => object(doc, i, &mut buf),
            Kind::True | Kind::False => assert_eq!(doc.bool(i), Some(node.kind == Kind::True)),
            Kind::Null => assert!(doc.is_null(i)),
        }
    }
}

/// A string decodes into its raw length, compares equal to what it decoded to, and goes
/// through the writer and back unchanged.
fn string(doc: &Doc<'_>, i: u32, buf: &mut Vec<u8>) {
    let Some(t) = doc.text(i) else {
        return;
    };
    // an escape is never shorter than what it stands for
    buf.clear();
    buf.resize(t.raw().len(), 0);
    let decoded = t.get(buf);
    assert!(decoded.is_ok(), "node {i} decodes into its raw length");
    let Ok(s) = decoded else {
        return;
    };
    assert!(t.eq_str(s), "node {i} equals its own text");
    assert_eq!(t.as_str().is_some(), !t.is_escaped());
    let mut out = vec![0u8; s.len() * 6 + 2];
    let wrote = write::string(&mut out, 0, s);
    assert!(wrote.is_ok(), "six bytes a byte and the quotes always fit");
    let Ok(end) = wrote else {
        return;
    };
    let mut nodes = [Node::EMPTY; 1];
    let back = parse(&out[..end], &mut nodes);
    assert!(back.is_ok(), "written {:?} does not parse", &out[..end]);
    if let Ok(back) = back {
        let again = back.text(0);
        assert!(
            again.is_some_and(|a| a.eq_str(s)),
            "{s:?} written and read back"
        );
    }
}

/// Every key finds a value at or after its own entry, the last of a repeated key wins.
fn object(doc: &Doc<'_>, i: u32, buf: &mut Vec<u8>) {
    let pairs = doc.entries(i).count();
    assert_eq!(pairs, doc.len(i));
    assert_eq!(doc.children(i).count(), 2 * pairs);
    for (k, v) in doc.entries(i) {
        let t = doc.text(k);
        assert!(t.is_some(), "key {k} is a string");
        let Some(t) = t else {
            return;
        };
        buf.clear();
        buf.resize(t.raw().len(), 0);
        let decoded = t.get(buf);
        assert!(decoded.is_ok(), "key {k} decodes");
        let Ok(key) = decoded else {
            return;
        };
        let found = doc.get(i, key);
        assert!(
            found.is_some_and(|f| f >= v),
            "key {key:?} finds its last value"
        );
    }
}
