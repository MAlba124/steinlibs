//! The device: resource tables, the timeline, frame slots, submit.
//!
//! One `Gpu` per process, owned by whoever runs the frame loop. Every call
//! takes `&mut self` or `&self` on it, there is no global state and no
//! internal locking, like ../NoGraphicsAPI.
//!
//! Frame slots are a state machine, one per frame in flight:
//!
//! ```text
//! Free ──begin_frame──► Recording ──submit──► InFlight(point) ──point retired──► Free
//! ```
//!
//! begin_frame picks the next slot round robin and blocks on its timeline
//! point if it is still in flight. That wait is the only place the CPU ever
//! waits on the GPU in a normal frame, and it bounds how far ahead the CPU
//! runs to `frames`.

use crate::GpuError;
use crate::cmd::Commands;
use crate::memory::{GpuPtr, Heap, HeapDesc};
use crate::pipeline::{ComputeDesc, Pipeline, RasterDesc};
use crate::surface::{Acquired, Surface, SurfaceDesc};
use wayland_client::Connection;
use wayland_client::protocol::wl_surface::WlSurface;
use crate::sync::TimelinePoint;
use crate::texture::{Texture, TextureDesc, TextureSlot};
use crate::vk::Vk;

/// Which physical device to open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Adapter {
    /// Integrated first, it drives the display on a hybrid laptop and
    /// costs less power. Every installed driver is loaded to decide, which
    /// is slow (117 ms here, NVIDIA's 99 of it); prefer [`Adapter::Drm`]
    /// with the compositor's device.
    #[default]
    Auto,
    /// The device behind a DRM node, primary or render, major and minor,
    /// e.g. the compositor's main device from linux-dmabuf feedback. Only
    /// that device's driver is loaded when it can be found (see loader.rs),
    /// which takes 13 ms rather than 117 here; otherwise every driver is.
    Drm { major: u32, minor: u32 },
    /// Position in vkEnumeratePhysicalDevices order.
    Index(u32),
    /// llvmpipe. Deterministic, needs no display, for tests.
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuDesc {
    pub adapter: Adapter,
    /// Enable VK_LAYER_KHRONOS_validation (the flake sets VK_LAYER_PATH),
    /// on by default in debug builds. Errors are counted, see
    /// [`Gpu::validation_errors`].
    /// TODO(backend): GPU assisted validation for buffer device address
    /// bounds, behind a flag, for when a shader chases a bad pointer.
    pub validation: bool,
    /// Frames in flight, 1..=MAX_FRAMES. 2 is the default.
    pub frames: u8,
    /// Size of the global texture table, its 2D and 3D arrays each.
    pub texture_slots: u32,
    /// Table sizes. Fixed at open, never grown.
    pub max_heaps: u32,
    pub max_textures: u32,
    pub max_pipelines: u32,
    pub max_surfaces: u32,
}

impl Default for GpuDesc {
    fn default() -> GpuDesc {
        GpuDesc {
            adapter: Adapter::Auto,
            validation: cfg!(debug_assertions),
            frames: 2,
            texture_slots: 4096,
            max_heaps: 64,
            max_textures: 4096,
            max_pipelines: 256,
            max_surfaces: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    #[default]
    Integrated,
    Discrete,
    Software,
}

/// Optional features, a bit set. The floor itself is not listed, a device
/// without it does not open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Features(pub u32);

impl Features {
    pub const DESCRIPTOR_BUFFER: Features = Features(1 << 0);
    pub const DESCRIPTOR_HEAP: Features = Features(1 << 1);
    pub const SHADER_OBJECT: Features = Features(1 << 2);
    pub const MESH: Features = Features(1 << 3);
    pub const HOST_IMAGE_COPY: Features = Features(1 << 4);
    pub const INCREMENTAL_PRESENT: Features = Features(1 << 5);
    pub const UNIFIED_LAYOUTS: Features = Features(1 << 6);
    pub const DUAL_SOURCE: Features = Features(1 << 7);
    pub const DRAW_INDIRECT_COUNT: Features = Features(1 << 8);
    pub const PRESENT_WAIT: Features = Features(1 << 9);
    pub const MEMORY_BUDGET: Features = Features(1 << 10);

    pub const fn has(self, f: Features) -> bool {
        self.0 & f.0 == f.0
    }
}

/// What the opened device can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub name: String,
    pub kind: DeviceKind,
    /// Vulkan major, minor, patch.
    pub api: [u32; 3],
    /// The device's DRM render and primary nodes, major and minor.
    pub drm_render: Option<[u32; 2]>,
    pub drm_primary: Option<[u32; 2]>,
    /// Bytes of GPU memory the CPU can map. All of it on UMA, the BAR
    /// window on a discrete card without ReBAR (246 MiB on the P620).
    pub mapped_vram: u64,
    pub max_root: u32,
    pub texture_slots: u32,
    pub features: Features,
    /// Whether the validation layer is on. Asked for but not installed
    /// means false, with a warning on stderr.
    pub validation: bool,
    /// Whether only this device's driver was loaded, through
    /// VK_LUNARG_direct_driver_loading, rather than every installed one.
    pub direct: bool,
    /// Whether [`Gpu::create_surface`] can work: the loader has
    /// VK_KHR_wayland_surface and the device VK_KHR_swapchain.
    pub present: bool,
}

/// A frame slot handed out by [`Gpu::begin_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub slot: u8,
    /// The point this frame's submit will signal.
    pub point: TimelinePoint,
}

