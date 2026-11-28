//! Heaps, GPU pointers, the CPU/GPU struct contract and the frame ring.
//!
//! ```text
//! Heap (one VkDeviceMemory + one VkBuffer spanning it, BDA on)
//!   ├── Ring: per-frame partitions, bump, reset when the frame retires
//!   └── TODO(3d): offset allocator for meshes and other long lived data
//! ```
//!
//! No buffer objects in the API. A heap is created once, its base address
//! is fetched once (vkGetBufferDeviceAddress), and every allocation after
//! that is an offset: `gpu = base + at`, `cpu = mapping + at`.

use core::marker::PhantomData;

use crate::{GpuError, Handle};

/// Where a heap lives.
///
/// Memory types are picked once at [`crate::Gpu::open`].
/// - `Mapped`: DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT. UMA on Intel,
///   the 246 MiB BAR heap on the P620. Falls back to HOST_VISIBLE |
///   HOST_COHERENT system memory when that runs out, which the GPU then
///   reads over PCIe.
/// - `GpuOnly`: DEVICE_LOCAL only. Textures and big static data, filled
///   by copy commands so the driver may compress it.
/// - `Readback`: HOST_VISIBLE | HOST_COHERENT | HOST_CACHED. Screenshots,
///   test captures. Coherent is required so no flush or invalidate calls
///   exist in the API.
/// - `Textures`: DEVICE_LOCAL, of a type every image we create accepts.
///   No spanning buffer and no GPU address, images are placed into it at
///   offsets. Kept apart from buffer heaps so a buffer never aliases an
///   optimally tiled image (bufferImageGranularity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    Mapped,
    GpuOnly,
    Readback,
    Textures,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapDesc {
    pub kind: MemoryKind,
    pub bytes: u64,
}

/// One device memory allocation and the buffer that spans it.
///
/// Buffer heaps get one VkBuffer spanning them with every usage bit we
/// need (STORAGE, INDEX, INDIRECT, TRANSFER_SRC/DST, SHADER_DEVICE_ADDRESS)
/// so a pointer into it is usable anywhere. Texture heaps get images bound
/// at offsets, sized up front with vkGetDeviceImageMemoryRequirements (core
/// 1.3), the fix for the "create the object first, then ask where it may
/// live" flaw the blog complains about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Heap(pub(crate) Handle);

/// A GPU virtual address, what shaders dereference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GpuPtr(pub u64);

impl GpuPtr {
    pub const NULL: GpuPtr = GpuPtr(0);

    pub const fn offset(self, bytes: u64) -> GpuPtr {
        GpuPtr(self.0 + bytes)
    }
}

/// An address and a length, for commands that need an extent: index data,
/// indirect arguments, copies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuRange {
    pub ptr: GpuPtr,
    pub bytes: u64,
}

/// `len` values of `T` at byte `at` of `heap`, visible to shaders at `gpu`.
/// Offsets, not references, the same idea as stein-arena's Span.
#[derive(Debug)]
pub struct Slice<T> {
    pub heap: Heap,
    pub at: u64,
    pub len: u32,
    pub gpu: GpuPtr,
    pub _t: PhantomData<T>,
}

impl<T> Clone for Slice<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Slice<T> {}

impl<T: GpuLayout> Slice<T> {
    pub const fn range(&self) -> GpuRange {
        GpuRange {
            ptr: self.gpu,
            bytes: self.len as u64 * T::SIZE as u64,
        }
    }

    /// Address of element `i`, for pointer math in root structs.
    pub const fn at(&self, i: u32) -> GpuPtr {
        self.gpu.offset(i as u64 * T::SIZE as u64)
    }
}

/// The byte layout a type has on the GPU. This is the CPU/GPU contract: the
/// shader's struct must put the same fields at the same offsets, and a test
/// per type pins those offsets.
///
/// Safe by construction. `store` writes little endian fields at fixed
/// offsets into `out`, which is exactly `SIZE` bytes. No transmutes, no
/// padding bytes left uninitialised, no bytemuck. The compiler turns a
/// store of plain fields into a few moves.
pub trait GpuLayout: Copy {
    const SIZE: usize;
    /// Placement alignment in a heap, which must satisfy the shader's
    /// buffer_reference_align.
    const ALIGN: usize;
    fn store(&self, out: &mut [u8]);
}

