#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn a_default_reservation_is_cheap_and_usable() {
    let mut a = Arena::new().expect("reserve");
    assert_eq!(a.capacity(), DEFAULT_CAP);
    assert!(a.is_empty());
    let s = a.push_bytes(b"hello").expect("alloc");
    assert_eq!(a.bytes(s), b"hello");
}

#[test]
fn allocations_respect_alignment() {
    let mut a = Arena::reserve(1 << 20).expect("reserve");
    a.push_bytes(b"x").expect("one byte");
    let s = a.alloc(16, 8).expect("aligned");
    assert_eq!(s.at % 8, 0);
    let s = a.alloc(1, 64).expect("aligned");
    assert_eq!(s.at % 64, 0);
}

#[test]
fn the_capacity_is_a_real_ceiling() {
    let mut a = Arena::reserve(1 << 16).expect("reserve");
    let cap = a.capacity();
    a.alloc(cap - 8, 1).expect("almost all of it");
    let e = a.alloc(64, 1);
    assert!(
        matches!(e, Err(ArenaError::Full { asked: 64, .. })),
        "{e:?}"
    );
    a.alloc(8, 1).expect("the tail is still usable");
}

#[test]
fn spans_survive_later_allocations() {
    let mut a = Arena::reserve(1 << 24).expect("reserve");
    let first = a.push_bytes(b"stable").expect("first");
    for i in 0..10_000u32 {
        a.push_bytes(&i.to_ne_bytes()).expect("more");
    }
    assert_eq!(a.bytes(first), b"stable", "the base moved");
}

#[test]
fn a_stale_span_reads_empty_not_wild() {
    let mut a = Arena::reserve(1 << 16).expect("reserve");
    let s = a.push_bytes(b"gone").expect("alloc");
    a.reset();
    assert_eq!(a.bytes(s), b"", "a span past the cursor leaked bytes");
    assert_eq!(
        a.bytes(Span {
            at: usize::MAX,
            len: 2
        }),
        b""
    );
}

#[test]
fn reset_reuses_the_same_addresses() {
    let mut a = Arena::reserve(1 << 20).expect("reserve");
    let s1 = a.push_bytes(b"one").expect("alloc");
    a.reset();
    let s2 = a.push_bytes(b"two").expect("alloc");
    assert_eq!(s1.at, s2.at);
    assert_eq!(a.bytes(s2), b"two");
}

#[test]
fn shrink_keeps_the_kept_prefix_readable() {
    let mut a = Arena::reserve(1 << 20).expect("reserve");
    let keep = a.push_bytes(&[7u8; 4096]).expect("head");
    a.alloc(1 << 19, 1).expect("bulk");
    a.shrink(8192);
    assert!(a.bytes(keep).iter().all(|b| *b == 7), "kept bytes changed");
    a.push_bytes(b"after").expect("still allocates");
}

#[test]
fn zero_capacity_is_refused() {
    assert!(matches!(Arena::reserve(0), Err(ArenaError::BadCap(0))));
}