/// The device.
///
/// Owns the Vulkan backend: the instance, one graphics+compute queue (async
/// compute and a copy queue are 3D era, and
/// ../NoGraphicsAPI/docs/known-driver-issues.md is a list of multi queue
/// driver bugs), the timeline semaphore, a command pool per frame slot,
/// the shared pipeline layout and texture set, the handle tables and a
/// deferred destroy queue keyed by timeline point. Dropping it waits for
/// the GPU to go idle and frees everything.
///
/// Not Send: heap mappings are raw pointers. One thread owns the frame
/// loop, other threads record [`Commands`] and hand them over.
pub struct Gpu {
    caps: Caps,
    vk: Vk,
}

impl core::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gpu")
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

impl Gpu {
    /// Create the instance (with validation when asked and installed), pick
    /// a device that meets the floor by `adapter`, and create it with the
    /// features we use enabled and nothing else.
    pub fn open(desc: &GpuDesc) -> Result<Gpu, GpuError> {
        let (vk, caps) = Vk::open(desc)?;
        Ok(Gpu { caps, vk })
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// Validation errors reported so far. Zero unless validation is on.
    /// Tests assert on it.
    pub fn validation_errors(&self) -> u32 {
        self.vk.validation_errors()
    }

    /// Wait for the next frame slot to retire and claim it, and free what
    /// the GPU has finished with. The caller resets its Ring partition to
    /// the returned slot.
    pub fn begin_frame(&mut self) -> Result<Frame, GpuError> {
        self.vk.begin_frame()
    }

    /// Replay `lists` into the frame's command buffer in order and submit,
    /// signalling the frame's timeline point. On an error nothing was
    /// submitted and the frame is still recording.
    ///
    /// TODO(present): wait on the acquire semaphore and signal the present
    /// semaphore when a list renders to a swapchain image.
    pub fn submit(&mut self, frame: Frame, lists: &[&Commands]) -> Result<TimelinePoint, GpuError> {
        self.vk.submit(frame, lists)
    }

    /// The point the most recent [`Gpu::begin_frame`] handed out. Pass it
    /// to a destroy call to free a resource once everything recorded so far
    /// is done with it.
    pub fn last_point(&self) -> TimelinePoint {
        self.vk.last_point()
    }

    /// The timestamps of the newest frame the GPU finished that recorded
    /// any, as (id, nanoseconds on the GPU clock), in the order recorded.
    /// Read without waiting at [`Gpu::begin_frame`]; empty on a device
    /// without timestamps.
    pub fn timestamps(&self) -> &[(u8, u64)] {
        self.vk.timestamps()
    }

    /// The last timeline point the GPU finished.
    pub fn completed(&self) -> Result<TimelinePoint, GpuError> {
        self.vk.completed()
    }

    /// Block until `point` retires or `timeout_ns` passes. Ok(false) on
    /// timeout.
    pub fn wait(&self, point: TimelinePoint, timeout_ns: u64) -> Result<bool, GpuError> {
        self.vk.wait(point, timeout_ns)
    }
}

/// Memory.
impl Gpu {
    pub fn create_heap(&mut self, desc: &HeapDesc) -> Result<Heap, GpuError> {
        self.vk.create_heap(desc)
    }

    /// The heap's GPU base address. Fetch once and keep it.
    pub fn heap_base(&self, heap: Heap) -> Result<GpuPtr, GpuError> {
        self.vk.heap_base(heap)
    }

    /// The CPU mapping of a Mapped or Readback heap, mapped once at
    /// creation and kept mapped. Coherent, so writes before a submit are
    /// seen by it, and GPU writes are readable once their timeline point
    /// retired (after a barrier to [`crate::Stage::HOST`]).
    pub fn mapped_mut(&mut self, heap: Heap) -> Result<&mut [u8], GpuError> {
        self.vk.mapped_mut(heap)
    }

