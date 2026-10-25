//! A list that grows in place on its own reservation. Never moves, full is an error.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use crate::{ArenaError, map, page, round_up};

pub struct Grow<T: Copy> {
    base: *mut T,
    cap: usize,
    len: usize,
    bytes: usize,
    _owns: PhantomData<T>,
}

// SAFETY: the mapping is exclusively owned and `T: Send`.
unsafe impl<T: Copy + Send> Send for Grow<T> {}
// SAFETY: `&self` only reads below `len`, writes need `&mut`.
unsafe impl<T: Copy + Sync> Sync for Grow<T> {}

impl<T: Copy> Grow<T> {
    /// Room for `cap` elements, reserved and not yet committed.
    pub fn reserve(cap: usize) -> Result<Grow<T>, ArenaError> {
        let size = size_of::<T>().max(1);
        let want = cap.checked_mul(size).ok_or(ArenaError::BadCap(cap))?;
        if want == 0 || align_of::<T>() > page() {
            return Err(ArenaError::BadCap(cap));
        }
        let bytes = round_up(want, page()).ok_or(ArenaError::BadCap(cap))?;
        let base = map(bytes)?;
        Ok(Grow {
            base: base.cast(),
            cap: bytes / size,
            len: 0,
            bytes,
            _owns: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Elements it can ever hold.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn push(&mut self, v: T) -> Result<(), ArenaError> {
        if self.len == self.cap || size_of::<T>() == 0 {
            return self.full(1);
        }
        // SAFETY: `len < cap`. Base is page aligned and `T` no more than that.
        unsafe { self.base.add(self.len).write(v) };
        self.len += 1;
        Ok(())
    }

    pub fn extend_from_slice(&mut self, vs: &[T]) -> Result<(), ArenaError> {
        if vs.len() > self.cap - self.len || size_of::<T>() == 0 {
            return self.full(vs.len());
        }
        // SAFETY: `vs.len()` unborrowed slots past `len`. `vs` cannot overlap
        // them while `&mut self` is held.
        unsafe {
            core::ptr::copy_nonoverlapping(vs.as_ptr(), self.base.add(self.len), vs.len());
        }
        self.len += vs.len();
        Ok(())
    }

    fn full(&self, asked: usize) -> Result<(), ArenaError> {
        Err(ArenaError::Full {
            asked,
            left: self.cap - self.len,
        })
    }

    pub fn pop(&mut self) -> Option<T> {
        self.len = self.len.checked_sub(1)?;
        // SAFETY: the element at the old `len - 1` was written.
        Some(unsafe { self.base.add(self.len).read() })
    }

    /// Keep the first `n`. Pages stay committed.
    pub fn truncate(&mut self, n: usize) {
        self.len = self.len.min(n);
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` elements are written, `self` owns the mapping.
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as above, and `&mut self` makes the borrow unique.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

impl<T: Copy> core::ops::Deref for Grow<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy> core::ops::DerefMut for Grow<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Copy + core::fmt::Debug> core::fmt::Debug for Grow<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: Copy> Drop for Grow<T> {
    fn drop(&mut self) {
        // SAFETY: the mapping `reserve` created, unmapped once.
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.bytes) };
    }
}

#[cfg(test)]
mod tests;
