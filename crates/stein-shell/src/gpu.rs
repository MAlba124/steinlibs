//! The GPU painter. The quads and atlas paint.rs draws, drawn by stein-gpu.
//!
//! ```text
//! present:  Quad[] ──convert──► ring ──► QuadPass ──► swapchain image ──► compositor
//! paint:    Quad[] ──convert──► ring ──► QuadPass ──► target ──download──► shm regions
//!           atlas bytes ──copy──► atlas heap ──read through a pointer──┘
//! ```
//!
//! Solid and glyph quads come out as paint.rs paints them, pixel for pixel
//! up to the blend's rounding, which tests/gpu_paint.rs checks against the
//! software painter through [`GpuPaint::paint`].
//!
//! The shell presents through [`GpuPaint::present`]: the whole frame is
//! redrawn into a swapchain image each time, and the damage only goes to
//! the compositor as a hint. [`GpuPaint::paint`] reads the frame back into
//! a pixel buffer instead, for tests and for devices that cannot present.
//!
//! One frame in flight. begin_frame waits for the previous frame, which is
//! what makes rewriting the one atlas heap and ring partition safe.
//!
//! Image quads sample one RGBA texture in [`IMAGE_SLOT`], uploaded through
//! a staging heap whenever the scene's image atlas changes version.
//!
//! Swapchains belong to the caller, one per wl_surface, and are passed in
//! to [`GpuPaint::present`]; [`GpuPaint::forget`] destroys one before its
//! surface goes.
//!
//! TODO(present): the atlas is copied into its heap every frame. The zero
//! copy form has stein-text rasterize straight into the mapping, which
//! needs its atlas storage to be injectable and its dirty rect to reach
//! here through Scene.

use core::fmt;

use stein_gpu::quad::{GpuQuad, QuadAtlas, QuadKind, QuadPass};
use stein_gpu::{
    Acquired, Adapter, Attachment, Commands, Format, Frame, Gpu, GpuDesc, GpuError, GpuLayout, GpuPtr, GpuRange,
    Hazard, Heap, HeapDesc, Load, MAX_COLOR, MemoryKind, Pass, RING_GRAIN, Region, Ring, Stage,
    Store, Surface, SurfaceDesc, SwapState, Target, Texture, TextureDesc, TextureSlot, Usage,
};
use stein_ui::draw::Quad;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::{
    self, ZwpLinuxDmabufFeedbackV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};

use crate::multi::MultiScene;
use crate::paint::{Atlas, BG, Backdrop, Images};

/// How long a frame may take before the painter gives up on the GPU.
const FRAME_TIMEOUT_NS: u64 = 2_000_000_000;

/// Frames in flight. The painter waits on every frame, so one is enough.
const FRAMES: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintError {
    Gpu(GpuError),
    /// The frame did not finish within FRAME_TIMEOUT_NS.
    Timeout,
    /// The pixel buffer is smaller than the surface it claims to hold.
    ShortBuffer,
}

impl fmt::Display for PaintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gpu(e) => write!(f, "gpu: {e}"),
            Self::Timeout => write!(f, "gpu frame timed out"),
            Self::ShortBuffer => write!(f, "pixel buffer smaller than the surface"),
        }
    }
}

impl std::error::Error for PaintError {}

impl From<GpuError> for PaintError {
    fn from(e: GpuError) -> PaintError {
        PaintError::Gpu(e)
    }
}

/// A buffer heap and its GPU address.
struct Buf {
    heap: Heap,
    base: GpuPtr,
    bytes: u64,
}

/// The texture slot image quads sample.
pub const IMAGE_SLOT: TextureSlot = TextureSlot(0);

/// The image atlas on the GPU and the staging it is uploaded from.
struct ImageTex {
    size: [u32; 2],
    version: u64,
    heap: Heap,
    texture: Texture,
    staging: Buf,
}

/// An upload this frame records before its pass.
#[derive(Clone, Copy)]
struct Upload {
    range: GpuRange,
    texture: Texture,
    size: [u32; 2],
}

/// The render target and the readback it is copied into, for one size.
struct Canvas {
    size: [u32; 2],
    heap: Heap,
    texture: Texture,
    readback: Buf,
}

