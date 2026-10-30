use super::*;
use crate::write::Writer;
use std::vec::Vec;

/// `a{sv}` with a string, a bool, a nested variant, an `as` and a dict,
/// then a u32 after it.
fn sample(buf: &mut [u8]) -> usize {
    let mut w = Writer::new(buf);
    let a = w.begin_array(b'{').unwrap();
    w.sv("Title", "s", |w| w.str("Network")).unwrap();
    w.sv("ItemIsMenu", "b", |w| w.bool(true)).unwrap();
    w.sv("Wrapped", "v", |w| {
        w.variant("i")?;
        w.i32(-7)
    })
    .unwrap();
    w.sv("Names", "as", |w| {
        let a = w.begin_array(b's')?;
        w.str("a")?;
        w.str("b")?;
        w.end_array(a)
    })
    .unwrap();
    w.sv("Inner", "a{sv}", |w| {
        let a = w.begin_array(b'{')?;
        w.sv("k", "u", |w| w.u32(9))?;
        w.end_array(a)
    })
    .unwrap();
    w.end_array(a).unwrap();
    w.u32(0xfeed).unwrap();
    w.len()
}

fn dict(buf: &[u8]) -> Dict<'_> {
    Dict::read(&mut Reader::new(buf, false)).unwrap()
}

#[test]
fn a_dict_reads_back_typed() {
    let mut buf = [0u8; 512];
    let n = sample(&mut buf);
    let b = &buf[..n];
    let keys: Vec<&str> = dict(b).map(|(k, _)| k).collect();
    assert_eq!(keys, ["Title", "ItemIsMenu", "Wrapped", "Names", "Inner"]);
    let title = dict(b).get("Title").unwrap();
    assert_eq!(title.str(), Some("Network"));
    assert_eq!(title.bool(), None, "the wrong type reads as None");
    assert_eq!(dict(b).get("ItemIsMenu").and_then(|v| v.bool()), Some(true));
    let wrapped = dict(b).get("Wrapped").and_then(|v| v.inner());
    assert_eq!(wrapped.and_then(|v| v.i32()), Some(-7));
    let names: Vec<&str> = dict(b)
        .get("Names")
        .and_then(|v| v.strs())
        .unwrap()
        .collect();
    assert_eq!(names, ["a", "b"]);
    let inner = dict(b).get("Inner").and_then(|v| v.dict()).unwrap();
    assert_eq!(inner.get("k").and_then(|v| v.u32()), Some(9));
    assert_eq!(dict(b).get("Missing").map(|v| v.sig), None);
}

#[test]
fn reading_a_dict_moves_past_it() {
    let mut buf = [0u8; 512];
    let n = sample(&mut buf);
    let mut r = Reader::new(&buf[..n], false);
    Dict::read(&mut r).unwrap();
    assert_eq!(r.u32(), Ok(0xfeed));
}

#[test]
fn a_truncated_dict_is_refused() {
    let mut buf = [0u8; 512];
    sample(&mut buf);
    // The array's length still claims all of it.
    let mut r = Reader::new(&buf[..40], false);
    assert!(Dict::read(&mut r).is_err());
}

#[test]
fn a_bare_string_array_reads_and_moves_past() {
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    let a = w.begin_array(b's').unwrap();
    w.str("org.mpris.MediaPlayer2.x").unwrap();
    w.str(":1.4").unwrap();
    w.end_array(a).unwrap();
    w.u32(5).unwrap();
    let n = w.len();
    let mut r = Reader::new(&buf[..n], false);
    let s = Strs::read(&mut r).unwrap();
    assert_eq!(s.collect::<Vec<_>>(), ["org.mpris.MediaPlayer2.x", ":1.4"]);
    assert_eq!(r.u32().unwrap(), 5);
}
