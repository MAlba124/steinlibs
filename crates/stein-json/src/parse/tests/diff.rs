//! Differential tests. The lane parser, its counter and `parse_vec` must
//! agree with the byte at a time original on every input: the same nodes,
//! or the same error at the same byte. Inputs are the generated corpora,
//! their truncations and corruptions, random documents, and strings built
//! to put every kind of trouble on every lane offset.

use std::string::String;
use std::vec;
use std::vec::Vec;

use super::corpus::{self, Rng};
use crate::oracle;
use crate::{ErrorKind, Node, count, parse, parse_vec};

fn shown(src: &[u8]) -> String {
    let s = String::from_utf8_lossy(&src[..src.len().min(120)]);
    if src.len() > 120 {
        std::format!("{s}... ({} bytes)", src.len())
    } else {
        s.into_owned()
    }
}

/// Every entry point agrees with the oracle on `src`.
fn same(src: &[u8]) {
    let want = oracle::count(src);
    assert_eq!(count(src), want, "count of {}", shown(src));
    // A node takes at least one byte, so this always holds them all.
    let cap = src.len() + 1;
    let mut a = vec![Node::EMPTY; cap];
    let mut b = vec![Node::EMPTY; cap];
    let ra = oracle::parse(src, &mut a);
    let rb = parse(src, &mut b).map(|d| d.nodes().len());
    assert_eq!(rb, ra, "parse of {}", shown(src));
    let mut v = Vec::new();
    let rv = parse_vec(src, &mut v).map(|d| d.nodes().len());
    assert_eq!(rv, ra, "parse_vec of {}", shown(src));
    if let Ok(n) = ra {
        assert_eq!(want, Ok(n));
        assert!(a[..n] == b[..n], "nodes of {}", shown(src));
        assert!(v[..n] == a[..n], "vec nodes of {}", shown(src));
    }
}

/// A slice too short fails at the same node either way.
fn short(src: &[u8]) {
    let Ok(n) = oracle::count(src) else {
        return;
    };
    for len in [0, 1, n / 2, n.saturating_sub(1)] {
        if len >= n {
            continue;
        }
        let mut a = vec![Node::EMPTY; len];
        let mut b = vec![Node::EMPTY; len];
        let ra = oracle::parse(src, &mut a);
        let rb = parse(src, &mut b).map(|d| d.nodes().len());
        assert_eq!(rb, ra, "{len} nodes for {}", shown(src));
        assert_eq!(ra.map_err(|e| e.kind), Err(ErrorKind::TooManyNodes));
    }
}

#[test]
fn corpora_agree() {
    for c in corpus::small() {
        same(c.text.as_bytes());
        short(c.text.as_bytes());
    }
}

/// Every prefix of the small documents and a spread of the larger ones.
#[test]
fn truncations_agree() {
    for c in corpus::small() {
        let src = c.text.as_bytes();
        let step = (src.len() / 400).max(1);
        for cut in (0..src.len()).step_by(step) {
            same(&src[..cut]);
        }
        for cut in src.len().saturating_sub(80)..src.len() {
            same(&src[..cut]);
        }
    }
}

const ALPHABET: &[u8] = b"{}[]\",:\\u0 \n\xff\xc3\x01ent-.9";

fn mutate(rng: &mut Rng, src: &mut Vec<u8>) {
    for _ in 0..rng.below(4) + 1 {
        if src.is_empty() {
            return;
        }
        let i = rng.below(src.len());
        match rng.below(3) {
            0 => src[i] = rng.next() as u8,
            1 => {
                src.remove(i);
            }
            _ => src.insert(i, ALPHABET[rng.below(ALPHABET.len())]),
        }
    }
}

#[test]
fn mutations_agree() {
    let mut rng = Rng(11);
    let base: Vec<Vec<u8>> = corpus::small()
        .into_iter()
        .map(|c| c.text.into_bytes())
        .collect();
    for round in 0..3000 {
        let mut src = base[round % base.len()].clone();
        if src.len() > 4000 {
            // Corrupt a window so the error lands in varied context.
            let at = rng.below(src.len() - 3000);
            src = src[at..at + 3000].to_vec();
        }
        mutate(&mut rng, &mut src);
        same(&src);
    }
}

