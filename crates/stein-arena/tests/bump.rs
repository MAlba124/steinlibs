//! Bump reset, and growing and freeing the newest raw region in place.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::{ArenaError, Bump};

#[test]
fn reset_reuses_the_same_bytes_and_they_read_zero() {
    let mut b = Bump::reserve(1 << 20).unwrap();
    let first = b.alloc_bytes(64).unwrap();
    first.fill(0xAA);
    let addr = first.as_ptr();
    let w = b.zeroed::<u32>(16).unwrap();
    w.fill(u32::MAX);
    b.reset();
    assert!(b.is_empty());
    let again = b.alloc_bytes(64).unwrap();
    assert_eq!(again.as_ptr(), addr, "reset did not rewind");
    assert!(again.iter().all(|&x| x == 0), "reused bytes kept old data");
    let w = b.zeroed::<u32>(16).unwrap();
    assert!(w.iter().all(|&x| x == 0), "reused table kept old data");
    let s = b.alloc_slice::<u16>(8, 7).unwrap();
    assert!(s.iter().all(|&x| x == 7));
}

#[test]
fn a_region_straddling_the_old_high_water_is_zero_throughout() {
    let mut b = Bump::reserve(1 << 20).unwrap();
    b.alloc_bytes(100).unwrap().fill(0xFF);
    b.reset();
    let r = b.alloc_bytes(4096).unwrap();
    assert!(r.iter().all(|&x| x == 0));
}

#[test]
fn raw_allocations_are_aligned_and_disjoint() {
    let b = Bump::reserve(1 << 20).unwrap();
    let a = b.alloc_raw(3, 1).unwrap();
    let c = b.alloc_raw(64, 64).unwrap();
    assert_eq!(c.as_ptr().addr() % 64, 0);
    assert!(c.as_ptr().addr() >= a.as_ptr().addr() + 3);
    let z = b.alloc_raw(8, 0).unwrap();
    assert!(z.as_ptr().addr() >= c.as_ptr().addr() + 64);
}

#[test]
fn only_the_newest_region_grows_in_place() {
    let b = Bump::reserve(1 << 16).unwrap();
    let a = b.alloc_raw(16, 8).unwrap();
    let a_end = a.as_ptr().wrapping_add(16);
    assert!(b.grow_last(a_end, 48), "the newest region should grow");
    assert_eq!(b.len(), a.as_ptr().addr() - base(&b) + 64);
    let c = b.alloc_raw(8, 8).unwrap();
    assert!(
        !b.grow_last(a.as_ptr().wrapping_add(64), 8),
        "an older region grew"
    );
    assert!(b.grow_last(c.as_ptr().wrapping_add(8), 8));
    let left = b.capacity() - b.len();
    assert!(
        !b.grow_last(c.as_ptr().wrapping_add(16), left + 1),
        "grew past the cap"
    );
    assert!(b.grow_last(c.as_ptr().wrapping_add(16), left));
    assert!(matches!(b.alloc_raw(1, 1), Err(ArenaError::Full { .. })));
}

#[test]
fn only_the_newest_region_goes_back() {
    let b = Bump::reserve(1 << 16).unwrap();
    let a = b.alloc_raw(32, 8).unwrap();
    let after_a = b.len();
    let c = b.alloc_raw(32, 8).unwrap();
    let after_c = b.len();
    // SAFETY: neither region is used again.
    unsafe {
        assert!(!b.free_last(a.as_ptr(), 32), "an interior free rewound");
        assert_eq!(b.len(), after_c);
        assert!(b.free_last(c.as_ptr(), 32));
    }
    assert_eq!(b.len(), after_a);
    let d = b.alloc_raw(32, 8).unwrap();
    assert_eq!(d, c, "the freed region was not reused");
}

#[test]
fn a_pointer_outside_the_mapping_is_refused() {
    let b = Bump::reserve(1 << 16).unwrap();
    b.alloc_raw(8, 8).unwrap();
    let other = [0u8; 8];
    assert!(!b.grow_last(other.as_ptr(), 8));
    // SAFETY: refused, nothing is reused.
    assert!(!unsafe { b.free_last(other.as_ptr(), 8) });
    assert!(!unsafe { b.free_last(core::ptr::null(), 8) });
}

/// The mapping's base address.
fn base(b: &Bump) -> usize {
    let len = b.len();
    let p = b.alloc_raw(0, 1).unwrap().as_ptr().addr();
    p - len
}