/// A frame [`GpuPaint::begin_present`] or [`GpuPaint::begin_paint`]
/// began, and where it draws.
#[derive(Clone, Copy, Debug)]
pub struct Begun {
    frame: Frame,
    target: Target,
    size: [u32; 2],
    /// What the quads draw over when nothing was recorded first.
    clear: [f32; 4],
}

/// A frame begun on a swapchain image.
#[derive(Clone, Copy, Debug)]
pub struct Presenting {
    pub begun: Begun,
    image: Acquired,
    remade: bool,
}

/// What a scene records its own GPU work with, for one frame: passes into
/// `cmds` that render into `target`, which the quads then draw over.
///
/// The device is the shell's, the same one every frame, so what a scene
/// makes on it lasts. Frames run one at a time, `frame.slot` is always
/// below `frames`.
pub struct GpuCtx<'a> {
    pub gpu: &'a mut Gpu,
    pub cmds: &'a mut Commands,
    pub frame: Frame,
    pub target: Target,
    pub size: [u32; 2],
    pub format: Format,
    pub frames: u8,
}

/// The quads and atlas of a frame, in their heaps.
struct Staged {
    list: stein_gpu::Slice<GpuQuad>,
    atlas: QuadAtlas,
}

pub struct GpuPaint {
    gpu: Gpu,
    pass: QuadPass,
    ring: Ring,
    ring_heap: Heap,
    atlas: Option<Buf>,
    canvas: Option<Canvas>,
    cmds: Commands,
    images: Option<ImageTex>,
}

impl Drop for GpuPaint {
    fn drop(&mut self) {
        let errors = self.gpu.validation_errors();
        if errors > 0 {
            eprintln!("stein-shell: {errors} vulkan validation errors, see above");
        }
    }
}

impl GpuPaint {
    /// A device by `adapter`, validated in debug builds.
    pub fn open(adapter: Adapter) -> Result<GpuPaint, PaintError> {
        let desc = GpuDesc {
            adapter,
            frames: FRAMES,
            texture_slots: 16,
            ..GpuDesc::default()
        };
        let mut gpu = Gpu::open(&desc)?;
        let pass = QuadPass::new(&mut gpu, Format::Bgra8Unorm)?;
        let (ring_heap, ring) = ring(&mut gpu, 1 << 20)?;
        Ok(GpuPaint {
            gpu,
            pass,
            ring,
            ring_heap,
            atlas: None,
            canvas: None,
            cmds: Commands::with_capacity(16, 256, 1),
            images: None,
        })
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// Whether [`GpuPaint::present`] can work on this device.
    pub fn can_present(&self) -> bool {
        self.gpu.caps().present
    }

    /// Draw `quads` over the background into the next swapchain image of
    /// `wl` at `size` and present it, `damage` as the compositor's hint.
    /// `swap` is `wl`'s swapchain, made on first use and remade when the
    /// size changes or it goes stale. Presenting commits `wl`, so anything
    /// double-buffered the caller set on it rides along.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        conn: &Connection,
        wl: &WlSurface,
        swap: &mut Option<Surface>,
        size: [u32; 2],
        quads: &[Quad],
        atlas: &Atlas<'_>,
        damage: &[[u32; 4]],
    ) -> Result<(), PaintError> {
        match self.begin_present(conn, wl, swap, size)? {
            Some(p) => self.finish_present(p, false, quads, atlas, damage),
            None => Ok(()),
        }
    }

    /// The first half of [`GpuPaint::present`]: a frame begun and the next
    /// swapchain image acquired, none when `size` is empty. Record into it
    /// through [`GpuPaint::ctx`], then [`GpuPaint::finish_present`].
    pub fn begin_present(
        &mut self,
        conn: &Connection,
        wl: &WlSurface,
        swap: &mut Option<Surface>,
        size: [u32; 2],
    ) -> Result<Option<Presenting>, PaintError> {
        self.begin_present_on(conn, wl, swap, size, Backdrop::Opaque)
    }

