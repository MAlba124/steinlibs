//! The driver's operation table. Generational slots in owned columns, sized once.
//!
//! The same rules as `stein_core::handle::Pool`, odd generations live, slot 0 first, but
//! owned, so the driver borrows nothing.

use crate::op::{Op, OpId};

pub(super) struct Table {
    slots: Box<[Option<Op>]>,
    generations: Box<[u32]>,
    /// Stack of free slot indices. Live entries are `free[..free_len]`.
    free: Box<[u32]>,
    free_len: u32,
    live: u32,
}

impl Table {
    /// At most `u32::MAX` slots, clamped.
    pub(super) fn new(n: usize) -> Table {
        let n = n.min(u32::MAX as usize) as u32;
        Table {
            slots: vec![None; n as usize].into_boxed_slice(),
            generations: vec![0; n as usize].into_boxed_slice(),
            // Descending so inserts fill slot 0 first.
            free: (0..n).rev().collect(),
            free_len: n,
            live: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.live as usize
    }

    pub(super) fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// `None` when full.
    pub(super) fn insert(&mut self, op: Op) -> Option<OpId> {
        if self.free_len == 0 {
            return None;
        }
        self.free_len -= 1;
        let i = self.free[self.free_len as usize] as usize;
        // Odd is live.
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.slots[i] = Some(op);
        self.live += 1;
        Some(id(i, self.generations[i]))
    }

    fn contains(&self, h: OpId) -> bool {
        let i = h.slot() as usize;
        let generation = h.to_user_data() as u32;
        i < self.slots.len() && generation & 1 == 1 && self.generations[i] == generation
    }

    pub(super) fn get(&self, h: OpId) -> Option<&Op> {
        if !self.contains(h) {
            return None;
        }
        self.slots[h.slot() as usize].as_ref()
    }

    pub(super) fn get_mut(&mut self, h: OpId) -> Option<&mut Op> {
        if !self.contains(h) {
            return None;
        }
        self.slots[h.slot() as usize].as_mut()
    }

    pub(super) fn remove(&mut self, h: OpId) -> Option<Op> {
        if !self.contains(h) {
            return None;
        }
        let i = h.slot() as usize;
        // Even is free. A slot at the last generation retires, never reused, since a
        // wrap would hand a stale id the next op in it. One slot lost per 2^31 reuses.
        self.generations[i] = self.generations[i].wrapping_add(1);
        if self.generations[i] != 0 {
            self.free[self.free_len as usize] = h.slot();
            self.free_len += 1;
        }
        self.live -= 1;
        self.slots[i].take()
    }

    /// First live handle at or after slot `from`, for walking while removing.
    pub(super) fn next_live(&self, from: u32) -> Option<OpId> {
        let at = from as usize;
        let i = at + self.slots.get(at..)?.iter().position(Option::is_some)?;
        Some(id(i, self.generations[i]))
    }
}

fn id(slot: usize, generation: u32) -> OpId {
    OpId::from_user_data(((slot as u64) << 32) | generation as u64)
}

#[cfg(test)]
mod tests;
