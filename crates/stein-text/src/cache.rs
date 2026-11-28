//! Open-addressed index from (glyph id, pixel size) to a row in the glyph table.
//!
//! `std::collections::HashMap` would do the job, but lookup sits on the per-frame
//! path and SipHash plus a `RandomState` is more machinery than a pair of `u32`s
//! deserve. This is a power-of-two table with linear probing and a multiply-shift
//! finaliser: one cache line touched on the common hit.
//!
//! Slots are `u64` with 0 meaning empty, so the stored key is the real key plus
//! one. `u64::MAX` is therefore not a usable key, which no real (gid, size) pair
//! reaches.

/// Hit and miss counters for the glyph cache.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct CacheStats {
    /// Lookups answered from the glyph table.
    pub hits: u64,
    /// Lookups that had to rasterize.
    pub misses: u64,
    /// Glyphs rasterized. Equal to `misses` unless rasterization failed.
    pub rasterized: u64,
}

impl CacheStats {
    /// Fraction of lookups served without rasterizing. Zero lookups reads as 0.
    #[must_use]
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f32 / total as f32
    }
}

/// Packs a face, a glyph id and a pixel size into one cache key.
///
/// The face matters because one atlas holds glyphs from several fonts: with
/// fallback, glyph 42 of the primary and glyph 42 of a fallback are different
/// pictures, and a key that could not tell them apart would serve whichever
/// was rasterised first for both.
///
/// A glyph id is sixteen bits in every OpenType font, so it is masked to that
/// and the face gets the room above it. Eight bits of face is far more fonts
/// than any fallback chain has.
#[must_use]
#[inline]
pub fn glyph_key(face: u32, glyph_id: u32, px_bits: u32) -> u64 {
    let face = u64::from(face & 0xff) << 48;
    let gid = u64::from(glyph_id & 0xffff) << 32;
    face | gid | u64::from(px_bits)
}

/// Quantises a pixel size to the bit pattern used in a cache key.
///
/// Sizes are rounded to 1/64 px before hashing so that two nominally equal sizes
/// that differ in the last float bit still share a cache entry.
#[must_use]
#[inline]
pub fn size_bits(px_per_em: f32) -> u32 {
    let q = (px_per_em * 64.0).round();
    if q.is_finite() && q > 0.0 {
        q as u32
    } else {
        0
    }
}

pub struct KeyMap {
    slots: Vec<u64>,
    values: Vec<u32>,
    len: usize,
}

impl KeyMap {
    /// Rounds `capacity` up to a power of two with room to stay under the load
    /// factor. Allocates once here and again only if the table outgrows it.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let want = (capacity.max(1) * 2).next_power_of_two();
        Self {
            slots: vec![0; want],
            values: vec![0; want],
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn clear(&mut self) {
        self.slots.fill(0);
        self.len = 0;
    }

    #[must_use]
    pub fn get(&self, key: u64) -> Option<u32> {
        let (i, found) = probe(&self.slots, key);
        found.then(|| self.values[i])
    }

    /// Inserts or overwrites. Grows when the table passes three quarters full,
    /// which only happens on a cache miss.
    pub fn insert(&mut self, key: u64, value: u32) {
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        if self.put(key, value) {
            self.len += 1;
        }
    }

    /// Returns true when a fresh slot was taken.
    fn put(&mut self, key: u64, value: u32) -> bool {
        let (i, found) = probe(&self.slots, key);
        self.slots[i] = key.wrapping_add(1);
        self.values[i] = value;
        !found
    }

    fn grow(&mut self) {
        let want = self.slots.len() * 2;
        let old_slots = core::mem::replace(&mut self.slots, vec![0; want]);
        let old_values = core::mem::replace(&mut self.values, vec![0; want]);
        let live = old_slots.iter().zip(&old_values).filter(|(s, _)| **s != 0);
        for (s, v) in live {
            self.put(s.wrapping_sub(1), *v);
        }
    }
}

/// Linear probe for `key`. Returns the slot it belongs in and whether that slot
/// already holds it. The table is never full, so the walk always terminates.
#[inline]
fn probe(slots: &[u64], key: u64) -> (usize, bool) {
    let stored = key.wrapping_add(1);
    let mask = slots.len() - 1;
    let mut i = (mix(key) as usize) & mask;
    while slots[i] != 0 && slots[i] != stored {
        i = (i + 1) & mask;
    }
    (i, slots[i] == stored)
}

/// The murmur3 64-bit finaliser. Cheap, and good enough to spread the low bits
/// of a glyph id across the table.
#[inline]
fn mix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^ (k >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_many_keys() {
        let mut m = KeyMap::with_capacity(8);
        for i in 0..1000u32 {
            m.insert(glyph_key(0, i, 1024), i * 3);
        }
        assert_eq!(m.len(), 1000);
        for i in 0..1000u32 {
            assert_eq!(m.get(glyph_key(0, i, 1024)), Some(i * 3));
        }
        assert_eq!(m.get(glyph_key(0, 1000, 1024)), None);
    }

    #[test]
    fn size_separates_entries() {
        let mut m = KeyMap::with_capacity(4);
        m.insert(glyph_key(0, 7, size_bits(12.0)), 1);
        m.insert(glyph_key(0, 7, size_bits(24.0)), 2);
        assert_eq!(m.get(glyph_key(0, 7, size_bits(12.0))), Some(1));
        assert_eq!(m.get(glyph_key(0, 7, size_bits(24.0))), Some(2));
    }

    #[test]
    fn reinsert_overwrites_without_growing_len() {
        let mut m = KeyMap::with_capacity(4);
        m.insert(glyph_key(0, 1, 64), 10);
        m.insert(glyph_key(0, 1, 64), 11);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(glyph_key(0, 1, 64)), Some(11));
    }

    #[test]
    fn hit_rate_is_zero_without_lookups() {
        assert_eq!(CacheStats::default().hit_rate(), 0.0);
        let s = CacheStats {
            hits: 3,
            misses: 1,
            rasterized: 1,
        };
        assert!((s.hit_rate() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn a_warm_table_never_grows() {
        let mut m = KeyMap::with_capacity(256);
        for i in 0..128u32 {
            m.insert(glyph_key(0, i, 1024), i);
        }
        let cap = m.capacity();
        for _ in 0..500 {
            let all = (0..128u32).all(|i| m.get(glyph_key(0, i, 1024)) == Some(i));
            assert!(all, "a warm lookup missed");
        }
        assert_eq!(cap, m.capacity());
    }
}
