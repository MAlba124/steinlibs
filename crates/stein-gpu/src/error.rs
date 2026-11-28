use core::fmt;

use crate::memory::MemoryKind;

/// Every way a GPU call fails. Driver results are mapped onto these in the
/// backend; only codes with no better home stay as [`GpuError::Vk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuError {
    /// Spec stage: the named call has no backend yet.
    Unbuilt(&'static str),
    /// No device meets the floor described in the crate docs.
    NoAdapter,
    /// A required feature or extension, by its Vulkan name.
    Missing(&'static str),
    OutOfMemory {
        kind: MemoryKind,
        asked: u64,
    },
    /// The ring's free space is used up by frames still in flight. The
    /// caller decides whether to wait on a timeline point or drop work.
    RingFull {
        asked: u64,
        left: u64,
    },
    /// A descriptor struct asks for something inconsistent or over a limit.
    BadDesc(&'static str),
    SlotRange {
        slot: u32,
        slots: u32,
    },
    /// A handle whose slot was destroyed, and maybe reused.
    Stale(&'static str),
    /// A resource table is at the capacity [`crate::GpuDesc`] gave it.
    Full(&'static str),
    /// A call out of order, like a draw outside a pass or two frames
    /// recording at once.
    State(&'static str),
    /// A pointer or range that falls outside every heap.
    Unmapped(u64),
    /// The swapchain no longer matches the surface. Recreate and retry.
    OutOfDate,
    SurfaceLost,
    DeviceLost,
    /// A VkResult with no better mapping.
    Vk(i32),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unbuilt(what) => write!(f, "{what} is not built yet"),
            Self::NoAdapter => write!(f, "no GPU meets the Vulkan 1.3 floor"),
            Self::Missing(name) => write!(f, "device lacks {name}"),
            Self::OutOfMemory { kind, asked } => {
                write!(f, "out of {kind:?} memory, {asked} bytes asked")
            }
            Self::RingFull { asked, left } => {
                write!(f, "ring full, {asked} bytes asked, {left} left")
            }
            Self::BadDesc(why) => write!(f, "bad descriptor, {why}"),
            Self::SlotRange { slot, slots } => {
                write!(f, "texture slot {slot} outside a table of {slots}")
            }
            Self::Stale(what) => write!(f, "stale {what} handle"),
            Self::Full(what) => write!(f, "{what} table full"),
            Self::State(why) => write!(f, "out of order, {why}"),
            Self::Unmapped(ptr) => write!(f, "gpu address {ptr:#x} is in no heap"),
            Self::OutOfDate => write!(f, "swapchain out of date"),
            Self::SurfaceLost => write!(f, "surface lost"),
            Self::DeviceLost => write!(f, "device lost"),
            Self::Vk(code) => write!(f, "vulkan error {code}"),
        }
    }
}

impl std::error::Error for GpuError {}
