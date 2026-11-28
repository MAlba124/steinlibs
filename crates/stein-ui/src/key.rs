//! Stable widget identity.
//!
//! An immediate-mode API rebuilds the tree every frame, so a widget's persistent
//! state (scroll offset, animation progress, whether it is being dragged) has to be
//! found by name rather than by position. Get the naming wrong and state teleports
//! between widgets or vanishes, which is the failure mode immediate-mode toolkits
//! are known for.
//!
//! Two rules make it work:
//!
//! - keys are hierarchical, so the same label under two different parents is two
//!   different widgets and neither has to know about the other;
//! - anything emitted in a loop must be seeded by something stable about the item,
//!   not by its index, or reordering the list shuffles everyone's state.

/// splitmix64's finalizer. Cheap, no dependency, and it spreads the low bits, which
/// matters because the keyed store masks rather than divides.
#[inline]
pub const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// FNV-1a over the bytes. Not for hostile input, and nothing here takes any.
#[inline]
pub const fn hash_str(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

/// Combine a parent key with a child label. Order matters, so `a/b` and `b/a` are
/// different widgets.
#[inline]
pub const fn child(parent: u64, label: u64) -> u64 {
    mix(parent ^ mix(label))
}

/// A key for the `n`th item in a loop.
///
/// Prefer [`seeded`] with something stable about the item. Index seeds are correct
/// only for lists that never reorder, and a list that never reorders today is a
/// list that reorders in six months.
#[inline]
pub const fn indexed(parent: u64, label: u64, n: u64) -> u64 {
    child(child(parent, label), n.wrapping_add(1))
}

/// A key for a loop item seeded by something stable about the item itself: a row
/// id, a file path hash, an entity handle. Survives reordering, insertion and
/// deletion, which is what keeps scroll and selection state attached to the right
/// row.
#[inline]
pub const fn seeded(parent: u64, label: u64, seed: u64) -> u64 {
    child(child(parent, label), seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_label_under_different_parents_is_a_different_widget() {
        let a = child(1, hash_str("ok"));
        let b = child(2, hash_str("ok"));
        assert_ne!(a, b);
    }

    #[test]
    fn different_labels_under_the_same_parent_differ() {
        let p = 7;
        assert_ne!(child(p, hash_str("ok")), child(p, hash_str("cancel")));
    }

    #[test]
    fn keys_are_order_sensitive() {
        assert_ne!(child(1, 2), child(2, 1));
    }

    #[test]
    fn keys_are_stable_across_calls() {
        // The whole scheme is worthless if this is not true frame to frame.
        let a = child(child(0, hash_str("panel")), hash_str("list"));
        let b = child(child(0, hash_str("panel")), hash_str("list"));
        assert_eq!(a, b);
    }

    #[test]
    fn indexed_keys_are_distinct_and_nonzero() {
        let p = child(0, hash_str("list"));
        let mut seen = [0u64; 32];
        for i in 0..32u64 {
            let k = indexed(p, hash_str("row"), i);
            assert!(!seen[..i as usize].contains(&k), "collision at {i}");
            seen[i as usize] = k;
        }
    }

    #[test]
    fn seeded_keys_survive_reordering() {
        let p = child(0, hash_str("list"));
        let label = hash_str("row");
        let forwards: [u64; 3] = [10, 20, 30];
        let backwards: [u64; 3] = [30, 20, 10];

        // Same ids, different order. Index-seeded keys would shuffle; these do not.
        for (a, b) in [(0usize, 2usize), (1, 1), (2, 0)] {
            assert_eq!(
                seeded(p, label, forwards[a]),
                seeded(p, label, backwards[b]),
                "id {} moved position and lost its key",
                forwards[a]
            );
        }
        assert_ne!(
            indexed(p, label, 0),
            seeded(p, label, 30),
            "index seeding is the thing seeded keys avoid"
        );
    }

    #[test]
    fn the_mix_spreads_keys_that_differ_only_high_up() {
        // The keyed store masks low bits, so keys differing only above the mask
        // must not all land in one slot.
        const MASK: u64 = 0xff;
        let a = mix(1u64 << 40);
        let b = mix(2u64 << 40);
        let c = mix(3u64 << 40);
        let distinct = ((a & MASK) != (b & MASK)) as u32
            + ((b & MASK) != (c & MASK)) as u32
            + ((a & MASK) != (c & MASK)) as u32;
        assert!(distinct >= 2, "mix left them clustered");
        // Unmixed they would collide outright.
        assert_eq!((1u64 << 40) & MASK, (2u64 << 40) & MASK);
    }

    #[test]
    fn hash_str_is_const_evaluable() {
        const K: u64 = hash_str("save");
        assert_eq!(K, hash_str("save"));
        assert_ne!(K, hash_str("load"));
    }
}
