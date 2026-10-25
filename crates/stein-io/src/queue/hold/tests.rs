#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn holds(entries: u16) -> Holds {
    let mut h = Holds::new();
    h.reset(entries);
    h
}

#[test]
fn nothing_is_held_before_a_reset() {
    let mut h = Holds::new();
    assert!(!h.insert(0, 0x1000, 8), "an empty table took a buffer");
    assert!(!h.remove(0));
    assert_eq!(h.hit(0, usize::MAX), None);
}

#[test]
fn buffers_come_back_in_any_order() {
    let mut h = holds(4);
    for bid in 0..4u16 {
        assert!(h.insert(bid, 0x1000 * (bid as usize + 1), 16));
    }
    assert_eq!(h.len(), 4);
    // Handed back out of ring order, as two multishot ops may.
    assert!(h.remove(2));
    assert!(h.remove(0));
    assert_eq!(h.hit(0x1000, 0x1010), None, "bid 0 still held");
    assert_eq!(h.hit(0x2000, 0x2001), Some((0x2000, 16)), "bid 1 lost");
    assert_eq!(h.hit(0x4000, 0x4001), Some((0x4000, 16)), "bid 3 lost");
    assert!(!h.remove(2), "handed back twice");
    assert_eq!(h.len(), 2);
}

#[test]
fn a_held_bid_is_refused_again() {
    let mut h = holds(4);
    assert!(h.insert(7, 0x1000, 8));
    assert!(!h.insert(7, 0x2000, 8), "one bid named two buffers");
    assert!(h.remove(7));
    assert!(h.insert(7, 0x2000, 8));
}

#[test]
fn a_reset_forgets_and_keeps_its_room() {
    let mut h = holds(8);
    for bid in 0..8u16 {
        assert!(h.insert(bid, 0x100 * (bid as usize + 1), 1));
    }
    h.reset(2);
    assert_eq!(h.len(), 0);
    assert_eq!(h.hit(0, usize::MAX), None);
    assert!(h.insert(9, 0x100, 1));
    assert!(h.insert(10, 0x200, 1));
    h.clear();
    assert_eq!(h.hit(0, usize::MAX), None);
}

/// Random inserts and removes against a plain list, so backward shifts never lose a bid.
#[test]
fn it_matches_a_plain_list() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for entries in [1u16, 2, 8, 64] {
        let mut h = holds(entries);
        let mut model: Vec<u16> = Vec::new();
        for _ in 0..20_000 {
            let r = next();
            // Bids clustered so they collide in the table.
            let bid = ((r >> 8) % 96) as u16 * 64;
            if r & 1 == 0 && model.len() < entries as usize {
                let fresh = !model.contains(&bid);
                assert_eq!(h.insert(bid, bid as usize + 1, 1), fresh);
                if fresh {
                    model.push(bid);
                }
            } else {
                let had = model.iter().position(|b| *b == bid);
                assert_eq!(h.remove(bid), had.is_some());
                if let Some(i) = had {
                    model.swap_remove(i);
                }
            }
            assert_eq!(h.len(), model.len());
        }
        for b in &model {
            let at = *b as usize + 1;
            assert!(h.hit(at, at + 1).is_some(), "bid {b} lost");
        }
    }
}
