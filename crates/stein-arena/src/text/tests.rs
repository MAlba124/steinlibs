//! Only checked bytes read as text, and nothing moves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::Text;

#[test]
fn committed_bytes_read_as_text() {
    let mut t = Text::reserve(1 << 20).unwrap();
    t.push(b"hello ").unwrap();
    assert_eq!(t.as_str(), "", "pending until committed");
    assert_eq!(t.pending(), b"hello ");
    t.commit().unwrap();
    t.push("w\u{f6}rld".as_bytes()).unwrap();
    t.commit().unwrap();
    assert_eq!(t.as_str(), "hello w\u{f6}rld");
    assert!(t.pending().is_empty());
}

#[test]
fn bad_bytes_stay_pending_and_discard() {
    let mut t = Text::reserve(1 << 16).unwrap();
    t.push(b"ok\n").unwrap();
    t.commit().unwrap();
    t.push(b"half \xc3").unwrap();
    assert!(t.commit().is_err());
    assert_eq!(t.as_str(), "ok\n");
    // Rest of the char arrives.
    t.push(b"\xa9\n").unwrap();
    t.commit().unwrap();
    assert_eq!(t.as_str(), "ok\nhalf \u{e9}\n");
    t.push(b"\xff\xfe").unwrap();
    assert!(t.commit().is_err());
    t.discard();
    assert!(t.pending().is_empty());
    assert_eq!(t.len(), "ok\nhalf \u{e9}\n".len());
}

#[test]
fn truncating_keeps_char_boundaries() {
    let mut t = Text::reserve(4096).unwrap();
    t.push("a\u{e9}b".as_bytes()).unwrap();
    t.commit().unwrap();
    assert!(!t.truncate(2), "inside the two byte char");
    assert_eq!(t.as_str(), "a\u{e9}b");
    assert!(t.truncate(1));
    assert_eq!(t.as_str(), "a");
    t.push(b"z").unwrap();
    t.commit().unwrap();
    assert_eq!(t.as_str(), "az");
}

#[test]
fn growing_never_moves_the_text() {
    let mut t = Text::reserve(64 << 20).unwrap();
    t.push(b"first").unwrap();
    t.commit().unwrap();
    let at = t.as_str().as_ptr();
    for _ in 0..10_000 {
        t.push(b"some line of text\n").unwrap();
        t.commit().unwrap();
    }
    assert_eq!(t.as_str().as_ptr(), at);
    assert!(t.as_str().starts_with("first"));
}

#[test]
fn the_ceiling_is_an_error() {
    let mut t = Text::reserve(4096).unwrap();
    let cap = t.capacity();
    t.push(&vec![b'a'; cap]).unwrap();
    assert!(t.push(b"b").is_err());
    t.commit().unwrap();
    assert_eq!(t.len(), cap);
}