    /// [`GpuPaint::begin_present`] over `bd`. A clear backdrop presents
    /// with alpha, so what is under the surface shows where nothing is
    /// drawn. The swapchain is remade when that changes.
    pub fn begin_present_on(
        &mut self,
        conn: &Connection,
        wl: &WlSurface,
        swap: &mut Option<Surface>,
        size: [u32; 2],
        bd: Backdrop,
    ) -> Result<Option<Presenting>, PaintError> {
        if size[0] == 0 || size[1] == 0 {
            return Ok(None);
        }
        let (surface, mut remade) = self.surface_for(conn, wl, swap, size, bd)?;
        let frame = self.gpu.begin_frame()?;
        let image = match self.gpu.acquire(surface, frame) {
            Err(GpuError::OutOfDate) => {
                self.gpu.resize_surface(surface, size)?;
                remade = true;
                self.gpu.acquire(surface, frame)?
            }
            other => other?,
        };
        self.cmds.clear();
        Ok(Some(Presenting {
            begun: Begun {
                frame,
                target: Target::Swapchain(surface),
                size: image.size,
                clear: clear_of(bd),
            },
            image,
            remade,
        }))
    }

    /// What a scene records its own passes with, into `b`'s target.
    pub fn ctx(&mut self, b: &Begun) -> GpuCtx<'_> {
        GpuCtx {
            gpu: &mut self.gpu,
            cmds: &mut self.cmds,
            frame: b.frame,
            target: b.target,
            size: b.size,
            format: Format::Bgra8Unorm,
            frames: FRAMES,
        }
    }

    /// Draw `quads` into the image `p` holds and present it. Over what was
    /// recorded into it when `drew`, over the background otherwise.
    pub fn finish_present(
        &mut self,
        p: Presenting,
        drew: bool,
        quads: &[Quad],
        atlas: &Atlas<'_>,
        damage: &[[u32; 4]],
    ) -> Result<(), PaintError> {
        let list = self.stage_quads(p.begun.frame, quads, atlas)?;
        record_quads(&mut self.cmds, &self.pass, &p.begun, list, drew);
        self.gpu.submit(p.begun.frame, &[&self.cmds])?;
        // A new swapchain's first frames are whole, so no hint: the
        // compositor then takes the entire surface as damaged.
        let damage = if p.remade { &[][..] } else { damage };
        match self.gpu.present(p.image, damage) {
            // Rendered and stale, the next frame remakes the swapchain.
            Ok(()) | Err(GpuError::OutOfDate) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Grow the atlas heap and ring to fit, copy the atlas, the image
    /// atlas and `quads` in, and record the image upload.
    fn stage_quads(&mut self, frame: Frame, quads: &[Quad], atlas: &Atlas<'_>) -> Result<Staged, PaintError> {
        self.fit_frame(atlas, quads.len())?;
        self.ring.begin_frame(frame.slot)?;
        let upload = self.prepare_images(&atlas.images)?;
        record_upload(&mut self.cmds, upload);
        let atlas = self.upload_atlas(atlas)?;
        let mapped = self.gpu.mapped_mut(self.ring_heap)?;
        let list = self.ring.push_iter(mapped, quads.iter().map(gpu_quad))?;
        Ok(Staged { list, atlas })
    }

    /// The swapchain for `wl` at `size`, made or remade as needed, and
    /// whether it was.
    fn surface_for(
        &mut self,
        conn: &Connection,
        wl: &WlSurface,
        swap: &mut Option<Surface>,
        size: [u32; 2],
        bd: Backdrop,
    ) -> Result<(Surface, bool), PaintError> {
        let alpha = bd == Backdrop::Clear;
        if let Some(s) = *swap
            && self.gpu.surface_desc(s)?.alpha != alpha
        {
            *swap = None;
            self.gpu.destroy_surface(s)?;
        }
        let Some(s) = *swap else {
            let desc = SurfaceDesc { alpha, ..SurfaceDesc::new(size) };
            let s = self.gpu.create_surface(conn, wl, &desc)?;
            *swap = Some(s);
            return Ok((s, true));
        };
        let (state, extent) = self.gpu.surface_state(s)?;
        let stale = state == SwapState::Stale || extent != size;
        if stale {
            self.gpu.resize_surface(s, size)?;
        }
        Ok((s, stale))
    }

    /// Draw `quads` over the background into a `size` surface and copy
    /// `regions` of it into `pixels`, rows `stride_px` apart. What
    /// paint::paint_regions does, on the GPU.
    pub fn paint(
        &mut self,
        pixels: &mut [u32],
        stride_px: usize,
        size: [u32; 2],
        quads: &[Quad],
        atlas: &Atlas<'_>,
        regions: &[[u32; 4]],
    ) -> Result<(), PaintError> {
        self.paint_on(Backdrop::Opaque, pixels, stride_px, size, quads, atlas, regions)
    }

    /// [`GpuPaint::paint`] over `bd`, what paint::paint_regions_on does.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_on(
        &mut self,
        bd: Backdrop,
        pixels: &mut [u32],
        stride_px: usize,
        size: [u32; 2],
        quads: &[Quad],
        atlas: &Atlas<'_>,
        regions: &[[u32; 4]],
    ) -> Result<(), PaintError> {
        if regions.is_empty() {
            return Ok(());
        }
        if stride_px < size[0] as usize || pixels.len() < stride_px * size[1] as usize {
            return Err(PaintError::ShortBuffer);
        }
        match self.begin_paint_on(size, bd)? {
            Some(b) => self.finish_paint(b, false, pixels, stride_px, quads, atlas, regions),
            None => Ok(()),
        }
    }

    /// The first half of [`GpuPaint::paint`]: a frame begun on an
    /// offscreen canvas of `size`, none when it is empty. Record into it
    /// through [`GpuPaint::ctx`], then [`GpuPaint::finish_paint`]. What a
    /// headless shot of a scene that draws its own passes goes through.
    pub fn begin_paint(&mut self, size: [u32; 2]) -> Result<Option<Begun>, PaintError> {
        self.begin_paint_on(size, Backdrop::Opaque)
    }

    /// [`GpuPaint::begin_paint`] over `bd`. Over a clear backdrop the
    /// pixels come back as premultiplied ARGB.
    pub fn begin_paint_on(&mut self, size: [u32; 2], bd: Backdrop) -> Result<Option<Begun>, PaintError> {
        if size[0] == 0 || size[1] == 0 {
            return Ok(None);
        }
        if self.canvas.as_ref().is_none_or(|c| c.size != size) {
            self.drop_canvas()?;
            self.canvas = Some(canvas(&mut self.gpu, size)?);
        }
        let texture = self.canvas.as_ref().ok_or(PaintError::ShortBuffer)?.texture;
        let frame = self.gpu.begin_frame()?;
        self.cmds.clear();
        Ok(Some(Begun {
            frame,
            target: Target::Texture(texture),
            size,
            clear: clear_of(bd),
        }))
    }

    /// Draw `quads` onto `b`'s canvas, over what was recorded into it when
    /// `drew`, wait, and copy `regions` into `pixels`.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_paint(
        &mut self,
        b: Begun,
        drew: bool,
        pixels: &mut [u32],
        stride_px: usize,
        quads: &[Quad],
        atlas: &Atlas<'_>,
        regions: &[[u32; 4]],
    ) -> Result<(), PaintError> {
        let [w, h] = b.size;
        if stride_px < w as usize || pixels.len() < stride_px * h as usize {
            return Err(PaintError::ShortBuffer);
        }
        let list = self.stage_quads(b.frame, quads, atlas)?;
        let canvas = self.canvas.as_ref().ok_or(PaintError::ShortBuffer)?;
        record_quads(&mut self.cmds, &self.pass, &b, list, drew);
        record_download(&mut self.cmds, canvas);
        let point = self.gpu.submit(b.frame, &[&self.cmds])?;
        if !self.gpu.wait(point, FRAME_TIMEOUT_NS)? {
            return Err(PaintError::Timeout);
        }
        let heap = canvas.readback.heap;
        let texels = self.gpu.mapped_mut(heap)?;
        copy_regions(texels, w as usize, pixels, stride_px, regions);
        Ok(())
    }

    /// Grow the atlas heap and ring to this frame's atlas and quads.
    fn fit_frame(&mut self, atlas: &Atlas<'_>, quads: usize) -> Result<(), PaintError> {
        let need = (atlas.pixels.len() as u64).next_multiple_of(4).max(4);
        if self.atlas.as_ref().is_none_or(|a| a.bytes < need) {
            self.drop_atlas()?;
            self.atlas = Some(buf(
                &mut self.gpu,
                MemoryKind::Mapped,
                need.next_power_of_two(),
            )?);
        }
        let need = (quads * GpuQuad::SIZE) as u64;
        if need > self.ring.partition() {
            self.gpu
                .destroy_heap(self.ring_heap, self.gpu.last_point())?;
            (self.ring_heap, self.ring) = ring(&mut self.gpu, need.next_power_of_two())?;
        }
        Ok(())
    }

    fn drop_canvas(&mut self) -> Result<(), GpuError> {
        let Some(old) = self.canvas.take() else {
            return Ok(());
        };
        let now = self.gpu.last_point();
        self.gpu.destroy_texture(old.texture, now)?;
        self.gpu.destroy_heap(old.heap, now)?;
        self.gpu.destroy_heap(old.readback.heap, now)
    }

    fn drop_atlas(&mut self) -> Result<(), GpuError> {
        match self.atlas.take() {
            Some(old) => self.gpu.destroy_heap(old.heap, self.gpu.last_point()),
            None => Ok(()),
        }
    }

    /// Destroy a swapchain, before the wl_surface it presents to goes.
    pub fn forget(&mut self, swap: Surface) -> Result<(), PaintError> {
        self.gpu.destroy_surface(swap).map_err(PaintError::from)
    }

    /// Stage the image atlas when it changed since the last upload, making
    /// its texture when the size did. The previous frame has finished, so
    /// the staging heap and the slot are free to rewrite.
    fn prepare_images(&mut self, img: &Images<'_>) -> Result<Option<Upload>, PaintError> {
        if !img.is_usable() {
            return Ok(None);
        }
        let current = self.images.as_ref();
        if current.is_some_and(|t| t.size == img.size && t.version == img.version) {
            return Ok(None);
        }
        if current.is_none_or(|t| t.size != img.size) {
            self.drop_images()?;
            let t = image_tex(&mut self.gpu, img.size)?;
            self.gpu.set_slot(IMAGE_SLOT, t.texture)?;
            self.images = Some(t);
        }
        let Some(t) = self.images.as_mut() else {
            return Ok(None);
        };
        let n = img.size[0] as usize * img.size[1] as usize * 4;
        let mapped = self.gpu.mapped_mut(t.staging.heap)?;
        mapped
            .get_mut(..n)
            .ok_or(PaintError::ShortBuffer)?
            .copy_from_slice(&img.pixels[..n]);
        t.version = img.version;
        Ok(Some(Upload {
            range: GpuRange {
                ptr: t.staging.base,
                bytes: n as u64,
            },
            texture: t.texture,
            size: t.size,
        }))
    }

    fn drop_images(&mut self) -> Result<(), GpuError> {
        let Some(old) = self.images.take() else {
            return Ok(());
        };
        let now = self.gpu.last_point();
        self.gpu.destroy_texture(old.texture, now)?;
        self.gpu.destroy_heap(old.heap, now)?;
        self.gpu.destroy_heap(old.staging.heap, now)
    }

    /// Copy the atlas bytes into their heap and say where they are.
    fn upload_atlas(&mut self, atlas: &Atlas<'_>) -> Result<QuadAtlas, PaintError> {
        let Some(buf) = self.atlas.as_ref() else {
            return Ok(QuadAtlas::NONE);
        };
        let n = atlas.pixels.len();
        let expected = atlas.size[0] as usize * atlas.size[1] as usize;
        if n == 0 || n < expected {
            return Ok(QuadAtlas::NONE);
        }
        let (heap, ptr) = (buf.heap, buf.base);
        let mapped = self.gpu.mapped_mut(heap)?;
        mapped
            .get_mut(..n)
            .ok_or(PaintError::ShortBuffer)?
            .copy_from_slice(atlas.pixels);
        Ok(QuadAtlas {
            ptr,
            size: atlas.size,
        })
    }
}

