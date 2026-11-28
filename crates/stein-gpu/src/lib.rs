//! A GPU layer shaped like modern hardware, not like 2016 APIs.
//!
//! ```text
//! stein-shell / apps
//!    │ quads, damage                      later: meshes, sort keys, frame graph
//!    ▼
//! quad (the 2D path)
//!    │ Commands (plain data), Slices (gpu pointers)
//!    ▼
//! Gpu: heaps, textures, pipelines, surfaces, submit, timeline
//!    │ replay
//!    ▼
//! vk (TODO, the only unsafe code)  Vulkan 1.3 + buffer device address + one bindless set
//! ```
//!
//! Phases 1 to 3 are built: device, heaps, ring, textures, pipelines,
//! frames, offscreen passes, readback, and presentation to a Wayland
//! surface. Tested headless on llvmpipe with validation on, presentation
//! checked live.
//!
//! # Model
//!
//! Taken from Sebastian Aaltonen's "No Graphics API" (../NoGraphicsAPI/blog.md):
//!
//! - Memory is heaps plus our own suballocation. The default kind is mapped,
//!   so the CPU writes straight into what the GPU reads. No buffer objects.
//! - Shaders follow 64 bit [`GpuPtr`]s. Vertex data, instances, materials and
//!   anything else are structs behind pointers. No vertex layouts.
//! - Each draw or dispatch gets one small root struct ([`GpuLayout`], at most
//!   [`MAX_ROOT`] bytes) holding pointers, texture slots and constants.
//! - Textures are `u32` [`TextureSlot`]s into one global table. No binding
//!   calls, no descriptor sets visible to users.
//! - Barriers name producer and consumer [`Stage`]s plus rare [`Hazard`]s,
//!   never resources. Images live in GENERAL layout.
//! - One timeline counter for CPU and GPU sync, one-shot command recording.
//!
//! Adapted the way ../NoGraphicsAPI adapted it for real Vulkan: the root is
//! copied as push constants rather than passed as a GPU pointer (one fewer
//! dependent load, costs GPU-generated roots), and samplers are a fixed
//! table instead of Metal style inline samplers.
//!
//! Taken from bgfx (../bgfx):
//!
//! - Recording is plain data. [`Commands`] is a flat op list the backend
//!   replays in one pass, like bgfx's frame of render items. Recording needs
//!   no device, runs on any thread and is testable without a GPU.
//! - Resources are small integer [`Handle`]s into device-owned tables.
//! - Per-frame transient data comes from a ring ([`Ring`]), the analogue of
//!   bgfx transient vertex and instance buffers.
//! - TODO(3d): bgfx's 64 bit sort keys (view, blend, program, depth) and
//!   radix sort belong in a 3D layer above this crate, not in it. 2D is
//!   painter's order and must not be reordered.
//!
//! Deliberately not taken from bgfx: uniforms, vertex layouts, per-draw
//! descriptor set allocation with a hash map cache (renderer_vk.cpp does
//! exactly the CPU work the blog argues away), the u64 state word and a
//! dedicated render thread.
//!
//! # Hardware floor
//!
//! Vulkan 1.3 core (dynamic rendering, synchronization2, timeline
//! semaphores, buffer device address, maintenance4) plus descriptor
//! indexing with update-after-bind sampled images. Checked with vulkaninfo
//! on 2026-09-25 on this machine:
//!
//! | device                    | driver         | floor | notable extras                         |
//! |---------------------------|----------------|-------|----------------------------------------|
//! | Intel UHD (Comet Lake)    | Mesa 26.1.5    | yes   | descriptor_buffer, shader_object, host_image_copy, incremental_present |
//! | Quadro P620 (Pascal)      | NVIDIA 580.173 | yes   | same set, 246 MiB mapped VRAM (no ReBAR) |
//! | llvmpipe                  | Mesa 26.1.5    | yes   | mesh_shader, unified_image_layouts     |
//!
//! None expose VK_EXT_descriptor_heap or VK_KHR_device_address_commands, so
//! ../NoGraphicsAPI itself would not run here. All push constant limits are
//! 256 bytes, [`MAX_ROOT`] is 128, the spec's guaranteed minimum.
//!
//! # Phases
//!
//! 1. Done. Headless: device, heaps, ring, textures, pipelines, frames,
//!    offscreen passes, readback. tests/headless.rs runs on llvmpipe by
//!    default and on a real device with `STEIN_GPU_ADAPTER=<index>`.
//! 2. Done. The quad path ([`quad`]) behind `STEIN_GPU` in stein-shell's
//!    paint seam, read back into shm until phase 3, and pixel-compared
//!    against the software painter (stein-shell tests/gpu_paint.rs).
//! 3. Done. Wayland swapchain through VK_KHR_wayland_surface, the whole
//!    frame redrawn and damage passed as the incremental_present hint.
//!    stein-shell presents with it by default (`STEIN_GPU=0` turns it
//!    off), checked live on
//!    all three devices here.
//! 4. Images: more texture slots, RGBA upload (stein-png).
//! 5. TODO(3d): depth, index buffers, instancing, indirect draws, compute.
//! 6. TODO(3d): GPU driven. Multi draw indirect with count, compute culling,
//!    mesh shaders where [`Features::MESH`] says so.
//!
//! # Open questions (research before the backend lands)
//!
//! - Descriptor model: one update-after-bind set (core, simplest) or
//!   VK_EXT_descriptor_buffer (the blog's model, present on all three
//!   devices) or waiting for VK_EXT_descriptor_heap. [`TextureSlot`] hides it.
//! - Pipelines or VK_EXT_shader_object (all three devices have it). Shader
//!   objects remove PSOs entirely, which is the blog's end goal.
//! - Shader source: GLSL with GL_EXT_buffer_reference (glslang in the
//!   flake), Slang, or rust-gpu. Precompiled SPIR-V checked in or built by
//!   build.rs.
//! - Adapter choice and startup: done. stein-shell asks the compositor
//!   for its device (linux-dmabuf feedback) and opens it as
//!   [`Adapter::Drm`], which loads only that device's driver through
//!   VK_LUNARG_direct_driver_loading (loader.rs). Opening the Intel GPU
//!   here went from 78 ms, every driver loaded including NVIDIA's, to 6.
//!   examples/startup.rs times it.
//! - Present: VK_KHR_wayland_surface is what phase 3 uses. Our own
//!   linux-dmabuf buffers with explicit sync (linux-drm-syncobj-v1, which
//!   sway offers here) would give control over buffer count and partial
//!   redraw, at the cost of doing the WSI's work ourselves.
//! - Uploads: the glyph atlas needs no image at all. It is bytes in a
//!   Mapped heap that the fragment shader reads through a pointer, a word
//!   at a time (done in phase 2). What is left is the per-frame copy into
//!   that heap: letting stein-text rasterize straight into the mapping
//!   makes it zero copy with no udmabuf or dmabuf import, which would get
//!   the same through an external memory path unverified on NVIDIA. New
//!   glyphs go into unused space, so in flight frames never see a write;
//!   reusing space waits for the timeline point of the last frame that
//!   drew it. host_image_copy stays the fallback for sampled images.
//! - Persistent suballocation: a TLSF style offset allocator (sebbbi's
//!   OffsetAllocator) for meshes and textures in phase 5.
//! - References: SDL3 GPU, sokol_gfx, Granite, Vello (if we ever need paths),
//!   Frostbite frame graph (for 3D barrier planning).

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

