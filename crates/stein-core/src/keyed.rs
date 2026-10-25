//! Widget state that outlives the frame that built it, pruned by last-touched frame.
//!
//! ```
//! use stein_core::keyed::KeyedStore;
//!
//! let mut keys = [0u64; 8];
//! let mut values: [Option<f32>; 8] = [const { None }; 8];
//! let mut touched = [0u32; 8];
//! let mut store = KeyedStore::new(&mut keys, &mut values, &mut touched)
//!     .expect("8 is a power of two");
//!
//! // Frame 0. The scroll view exists, so it asks for its offset and scrolls.
//! *store.get_or_insert_with(0xfeed, 0, || 0.0).expect("room") += 12.0;
//! assert_eq!(store.get(0xfeed), Some(&12.0));
//!
//! // Thirty frames later it has not been built once. Its state goes with it.
//! store.prune(30, 4);
//! assert!(store.is_empty());
//! ```

/// Smallest capacity with a usable slot under the load ceiling.
pub const MIN_CAPACITY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyedError {
    /// The three backing slices differ in length.
    LengthMismatch {
        keys: usize,
        values: usize,
        last_touched: usize,
    },
    /// Capacity must be a power of two so probing can mask.
    NotPowerOfTwo(usize),
    /// Below [`MIN_CAPACITY`], where the load ceiling leaves no usable slot.
    TooSmall(usize),
    /// The live count is a `u32`.
    TooManySlots(usize),
}

/// Frame-pruned state keyed by a caller-supplied hash, over parallel borrowed columns.
///
/// Key 0 is ordinary. Occupancy is `values[i].is_some()`.
pub struct KeyedStore<'a, T> {
    keys: &'a mut [u64],
    values: &'a mut [Option<T>],
    /// Frame each entry was last reached on. Meaningless where the value is `None`.
    last_touched: &'a mut [u32],
    live: u32,
}

/// Where a probe came to rest.
enum Probe {
    Hit(usize),
    /// Absent, would go here.
    Free(usize),
    /// No match or gap in the whole table. Unreachable under the load ceiling.
    Wrapped,
}