/// A software quad as the shader reads it.
pub fn gpu_quad(q: &Quad) -> GpuQuad {
    GpuQuad {
        rect: [q.rect.min[0], q.rect.min[1], q.rect.max[0], q.rect.max[1]],
        clip: [q.clip.min[0], q.clip.min[1], q.clip.max[0], q.clip.max[1]],
        uv: q.uv,
        color: q.color,
        slot: IMAGE_SLOT,
        kind: if q.is_textured() {
            QuadKind::Coverage
        } else if q.is_image() {
            QuadKind::Image
        } else {
            QuadKind::Solid
        },
        radius: q.shape.radius,
        stroke: q.shape.stroke,
    }
}

/// Draw the quads into `b`'s target, over what a scene drew there when
/// `drew`, over the background otherwise.
fn record_quads(c: &mut Commands, pass: &QuadPass, b: &Begun, s: Staged, drew: bool) {
    let load = if drew {
        c.barrier(Stage::COLOR_OUT, Stage::COLOR_OUT, Hazard::NONE);
        Load::Keep
    } else {
        Load::Clear(b.clear)
    };
    let mut color = [None; MAX_COLOR];
    color[0] = Some(Attachment {
        target: b.target,
        load,
        store: Store::Store,
    });
    c.begin_pass(Pass {
        color,
        depth: None,
        size: b.size,
    });
    pass.record(c, s.list, b.size, s.atlas);
    c.end_pass();
}

