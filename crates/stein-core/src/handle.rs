//! Generational handles and the pool they index into.
//!
//! A slot's generation is even while free and odd while live. It only rises, and a
//! slot whose last live generation is removed retires for good, so no handle is reissued.

use core::marker::PhantomData;

/// Terminal generation. Reached by removing the last live one, `u32::MAX - 2`.
pub(crate) const RETIRED: u32 = u32::MAX - 1;

/// Even resting generation for a slot found in reused storage. Live rounds up to free.
pub(crate) const fn settle(generation: u32) -> u32 {
    let g = generation.saturating_add(generation & 1);
    if g > RETIRED { RETIRED } else { g }
}

/// Index plus generation, type-tagged by `T`.
pub struct Handle<T> {
    index: u32,
    generation: u32,
    _marker: PhantomData<fn() -> T>,
}

// Manual impls, derive would require `T: Clone`.
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Handle<T> {}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, o: &Self) -> bool {
        self.index == o.index && self.generation == o.generation
    }
}
impl<T> Eq for Handle<T> {}
impl<T> core::hash::Hash for Handle<T> {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        self.index.hash(h);
        self.generation.hash(h);
    }
}
impl<T> core::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "#{}g{}", self.index, self.generation)
    }
}

impl<T> Handle<T> {
    pub fn slot(&self) -> u32 {
        self.index
    }

    /// Pack into `io_uring` `user_data`. Inverse of [`Handle::from_user_data`].
    pub fn to_user_data(&self) -> u64 {
        (self.index as u64) << 32 | self.generation as u64
    }

    /// Unpack a completion's `user_data`. Untrusted, check with [`Pool::contains`].
    pub fn from_user_data(bits: u64) -> Self {
        Handle {
            index: (bits >> 32) as u32,
            generation: bits as u32,
            _marker: PhantomData,
        }
    }
}

const _: () = assert!(core::mem::size_of::<Handle<u8>>() == 8);
const _: () = assert!(!core::mem::needs_drop::<Handle<u8>>());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolError {
    /// The three backing slices differ in length.
    LengthMismatch {
        slots: usize,
        generations: usize,
        free: usize,
    },
    /// More slots than a `u32` index addresses.
    TooManySlots(usize),
}

/// Fixed-capacity pool over caller-supplied parallel slices. Never grows.
pub struct Pool<'a, T> {
    slots: &'a mut [Option<T>],
    generations: &'a mut [u32],
    /// Stack of free slot indices. Live entries are `free[..free_len]`.
    free: &'a mut [u32],
    free_len: u32,
    live: u32,
}

