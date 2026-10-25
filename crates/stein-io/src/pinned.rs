//! Memory the kernel reads or writes by address.
//!
//! Held as a raw pointer, so no Rust reference ever covers bytes the kernel may touch.
//! Owned memory is freed on drop unless leaked, which a drain that timed out does since
//! the kernel may still use it.

use core::marker::PhantomData;
use core::ptr::NonNull;

pub(crate) struct Pinned<T> {
    ptr: NonNull<T>,
    len: usize,
    /// Freed on drop. False for borrowed or leaked memory.
    owned: bool,
    _own: PhantomData<T>,
}

// SAFETY: sole owner of its memory, or of a unique borrow of it, like `Box<[T]>`.
unsafe impl<T: Send> Send for Pinned<T> {}

impl<T> Pinned<T> {
    pub(crate) fn new(b: Box<[T]>) -> Pinned<T> {
        let len = b.len();
        let raw = Box::into_raw(b).cast::<T>();
        Pinned {
            // A boxed slice is never null, even when empty.
            ptr: NonNull::new(raw).unwrap_or(NonNull::dangling()),
            len,
            owned: true,
            _own: PhantomData,
        }
    }

    /// Over borrowed memory, never freed. The caller keeps the borrow alive as long.
    #[cfg(feature = "driver")]
    pub(crate) fn borrowed(s: &mut [T]) -> Pinned<T> {
        let len = s.len();
        Pinned {
            ptr: NonNull::new(s.as_mut_ptr()).unwrap_or(NonNull::dangling()),
            len,
            owned: false,
            _own: PhantomData,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Element `i`, or the end for `i == len`. Clamped, never past the end.
    pub(crate) fn at(&self, i: usize) -> *mut T {
        self.ptr.as_ptr().wrapping_add(i.min(self.len))
    }

    /// Never free it. The kernel may still use it.
    pub(crate) fn leak(&mut self) {
        self.owned = false;
    }

    /// The box back, for owned memory not leaked.
    pub(crate) fn into_box(mut self) -> Option<Box<[T]>> {
        if !self.owned {
            return None;
        }
        self.owned = false;
        let raw = core::ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len);
        // SAFETY: made by `Box::into_raw` in `new`, and `owned` was set until now, so
        // nothing freed or took it back.
        Some(unsafe { Box::from_raw(raw) })
    }
}

impl<T> Drop for Pinned<T> {
    fn drop(&mut self) {
        if self.owned {
            let raw = core::ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len);
            // SAFETY: as in `into_box`.
            drop(unsafe { Box::from_raw(raw) });
        }
    }
}