const PIECES: &[&str] = &[
    "plain",
    " ",
    "é",
    "日本",
    "😀",
    "\\n",
    "\\\"",
    "\\\\",
    "\\/",
    "\\b",
    "\\f",
    "\\r",
    "\\t",
    "\\u0041",
    "\\u00e9",
    "\\ud83d\\ude00",
    "\\uD83D\\uDE00",
    "0123456789abcdef",
    "\t",
    "\u{7f}",
];

fn ws(rng: &mut Rng, out: &mut Vec<u8>) {
    let n = match rng.below(8) {
        0 => rng.below(70),
        1..=4 => rng.below(3),
        _ => 0,
    };
    for _ in 0..n {
        out.push(b" \n\t\r"[rng.below(4)]);
    }
}

fn gen_string(rng: &mut Rng, out: &mut Vec<u8>) {
    out.push(b'"');
    for _ in 0..rng.below(12) {
        out.extend_from_slice(PIECES[rng.below(PIECES.len())].as_bytes());
    }
    out.push(b'"');
}

fn gen_number(rng: &mut Rng, out: &mut Vec<u8>) {
    let s = match rng.below(8) {
        0 => "0",
        1 => "-0",
        2 => "12345678901234567890",
        3 => "1.5",
        4 => "-1.25e+10",
        5 => "1E-2",
        6 => "3e5",
        _ => "7",
    };
    out.extend_from_slice(s.as_bytes());
}

/// A random valid value, `depth` levels of containers at most.
fn gen_value(rng: &mut Rng, depth: usize, out: &mut Vec<u8>) {
    ws(rng, out);
    match rng.below(if depth == 0 { 5 } else { 7 }) {
        0 => out.extend_from_slice(b"null"),
        1 => out.extend_from_slice(b"true"),
        2 => out.extend_from_slice(b"false"),
        3 => gen_number(rng, out),
        4 => gen_string(rng, out),
        5 => {
            out.push(b'[');
            let n = rng.below(5);
            for i in 0..n {
                if i > 0 {
                    ws(rng, out);
                    out.push(b',');
                }
                gen_value(rng, depth - 1, out);
            }
            ws(rng, out);
            out.push(b']');
        }
        _ => {
            out.push(b'{');
            let n = rng.below(5);
            for i in 0..n {
                if i > 0 {
                    ws(rng, out);
                    out.push(b',');
                }
                ws(rng, out);
                gen_string(rng, out);
                ws(rng, out);
                out.push(b':');
                gen_value(rng, depth - 1, out);
            }
            ws(rng, out);
            out.push(b'}');
        }
    }
    ws(rng, out);
}

#[test]
fn random_documents_agree() {
    let mut rng = Rng(12);
    for _ in 0..4000 {
        let mut src = Vec::new();
        gen_value(&mut rng, 4, &mut src);
        same(&src);
        short(&src);
        mutate(&mut rng, &mut src);
        same(&src);
    }
}

#[test]
fn nesting_agrees() {
    for depth in [1, 31, 32, 33, 127, 128, 129, 130, 300] {
        let mut s = String::new();
        for d in 0..depth {
            s.push(if d % 3 == 0 { '{' } else { '[' });
            if d % 3 == 0 {
                s.push_str("\"k\":");
            }
        }
        s.push('1');
        for d in (0..depth).rev() {
            s.push(if d % 3 == 0 { '}' } else { ']' });
        }
        same(s.as_bytes());
        same(&s.as_bytes()[..s.len() / 2]);
        same(&"[".repeat(depth).into_bytes());
    }
}

/// A string of `len` plain bytes with `bad` written over offset `at`, in a
/// document and on its own.
fn string_with(len: usize, at: usize, bad: &[u8]) -> Vec<u8> {
    let mut s = vec![b'a'; len];
    for (k, &b) in bad.iter().enumerate() {
        if let Some(slot) = s.get_mut(at + k) {
            *slot = b;
        }
    }
    let mut doc = Vec::from(&b"{\"key\":\""[..]);
    doc.extend_from_slice(&s);
    doc.extend_from_slice(b"\",\"n\":1}");
    doc
}

