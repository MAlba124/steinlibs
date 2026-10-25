//! Buffer arena. One allocation split into equal buffers, handed out by id.
//!
//! Ids are 16 bits to match the `io_uring` buffer id width. Each buffer is free, the
//! caller's, or the kernel's, and only the caller's are readable or writable through the
//! arena. The arena is held as a raw pointer, so reading one buffer never borrows another
//! the kernel is writing.

use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::error::{IoError, PoolFull};
use crate::pinned::Pinned;

/// A buffer the caller holds. Made only by its arena and never copied, so a call that
/// takes one by value spends it, and an id of another arena names nothing here.
///
/// None is made from a number.
///
/// ```compile_fail,E0423
/// let id = stein_io::BufId(3);
/// ```
///
/// ```compile_fail,E0451
/// let id = stein_io::BufId { index: 3, stamp: 1 };
/// ```
///
/// A released one is gone.
///
/// ```compile_fail,E0382
/// let mut io = stein_io::Io::new(stein_io::Storage::new(4, 4), 64).unwrap();
/// let b = io.alloc().unwrap();
/// io.release(b);
/// io.buf_mut(&b);
/// ```
///
/// So is a sent one.
///
/// ```compile_fail,E0382
/// let mut io = stein_io::Io::new(stein_io::Storage::new(4, 4), 64).unwrap();
/// let b = io.alloc().unwrap();
/// let _ = io.send(0, b, 4);
/// let _ = io.send(0, b, 4);
/// ```
#[must_use = "a buffer neither released nor handed to an operation is lost to the arena"]
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct BufId {
    index: u16,
    /// Its arena's, see `STAMPS`.
    stamp: u32,
}

impl BufId {
    /// Index in its arena, for logs.
    pub fn index(&self) -> u16 {
        self.index
    }
}

/// A buffer the caller holds, from a completion. `len` is how much the kernel wrote.
#[must_use = "a buffer neither released nor handed to an operation is lost to the arena"]
#[derive(Debug, PartialEq, Eq)]
pub struct Buf {
    pub id: BufId,
    pub len: u32,
}

impl From<Buf> for BufId {
    fn from(b: Buf) -> BufId {
        b.id
    }
}

/// Arena stamps, so no id of one arena matches another. Wraps after 2^32 arenas.
static STAMPS: AtomicU32 = AtomicU32::new(1);

/// Who holds a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Own {
    /// On the free list, or in the provided ring where the kernel may take it any time.
    Free,
    /// Taken by [`Bufs::take`], claimed, or handed back in a completion.
    Caller,
    /// Lent to an operation, or kept after a cancel until the kernel lets go.
    Kernel,
}

const FREE: u8 = 0;
const CALLER: u8 = 1;
const KERNEL: u8 = 2;

/// Largest arena the id width allows.
pub const MAX_BUFFERS: usize = u16::MAX as usize;

/// A column borrowed from the caller or owned.
enum Col<'a, T> {
    Lent(&'a mut [T]),
    Own(Box<[T]>),
}

impl<T> Deref for Col<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Col::Lent(s) => s,
            Col::Own(b) => b,
        }
    }
}

impl<T> DerefMut for Col<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Col::Lent(s) => s,
            Col::Own(b) => b,
        }
    }
}

pub struct Bufs<'a> {
    /// The arena. Raw, since the kernel writes buffers it holds while the caller reads others.
    mem: Pinned<u8>,
    /// Ids not on loan. Live entries are `free[..free_len]`.
    free: Col<'a, u16>,
    /// One [`Own`] per buffer.
    state: Col<'a, u8>,
    size: u32,
    count: u16,
    free_len: u16,
    /// Buffers `0..reserved` belong to a provided buffer ring, never the free list.
    reserved: u16,
    /// What its ids carry.
    stamp: u32,
    _lent: PhantomData<&'a mut [u8]>,
}

/// How many bytes of state an arena of `count` buffers needs, one per buffer.
pub const fn state_bytes(count: usize) -> usize {
    count
}

