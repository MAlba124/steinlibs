//! Stages, hazards and the timeline.
//!
//! A barrier says "work in these stages before, work in those stages after"
//! and nothing about resources. The backend turns it into one global
//! VkMemoryBarrier2. Layout transitions do not exist in the API: images sit
//! in GENERAL (VK_KHR_unified_image_layouts where present makes that free,
//! elsewhere it costs little on current desktop GPUs).

use core::ops::BitOr;

/// Pipeline stages, a bit set.
///
/// Each bit maps to VK_PIPELINE_STAGE_2_* and the access masks follow from
/// the stage (writes for producers, reads and writes for consumers), so
/// callers never spell out access flags. PRESENT maps to nothing until the
/// swapchain lands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Stage(pub u32);

impl Stage {
    pub const NONE: Stage = Stage(0);
    pub const HOST: Stage = Stage(1 << 0);
    pub const TRANSFER: Stage = Stage(1 << 1);
    pub const COMPUTE: Stage = Stage(1 << 2);
    /// Command processor reading indirect arguments and index data.
    pub const INDIRECT: Stage = Stage(1 << 3);
    pub const VERTEX: Stage = Stage(1 << 4);
    pub const FRAGMENT: Stage = Stage(1 << 5);
    pub const COLOR_OUT: Stage = Stage(1 << 6);
    /// Early and late depth/stencil tests.
    pub const DEPTH: Stage = Stage(1 << 7);
    /// The presentation engine, for the swapchain image handoff only.
    pub const PRESENT: Stage = Stage(1 << 8);
    pub const ALL: Stage = Stage((1 << 9) - 1);

    pub const fn contains(self, other: Stage) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Stage {
    type Output = Stage;
    fn bitor(self, rhs: Stage) -> Stage {
        Stage(self.0 | rhs.0)
    }
}

/// The few caches a stage pair does not imply. Most barriers pass NONE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Hazard(pub u32);

impl Hazard {
    pub const NONE: Hazard = Hazard(0);
    /// The texture table was written on the GPU, invalidate descriptor caches.
    pub const DESCRIPTORS: Hazard = Hazard(1 << 0);
    /// Draw or dispatch arguments were written on the GPU, stall the
    /// command processor's prefetch.
    pub const DRAW_ARGS: Hazard = Hazard(1 << 1);
    /// A depth buffer was written by compute, drop HiZ state.
    pub const DEPTH: Hazard = Hazard(1 << 2);
}

impl BitOr for Hazard {
    type Output = Hazard;
    fn bitor(self, rhs: Hazard) -> Hazard {
        Hazard(self.0 | rhs.0)
    }
}

/// A value of the device's single timeline semaphore. Every submit signals
/// the next one, so "has X finished" is `completed >= point`.
///
/// One VkSemaphore of type TIMELINE for the whole device. Frame pacing,
/// deferred destruction, ring reuse and readback all key off it. Split
/// barriers (the blog's signal after / wait before on a GPU address) have
/// no Vulkan 1.3 equivalent worth using, so there are none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelinePoint(pub u64);