/// Read the whole canvas back.
fn record_download(c: &mut Commands, canvas: &Canvas) {
    let [w, h] = canvas.size;
    c.barrier(Stage::COLOR_OUT, Stage::TRANSFER, Hazard::NONE);
    let range = GpuRange {
        ptr: canvas.readback.base,
        bytes: u64::from(w) * u64::from(h) * 4,
    };
    c.download(canvas.texture, Region::d2([0, 0], canvas.size), range);
    c.barrier(Stage::TRANSFER, Stage::HOST, Hazard::NONE);
}

/// Copy the staged image atlas into its texture ahead of the pass.
fn record_upload(c: &mut Commands, upload: Option<Upload>) {
    if let Some(u) = upload {
        c.upload(u.range, u.texture, Region::d2([0, 0], u.size));
        c.barrier(Stage::TRANSFER, Stage::FRAGMENT, Hazard::NONE);
    }
}

fn image_tex(gpu: &mut Gpu, size: [u32; 2]) -> Result<ImageTex, GpuError> {
    let usage = Usage::SAMPLED.with(Usage::COPY_DST);
    let desc = TextureDesc::d2(size, Format::Rgba8Unorm, usage);
    let (bytes, _) = gpu.texture_footprint(&desc)?;
    let heap = gpu.create_heap(&HeapDesc {
        kind: MemoryKind::Textures,
        bytes,
    })?;
    let texture = gpu.create_texture(&desc, heap, 0)?;
    let staging = buf(
        gpu,
        MemoryKind::Mapped,
        u64::from(size[0]) * u64::from(size[1]) * 4,
    )?;
    Ok(ImageTex {
        size,
        version: 0,
        heap,
        texture,
        staging,
    })
}

