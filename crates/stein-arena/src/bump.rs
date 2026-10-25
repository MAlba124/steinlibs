//! Bump arena handing out slices from `&self`, valid until [`Bump::reset`].

use core::cell::Cell;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use crate::{ArenaError, map};

mod sealed {
    pub trait Sealed {}
}

/// Types whose all-zero bit pattern is valid. Sealed, primitives only.
pub trait Zero: Copy + sealed::Sealed {}

macro_rules! zero {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl Zero for $t {}
    )*};
}

zero!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

pub struct Bump {
    base: *mut u8,
    cap: usize,
    len: Cell<usize>,
    /// High-water mark. Bytes past it are still kernel zeroes.
    high: Cell<usize>,
}

// SAFETY: the mapping is exclusively owned.
unsafe impl Send for Bump {}

impl Bump {
    /// Reserve `cap` bytes of address space. Committed on first write.
    pub fn reserve(cap: usize) -> Result<Bump, ArenaError> {
        if cap == 0 {
            return Err(ArenaError::BadCap(cap));
        }
        let page = rustix::param::page_size();
        let cap = cap
            .checked_add(page - 1)
            .map(|c| c - c % page)
            .ok_or(ArenaError::BadCap(cap))?;
        Ok(Bump {
            base: map(cap)?,
            cap,
            len: Cell::new(0),
            high: Cell::new(0),
        })
    }

    pub fn len(&self) -> usize {
        self.len.get()
    }

    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Rewind to empty. Pages stay committed.
    pub fn reset(&mut self) {
        self.len.set(0);
    }

    fn take(&self, len: usize, align: usize) -> Result<usize, ArenaError> {
        let align = align.max(1);
        let at = self.len.get();
        let rem = at % align;
        let at = if rem == 0 { at } else { at + (align - rem) };
        let end = at.checked_add(len).ok_or(ArenaError::Full {
            asked: len,
            left: self.cap - self.len.get(),
        })?;
        if end > self.cap {
            return Err(ArenaError::Full {
                asked: len,
                left: self.cap - self.len.get(),
            });
        }
        self.len.set(end);
        self.high.set(self.high.get().max(end));
        Ok(at)
    }

    /// [`Bump::take`] with reused bytes cleared.
    fn take_zero(&self, len: usize, align: usize) -> Result<usize, ArenaError> {
        let dirty = self.high.get();
        let at = self.take(len, align)?;
        if at < dirty {
            let n = len.min(dirty - at);
            // SAFETY: inside the mapping and just handed out, so unaliased.
            unsafe { self.base.add(at).write_bytes(0, n) };
        }
        Ok(at)
    }

    /// Raw `len` bytes at `align`, contents unspecified. For allocator shims.
    pub fn alloc_raw(&self, len: usize, align: usize) -> Result<NonNull<u8>, ArenaError> {
        let at = self.take(len, align)?;
        // SAFETY: inside the mapping, base is never null.
        Ok(unsafe { NonNull::new_unchecked(self.base.add(at)) })
    }

    /// Extend the region ending at `end` by `more` bytes in place. False when
    /// `end` is not the cursor or capacity is spent.
    pub fn grow_last(&self, end: *const u8, more: usize) -> bool {
        let at = self.len.get();
        if end.addr() != self.base.addr().wrapping_add(at) {
            return false;
        }
        match at.checked_add(more) {
            Some(e) if e <= self.cap => {
                self.len.set(e);
                self.high.set(self.high.get().max(e));
                true
            }
            _ => false,
        }
    }

    /// Rewind the cursor to `start` if `start + len` is the cursor. False otherwise.
    ///
    /// # Safety
    ///
    /// `start` began a region from this arena and is never accessed again.
    pub unsafe fn free_last(&self, start: *const u8, len: usize) -> bool {
        let Some(at) = start.addr().checked_sub(self.base.addr()) else {
            return false;
        };
        if at.checked_add(len) != Some(self.len.get()) {
            return false;
        }
        self.len.set(at);
        true
    }

