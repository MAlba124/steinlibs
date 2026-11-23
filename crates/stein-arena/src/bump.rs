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
    /// Offset of the newest non-empty region, [`NO_LAST`] once unknown. Checks `free_last`.
    #[cfg(debug_assertions)]
    last: Cell<usize>,
}

#[cfg(debug_assertions)]
const NO_LAST: usize = usize::MAX;

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
            #[cfg(debug_assertions)]
            last: Cell::new(NO_LAST),
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
        #[cfg(debug_assertions)]
        self.last.set(NO_LAST);
    }

    /// `align` is a power of two, so the round up is a mask, not a divide. The address is
    /// aligned, not the offset, as the base is only page aligned.
    fn take(&self, len: usize, align: usize) -> Result<usize, ArenaError> {
        let align = align.max(1);
        debug_assert!(align.is_power_of_two(), "align {align}");
        let full = || ArenaError::Full {
            asked: len,
            left: self.cap - self.len.get(),
        };
        let base = self.base.addr();
        // Cursor address cannot overflow, it is inside the mapping.
        let addr = base + self.len.get();
        let at = (addr.checked_add(align - 1).ok_or_else(full)? & !(align - 1)) - base;
        let end = at.checked_add(len).ok_or_else(full)?;
        if end > self.cap {
            return Err(full());
        }
        self.len.set(end);
        self.high.set(self.high.get().max(end));
        #[cfg(debug_assertions)]
        if len > 0 {
            self.last.set(at);
        }
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

    /// Raw `len` bytes at `align`, a power of two, contents unspecified. For allocator
    /// shims.
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
    /// The cursor check cannot tell a region from a longer run of regions, so an
    /// oversized `len` from an older region would rewind over newer ones. Debug builds
    /// catch that while the newest region is known.
    ///
    /// # Safety
    ///
    /// `start` began a region from this arena and `len` is that region's exact current
    /// length, counting every successful [`Bump::grow_last`]. The caller exclusively owns
    /// all of `start..start + len`, so no other live region lies in it, and never
    /// accesses it again.
    pub unsafe fn free_last(&self, start: *const u8, len: usize) -> bool {
        let Some(at) = start.addr().checked_sub(self.base.addr()) else {
            return false;
        };
        if at.checked_add(len) != Some(self.len.get()) {
            return false;
        }
        // An empty free rewinds nothing and leaves the newest region known.
        #[cfg(debug_assertions)]
        if len > 0 {
            let last = self.last.replace(NO_LAST);
            debug_assert!(
                last == NO_LAST || last == at,
                "free_last at {at} len {len} covers the newer region at {last}"
            );
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
    fn raw_allocations_round_up_to_their_alignment() {
        let b = Bump::reserve(1 << 16).unwrap();
        for align in [1usize, 2, 8, 64, 4096] {
            // Three bytes in, then one aligned, so the gap is exactly the round up.
            let p = b.alloc_raw(3, 1).unwrap().as_ptr() as usize;
            let q = b.alloc_raw(1, align).unwrap().as_ptr() as usize;
            assert_eq!(q % align, 0, "align {align}");
            assert!(q >= p + 3 && q - (p + 3) < align, "align {align}");
        }
    }

    #[derive(Clone, Copy)]
    #[repr(C, align(8192))]
    struct Wide(u8);

    #[test]
    fn alignment_above_the_page_is_of_the_address() {
        // A page aligned base is often not 8192 aligned, so try several mappings.
        let held: Vec<Bump> = (1..=16usize)
            .map(|i| Bump::reserve((i << 12) + (1 << 18)).unwrap())
            .collect();
        for b in &held {
            b.alloc_raw(1, 1).unwrap();
            for align in [8192usize, 1 << 14, 1 << 16] {
                let p = b.alloc_raw(1, align).unwrap().as_ptr().addr();
                assert_eq!(p % align, 0, "align {align}");
            }
            let s = b.alloc_slice(2, Wide(7)).unwrap();
            assert_eq!(s.as_ptr().addr() % 8192, 0, "a Wide slice unaligned");
            assert_eq!(s[1].0, 7);
        }
    }

    #[test]
    fn padding_to_an_address_counts_against_the_capacity() {
        for _ in 0..16 {
            let b = Bump::reserve(1 << 16).unwrap();
            let base = b.alloc_raw(0, 1).unwrap().as_ptr().addr();
            let whole = b.alloc_raw(1 << 16, 1 << 16);
            assert_eq!(whole.is_ok(), base.is_multiple_of(1 << 16), "base {base:#x}");
            if let Ok(p) = whole {
                assert_eq!(p.as_ptr().addr(), base);
            }
        }
    }

    #[test]
    fn a_take_past_the_capacity_is_full_not_wild() {
        let b = Bump::reserve(1 << 12).unwrap();
        let cap = b.capacity();
        assert!(b.alloc_raw(cap - 1, 1).is_ok());
        // One byte left, an aligned take of it needs a gap it does not have.
        assert!(matches!(b.alloc_raw(1, 64), Err(ArenaError::Full { .. })));
        assert!(b.alloc_raw(1, 1).is_ok());
        assert!(matches!(b.alloc_raw(1, 1), Err(ArenaError::Full { .. })));
        assert!(matches!(
            b.alloc_raw(usize::MAX, 8),
            Err(ArenaError::Full { .. })
        ));
    }

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