mod cmd;
mod device;
mod error;
mod loader;
mod memory;
mod pipeline;
pub mod quad;
mod surface;
mod sync;
mod table;
mod texture;
#[allow(unsafe_code)]
mod vk;

pub use cmd::{Attachment, Commands, Load, Op, Pass, RootRef, Store, Target};
pub use device::{Adapter, Caps, DeviceKind, Features, Frame, Gpu, GpuDesc};
pub use error::GpuError;
pub use memory::{
    GpuLayout, GpuPtr, GpuRange, Heap, HeapDesc, MemoryKind, RING_GRAIN, Ring, Slice, put,
};
pub use pipeline::{
    Blend, Compare, ComputeDesc, Cull, DepthDesc, Pipeline, RasterDesc, SPIRV_MAGIC, Spirv,
    Topology, spirv_words,
};
pub use surface::{Acquired, PresentMode, Surface, SurfaceDesc, SwapState};
pub use sync::{Hazard, Stage, TimelinePoint};
pub use texture::{Dim, Format, Region, Sampler, Texture, TextureDesc, TextureSlot, Usage};

/// Largest root struct a draw or dispatch takes, in bytes. 128 is the push
/// constant size every Vulkan device guarantees.
pub const MAX_ROOT: usize = 128;

/// Most colour attachments a pass or pipeline has.
pub const MAX_COLOR: usize = 4;

/// Most frames the CPU records ahead of the GPU.
pub const MAX_FRAMES: usize = 3;

/// An index into one of the device's tables. 24 bits of index, 8 of
/// generation, so a handle to a destroyed and reused slot is caught.
///
/// The tables are sized at [`Gpu::open`] from [`GpuDesc`] limits and never
/// grown. A lookup checks the generation and returns [`GpuError::Stale`] on
/// mismatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle(u32);

impl Handle {
    pub const MAX_INDEX: u32 = (1 << 24) - 1;

    /// `index` is masked to 24 bits.
    pub const fn new(index: u32, generation: u8) -> Handle {
        Handle((index & Self::MAX_INDEX) | (generation as u32) << 24)
    }

    pub const fn index(self) -> u32 {
        self.0 & Self::MAX_INDEX
    }

    pub const fn generation(self) -> u8 {
        (self.0 >> 24) as u8
    }
}

#[cfg(test)]
mod tests;
