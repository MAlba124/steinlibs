//! Fixed capacity slot tables behind [`Handle`]s.
//!
//! Capacity is reserved at [`crate::Gpu::open`] and never grown, so an
//! insert past it is [`GpuError::Full`] and steady state inserts and removes
//! allocate nothing. A slot's generation bumps on removal, so a handle to a
//! removed slot is [`GpuError::Stale`] even after the slot is reused.

use crate::{GpuError, Handle};

pub(crate) struct Slots<T> {
    items: Vec<Option<T>>,
    gens: Vec<u8>,
    free: Vec<u32>,
    cap: u32,
    what: &'static str,
}

impl<T> Slots<T> {
    pub(crate) fn new(cap: u32, what: &'static str) -> Slots<T> {
        let cap = cap.min(Handle::MAX_INDEX + 1);
        Slots {
            items: Vec::with_capacity(cap as usize),
            gens: Vec::with_capacity(cap as usize),
            free: Vec::with_capacity(cap as usize),
            cap,
            what,
        }
    }

    pub(crate) fn insert(&mut self, v: T) -> Result<Handle, GpuError> {
        if let Some(i) = self.free.pop() {
            let at = i as usize;
            self.items[at] = Some(v);
            return Ok(Handle::new(i, self.gens[at]));
        }
        let i = self.items.len() as u32;
        if i >= self.cap {
            return Err(GpuError::Full(self.what));
        }
        self.items.push(Some(v));
        self.gens.push(0);
        Ok(Handle::new(i, 0))
    }

    /// Whether an insert would succeed, checked before creating the Vulkan
    /// objects that go in.
    pub(crate) fn has_room(&self) -> bool {
        !self.free.is_empty() || (self.items.len() as u32) < self.cap
    }

    pub(crate) fn get(&self, h: Handle) -> Result<&T, GpuError> {
        let at = h.index() as usize;
        match (self.items.get(at), self.gens.get(at)) {
            (Some(Some(v)), Some(&g)) if g == h.generation() => Ok(v),
            _ => Err(GpuError::Stale(self.what)),
        }
    }

    pub(crate) fn get_mut(&mut self, h: Handle) -> Result<&mut T, GpuError> {
        let at = h.index() as usize;
        match (self.items.get_mut(at), self.gens.get(at)) {
            (Some(Some(v)), Some(&g)) if g == h.generation() => Ok(v),
            _ => Err(GpuError::Stale(self.what)),
        }
    }

    pub(crate) fn remove(&mut self, h: Handle) -> Result<T, GpuError> {
        self.get(h)?;
        let at = h.index() as usize;
        let v = self.items[at].take().ok_or(GpuError::Stale(self.what))?;
        self.gens[at] = self.gens[at].wrapping_add(1);
        self.free.push(h.index());
        Ok(v)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter().flatten()
    }

    /// Everything still live, for teardown.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.free.clear();
        self.gens.clear();
        self.items.drain(..).flatten()
    }
}

#[cfg(test)]
mod tests;