    /// Freed once `after` retires. A Textures heap must be empty first.
    pub fn destroy_heap(&mut self, heap: Heap, after: TimelinePoint) -> Result<(), GpuError> {
        self.vk.destroy_heap(heap, after)
    }
}

/// Textures.
impl Gpu {
    /// Size and alignment `desc` needs inside a Textures heap.
    pub fn texture_footprint(&self, desc: &TextureDesc) -> Result<(u64, u64), GpuError> {
        self.vk.texture_footprint(desc)
    }

    /// Place a texture at byte `at` of a Textures `heap`. The caller's
    /// allocator picked `at` from [`Gpu::texture_footprint`]. It starts in
    /// GENERAL layout with undefined contents from the next submit on.
    ///
    /// TODO(backend): a convenience that owns one Textures heap and bumps
    /// through it, for 2D where textures are few and live forever.
    pub fn create_texture(
        &mut self,
        desc: &TextureDesc,
        heap: Heap,
        at: u64,
    ) -> Result<Texture, GpuError> {
        self.vk.create_texture(desc, heap, at)
    }

    /// Point `slot` of the texture table at `texture`, in the `texture3D`
    /// array for a [`crate::Dim::D3`] texture and the `texture2D` one
    /// otherwise. Only for slots no in-flight frame reads, or the old
    /// contents may be sampled.
    pub fn set_slot(&mut self, slot: TextureSlot, texture: Texture) -> Result<(), GpuError> {
        self.vk.set_slot(slot, texture)
    }

    /// Freed once `after` retires. Slots still naming it must not be
    /// sampled afterwards.
    pub fn destroy_texture(
        &mut self,
        texture: Texture,
        after: TimelinePoint,
    ) -> Result<(), GpuError> {
        self.vk.destroy_texture(texture, after)
    }
}

/// Pipelines.
impl Gpu {
    /// TODO(backend): a VkPipelineCache persisted under the XDG cache dir
    /// (stein-xdg) so the second start compiles nothing.
    pub fn create_pipeline(&mut self, desc: &RasterDesc<'_>) -> Result<Pipeline, GpuError> {
        self.vk.create_pipeline(desc)
    }

    pub fn create_compute(&mut self, desc: &ComputeDesc<'_>) -> Result<Pipeline, GpuError> {
        self.vk.create_compute(desc)
    }

    pub fn destroy_pipeline(
        &mut self,
        pipeline: Pipeline,
        after: TimelinePoint,
    ) -> Result<(), GpuError> {
        self.vk.destroy_pipeline(pipeline, after)
    }
}

/// Presentation.
impl Gpu {
    /// A swapchain on `surface`. Needs [`Caps::present`] and a queue that
    /// can present to it. Keeps clones of `conn` and `surface` alive.
    pub fn create_surface(&mut self, conn: &Connection, surface: &WlSurface, desc: &SurfaceDesc) -> Result<Surface, GpuError> {
        self.vk.create_surface(conn, surface, desc)
    }

    /// Recreate the swapchain at `size`, the only way out of
    /// [`crate::SwapState::Stale`]. Waits for the device to go idle.
    pub fn resize_surface(&mut self, surface: Surface, size: [u32; 2]) -> Result<(), GpuError> {
        self.vk.resize_surface(surface, size)
    }

    /// Whether the swapchain needs [`Gpu::resize_surface`] before the next
    /// acquire, and the size it was made at.
    pub fn surface_state(&self, surface: Surface) -> Result<(crate::SwapState, [u32; 2]), GpuError> {
        self.vk.surface_state(surface)
    }

    /// What the surface was made with.
    pub fn surface_desc(&self, surface: Surface) -> Result<SurfaceDesc, GpuError> {
        self.vk.surface_desc(surface)
    }

    /// The next swapchain image for `frame`. [`GpuError::OutOfDate`] means
    /// resize and try again in the same frame.
    pub fn acquire(&mut self, surface: Surface, frame: Frame) -> Result<Acquired, GpuError> {
        self.vk.acquire(surface, frame)
    }

    /// Present an image whose frame was submitted, with its damage as
    /// x y w h in pixels. Damage goes out through VK_KHR_incremental_present
    /// when the device has it, a hint the compositor may use to limit its
    /// work. [`GpuError::OutOfDate`] leaves the surface Stale; the frame was
    /// still rendered and the next one resizes.
    pub fn present(&mut self, image: Acquired, damage: &[[u32; 4]]) -> Result<(), GpuError> {
        self.vk.present(image, damage)
    }

    /// Destroy the swapchain and surface now, after waiting for the device
    /// to go idle. Call before destroying the wl_surface.
    pub fn destroy_surface(&mut self, surface: Surface) -> Result<(), GpuError> {
        self.vk.destroy_surface(surface)
    }
}