const BAD: &[&[u8]] = &[
    b"\x01",
    b"\x1f",
    b"\x7f",
    b"\xff",
    b"\xc3",
    b"\xc3\xa9",
    b"\xe6\x97\xa5",
    b"\xf0\x9f\x98\x80",
    b"\xc0\x80",
    b"\xed\xa0\x80",
    b"\x80",
    b"\xf5\x80\x80\x80",
    b"\\",
    b"\\n",
    b"\\x",
    b"\\u",
    b"\\u00",
    b"\\u00e9",
    b"\\ud800",
    b"\\ud800\\udc00",
    b"\\udc00",
    b"\"",
    b"\\\"",
];

/// Every kind of trouble at every offset of strings around the lane
/// boundaries, and truncated at every point.
#[test]
fn lane_offsets_agree() {
    let mut lens: Vec<usize> = (0..=40).collect();
    lens.extend(58..=70);
    lens.extend([95, 96, 97, 127, 128, 129, 1000]);
    for &len in &lens {
        for bad in BAD {
            for at in (0..len.min(40)).chain(len.saturating_sub(6)..len + 1) {
                same(&string_with(len, at, bad));
            }
        }
        let doc = string_with(len, 0, b"");
        for cut in 0..doc.len() {
            same(&doc[..cut]);
        }
    }
}

#[test]
fn long_strings_agree() {
    let mut rng = Rng(13);
    for k in [0usize, 1, 7, 31, 32, 33, 64, 500] {
        let mut s = String::from("[\"");
        for i in 0..100_000 {
            if k > 0 && i % k == 0 {
                s.push_str(PIECES[rng.below(PIECES.len())]);
            } else {
                s.push('x');
            }
        }
        s.push_str("\"]");
        same(s.as_bytes());
        let mut bytes = s.into_bytes();
        bytes[50_000] = 0x02;
        same(&bytes);
        bytes[40_000] = 0xff;
        same(&bytes);
    }
}

#[test]
fn numbers_and_whitespace_agree() {
    for n in [
        "0",
        "-0",
        "-",
        "1.",
        "1.e5",
        ".5",
        "1e",
        "1e+",
        "1E5",
        "-1.5E-7",
        "01",
        "00",
        "-01",
        "1e5.5",
        "1+1",
        "9".repeat(400).as_str(),
        "+1",
        "1 ",
        " 1",
        "1_",
        "0x10",
        "1e1e1",
        "--1",
        "1..2",
    ] {
        same(n.as_bytes());
        same(std::format!("[{n},{n}]").as_bytes());
        same(std::format!("{{\"a\":{n}}}").as_bytes());
    }
    for run in [0usize, 1, 2, 31, 32, 33, 63, 64, 65, 200] {
        let sp = " ".repeat(run);
        let nl = "\n\t\r ".repeat(run / 4 + 1);
        for w in [&sp, &nl] {
            same(std::format!("{w}[{w}1{w},{w}\"a\"{w}]{w}").as_bytes());
            same(std::format!("{w}{{{w}\"a\"{w}:{w}true{w}}}{w}").as_bytes());
            same(w.as_bytes());
            same(std::format!("{w}\x0b1").as_bytes());
            same(std::format!("[1{w}").as_bytes());
        }
    }
    for lit in [
        "t", "tr", "tru", "true", "trux", "nul", "null", "nulL", "f", "fals", "false",
    ] {
        same(lit.as_bytes());
        same(std::format!("[{lit}]").as_bytes());
    }
    same(b"");
    same(b"]");
    same(b"}");
    same(b",");
    same(b"[1,]");
    same(b"{\"a\":1,}");
    same(b"{\"a\" 1}");
    same(b"{1:2}");
    same(b"[1 2]");
    same(b"\"a\" \"b\"");
}
