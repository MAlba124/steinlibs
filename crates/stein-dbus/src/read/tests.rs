use super::*;
use crate::write::Writer;
use std::vec::Vec;

fn items<'a>(buf: &'a [u8], sig: &'a str) -> Result<Vec<Item<'a>>, WireError> {
    let mut out = Vec::new();
    walk(&mut Reader::new(buf, false), sig.as_bytes(), &mut |i| {
        out.push(i)
    })?;
    Ok(out)
}

#[test]
fn basics_round_trip() {
    let mut buf = [0u8; 128];
    let mut w = Writer::new(&mut buf);
    w.u8(7).unwrap();
    w.bool(true).unwrap();
    w.i16(-2).unwrap();
    w.u64(1 << 40).unwrap();
    w.f64(1.5).unwrap();
    w.str("héllo").unwrap();
    w.path("/org/x").unwrap();
    w.sig("a{sv}").unwrap();
    let n = w.len();
    let got = items(&buf[..n], "ybntdsog").unwrap();
    assert_eq!(
        got,
        [
            Item::Byte(7),
            Item::Bool(true),
            Item::I16(-2),
            Item::U64(1 << 40),
            Item::F64(1.5),
            Item::Str("héllo"),
            Item::Path("/org/x"),
            Item::Sig("a{sv}"),
        ]
    );
}

/// Byte for byte, a `ybu` needs three bytes of padding before the u32.
#[test]
fn layout_matches_the_spec() {
    let mut buf = [0u8; 32];
    let mut w = Writer::new(&mut buf);
    w.u8(1).unwrap();
    w.bool(false).unwrap();
    w.str("ab").unwrap();
    let n = w.len();
    assert_eq!(
        &buf[..n],
        &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, b'a', b'b', 0][..]
    );
}

#[test]
fn a_dict_of_variants_round_trips() {
    let mut buf = [0u8; 256];
    let mut w = Writer::new(&mut buf);
    let a = w.begin_array(b'{').unwrap();
    for (k, v) in [("one", 1u32), ("two", 2)] {
        w.begin_struct().unwrap();
        w.str(k).unwrap();
        w.variant("u").unwrap();
        w.u32(v).unwrap();
    }
    w.begin_struct().unwrap();
    w.str("list").unwrap();
    w.variant("as").unwrap();
    let inner = w.begin_array(b's').unwrap();
    w.str("x").unwrap();
    w.end_array(inner).unwrap();
    w.end_array(a).unwrap();
    let n = w.len();

    let got = items(&buf[..n], "a{sv}").unwrap();
    assert_eq!(got[0], Item::Array(b"{sv}"));
    assert!(got.contains(&Item::Str("two")));
    assert!(got.contains(&Item::U32(2)));
    assert!(got.contains(&Item::Variant("as")));
    assert!(got.contains(&Item::Str("x")));
    assert_eq!(got.last(), Some(&Item::ArrayEnd));
}

/// An empty array of 8 aligned elements still pads to 8.
#[test]
fn an_empty_array_still_pads() {
    let mut buf = [0u8; 32];
    let mut w = Writer::new(&mut buf);
    let a = w.begin_array(b'(').unwrap();
    w.end_array(a).unwrap();
    assert_eq!(w.len(), 8);
    let got = items(&buf[..8], "a(ii)").unwrap();
    assert_eq!(got, [Item::Array(b"(ii)"), Item::ArrayEnd]);
}

#[test]
fn big_endian_reads() {
    let buf = [0, 0, 0, 5, 0, 0, 0, 2, b'h', b'i', 0];
    let mut r = Reader::new(&buf, true);
    assert_eq!(r.u32(), Ok(5));
    assert_eq!(r.str(), Ok("hi"));
}

#[test]
fn malformed_input_is_refused() {
    // Nonzero padding.
    assert_eq!(
        items(&[1, 9, 0, 0, 0, 0, 0, 0], "yu"),
        Err(WireError::Padding)
    );
    // Boolean 2.
    assert_eq!(items(&[2, 0, 0, 0], "b"), Err(WireError::Bool(2)));
    // String without its NUL.
    assert_eq!(items(&[1, 0, 0, 0, b'a', b'b'], "s"), Err(WireError::Str));
    // String with a NUL inside.
    assert_eq!(items(&[1, 0, 0, 0, 0, 0], "s"), Err(WireError::Str));
    // Length past the end.
    assert_eq!(items(&[9, 0, 0, 0, b'a'], "s"), Err(WireError::Truncated));
    // Leftover bytes.
    assert_eq!(items(&[1, 2], "y"), Err(WireError::Trailing));
    // Not a path.
    assert_eq!(items(&[1, 0, 0, 0, b'x', 0], "o"), Err(WireError::Path));
}

#[test]
fn an_element_past_the_array_is_refused() {
    // Array claims 2 bytes, holds a u32.
    let buf = [2, 0, 0, 0, 1, 0, 0, 0];
    assert_eq!(items(&buf, "au"), Err(WireError::ArrayOverrun));
}

#[test]
fn a_huge_array_length_is_refused() {
    let buf = (1u32 << 27).to_le_bytes();
    assert_eq!(items(&buf, "ay"), Err(WireError::ArrayTooLong(1 << 27)));
}

/// Variants nest without a signature saying so, so depth is counted as
/// they are read.
#[test]
fn variants_nested_too_deep_are_refused() {
    let mut buf = [0u8; 512];
    let mut w = Writer::new(&mut buf);
    for _ in 0..70 {
        w.variant("v").unwrap();
    }
    w.variant("y").unwrap();
    w.u8(1).unwrap();
    let n = w.len();
    assert_eq!(items(&buf[..n], "v"), Err(WireError::TooDeep));
}

#[test]
fn skip_steps_over_one_value() {
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    let a = w.begin_array(b's').unwrap();
    w.str("a").unwrap();
    w.str("b").unwrap();
    w.end_array(a).unwrap();
    w.u32(42).unwrap();
    let n = w.len();
    let mut r = Reader::new(&buf[..n], false);
    skip(&mut r, b"as").unwrap();
    assert_eq!(r.u32(), Ok(42));
}

#[test]
fn writer_refuses_what_it_cannot_hold() {
    let mut buf = [0u8; 6];
    let mut w = Writer::new(&mut buf);
    assert_eq!(w.str("abc"), Err(WireError::Full));
    let mut w = Writer::new(&mut buf);
    assert_eq!(w.str("a\0b"), Err(WireError::Str));
    assert_eq!(w.path("nope"), Err(WireError::Path));
    assert!(matches!(w.sig("a"), Err(WireError::Sig(_))));
    assert!(matches!(w.variant("ss"), Err(WireError::Sig(_))));
}