    /// `n` copies of `init`.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice<T: Copy>(&self, n: usize, init: T) -> Result<&mut [T], ArenaError> {
        let bytes = n
            .checked_mul(size_of::<T>())
            .ok_or(ArenaError::Full { asked: n, left: 0 })?;
        let at = self.take(bytes, align_of::<T>().max(1))?;
        // SAFETY: inside the mapping, aligned, unaliased. Every element is
        // written before the slice is formed.
        unsafe {
            let p = self.base.add(at).cast::<T>();
            for i in 0..n {
                p.add(i).write(init);
            }
            Ok(core::slice::from_raw_parts_mut(p, n))
        }
    }

    /// `n` zeroes of `T`. Fresh pages are not written, reused bytes are cleared.
    #[allow(clippy::mut_from_ref)]
    pub fn zeroed<T: Zero>(&self, n: usize) -> Result<&mut [T], ArenaError> {
        let bytes = n
            .checked_mul(size_of::<T>())
            .ok_or(ArenaError::Full { asked: n, left: 0 })?;
        let at = self.take_zero(bytes, align_of::<T>().max(1))?;
        // SAFETY: inside the mapping, aligned, unaliased, and zero, which `Zero` makes valid.
        Ok(unsafe { core::slice::from_raw_parts_mut(self.base.add(at).cast::<T>(), n) })
    }

    /// `n` zero bytes. Fresh pages are not written, reused bytes are cleared.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_bytes(&self, n: usize) -> Result<&mut [u8], ArenaError> {
        let at = self.take_zero(n, 1)?;
        // SAFETY: inside the mapping, unaliased, and zero.
        Ok(unsafe { core::slice::from_raw_parts_mut(self.base.add(at), n) })
    }

    #[allow(clippy::mut_from_ref)]
    pub fn copy(&self, bytes: &[u8]) -> Result<&mut [u8], ArenaError> {
        let out = self.alloc_bytes(bytes.len())?;
        out.copy_from_slice(bytes);
        Ok(out)
    }

    #[allow(clippy::mut_from_ref)]
    pub fn copy_str(&self, s: &str) -> Result<&mut str, ArenaError> {
        let out = self.copy(s.as_bytes())?;
        // SAFETY: exact copy of a `str`.
        Ok(unsafe { core::str::from_utf8_unchecked_mut(out) })
    }
}

impl Drop for Bump {
    fn drop(&mut self) {
        // SAFETY: the mapping `reserve` created, unmapped once.
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.cap) };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn slices_are_disjoint_aligned_and_zero() {
        let b = Bump::reserve(1 << 20).unwrap();
        let a = b.alloc_bytes(3).unwrap();
        let w = b.alloc_slice::<u64>(2, 0).unwrap();
        let o = b.alloc_slice::<Option<u32>>(4, None).unwrap();
        assert_eq!(a, &[0, 0, 0]);
        assert_eq!(w, &[0, 0]);
        assert_eq!(o, &[None; 4]);
        assert_eq!(w.as_ptr() as usize % align_of::<u64>(), 0);
        a[0] = 7;
        w[1] = 9;
        o[2] = Some(1);
        assert_eq!(a, &[7, 0, 0]);
        assert_eq!(w, &[0, 9]);
        assert_eq!(o[2], Some(1));
        let s = b.copy_str("hi").unwrap();
        assert_eq!(s, "hi");
        assert!(b.len() >= 3 + 16 + 32 + 2);
    }

    #[test]
    fn zeroed_slices_are_zero_and_leave_pages_alone() {
        let b = Bump::reserve(1 << 30).unwrap();
        let big = b.zeroed::<u64>(64 << 20).unwrap();
        let f = b.zeroed::<f32>(3).unwrap();
        assert_eq!(big[0], 0);
        assert_eq!(big[(64 << 20) - 1], 0);
        assert_eq!(f, &[0.0; 3]);
        big[5] = 9;
        assert_eq!(big[5], 9);
        assert_eq!(f.as_ptr() as usize % align_of::<f32>(), 0);
    }

    #[test]
    fn a_spent_reservation_says_so() {
        let b = Bump::reserve(1).unwrap();
        let page = rustix::param::page_size();
        assert_eq!(b.capacity(), page);
        assert!(b.alloc_bytes(page).is_ok());
        assert!(matches!(b.alloc_bytes(1), Err(ArenaError::Full { .. })));
        assert!(matches!(Bump::reserve(0), Err(ArenaError::BadCap(0))));
    }
}