/// paint.rs's background as a clear colour, or nothing at all.
fn clear_of(bd: Backdrop) -> [f32; 4] {
    let channel = |shift: u32| ((BG >> shift) & 0xff) as f32 / 255.0;
    match bd {
        Backdrop::Opaque => [channel(16), channel(8), channel(0), 1.0],
        Backdrop::Clear => [0.0; 4],
    }
}

/// BGRA bytes are XRGB words on a little endian machine, the shm format.
fn copy_regions(
    texels: &[u8],
    w: usize,
    pixels: &mut [u32],
    stride_px: usize,
    regions: &[[u32; 4]],
) {
    for &[x, y, rw, rh] in regions {
        for row in y as usize..(y + rh) as usize {
            copy_span(texels, w, pixels, stride_px, [x as usize, row, rw as usize]);
        }
    }
}

/// `n` pixels of row `y` from `x`, skipped if either side is short.
fn copy_span(texels: &[u8], w: usize, pixels: &mut [u32], stride_px: usize, [x, y, n]: [usize; 3]) {
    let src = texels.get((y * w + x) * 4..(y * w + x + n) * 4);
    let dst = pixels.get_mut(y * stride_px + x..y * stride_px + x + n);
    if let (Some(src), Some(dst)) = (src, dst) {
        copy_row(src, dst);
    }
}

