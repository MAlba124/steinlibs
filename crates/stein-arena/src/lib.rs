//! Bump allocation over one reserved mapping. Never grows, full is an error.
//!
//! Under `vm.overcommit_memory=2` reservations count against the commit limit.

use core::fmt;
use rustix::io::Errno;
use rustix::mm::{Advice, MapFlags, ProtFlags};

pub const DEFAULT_CAP: usize = 8 << 30;

/// Byte range of an arena. A stale span reads wrong bytes, never dangles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: usize,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArenaError {
    /// The kernel refused the reservation.
    Map(Errno),
    /// Capacity exhausted.
    Full { asked: usize, left: usize },
    /// Zero, or overflows page rounding.
    BadCap(usize),
}

impl fmt::Display for ArenaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Map(e) => write!(f, "reserving the mapping: {e}"),
            Self::Full { asked, left } => {
                write!(f, "arena full: {asked} bytes asked, {left} left")
            }
            Self::BadCap(c) => write!(f, "unusable capacity {c}"),
        }
    }
}

impl std::error::Error for ArenaError {}

/// One reserved mapping and a cursor.
pub struct Arena {
    base: *mut u8,
    cap: usize,
    len: usize,
}

// SAFETY: the mapping is exclusively owned.
unsafe impl Send for Arena {}
// SAFETY: `&self` only reads below the cursor, writes need `&mut self`.
unsafe impl Sync for Arena {}

impl Arena {
    /// Reserve `cap` bytes of address space. Committed on first write.
    pub fn reserve(cap: usize) -> Result<Arena, ArenaError> {
        if cap == 0 {
            return Err(ArenaError::BadCap(cap));
        }
        let cap = round_up(cap, page()).ok_or(ArenaError::BadCap(cap))?;
        Ok(Arena {
            base: map(cap)?,
            cap,
            len: 0,
        })
    }

    /// [`Arena::reserve`] at [`DEFAULT_CAP`].
    pub fn new() -> Result<Arena, ArenaError> {
        Arena::reserve(DEFAULT_CAP)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Uninitialised room, `align` a power of two. Zero on first use, stale after [`Arena::reset`].
    pub fn alloc(&mut self, len: usize, align: usize) -> Result<Span, ArenaError> {
        let at = round_up(self.len, align.max(1)).ok_or(ArenaError::Full {
            asked: len,
            left: 0,
        })?;
        let end = at.checked_add(len).ok_or(ArenaError::Full {
            asked: len,
            left: self.cap - self.len,
        })?;
        if end > self.cap {
            return Err(ArenaError::Full {
                asked: len,
                left: self.cap - self.len,
            });
        }
        self.len = end;
        Ok(Span { at, len })
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Span, ArenaError> {
        let span = self.alloc(bytes.len(), 1)?;
        self.bytes_mut(span).copy_from_slice(bytes);
        Ok(span)
    }

    /// A span past the cursor reads empty.
    pub fn bytes(&self, span: Span) -> &[u8] {
        let Some(end) = span.at.checked_add(span.len) else {
            return &[];
        };
        if end > self.len {
            return &[];
        }
        // SAFETY: below the cursor, inside the mapping, writes need `&mut self`.
        unsafe { core::slice::from_raw_parts(self.base.add(span.at), span.len) }
    }

    pub fn bytes_mut(&mut self, span: Span) -> &mut [u8] {
        let Some(end) = span.at.checked_add(span.len) else {
            return &mut [];
        };
        if end > self.len {
            return &mut [];
        }
        // SAFETY: as `bytes`, `&mut self` makes this exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(span.at), span.len) }
    }

    /// Rewind to empty. Pages stay committed, [`Arena::shrink`] releases them.
    pub fn reset(&mut self) {
        self.len = 0;
    }

    /// Let the kernel reclaim pages past `keep`. The reservation stays.
    pub fn shrink(&mut self, keep: usize) {
        let keep = round_up(keep.min(self.cap), page()).unwrap_or(self.cap);
        if keep >= self.cap {
            return;
        }
        // SAFETY: page-aligned tail of our mapping, stays mapped.
        let _ =
            unsafe { rustix::mm::madvise(self.base.add(keep).cast(), self.cap - keep, DISPOSABLE) };
        self.len = self.len.min(keep);
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: the mapping `reserve` created, unmapped once.
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.cap) };
    }
}

/// Reclaimable while mapped. Linux refills with zeroes, Apple keeps the bytes.
#[cfg(any(target_os = "linux", target_os = "android"))]
const DISPOSABLE: Advice = Advice::LinuxFree;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const DISPOSABLE: Advice = Advice::DontNeed;

/// NORESERVE keeps a big cap out of the commit charge. The BSDs lack it.
#[cfg(not(any(target_os = "freebsd", target_os = "dragonfly")))]
const RESERVE: MapFlags = MapFlags::PRIVATE.union(MapFlags::NORESERVE);
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
const RESERVE: MapFlags = MapFlags::PRIVATE;

/// A fresh anonymous mapping of `bytes`, page aligned, untouched.
pub(crate) fn map(bytes: usize) -> Result<*mut u8, ArenaError> {
    // SAFETY: fresh anonymous mapping at a kernel-chosen address.
    let base = unsafe {
        rustix::mm::mmap_anonymous(
            core::ptr::null_mut(),
            bytes,
            ProtFlags::READ | ProtFlags::WRITE,
            RESERVE,
        )
    }
    .map_err(ArenaError::Map)?;
    Ok(base.cast())
}

fn page() -> usize {
    rustix::param::page_size()
}

fn round_up(v: usize, to: usize) -> Option<usize> {
    let m = v % to;
    if m == 0 {
        return Some(v);
    }
    v.checked_add(to - m)
}

pub mod bump;
pub mod counting;
pub mod grow;
pub mod interner;
pub mod prefetch;
pub mod text;
pub use bump::{Bump, Zero};
pub use counting::Counting;
pub use grow::Grow;
pub use interner::Interner;
pub use prefetch::prefetch;
pub use text::Text;

#[cfg(test)]
mod tests;