impl<'a, T> Pool<'a, T> {
    /// Take the storage and reset it to empty, discarding prior contents.
    ///
    /// Generations carry over, so handles from an earlier pool on the same columns
    /// stay dead. Zeroed columns start fresh and cannot know handles minted on other
    /// storage, drop those before reusing their ids here. Retired slots stay retired.
    pub fn new(
        slots: &'a mut [Option<T>],
        generations: &'a mut [u32],
        free: &'a mut [u32],
    ) -> Result<Self, PoolError> {
        if slots.len() != generations.len() || slots.len() != free.len() {
            return Err(PoolError::LengthMismatch {
                slots: slots.len(),
                generations: generations.len(),
                free: free.len(),
            });
        }
        if slots.len() > u32::MAX as usize {
            return Err(PoolError::TooManySlots(slots.len()));
        }
        for s in slots.iter_mut() {
            *s = None;
        }
        // Pushed descending so inserts fill slot 0 first. Replay depends on this order.
        let mut free_len = 0;
        for i in (0..generations.len()).rev() {
            let g = settle(generations[i]);
            generations[i] = g;
            if g != RETIRED {
                free[free_len] = i as u32;
                free_len += 1;
            }
        }
        Ok(Pool {
            slots,
            generations,
            free,
            free_len: free_len as u32,
            live: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.live as usize
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    /// Slots in the storage, retired ones included.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Slots whose generations ran out. They never hold a value again.
    pub fn retired(&self) -> usize {
        self.slots.len() - self.live as usize - self.free_len as usize
    }

    /// `None` when full, that is when `len() + retired() == capacity()`.
    pub fn insert(&mut self, value: T) -> Option<Handle<T>> {
        if self.free_len == 0 {
            return None;
        }
        self.free_len -= 1;
        let index = self.free[self.free_len as usize];
        let i = index as usize;
        debug_assert!(self.slots[i].is_none(), "free list pointed at a live slot");
        self.generations[i] += 1; // odd => live
        self.slots[i] = Some(value);
        self.live += 1;
        Some(Handle {
            index,
            generation: self.generations[i],
            _marker: PhantomData,
        })
    }

    pub fn contains(&self, h: Handle<T>) -> bool {
        (h.index as usize) < self.slots.len()
            && h.generation & 1 == 1
            && self.generations[h.index as usize] == h.generation
    }

    pub fn get(&self, h: Handle<T>) -> Option<&T> {
        if !self.contains(h) {
            return None;
        }
        self.slots[h.index as usize].as_ref()
    }

    pub fn get_mut(&mut self, h: Handle<T>) -> Option<&mut T> {
        if !self.contains(h) {
            return None;
        }
        self.slots[h.index as usize].as_mut()
    }

    pub fn remove(&mut self, h: Handle<T>) -> Option<T> {
        if !self.contains(h) {
            return None;
        }
        let i = h.index as usize;
        self.generations[i] += 1; // even => free
        if self.generations[i] != RETIRED {
            self.free[self.free_len as usize] = h.index;
            self.free_len += 1;
        }
        self.live -= 1;
        self.slots[i].take()
    }

    /// Live entries with their handles, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        let generations = &*self.generations;
        self.slots.iter().enumerate().filter_map(move |(i, slot)| {
            slot.as_ref().map(|v| {
                let h = Handle {
                    index: i as u32,
                    generation: generations[i],
                    _marker: PhantomData,
                };
                (h, v)
            })
        })
    }

    /// First live handle at or after slot `from`. Cursor form of [`Pool::iter`]
    /// for mutating while walking.
    ///
    /// ```
    /// # use stein_core::handle::Pool;
    /// # let mut slots: [Option<u32>; 4] = [const { None }; 4];
    /// # let mut gens = [0u32; 4];
    /// # let mut free = [0u32; 4];
    /// # let mut pool = Pool::new(&mut slots, &mut gens, &mut free).ok().expect("lengths agree");
    /// # pool.insert(1);
    /// let mut slot = 0;
    /// while let Some(h) = pool.next_live(slot) {
    ///     slot = h.slot() + 1;
    ///     pool.remove(h);
    /// }
    /// assert!(pool.is_empty());
    /// ```
    pub fn next_live(&self, from: u32) -> Option<Handle<T>> {
        let mut i = from as usize;
        while i < self.slots.len() {
            if self.slots[i].is_some() {
                return Some(Handle {
                    index: i as u32,
                    generation: self.generations[i],
                    _marker: PhantomData,
                });
            }
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Thing;

    /// Stack backing storage for a test pool.
    macro_rules! pool {
        ($t:ty, $n:expr) => {{
            let slots: [Option<$t>; $n] = [const { None }; $n];
            (slots, [0u32; $n], [0u32; $n])
        }};
    }

    #[test]
    fn insert_get_remove() {
        let (mut s, mut g, mut f) = pool!(u32, 4);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(10).expect("capacity 4");
        assert_eq!(p.get(a), Some(&10));
        assert_eq!(p.remove(a), Some(10));
        assert_eq!(p.get(a), None);
        assert!(p.is_empty());
    }

    #[test]
    fn recycled_slot_rejects_the_old_handle() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(1).expect("capacity 2");
        p.remove(a);
        let b = p.insert(2).expect("slot was freed");
        assert_eq!(a.slot(), b.slot(), "slot should be reused");
        assert_ne!(a, b);
        assert!(p.get(a).is_none(), "stale handle must stay dead");
    }

    #[test]
    fn capacity_is_a_hard_ceiling() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        assert!(p.insert(1).is_some());
        assert!(p.insert(2).is_some());
        assert!(p.insert(3).is_none(), "must refuse rather than grow");
        assert_eq!(p.capacity(), 2, "must not grow");
    }

    #[test]
    fn handles_are_type_tagged() {
        // Distinct types. A compile failure is not assertable without trybuild.
        let _a: Handle<u32>;
        let _b: Handle<Thing>;
        assert_eq!(core::mem::size_of::<Handle<Thing>>(), 8);
    }

    #[test]
    fn iteration_is_slot_ordered_and_live_only() {
        let (mut s, mut g, mut f) = pool!(u32, 4);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(1).expect("cap");
        let _b = p.insert(2).expect("cap");
        let _c = p.insert(3).expect("cap");
        p.remove(a);
        let mut seen = [0u32; 4];
        let mut n = 0;
        for (_, v) in p.iter() {
            seen[n] = *v;
            n += 1;
        }
        assert_eq!(&seen[..n], &[2, 3], "live only, in slot order");
    }

    #[test]
    fn cursor_walk_matches_iter() {
        let (mut s, mut g, mut f) = pool!(u32, 8);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(1).expect("cap");
        let _b = p.insert(2).expect("cap");
        let c = p.insert(3).expect("cap");
        let _d = p.insert(4).expect("cap");
        p.remove(a);
        p.remove(c);

        let mut by_iter = [0u32; 8];
        let mut n = 0;
        for (h, _) in p.iter() {
            by_iter[n] = h.slot();
            n += 1;
        }

        let mut by_cursor = [0u32; 8];
        let mut m = 0;
        let mut slot = 0;
        while let Some(h) = p.next_live(slot) {
            by_cursor[m] = h.slot();
            m += 1;
            slot = h.slot() + 1;
        }

        assert_eq!(&by_iter[..n], &by_cursor[..m]);
    }

    #[test]
    fn cursor_survives_removal_mid_walk() {
        let (mut s, mut g, mut f) = pool!(u32, 4);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        for v in 0..4 {
            p.insert(v).expect("cap");
        }
        let mut slot = 0;
        let mut removed = 0;
        while let Some(h) = p.next_live(slot) {
            slot = h.slot() + 1;
            if h.slot() % 2 == 0 {
                p.remove(h);
                removed += 1;
            }
        }
        assert_eq!(removed, 2);
        assert_eq!(p.len(), 2, "odd slots survive");
    }

    #[test]
    fn mismatched_storage_is_refused() {
        let mut s: [Option<u32>; 4] = [const { None }; 4];
        let mut g = [0u32; 3];
        let mut f = [0u32; 4];
        assert_eq!(
            Pool::new(&mut s, &mut g, &mut f).err(),
            Some(PoolError::LengthMismatch {
                slots: 4,
                generations: 3,
                free: 4
            })
        );
    }

    #[test]
    fn reused_storage_starts_empty() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        {
            let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
            p.insert(99).expect("cap");
        }
        let p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        assert!(p.is_empty(), "a fresh pool must not inherit old entries");
        assert!(p.next_live(0).is_none());
    }

    #[test]
    fn rebuilt_pool_rejects_handles_from_its_last_life() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let bits = {
            let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
            p.insert(1).expect("cap").to_user_data()
        };
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let fresh = p.insert(2).expect("cap");
        let stale: Handle<u32> = Handle::from_user_data(bits);
        assert_eq!(stale.slot(), fresh.slot(), "setup: the slot is reused");
        assert!(!p.contains(stale), "a late completion named the replacement");
        assert_eq!(p.get(stale), None);
        assert_eq!(p.remove(stale), None);
        assert_eq!(p.get(fresh), Some(&2));
    }