impl<'a> Bufs<'a> {
    /// Divide `mem` into buffers of `size` bytes. `free` and `state` must fit
    /// `mem.len() / size` buffers, at most [`MAX_BUFFERS`], see [`state_bytes`].
    pub fn new(
        mem: &'a mut [u8],
        free: &'a mut [u16],
        state: &'a mut [u8],
        size: u32,
    ) -> Result<Bufs<'a>, IoError> {
        let count = count_of(mem.len(), size)?;
        if free.len() < count || state.len() < state_bytes(count) {
            return Err(IoError::BadArena {
                bytes: mem.len(),
                buffer: size as usize,
            });
        }
        Ok(Bufs::start(
            Pinned::borrowed(mem),
            Col::Lent(free),
            Col::Lent(state),
            size,
            count,
        ))
    }

    fn start(
        mem: Pinned<u8>,
        free: Col<'a, u16>,
        state: Col<'a, u8>,
        size: u32,
        count: usize,
    ) -> Bufs<'a> {
        let mut b = Bufs {
            mem,
            free,
            state,
            size,
            count: count as u16,
            free_len: count as u16,
            reserved: 0,
            stamp: STAMPS.fetch_add(1, Ordering::Relaxed),
            _lent: PhantomData,
        };
        // Descending, so the first take pops id 0 and a trace reads in order.
        for (i, f) in b.free.iter_mut().take(count).enumerate() {
            *f = (count - 1 - i) as u16;
        }
        b.state.fill(FREE);
        b
    }

    /// Hand the first `n` buffers to a provided buffer ring. [`Bufs::take`] never returns them.
    /// Refused while any buffer is on loan, as the free list is rebuilt. Buffers already
    /// offered to a ring are the caller's to take back first.
    pub fn reserve(&mut self, n: u16) -> Result<(), IoError> {
        if n > self.count {
            return Err(IoError::TooManySlots(n as usize));
        }
        let held = self
            .state
            .iter()
            .take(self.count as usize)
            .position(|s| *s != FREE);
        if let Some(i) = held {
            return Err(IoError::Loaned(i as u16));
        }
        let rest = self.count - n;
        let count = self.count;
        for (i, f) in self.free.iter_mut().take(rest as usize).enumerate() {
            *f = count - 1 - i as u16;
        }
        self.free_len = rest;
        self.reserved = n;
        Ok(())
    }

    /// Buffers owned by the ring.
    pub fn reserved(&self) -> u16 {
        self.reserved
    }

    pub fn in_ring(&self, id: &BufId) -> bool {
        self.index(id).is_some_and(|i| self.ring_at(i))
    }

    /// Take a ring buffer the kernel chose. False unless it was free in the ring,
    /// and then it must not be delivered.
    pub(crate) fn claim(&mut self, i: u16) -> bool {
        if !self.ring_at(i) || self.code_at(i) != Some(FREE) {
            return false;
        }
        self.set(i, CALLER);
        true
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn count(&self) -> usize {
        self.count as usize
    }

    pub fn available(&self) -> usize {
        self.free_len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.free_len == 0
    }

    /// A buffer for the caller.
    pub fn take(&mut self) -> Result<BufId, PoolFull> {
        let i = self.pop(CALLER)?;
        Ok(self.mint(i))
    }

    /// Give a buffer back. False unless the caller holds it, as for an id of another arena.
    pub fn release(&mut self, id: impl Into<BufId>) -> bool {
        let id = id.into();
        let Some(i) = self.index(&id).filter(|i| self.code_at(*i) == Some(CALLER)) else {
            return false;
        };
        self.unlist(i);
        true
    }

    /// Out of the free list, held by the caller or the kernel.
    pub fn is_loaned(&self, id: &BufId) -> bool {
        self.code(id).is_some_and(|c| c != FREE)
    }

    /// Who holds it. `None` for an id the arena does not have.
    pub fn owner(&self, id: &BufId) -> Option<Own> {
        match self.code(id)? {
            FREE => Some(Own::Free),
            CALLER => Some(Own::Caller),
            _ => Some(Own::Kernel),
        }
    }

    /// The whole buffer. Empty unless the caller holds it.
    pub fn bytes(&self, id: &BufId) -> &[u8] {
        match self.held_span(id) {
            // SAFETY: in the arena, and the caller's, so the kernel holds none of its
            // bytes. A mutable view needs `&mut self`, which this borrow excludes.
            Some((at, len)) => unsafe { core::slice::from_raw_parts(self.mem.at(at), len) },
            None => &[],
        }
    }

    /// The whole buffer to fill. Empty unless the caller holds it.
    pub fn bytes_mut(&mut self, id: &BufId) -> &mut [u8] {
        match self.held_span(id) {
            // SAFETY: as in `bytes`, and `&mut self` makes this the only view.
            Some((at, len)) => unsafe { core::slice::from_raw_parts_mut(self.mem.at(at), len) },
            None => &mut [],
        }
    }

    /// The filled part. `len` is clamped to the buffer. Empty unless the caller holds it.
    pub fn filled(&self, b: &Buf) -> &[u8] {
        let whole = self.bytes(&b.id);
        let n = (b.len as usize).min(whole.len());
        &whole[..n]
    }

    /// Copy the filled part of `src` into `dst` at `at`, as much as fits. Returns bytes
    /// copied, zero unless the caller holds both.
    pub fn copy_into(&mut self, dst: &BufId, at: usize, src: &Buf) -> usize {
        self.copy_range(dst, at, &src.id, 0, src.len as usize)
    }

    /// Copy up to `len` bytes between buffers. Returns bytes copied, zero unless the caller
    /// holds both. Ranges may overlap.
    pub fn copy_range(
        &mut self,
        dst: &BufId,
        dst_at: usize,
        src: &BufId,
        src_at: usize,
        len: usize,
    ) -> usize {
        let (Some((s, sl)), Some((d, dl))) = (self.held_span(src), self.held_span(dst)) else {
            return 0;
        };
        let n = len
            .min(sl.saturating_sub(src_at))
            .min(dl.saturating_sub(dst_at));
        if n > 0 {
            // SAFETY: both ranges lie in caller-held buffers of the arena, clamped above.
            // `copy` allows overlap, and no reference covers either range meanwhile.
            unsafe { core::ptr::copy(self.mem.at(s + src_at), self.mem.at(d + dst_at), n) };
        }
        n
    }

    /// Start of buffer `i` and its size, whoever holds it. For the driver's own `Mem`s.
    pub(crate) fn raw(&self, i: u16) -> Option<(*mut u8, usize)> {
        let (at, len) = self.span(i)?;
        Some((self.mem.at(at), len))
    }

    /// A free buffer straight to an operation.
    pub(crate) fn take_lent(&mut self) -> Result<u16, PoolFull> {
        self.pop(KERNEL)
    }

    /// Hand a caller's buffer to an operation, returning its index. `None` unless the
    /// caller holds it.
    pub(crate) fn lend(&mut self, id: BufId) -> Option<u16> {
        let i = self
            .index(&id)
            .filter(|i| self.code_at(*i) == Some(CALLER))?;
        self.set(i, KERNEL);
        Some(i)
    }

    /// An operation is done with buffer `i` and hands it to the caller.
    pub(crate) fn land(&mut self, i: u16) -> BufId {
        if self.code_at(i) == Some(KERNEL) {
            self.set(i, CALLER);
        }
        self.mint(i)
    }

    /// Free a buffer the caller or an operation holds. False if already free.
    pub(crate) fn free(&mut self, i: u16) -> bool {
        if !self.loaned_at(i) {
            return false;
        }
        self.unlist(i);
        true
    }

    /// Buffer `i` belongs to the provided ring.
    pub(crate) fn ring_at(&self, i: u16) -> bool {
        i < self.reserved
    }

    /// Buffer `i` is out of the free list, held by the caller or the kernel.
    pub(crate) fn loaned_at(&self, i: u16) -> bool {
        self.code_at(i).is_some_and(|c| c != FREE)
    }

    /// Never free the arena. The kernel may still write it.
    pub(crate) fn leak(&mut self) {
        self.mem.leak();
    }

    /// Off the free list, marked `code`.
    fn pop(&mut self, code: u8) -> Result<u16, PoolFull> {
        if self.free_len == 0 {
            return Err(PoolFull);
        }
        self.free_len -= 1;
        let i = self.free[self.free_len as usize];
        self.set(i, code);
        Ok(i)
    }

    /// Mark free, and back on the free list unless a ring buffer.
    fn unlist(&mut self, i: u16) {
        self.set(i, FREE);
        if self.ring_at(i) {
            // The driver returns ring buffers to the ring, not the free list.
            return;
        }
        self.free[self.free_len as usize] = i;
        self.free_len += 1;
    }

    /// An id of this arena only, by its stamp.
    fn mint(&self, i: u16) -> BufId {
        BufId {
            index: i,
            stamp: self.stamp,
        }
    }

    /// The index `id` names here. `None` for one of another arena.
    fn index(&self, id: &BufId) -> Option<u16> {
        (id.stamp == self.stamp && id.index < self.count).then_some(id.index)
    }

    fn code(&self, id: &BufId) -> Option<u8> {
        self.code_at(self.index(id)?)
    }

    fn code_at(&self, i: u16) -> Option<u8> {
        if i >= self.count {
            return None;
        }
        self.state.get(i as usize).copied()
    }

    fn set(&mut self, i: u16, code: u8) {
        if let Some(s) = self.state.get_mut(i as usize) {
            *s = code;
        }
    }

    fn span(&self, i: u16) -> Option<(usize, usize)> {
        if i >= self.count {
            return None;
        }
        Some((i as usize * self.size as usize, self.size as usize))
    }

    fn held_span(&self, id: &BufId) -> Option<(usize, usize)> {
        let i = self.index(id)?;
        if self.code_at(i) != Some(CALLER) {
            return None;
        }
        self.span(i)
    }
}

impl Bufs<'static> {
    /// `count` buffers of `size` bytes, allocated here and freed on drop.
    pub fn owned(count: usize, size: u32) -> Result<Bufs<'static>, IoError> {
        let bytes = count.saturating_mul(size as usize);
        let count = count_of(bytes, size)?;
        Ok(Bufs::start(
            Pinned::new(vec![0u8; bytes].into_boxed_slice()),
            Col::Own(vec![0u16; count].into_boxed_slice()),
            Col::Own(vec![FREE; state_bytes(count)].into_boxed_slice()),
            size,
            count,
        ))
    }
}

/// Buffers of `size` in `bytes`, refused for none or more than an id names.
fn count_of(bytes: usize, size: u32) -> Result<usize, IoError> {
    if size == 0 || bytes < size as usize {
        return Err(IoError::BadArena {
            bytes,
            buffer: size as usize,
        });
    }
    let count = bytes / size as usize;
    if count > MAX_BUFFERS {
        return Err(IoError::TooManySlots(count));
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