/// Write `bytes` at `at`, the building block of [`GpuLayout::store`] for
/// user structs. Out of range is skipped rather than panicking, callers
/// always hand over a `SIZE` slice.
#[inline]
pub fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(dst) = out.get_mut(at..at + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

macro_rules! scalar {
    ($($t:ty),*) => {$(
        impl GpuLayout for $t {
            const SIZE: usize = size_of::<$t>();
            const ALIGN: usize = size_of::<$t>();
            #[inline]
            fn store(&self, out: &mut [u8]) {
                put(out, 0, &self.to_le_bytes());
            }
        }
    )*};
}

scalar!(u32, i32, f32, u64);

/// No root at all.
impl GpuLayout for () {
    const SIZE: usize = 0;
    const ALIGN: usize = 4;
    fn store(&self, _: &mut [u8]) {}
}

impl GpuLayout for GpuPtr {
    const SIZE: usize = 8;
    const ALIGN: usize = 8;
    #[inline]
    fn store(&self, out: &mut [u8]) {
        put(out, 0, &self.0.to_le_bytes());
    }
}

impl<T: GpuLayout, const N: usize> GpuLayout for [T; N] {
    const SIZE: usize = T::SIZE * N;
    const ALIGN: usize = T::ALIGN;
    #[inline]
    fn store(&self, out: &mut [u8]) {
        for (v, dst) in self.iter().zip(out.chunks_exact_mut(T::SIZE)) {
            v.store(dst);
        }
    }
}

/// Per-frame transient memory: roots too big to push, quads, instance data,
/// upload staging. bgfx's transient buffers, the blog's GPUBumpAllocator.
///
/// ```text
/// [ frame 0 | frame 1 | frame 2 ]   one Mapped heap range, split evenly
///      ^ head bumps within the current frame's partition
/// ```
///
/// Fixed partitions, one per frame slot. `begin_frame(slot)` is only called
/// once that slot's previous [`crate::TimelinePoint`] has retired, which
/// [`crate::Gpu::begin_frame`] guarantees, so resetting `head` is the whole
/// reclaim. A partition running out is [`GpuError::RingFull`], never a
/// hidden stall or a silent wrap onto live data.
///
/// TODO(ring): measure peak use per frame and size the ring from it. Only
/// if partitions waste too much move to one shared ring whose tail follows
/// retired timeline points.
#[derive(Debug)]
pub struct Ring {
    heap: Heap,
    /// Where the ring starts inside the heap.
    at: u64,
    /// The heap's GPU base address.
    base: GpuPtr,
    partition: u64,
    frames: u8,
    slot: u8,
    head: u64,
}

/// Partitions are multiples of this, so every alignment up to it holds.
pub const RING_GRAIN: u64 = 256;

impl Ring {
    /// `bytes` of `heap` starting at `at`, split across `frames`. `base` is
    /// [`crate::Gpu::heap_base`] of the heap. `at` must be a multiple of
    /// [`RING_GRAIN`].
    pub fn new(
        heap: Heap,
        at: u64,
        base: GpuPtr,
        bytes: u64,
        frames: u8,
    ) -> Result<Ring, GpuError> {
        if frames == 0 || frames as usize > crate::MAX_FRAMES {
            return Err(GpuError::BadDesc("ring frames outside 1..=MAX_FRAMES"));
        }
        if !at.is_multiple_of(RING_GRAIN) {
            return Err(GpuError::BadDesc("ring start not a multiple of RING_GRAIN"));
        }
        let partition = bytes / u64::from(frames) / RING_GRAIN * RING_GRAIN;
        if partition == 0 {
            return Err(GpuError::BadDesc("ring smaller than one grain per frame"));
        }
        Ok(Ring {
            heap,
            at,
            base,
            partition,
            frames,
            slot: 0,
            head: 0,
        })
    }

    pub fn heap(&self) -> Heap {
        self.heap
    }

    /// Bytes per frame.
    pub fn partition(&self) -> u64 {
        self.partition
    }

    /// Bytes used in the current frame.
    pub fn used(&self) -> u64 {
        self.head
    }

    /// Switch to `slot`'s partition and forget what it held.
    pub fn begin_frame(&mut self, slot: u8) -> Result<(), GpuError> {
        if slot >= self.frames {
            return Err(GpuError::BadDesc("ring slot past its frame count"));
        }
        self.slot = slot;
        self.head = 0;
        Ok(())
    }

    /// Room for `bytes` at `align` (a power of two up to [`RING_GRAIN`]) in
    /// the current partition. Returns the heap offset and its GPU address.
    pub fn alloc(&mut self, bytes: u64, align: u64) -> Result<(u64, GpuPtr), GpuError> {
        if !align.is_power_of_two() || align > RING_GRAIN {
            return Err(GpuError::BadDesc(
                "ring alignment not a power of two up to RING_GRAIN",
            ));
        }
        let start = self.head.next_multiple_of(align);
        let end = start.checked_add(bytes).filter(|&e| e <= self.partition);
        let Some(end) = end else {
            return Err(GpuError::RingFull {
                asked: bytes,
                left: self.partition - self.head,
            });
        };
        self.head = end;
        let off = self.at + u64::from(self.slot) * self.partition + start;
        Ok((off, self.base.offset(off)))
    }

    /// Pack `items` into the ring. `mapped` is the heap's mapping from
    /// [`crate::Gpu::mapped_mut`].
    pub fn push<T: GpuLayout>(
        &mut self,
        mapped: &mut [u8],
        items: &[T],
    ) -> Result<Slice<T>, GpuError> {
        self.push_iter(mapped, items.iter().copied())
    }

    /// Pack whatever `items` yields, converting on the way in, so a caller's
    /// own structs go straight into the mapping with no intermediate Vec.
    pub fn push_iter<T, I>(&mut self, mapped: &mut [u8], items: I) -> Result<Slice<T>, GpuError>
    where
        T: GpuLayout,
        I: ExactSizeIterator<Item = T>,
    {
        let len = u32::try_from(items.len())
            .map_err(|_| GpuError::BadDesc("more than u32::MAX items"))?;
        let bytes = u64::from(len) * T::SIZE as u64;
        let (at, gpu) = self.alloc(bytes, T::ALIGN as u64)?;
        let dst = usize::try_from(at)
            .ok()
            .and_then(|a| mapped.get_mut(a..a + bytes as usize))
            .ok_or(GpuError::BadDesc("mapping shorter than the ring"))?;
        store_all(items, dst);
        Ok(Slice {
            heap: self.heap,
            at,
            len,
            gpu,
            _t: PhantomData,
        })
    }
}

fn store_all<T: GpuLayout>(items: impl Iterator<Item = T>, dst: &mut [u8]) {
    if T::SIZE == 0 {
        return;
    }
    for (v, out) in items.zip(dst.chunks_exact_mut(T::SIZE)) {
        v.store(out);
    }
}

#[cfg(test)]
mod tests;