    #[test]
    fn exhausted_generation_never_revalidates_an_old_handle() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let first = p.insert(0).expect("cap");
        p.remove(first);
        // Free, one live generation left.
        p.generations[0] = u32::MAX - 3;
        let last = p.insert(1).expect("cap");
        assert_eq!((last.slot(), last.generation), (0, u32::MAX - 2));
        assert_eq!(p.remove(last), Some(1));
        assert_eq!(p.retired(), 1, "slot 0 must retire");
        assert!(!p.contains(last));
        for v in 2..6 {
            let h = p.insert(v).expect("slot 1 is free");
            assert_eq!(h.slot(), 1, "a retired slot was reissued");
            assert!(!p.contains(first), "cycle {v} revalidated the first handle");
            p.remove(h);
        }
        // Full counts the retired slot.
        let held = p.insert(7).expect("slot 1 is free");
        assert_eq!(p.insert(8), None);
        assert_eq!((p.len(), p.retired(), p.capacity()), (1, 1, 2));
        assert_eq!(p.remove(held), Some(7));
        // Retirement survives a rebuild on the same columns.
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        assert_eq!(p.retired(), 1);
        assert_eq!(p.insert(9).map(|h| h.slot()), Some(1));
        assert_eq!(p.insert(10), None);
    }

    #[test]
    fn rebuild_settles_any_stored_generation() {
        let (mut s, _, mut f) = pool!(u32, 5);
        let mut g = [0, 7, u32::MAX - 2, u32::MAX - 1, u32::MAX];
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        // Live rounds up to free, the top two and a spent live one retire.
        assert_eq!(p.retired(), 3);
        let a = p.insert(1).expect("cap");
        let b = p.insert(2).expect("cap");
        assert_eq!((a.slot(), a.generation), (0, 1));
        assert_eq!((b.slot(), b.generation), (1, 9));
        assert_eq!(p.insert(3), None);
    }

    #[test]
    fn user_data_round_trips() {
        let (mut s, mut g, mut f) = pool!(u32, 4);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(5).expect("cap");
        let back: Handle<u32> = Handle::from_user_data(a.to_user_data());
        assert_eq!(a, back);
        assert_eq!(p.get(back), Some(&5));
    }

    #[test]
    fn stale_user_data_is_rejected() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let mut p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        let a = p.insert(1).expect("cap");
        let bits = a.to_user_data();
        p.remove(a);
        p.insert(2).expect("slot was freed");
        // Late completion naming a reused slot.
        let stale: Handle<u32> = Handle::from_user_data(bits);
        assert!(p.get(stale).is_none());
    }

    #[test]
    fn even_generation_is_never_live() {
        let (mut s, mut g, mut f) = pool!(u32, 2);
        let p = Pool::new(&mut s, &mut g, &mut f).expect("lengths agree");
        // Generation 0 means never used.
        let zeroed: Handle<u32> = Handle::from_user_data(0);
        assert!(!p.contains(zeroed), "a zeroed handle must not name slot 0");
    }
}
