//! Presentation to a Wayland surface through a swapchain.
//!
//! The swapchain is a small state machine, driven by what acquire and
//! present report:
//!
//! ```text
//!            acquire ok                present ok
//!   Ready ──────────────► Acquired ──────────────► Ready
//!     ▲                      │ suboptimal / out of date
//!     │ resize_surface       ▼
//!     └─────────────── Stale ◄──── acquire or present said so
//! ```
//!
//! Stale is entered on OUT_OF_DATE or SUBOPTIMAL, and the only way out is
//! [`crate::Gpu::resize_surface`]. Acquiring on a stale swapchain is
//! [`crate::GpuError::OutOfDate`]. Lost (surface or device) is terminal and
//! reported as an error.
//!
//! One frame in flight renders into one acquired image:
//!
//! ```text
//! begin_frame ─► acquire ─► record Target::Swapchain ─► submit ─► present
//! ```
//!
//! Submit waits for the image (the frame slot's acquire semaphore), moves
//! it to GENERAL, and back to PRESENT_SRC at the end, signalling the
//! image's present semaphore that present waits on. One present semaphore
//! per image avoids reusing one the presentation engine may still hold.

use crate::Handle;
use crate::texture::Format;

/// FIFO is the only mode Vulkan guarantees and matches stein-shell's frame
/// callback pacing. Mailbox and Immediate fall back to FIFO where missing.
///
/// TODO(research): VK_EXT_swapchain_maintenance1 (on all three devices)
/// switches modes without recreating.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PresentMode {
    #[default]
    Fifo,
    Mailbox,
    Immediate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceDesc {
    /// Tried in order. Bgra8Unorm first: it is the software painter's
    /// pixel order and keeps blending in the same (non linear) space, so
    /// GPU and CPU output compare pixel for pixel.
    pub formats: [Format; 2],
    pub mode: PresentMode,
    /// Swapchain images, clamped to what the surface allows.
    pub images: u8,
    /// Used when the surface does not dictate its own size, which on
    /// Wayland is always.
    pub size: [u32; 2],
    /// The image's alpha is the surface's, premultiplied, so what is under
    /// the surface shows where it is clear. Opaque otherwise.
    pub alpha: bool,
}

impl SurfaceDesc {
    /// BGRA, FIFO, three images, opaque.
    pub const fn new(size: [u32; 2]) -> SurfaceDesc {
        SurfaceDesc {
            formats: [Format::Bgra8Unorm, Format::Rgba8Unorm],
            mode: PresentMode::Fifo,
            images: 3,
            size,
            alpha: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SwapState {
    #[default]
    Ready,
    Acquired,
    Stale,
}

/// A VkSurfaceKHR, its swapchain with a view and present semaphore per
/// image, and the [`SwapState`]. It holds clones of the connection and the
/// wl_surface it was made from, so neither pointer dangles while it
/// exists; the caller must still not destroy the wl_surface before
/// [`crate::Gpu::destroy_surface`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Surface(pub(crate) Handle);

/// This frame's swapchain image. Render into it with
/// [`crate::Target::Swapchain`], then present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acquired {
    pub surface: Surface,
    pub image: u32,
    /// The swapchain's size, which is what the pass renders at.
    pub size: [u32; 2],
    /// The image is usable but the swapchain wants recreating after this
    /// frame.
    pub suboptimal: bool,
}