/// splitmix64 finalizer. Caller hashes may be weak in the low bits masking uses.
const fn mix(key: u64) -> u64 {
    let mut z = key;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl<'a, T> KeyedStore<'a, T> {
    /// Take the storage and reset it to empty, discarding prior contents.
    pub fn new(
        keys: &'a mut [u64],
        values: &'a mut [Option<T>],
        last_touched: &'a mut [u32],
    ) -> Result<Self, KeyedError> {
        if keys.len() != values.len() || keys.len() != last_touched.len() {
            return Err(KeyedError::LengthMismatch {
                keys: keys.len(),
                values: values.len(),
                last_touched: last_touched.len(),
            });
        }
        if !keys.len().is_power_of_two() {
            return Err(KeyedError::NotPowerOfTwo(keys.len()));
        }
        if keys.len() < MIN_CAPACITY {
            return Err(KeyedError::TooSmall(keys.len()));
        }
        if keys.len() > u32::MAX as usize {
            return Err(KeyedError::TooManySlots(keys.len()));
        }
        for k in keys.iter_mut() {
            *k = 0;
        }
        for v in values.iter_mut() {
            *v = None;
        }
        for t in last_touched.iter_mut() {
            *t = 0;
        }
        Ok(KeyedStore {
            keys,
            values,
            last_touched,
            live: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.live as usize
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn capacity(&self) -> usize {
        self.keys.len()
    }

    /// Insert ceiling, 3/4 of capacity. Exact since capacity is a power of two >= 4.
    pub fn load_limit(&self) -> usize {
        self.keys.len() / 4 * 3
    }

    /// Walk `key`'s chain from home to a hit or the first empty slot.
    fn probe(&self, key: u64) -> Probe {
        let mask = self.keys.len() - 1;
        let mut i = (mix(key) as usize) & mask;
        for _ in 0..self.keys.len() {
            if self.values[i].is_none() {
                return Probe::Free(i);
            }
            if self.keys[i] == key {
                return Probe::Hit(i);
            }
            i = (i + 1) & mask;
        }
        Probe::Wrapped
    }

    pub fn get(&self, key: u64) -> Option<&T> {
        match self.probe(key) {
            Probe::Hit(i) => self.values[i].as_ref(),
            _ => None,
        }
    }

    /// Does not stamp the frame, so peeking does not keep an entry alive.
    pub fn get_mut(&mut self, key: u64) -> Option<&mut T> {
        match self.probe(key) {
            Probe::Hit(i) => self.values[i].as_mut(),
            _ => None,
        }
    }

    /// Fetch or create `key` and stamp it with `frame`. `None` for a new key at the
    /// load ceiling.
    pub fn get_or_insert_with(
        &mut self,
        key: u64,
        frame: u32,
        f: impl FnOnce() -> T,
    ) -> Option<&mut T> {
        let slot = match self.probe(key) {
            Probe::Hit(i) => i,
            Probe::Free(i) => {
                if self.live as usize + 1 > self.load_limit() {
                    return None;
                }
                self.keys[i] = key;
                self.values[i] = Some(f());
                self.live += 1;
                i
            }
            Probe::Wrapped => return None,
        };
        self.last_touched[slot] = frame;
        self.values[slot].as_mut()
    }

    pub fn remove(&mut self, key: u64) -> Option<T> {
        match self.probe(key) {
            Probe::Hit(i) => self.remove_at(i),
            _ => None,
        }
    }

    /// Clear slot `i` and backward-shift its chain over the hole. No tombstones.
    fn remove_at(&mut self, i: usize) -> Option<T> {
        let mask = self.keys.len() - 1;
        let taken = self.values[i].take();
        self.live -= 1;

        let mut hole = i;
        let mut j = (i + 1) & mask;
        while self.values[j].is_some() {
            let home = (mix(self.keys[j]) as usize) & mask;
            // Shift j only if the hole is on j's chain from home, else j is unreachable.
            if (j.wrapping_sub(home) & mask) >= (j.wrapping_sub(hole) & mask) {
                self.keys[hole] = self.keys[j];
                self.values[hole] = self.values[j].take();
                self.last_touched[hole] = self.last_touched[j];
                hole = j;
            }
            j = (j + 1) & mask;
        }
        // Cosmetic, keeps stale keys out of a debugger view.
        self.keys[hole] = 0;
        self.last_touched[hole] = 0;
        taken
    }

    /// Evict entries older than `max_age` frames, return the count. Inclusive, and
    /// stamps ahead of `frame` survive.
    pub fn prune(&mut self, frame: u32, max_age: u32) -> usize {
        let mut evicted = 0;
        let mut i = 0;
        while i < self.keys.len() {
            let stale =
                self.values[i].is_some() && frame.saturating_sub(self.last_touched[i]) > max_age;
            if stale {
                self.remove_at(i);
                evicted += 1;
                // Do not advance. The shift may pull an unchecked entry into slot i.
                continue;
            }
            i += 1;
        }
        evicted
    }

    /// Incremental [`KeyedStore::prune`] over at most `budget` slots from `cursor`.
    /// Returns evicted count and resume cursor. Entries may outlive `max_age` by one sweep.
    pub fn prune_from(
        &mut self,
        frame: u32,
        max_age: u32,
        cursor: usize,
        budget: usize,
    ) -> (usize, usize) {
        let cap = self.keys.len();
        if cap == 0 || budget == 0 {
            return (0, cursor);
        }
        let mut i = cursor % cap;
        let mut evicted = 0;
        for _ in 0..budget {
            let stale =
                self.values[i].is_some() && frame.saturating_sub(self.last_touched[i]) > max_age;
            if stale {
                self.remove_at(i);
                evicted += 1;
                // Do not advance, as in `prune`.
                continue;
            }
            i = (i + 1) % cap;
        }
        (evicted, i)
    }

    /// Live entries in slot order. Stable for a given table state, not insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &T)> {
        let keys = &*self.keys;
        self.values
            .iter()
            .enumerate()
            .filter_map(move |(i, v)| v.as_ref().map(|v| (keys[i], v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stack backing storage for a test table.
    macro_rules! store {
        ($t:ty, $n:expr) => {{
            let values: [Option<$t>; $n] = [const { None }; $n];
            ([0u64; $n], values, [0u32; $n])
        }};
    }

    /// Fill `out` with distinct keys homed at `slot` in a table of `cap`, via the real finalizer.
    fn keys_at(slot: usize, cap: usize, out: &mut [u64]) {
        let mask = (cap - 1) as u64;
        let mut k = 1u64;
        let mut n = 0;
        while n < out.len() && k < 1_000_000 {
            if (mix(k) & mask) as usize == slot {
                out[n] = k;
                n += 1;
            }
            k += 1;
        }
        assert_eq!(
            n,
            out.len(),
            "could not find {} keys for slot {slot}",
            out.len()
        );
    }

    fn home(key: u64, cap: usize) -> usize {
        (mix(key) as usize) & (cap - 1)
    }

    #[test]
    fn insert_and_retrieve() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        assert_eq!(s.get_or_insert_with(0xabc, 0, || 7), Some(&mut 7));
        assert_eq!(s.get(0xabc), Some(&7));
        assert_eq!(s.len(), 1);

        *s.get_or_insert_with(0xabc, 1, || 99).expect("room") += 1;
        assert_eq!(s.get(0xabc), Some(&8));
        assert_eq!(s.len(), 1, "an existing key must not insert again");
    }

    #[test]
    fn absent_key_misses() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        s.get_or_insert_with(1, 0, || 1).expect("room");
        assert_eq!(s.get(2), None);
        assert_eq!(s.get_mut(2), None);
        assert_eq!(s.remove(2), None);
        assert_eq!(s.len(), 1, "a miss must not disturb the table");
    }

    #[test]
    fn key_zero_is_an_ordinary_key() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        // Absent before insert despite zeroed key slots.
        assert_eq!(s.get(0), None);
        s.get_or_insert_with(0, 0, || 5).expect("room");
        assert_eq!(s.get(0), Some(&5), "0 must store like any other key");
        // Must not shadow a key sharing its chain.
        let mut chain = [0u64; 2];
        keys_at(home(0, 8), 8, &mut chain);
        s.get_or_insert_with(chain[0], 0, || 6).expect("room");
        assert_eq!(s.get(0), Some(&5));
        assert_eq!(s.get(chain[0]), Some(&6));
        assert_eq!(s.remove(0), Some(5));
        assert_eq!(s.get(0), None);
        assert_eq!(
            s.get(chain[0]),
            Some(&6),
            "the chain must survive removing 0"
        );
    }

    #[test]
    fn colliding_keys_both_resolve() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 3];
        keys_at(3, 8, &mut c);
        assert_eq!(home(c[0], 8), home(c[1], 8), "generator failed to collide");
        for (n, &key) in c.iter().enumerate() {
            s.get_or_insert_with(key, 0, || n as u32).expect("room");
        }
        for (n, &key) in c.iter().enumerate() {
            assert_eq!(s.get(key), Some(&(n as u32)), "key {key} lost in its chain");
        }
        // Displaced entries follow the shared home.
        assert_eq!(s.keys[3], c[0]);
        assert_eq!(s.keys[4], c[1]);
        assert_eq!(s.keys[5], c[2]);
    }

    #[test]
    fn backward_shift_keeps_the_chain_intact() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 3];
        keys_at(3, 8, &mut c);
        for (n, &key) in c.iter().enumerate() {
            s.get_or_insert_with(key, 0, || n as u32).expect("room");
        }
        // Remove the middle. The tail must shift back over the hole.
        assert_eq!(s.remove(c[1]), Some(1));
        assert_eq!(s.get(c[1]), None);
        assert_eq!(s.get(c[0]), Some(&0), "chain head lost");
        assert_eq!(
            s.get(c[2]),
            Some(&2),
            "chain tail lost after the middle went"
        );
        assert_eq!(
            s.keys[4], c[2],
            "tail must shift back, not leave a tombstone"
        );
        assert!(s.values[5].is_none(), "the vacated slot must be empty");
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn backward_shift_survives_removing_the_chain_head() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 3];
        keys_at(3, 8, &mut c);
        for (n, &key) in c.iter().enumerate() {
            s.get_or_insert_with(key, 0, || n as u32).expect("room");
        }
        assert_eq!(s.remove(c[0]), Some(0));
        assert_eq!(s.get(c[1]), Some(&1));
        assert_eq!(s.get(c[2]), Some(&2));
        assert_eq!(s.keys[3], c[1], "the whole chain must slide back");
        assert_eq!(s.keys[4], c[2]);
        assert!(s.values[5].is_none());
    }

    #[test]
    fn backward_shift_respects_a_chain_that_wraps() {
        // Homed at the last slot so the chain wraps, catching unmasked subtraction.
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 3];
        keys_at(7, 8, &mut c);
        for (n, &key) in c.iter().enumerate() {
            s.get_or_insert_with(key, 0, || n as u32).expect("room");
        }
        assert_eq!(s.keys[7], c[0]);
        assert_eq!(s.keys[0], c[1], "chain should wrap to slot 0");
        assert_eq!(s.keys[1], c[2]);
        assert_eq!(s.remove(c[1]), Some(1));
        assert_eq!(s.get(c[0]), Some(&0));
        assert_eq!(s.get(c[2]), Some(&2), "wrapped tail lost");
        assert_eq!(s.keys[0], c[2]);
    }

    #[test]
    fn backward_shift_leaves_an_unrelated_neighbour_alone() {
        // Slot 5 is another key's home and must not shift.
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 2];
        keys_at(3, 8, &mut c);
        let mut other = [0u64; 1];
        keys_at(5, 8, &mut other);
        s.get_or_insert_with(c[0], 0, || 0).expect("room");
        s.get_or_insert_with(c[1], 0, || 1).expect("room");
        s.get_or_insert_with(other[0], 0, || 2).expect("room");
        assert_eq!(
            s.keys[5], other[0],
            "setup: the neighbour is at its own home"
        );

        assert_eq!(s.remove(c[0]), Some(0));
        assert_eq!(s.keys[3], c[1], "the chain member shifts back");
        assert_eq!(s.keys[5], other[0], "the neighbour must not move");
        assert_eq!(s.get(other[0]), Some(&2));
    }

    #[test]
    fn prune_evicts_stale_and_keeps_fresh() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        s.get_or_insert_with(10, 0, || 0).expect("room");
        s.get_or_insert_with(20, 5, || 1).expect("room");
        s.get_or_insert_with(30, 9, || 2).expect("room");

        // Frame 10, max_age 4. Only the frame-9 entry survives.
        assert_eq!(s.prune(10, 4), 2);
        assert_eq!(s.get(10), None);
        assert_eq!(s.get(20), None);
        assert_eq!(s.get(30), Some(&2));
        assert_eq!(s.len(), 1);

        // Inclusive boundary.
        assert_eq!(s.prune(13, 4), 0);
        assert_eq!(s.get(30), Some(&2));
        assert_eq!(s.prune(14, 4), 1);
        assert!(s.is_empty());
    }

    #[test]
    fn prune_through_the_middle_of_a_chain_keeps_the_tail() {
        let (mut k, mut v, mut t) = store!(u32, 16);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 4];
        keys_at(6, 16, &mut c);
        // Alternate stamps so prune removes interior links.
        for (n, &key) in c.iter().enumerate() {
            let frame = if n % 2 == 0 { 0 } else { 20 };
            s.get_or_insert_with(key, frame, || n as u32).expect("room");
        }
        assert_eq!(s.prune(20, 2), 2, "the two old links should go");
        assert_eq!(s.get(c[0]), None);
        assert_eq!(s.get(c[2]), None);
        assert_eq!(s.get(c[1]), Some(&1), "surviving link lost");
        assert_eq!(s.get(c[3]), Some(&3), "surviving tail lost");
        assert_eq!(s.keys[6], c[1], "survivors must compact onto the home slot");
        assert_eq!(s.keys[7], c[3]);
    }

    #[test]
    fn prune_rescans_a_slot_it_just_refilled() {
        // The entry shifted into the pruned slot is also stale.
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut c = [0u64; 3];
        keys_at(2, 8, &mut c);
        for (n, &key) in c.iter().enumerate() {
            s.get_or_insert_with(key, 0, || n as u32).expect("room");
        }
        assert_eq!(s.prune(50, 1), 3, "every link is stale");
        assert!(s.is_empty());
        assert!(s.iter().next().is_none());
    }

    #[test]
    fn insert_refuses_at_the_load_ceiling() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        assert_eq!(s.load_limit(), 6);
        for n in 0..6u64 {
            s.get_or_insert_with(n + 1, 0, || n as u32)
                .expect("under the ceiling");
        }
        assert!(
            s.get_or_insert_with(100, 0, || 0).is_none(),
            "must refuse rather than probe a nearly full table"
        );
        assert_eq!(s.len(), 6);
        assert_eq!(s.capacity(), 8, "must not grow");
        // An already-present key still resolves at the ceiling.
        assert_eq!(s.get_or_insert_with(1, 1, || 0), Some(&mut 0));
        // Freeing a slot admits the refused key.
        assert_eq!(s.remove(1), Some(0));
        assert!(s.get_or_insert_with(100, 0, || 42).is_some());
    }

    #[test]
    fn reused_storage_starts_empty() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        {
            let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
            s.get_or_insert_with(99, 0, || 1).expect("room");
        }
        let s: KeyedStore<'_, u32> = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        assert!(s.is_empty(), "a fresh store must not inherit old entries");
        assert_eq!(s.get(99), None, "stale state must not resurrect");
        assert!(s.iter().next().is_none());
    }

    #[test]
    fn iteration_is_slot_ordered_and_live_only() {
        let (mut k, mut v, mut t) = store!(u32, 8);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut late = [0u64; 2];
        keys_at(5, 8, &mut late);
        let mut early = [0u64; 1];
        keys_at(1, 8, &mut early);
        // Inserted last but homed first, so slot order and insertion order differ.
        s.get_or_insert_with(late[0], 0, || 10).expect("room");
        s.get_or_insert_with(late[1], 0, || 20).expect("room");
        s.get_or_insert_with(early[0], 0, || 30).expect("room");

        let mut seen = [0u32; 8];
        let mut n = 0;
        for (_, value) in s.iter() {
            seen[n] = *value;
            n += 1;
        }
        assert_eq!(&seen[..n], &[30, 10, 20], "slot order, not insertion order");

        // Repeatable, dead entries never appear.
        s.remove(late[0]);
        let mut again = [0u32; 8];
        let mut m = 0;
        for (_, value) in s.iter() {
            again[m] = *value;
            m += 1;
        }
        assert_eq!(&again[..m], &[30, 20]);
        let repeat: [u32; 2] = [
            *s.iter().next().expect("two live").1,
            *s.iter().nth(1).expect("two live").1,
        ];
        assert_eq!(repeat, [30, 20], "iteration must be repeatable");
    }

    #[test]
    fn the_finalizer_spreads_keys_that_differ_only_high_up() {
        // Unmixed, all of these land on slot 0.
        let (mut k, mut v, mut t) = store!(u32, 16);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut homes = [0u8; 16];
        let mut distinct = 0;
        for i in 0..8u64 {
            let key = i << 32;
            assert_eq!(key as u16 & 15, 0, "setup: unmixed these all collide");
            s.get_or_insert_with(key, 0, || i as u32).expect("room");
            let h = home(key, 16);
            if homes[h] == 0 {
                distinct += 1;
            }
            homes[h] += 1;
        }
        assert!(
            distinct >= 4,
            "finalizer spread 8 keys over only {distinct} slots"
        );
        for i in 0..8u64 {
            assert_eq!(s.get(i << 32), Some(&(i as u32)));
        }
    }

    #[test]
    fn storage_shape_is_validated() {
        let mut k = [0u64; 8];
        let mut v: [Option<u32>; 7] = [const { None }; 7];
        let mut t = [0u32; 8];
        assert_eq!(
            KeyedStore::new(&mut k, &mut v, &mut t).err(),
            Some(KeyedError::LengthMismatch {
                keys: 8,
                values: 7,
                last_touched: 8
            })
        );

        let (mut k, mut v, mut t) = store!(u32, 6);
        assert_eq!(
            KeyedStore::new(&mut k, &mut v, &mut t).err(),
            Some(KeyedError::NotPowerOfTwo(6)),
            "a non power of two would force a modulo in the probe"
        );

        let (mut k, mut v, mut t) = store!(u32, 2);
        assert_eq!(
            KeyedStore::new(&mut k, &mut v, &mut t).err(),
            Some(KeyedError::TooSmall(2)),
            "a table that can never accept an insert is a construction error"
        );

        let (mut k, mut v, mut t): ([u64; 0], [Option<u32>; 0], [u32; 0]) = ([], [], []);
        assert_eq!(
            KeyedStore::new(&mut k, &mut v, &mut t).err(),
            Some(KeyedError::NotPowerOfTwo(0))
        );
    }

    // Property test. Seeded PRNG so failures reproduce.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
    }

    const PROP_CAP: usize = 64;
    /// Small so repeats and home-slot collisions are frequent.
    const PROP_KEY_SPACE: u64 = 200;

    /// Expected table contents.
    struct Model {
        key: [u64; PROP_CAP],
        frame: [u32; PROP_CAP],
        len: usize,
    }

    /// Guards against a degenerate generator.
    #[derive(Default)]
    struct Coverage {
        inserts: usize,
        refreshes: usize,
        evicted: usize,
        refusals: usize,
        collided: usize,
    }

    fn model_touch(m: &mut Model, key: u64, frame: u32) -> bool {
        for i in 0..m.len {
            if m.key[i] == key {
                m.frame[i] = frame;
                return false;
            }
        }
        m.key[m.len] = key;
        m.frame[m.len] = frame;
        m.len += 1;
        true
    }

    fn model_prune(m: &mut Model, frame: u32, max_age: u32) -> usize {
        let before = m.len;
        let mut w = 0;
        for r in 0..before {
            if frame.saturating_sub(m.frame[r]) <= max_age {
                m.key[w] = m.key[r];
                m.frame[w] = m.frame[r];
                w += 1;
            }
        }
        m.len = w;
        before - w
    }

    /// True when two live keys share a home slot.
    fn model_collides(m: &Model) -> bool {
        let mut seen = [0u8; PROP_CAP];
        for &key in &m.key[..m.len] {
            let h = home(key, PROP_CAP);
            seen[h] += 1;
            if seen[h] > 1 {
                return true;
            }
        }
        false
    }

    fn run_cycle(seed: u64, cov: &mut Coverage) {
        let (mut k, mut v, mut t) = store!(u64, PROP_CAP);
        let mut s = KeyedStore::new(&mut k, &mut v, &mut t).expect("power of two");
        let mut rng = Rng(seed);
        let mut m = Model {
            key: [0; PROP_CAP],
            frame: [0; PROP_CAP],
            len: 0,
        };

        for frame in 0..120u32 {
            for _ in 0..3 {
                let key = rng.below(PROP_KEY_SPACE);
                let Some(value) = s.get_or_insert_with(key, frame, || key) else {
                    cov.refusals += 1;
                    assert_eq!(m.len, s.load_limit(), "seed {seed}: refused early");
                    continue;
                };
                assert_eq!(*value, key, "seed {seed}: slot held the wrong value");
                if model_touch(&mut m, key, frame) {
                    cov.inserts += 1;
                } else {
                    cov.refreshes += 1;
                }
            }

            if rng.below(4) == 0 {
                let max_age = rng.below(6) as u32;
                let got = s.prune(frame, max_age);
                assert_eq!(got, model_prune(&mut m, frame, max_age), "seed {seed}");
                cov.evicted += got;
            }

            assert_eq!(s.len(), m.len, "seed {seed} frame {frame}: length drifted");
            for &key in &m.key[..m.len] {
                assert_eq!(
                    s.get(key),
                    Some(&key),
                    "seed {seed} frame {frame}: lost live key {key}"
                );
            }
            if model_collides(&m) {
                cov.collided += 1;
            }
        }
    }

    #[test]
    fn random_insert_and_prune_cycles_never_lose_a_live_key() {
        let mut cov = Coverage::default();
        for seed in 1..300u64 {
            run_cycle(seed, &mut cov);
        }
        assert!(cov.inserts > 5_000, "only {} inserts", cov.inserts);
        assert!(cov.refreshes > 5_000, "only {} refreshes", cov.refreshes);
        assert!(cov.evicted > 5_000, "prune evicted only {}", cov.evicted);
        assert!(
            cov.refusals > 100,
            "the ceiling was hit only {} times",
            cov.refusals
        );
        assert!(
            cov.collided > 10_000,
            "only {} steps had a home-slot collision, chains were not exercised",
            cov.collided
        );
    }
}