fn copy_row(src: &[u8], dst: &mut [u32]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
        *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    }
}

fn buf(gpu: &mut Gpu, kind: MemoryKind, bytes: u64) -> Result<Buf, GpuError> {
    let heap = gpu.create_heap(&HeapDesc { kind, bytes })?;
    let base = gpu.heap_base(heap)?;
    Ok(Buf { heap, base, bytes })
}

fn ring(gpu: &mut Gpu, per_frame: u64) -> Result<(Heap, Ring), GpuError> {
    let bytes = per_frame.max(RING_GRAIN) * u64::from(FRAMES);
    let b = buf(gpu, MemoryKind::Mapped, bytes)?;
    let ring = Ring::new(b.heap, 0, b.base, bytes, FRAMES)?;
    Ok((b.heap, ring))
}

fn canvas(gpu: &mut Gpu, size: [u32; 2]) -> Result<Canvas, GpuError> {
    let usage = Usage::COLOR.with(Usage::COPY_SRC);
    let desc = TextureDesc::d2(size, Format::Bgra8Unorm, usage);
    let (bytes, _) = gpu.texture_footprint(&desc)?;
    let heap = gpu.create_heap(&HeapDesc {
        kind: MemoryKind::Textures,
        bytes,
    })?;
    let texture = gpu.create_texture(&desc, heap, 0)?;
    let readback = buf(
        gpu,
        MemoryKind::Readback,
        u64::from(size[0]) * u64::from(size[1]) * 4,
    )?;
    Ok(Canvas {
        size,
        heap,
        texture,
        readback,
    })
}

/// Which device to paint on. The GPU the compositor renders on unless
/// `STEIN_GPU` says otherwise: `0` or `off` for the software painter,
/// `software` for llvmpipe, `gpuN` for the Nth device in Vulkan's order.
/// `1`, `auto` and empty mean the default.
pub fn adapter_from_env(conn: &Connection) -> Option<Adapter> {
    let v = std::env::var("STEIN_GPU").unwrap_or_default();
    match v.as_str() {
        "0" | "off" => None,
        "" | "1" | "auto" => Some(match compositor_device(conn) {
            Some([major, minor]) => Adapter::Drm { major, minor },
            None => Adapter::Auto,
        }),
        "software" => Some(Adapter::Software),
        n => n.strip_prefix("gpu")?.parse().ok().map(Adapter::Index),
    }
}

/// The DRM node the compositor renders on, from linux-dmabuf feedback: its
/// main device, or the first tranche's target from version 6 on. Rendering
/// there means no copy between GPUs, and naming it lets stein-gpu load only
/// that device's driver. None when the compositor has no feedback (dmabuf
/// before version 4) or says nothing usable.
///
/// Runs on its own event queue, so the shell's events wait untouched.
pub fn compositor_device(conn: &Connection) -> Option<[u32; 2]> {
    let mut queue = conn.new_event_queue::<Probe>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue.roundtrip(&mut probe).ok()?;
    let (name, version) = probe.dmabuf?;
    let dmabuf: ZwpLinuxDmabufV1 = registry.bind(name, version.min(5), &qh, ());
    let feedback = dmabuf.get_default_feedback(&qh, ());
    // One roundtrip normally carries the whole feedback; a few more cover a
    // compositor that splits it.
    for _ in 0..4 {
        if probe.done {
            break;
        }
        queue.roundtrip(&mut probe).ok()?;
    }
    feedback.destroy();
    dmabuf.destroy();
    let dev = probe.main.or(probe.tranche)?;
    Some([rustix::fs::major(dev), rustix::fs::minor(dev)])
}

