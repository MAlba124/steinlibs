use super::*;

#[test]
fn a_removed_handle_is_stale_even_after_reuse() {
    let mut t = Slots::new(4, "thing");
    let a = t.insert(1u32).unwrap();
    assert_eq!(t.remove(a), Ok(1));
    assert_eq!(t.get(a), Err(GpuError::Stale("thing")));
    let b = t.insert(2).unwrap();
    assert_eq!(b.index(), a.index(), "the slot is reused");
    assert_eq!(t.get(a), Err(GpuError::Stale("thing")));
    assert_eq!(t.get(b), Ok(&2));
    assert_eq!(t.remove(a), Err(GpuError::Stale("thing")));
}

#[test]
fn capacity_is_a_ceiling_and_frees_make_room() {
    let mut t = Slots::new(2, "thing");
    let a = t.insert(1u32).unwrap();
    t.insert(2).unwrap();
    assert_eq!(t.insert(3), Err(GpuError::Full("thing")));
    t.remove(a).unwrap();
    assert!(t.insert(3).is_ok());
    assert_eq!(t.iter().copied().sum::<u32>(), 5);
}

#[test]
fn generations_wrap_without_reviving_old_handles() {
    let mut t = Slots::new(1, "thing");
    let first = t.insert(0u32).unwrap();
    let mut h = first;
    for i in 0..255u32 {
        t.remove(h).unwrap();
        h = t.insert(i).unwrap();
    }
    // 255 bumps later the generation is one short of wrapping to `first`.
    assert_eq!(t.get(first), Err(GpuError::Stale("thing")));
}

#[test]
fn drain_hands_back_the_live_items() {
    let mut t = Slots::new(4, "thing");
    let a = t.insert(1u32).unwrap();
    t.insert(2).unwrap();
    t.insert(3).unwrap();
    t.remove(a).unwrap();
    let mut left: Vec<u32> = t.drain().collect();
    left.sort_unstable();
    assert_eq!(left, [2, 3]);
    assert_eq!(t.iter().count(), 0);
}