/// What the dmabuf probe has heard so far.
#[derive(Default)]
pub struct Probe {
    /// The zwp_linux_dmabuf_v1 global and its version, at least 4.
    dmabuf: Option<(u32, u32)>,
    main: Option<u64>,
    tranche: Option<u64>,
    done: bool,
}

/// A dev_t as the protocol sends it, native endian.
fn dev_t(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?))
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == ZwpLinuxDmabufV1::interface().name
            && version >= 4
        {
            probe.dmabuf = Some((name, version));
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Probe {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event;
        match event {
            Event::MainDevice { device } => probe.main = dev_t(&device),
            Event::TrancheTargetDevice { device } if probe.tranche.is_none() => probe.tranche = dev_t(&device),
            Event::Done => probe.done = true,
            // The format table's fd closes as the event drops.
            _ => {}
        }
    }
}

/// Paint with the GPU when there is one, the software painter otherwise.
/// A GPU that fails is dropped with a note on stderr and this frame and all
/// later ones fall back to software.
#[allow(clippy::too_many_arguments)]
pub fn paint_frame(
    gpu: &mut Option<GpuPaint>,
    blend: crate::Blend,
    bd: Backdrop,
    pixels: &mut [u32],
    stride_px: usize,
    size: [u32; 2],
    quads: &[Quad],
    atlas: &Atlas<'_>,
    regions: &[[u32; 4]],
) {
    if let Some(g) = gpu.as_mut() {
        match g.paint_on(bd, pixels, stride_px, size, quads, atlas, regions) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("stein-shell: gpu painting failed, falling back to software: {e}");
                *gpu = None;
            }
        }
    }
    crate::paint::paint_regions_on(blend, bd, pixels, stride_px, quads, atlas, regions);
}

/// Whether the shell should present through the GPU this frame.
pub fn presents(gpu: &Option<GpuPaint>) -> bool {
    gpu.as_ref().is_some_and(GpuPaint::can_present)
}

/// Present `scene`'s surface `key` with the GPU, its own passes first if
/// it has any. False when there is none or it just failed, in which case
/// it is dropped with a note on stderr and the caller paints this frame in
/// software.
#[allow(clippy::too_many_arguments)]
pub fn present_frame(
    gpu: &mut Option<GpuPaint>,
    conn: &Connection,
    wl: &WlSurface,
    swap: &mut Option<Surface>,
    size: [u32; 2],
    scene: &mut dyn MultiScene,
    key: u32,
    damage: &[[u32; 4]],
) -> bool {
    let Some(g) = gpu.as_mut() else {
        return false;
    };
    match present_scene(g, conn, wl, swap, size, scene, key, damage) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("stein-shell: gpu present failed, falling back to software: {e}");
            *gpu = None;
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn present_scene(
    g: &mut GpuPaint,
    conn: &Connection,
    wl: &WlSurface,
    swap: &mut Option<Surface>,
    size: [u32; 2],
    scene: &mut dyn MultiScene,
    key: u32,
    damage: &[[u32; 4]],
) -> Result<(), PaintError> {
    let Some(p) = g.begin_present_on(conn, wl, swap, size, scene.backdrop(key))? else {
        return Ok(());
    };
    let drew = scene.draw_gpu(key, &mut g.ctx(&p.begun));
    let (pixels, asize) = scene.atlas();
    let atlas = Atlas {
        pixels,
        size: asize,
        images: scene.images(),
    };
    g.finish_present(p, drew, scene.quads(key), &atlas, damage)
}

/// Open the device [`adapter_from_env`] picks, or none, which means the
/// software painter.
pub fn open_from_env(conn: &Connection) -> Option<GpuPaint> {
    let adapter = adapter_from_env(conn)?;
    match GpuPaint::open(adapter) {
        Ok(g) => {
            let c = g.gpu().caps();
            let drivers = if c.direct { "its driver alone" } else { "every driver" };
            eprintln!("stein-shell: painting on {} ({:?}), loaded {drivers}", c.name, c.kind);
            Some(g)
        }
        Err(e) => {
            eprintln!("stein-shell: no gpu ({e}), painting in software");
            None
        }
    }
}
