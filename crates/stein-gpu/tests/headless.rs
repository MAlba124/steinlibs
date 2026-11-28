//! Phase 1 end to end on llvmpipe with validation on: compute through
//! pointers, copies, texture round trips, offscreen passes, the quad
//! pipeline, the frame state machine and misuse. Every test ends by
//! checking the validation layer reported nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_gpu::quad::{GpuQuad, QuadAtlas, QuadKind, QuadPass};
use stein_gpu::{
    Adapter, Attachment, Commands, ComputeDesc, DeviceKind, Format, Frame, Gpu, GpuDesc, GpuError,
    GpuLayout, GpuPtr, GpuRange, Hazard, Heap, HeapDesc, Load, MAX_COLOR, MemoryKind, Pass, Region,
    Ring, Spirv, Stage, Store, Target, Texture, TextureDesc, TextureSlot, TimelinePoint, Usage,
    put, spirv_words,
};

const SECOND: u64 = 5_000_000_000;

struct Rig {
    gpu: Gpu,
    mapped: Heap,
    ring: Ring,
    readback: Heap,
    readback_base: GpuPtr,
    textures: Heap,
    texture_at: u64,
}

/// llvmpipe unless STEIN_GPU_ADAPTER picks a device by index, so the same
/// suite runs on real hardware too.
fn adapter() -> Adapter {
    match std::env::var("STEIN_GPU_ADAPTER")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(i) => Adapter::Index(i),
        None => Adapter::Software,
    }
}

/// The device with validation, or None (and a note) when this box has no
/// software Vulkan. Anything else failing is a test failure.
fn rig() -> Option<Rig> {
    let desc = GpuDesc {
        adapter: adapter(),
        validation: true,
        ..GpuDesc::default()
    };
    let mut gpu = match Gpu::open(&desc) {
        Ok(g) => g,
        Err(GpuError::NoAdapter) => {
            eprintln!("skipped: no llvmpipe on this machine");
            return None;
        }
        Err(e) => panic!("open: {e}"),
    };
    assert!(
        gpu.caps().validation,
        "validation layer missing, run inside `nix develop` so VK_LAYER_PATH is set"
    );
    let mapped = gpu
        .create_heap(&HeapDesc {
            kind: MemoryKind::Mapped,
            bytes: 1 << 20,
        })
        .unwrap();
    let base = gpu.heap_base(mapped).unwrap();
    let ring = Ring::new(mapped, 0, base, 1 << 20, desc.frames).unwrap();
    let readback = gpu
        .create_heap(&HeapDesc {
            kind: MemoryKind::Readback,
            bytes: 1 << 20,
        })
        .unwrap();
    let readback_base = gpu.heap_base(readback).unwrap();
    let textures = gpu
        .create_heap(&HeapDesc {
            kind: MemoryKind::Textures,
            bytes: 8 << 20,
        })
        .unwrap();
    Some(Rig {
        gpu,
        mapped,
        ring,
        readback,
        readback_base,
        textures,
        texture_at: 0,
    })
}

impl Rig {
    fn frame(&mut self) -> Frame {
        let f = self.gpu.begin_frame().unwrap();
        self.ring.begin_frame(f.slot).unwrap();
        f
    }

    fn run(&mut self, f: Frame, cmds: &Commands) {
        let p = self.gpu.submit(f, &[cmds]).unwrap();
        assert!(self.gpu.wait(p, SECOND).unwrap(), "frame {p:?} timed out");
    }

    fn push<T: GpuLayout>(&mut self, items: &[T]) -> stein_gpu::Slice<T> {
        let mapped = self.gpu.mapped_mut(self.mapped).unwrap();
        self.ring.push(mapped, items).unwrap()
    }

    /// Bump a texture into the textures heap.
    fn texture(&mut self, desc: &TextureDesc) -> Texture {
        let (size, align) = self.gpu.texture_footprint(desc).unwrap();
        let at = self.texture_at.next_multiple_of(align.max(1));
        let t = self.gpu.create_texture(desc, self.textures, at).unwrap();
        self.texture_at = at + size;
        t
    }

    fn readback(&mut self, at: u64, bytes: usize) -> Vec<u8> {
        let m = self.gpu.mapped_mut(self.readback).unwrap();
        m[at as usize..at as usize + bytes].to_vec()
    }

    fn finish(self) {
        assert_eq!(
            self.gpu.validation_errors(),
            0,
            "validation errors, see stderr"
        );
    }
}

#[test]
fn opens_a_device_on_the_floor() {
    let Some(r) = rig() else { return };
    let c = r.gpu.caps();
    if adapter() == Adapter::Software {
        assert_eq!(c.kind, DeviceKind::Software);
    }
    eprintln!("device: {} {:?}", c.name, c.kind);
    assert!(c.api[0] == 1 && c.api[1] >= 3, "api {:?}", c.api);
    assert_eq!(c.max_root, 128);
    assert_eq!(c.texture_slots, GpuDesc::default().texture_slots);
    r.finish();
}

/// The root holds one pointer to params in the ring, which point at the
/// output, also in the ring. Nothing is bound.
#[derive(Clone, Copy)]
struct Params {
    out: GpuPtr,
    count: u32,
    add: u32,
}

impl GpuLayout for Params {
    const SIZE: usize = 16;
    const ALIGN: usize = 8;
    fn store(&self, out: &mut [u8]) {
        put(out, 0, &self.out.0.to_le_bytes());
        put(out, 8, &self.count.to_le_bytes());
        put(out, 12, &self.add.to_le_bytes());
    }
}

#[test]
fn compute_follows_pointers_through_the_ring() {
    let Some(mut r) = rig() else { return };
    let words = spirv_words(include_bytes!("../shaders/spv/fill.comp.spv")).unwrap();
    let fill = r
        .gpu
        .create_compute(&ComputeDesc {
            shader: Spirv {
                words: &words,
                entry: "main",
            },
        })
        .unwrap();
    let f = r.frame();
    let out = r.push(&[0u32; 1000]);
    let params = r.push(&[Params {
        out: out.gpu,
        count: 1000,
        add: 7,
    }]);
    let mut c = Commands::default();
    c.set_pipeline(fill);
    c.dispatch(&params.gpu, [16, 1, 1]);
    c.barrier(Stage::COMPUTE, Stage::HOST, Hazard::NONE);
    r.run(f, &c);
    let m = r.gpu.mapped_mut(r.mapped).unwrap();
    for i in 0..1000usize {
        let at = out.at as usize + i * 4;
        let v = u32::from_le_bytes(m[at..at + 4].try_into().unwrap());
        assert_eq!(v, i as u32 * 3 + 7, "word {i}");
    }
    r.finish();
}

#[test]
fn copies_move_bytes_between_heaps() {
    let Some(mut r) = rig() else { return };
    let gpu_only = r
        .gpu
        .create_heap(&HeapDesc {
            kind: MemoryKind::GpuOnly,
            bytes: 4096,
        })
        .unwrap();
    let gpu_base = r.gpu.heap_base(gpu_only).unwrap();
    let f = r.frame();
    let data: Vec<u32> = (0..256).map(|i| i * 0x0101_0101).collect();
    let src = r.push(&data);
    let mut c = Commands::default();
    c.copy(src.range(), gpu_base.offset(512));
    c.barrier(Stage::TRANSFER, Stage::TRANSFER, Hazard::NONE);
    c.copy(
        GpuRange {
            ptr: gpu_base.offset(512),
            bytes: 1024,
        },
        r.readback_base.offset(64),
    );
    c.barrier(Stage::TRANSFER, Stage::HOST, Hazard::NONE);
    r.run(f, &c);
    let got = r.readback(64, 1024);
    let want: Vec<u8> = data.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(got, want);
    r.gpu.destroy_heap(gpu_only, r.gpu.last_point()).unwrap();
    r.finish();
}

#[test]
fn textures_round_trip_through_upload_and_download() {
    let Some(mut r) = rig() else { return };
    let desc = TextureDesc::d2(
        [16, 8],
        Format::Rgba8Unorm,
        Usage::COPY_DST.with(Usage::COPY_SRC),
    );
    let t = r.texture(&desc);
    let texels: Vec<u32> = (0..16 * 8)
        .map(|i| 0x0102_0304u32.wrapping_mul(i + 1))
        .collect();
    let f = r.frame();
    let src = r.push(&texels);
    let region = Region::d2([0, 0], [16, 8]);
    let mut c = Commands::default();
    c.upload(src.range(), t, region);
    c.barrier(Stage::TRANSFER, Stage::TRANSFER, Hazard::NONE);
    c.download(
        t,
        region,
        GpuRange {
            ptr: r.readback_base,
            bytes: 16 * 8 * 4,
        },
    );
    c.barrier(Stage::TRANSFER, Stage::HOST, Hazard::NONE);
    r.run(f, &c);
    let want: Vec<u8> = texels.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(r.readback(0, want.len()), want);
    r.finish();
}

/// Begin a frame, let `record` fill a fresh BGRA target of `size`, then
/// read the target back.
fn render(
    r: &mut Rig,
    size: [u32; 2],
    record: impl FnOnce(&mut Rig, &mut Commands, Texture),
) -> Vec<u8> {
    let usage = Usage::COLOR.with(Usage::COPY_SRC);
    let target = r.texture(&TextureDesc::d2(size, Format::Bgra8Unorm, usage));
    let f = r.frame();
    let mut c = Commands::default();
    record(r, &mut c, target);
    c.barrier(Stage::COLOR_OUT, Stage::TRANSFER, Hazard::NONE);
    let bytes = u64::from(size[0] * size[1] * 4);
    c.download(
        target,
        Region::d2([0, 0], size),
        GpuRange {
            ptr: r.readback_base,
            bytes,
        },
    );
    c.barrier(Stage::TRANSFER, Stage::HOST, Hazard::NONE);
    r.run(f, &c);
    r.readback(0, bytes as usize)
}

fn pass(target: Texture, size: [u32; 2], load: Load) -> Pass {
    let mut color = [None; MAX_COLOR];
    color[0] = Some(Attachment {
        target: Target::Texture(target),
        load,
        store: Store::Store,
    });
    Pass {
        color,
        depth: None,
        size,
    }
}

fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * w + x) * 4) as usize;
    img[at..at + 4].try_into().unwrap()
}

fn near(got: [u8; 4], want: [u8; 4]) -> bool {
    got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1)
}

#[test]
fn a_cleared_target_reads_back() {
    let Some(mut r) = rig() else { return };
    let img = render(&mut r, [32, 32], |_, c, t| {
        c.begin_pass(pass(t, [32, 32], Load::Clear([1.0, 0.5, 0.0, 1.0])));
        c.end_pass();
    });
    // BGRA in memory.
    for (x, y) in [(0, 0), (31, 31), (7, 20)] {
        assert!(
            near(px(&img, 32, x, y), [0, 128, 255, 255]),
            "{:?}",
            px(&img, 32, x, y)
        );
    }
    r.finish();
}

fn quad(rect: [f32; 4], clip: [f32; 4], color: u32) -> GpuQuad {
    GpuQuad {
        rect,
        clip,
        color,
        ..GpuQuad::default()
    }
}

const ALL: [f32; 4] = [0.0, 0.0, 64.0, 64.0];

/// Solid, clipped, alpha blended, glyph and image quads in one draw. The
/// glyph reads a byte atlas through a pointer, the image a texture slot.
#[test]
fn quads_render_clipped_blended_and_textured() {
    let Some(mut r) = rig() else { return };
    let quads = QuadPass::new(&mut r.gpu, Format::Bgra8Unorm).unwrap();
    let usage = Usage::SAMPLED.with(Usage::COPY_DST);
    let image = r.texture(&TextureDesc::d2([2, 2], Format::Rgba8Unorm, usage));
    r.gpu.set_slot(TextureSlot(3), image).unwrap();
    let img = render(&mut r, [64, 64], |r, c, t| {
        // Magenta everywhere, as RGBA bytes.
        let src = r.push(&[0xFFFF_00FFu32; 4]);
        c.upload(src.range(), image, Region::d2([0, 0], [2, 2]));
        c.barrier(Stage::TRANSFER, Stage::FRAGMENT, Hazard::NONE);
        // A 4x4 byte atlas, the left half of every row covered.
        let bytes = [255u8, 255, 0, 0].repeat(4);
        let words: Vec<u32> = bytes
            .chunks(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        let atlas = QuadAtlas {
            ptr: r.push(&words).gpu,
            size: [4, 4],
        };
        let glyph = GpuQuad {
            uv: [0.0, 0.0, 1.0, 1.0],
            kind: QuadKind::Coverage,
            ..quad([0.0, 40.0, 4.0, 44.0], ALL, 0x00FF_FFFF)
        };
        let picture = GpuQuad {
            uv: [0.0, 0.0, 1.0, 1.0],
            slot: TextureSlot(3),
            kind: QuadKind::Image,
            ..quad([48.0, 48.0, 56.0, 56.0], ALL, 0xFFFF_FFFF)
        };
        let list = r.push(&[
            quad([8.0, 8.0, 24.0, 24.0], ALL, 0xFFFF_0000),
            quad([30.0, 0.0, 60.0, 40.0], [40.0, 10.0, 50.0, 20.0], 0xFF00_FF00),
            quad([16.0, 16.0, 24.0, 24.0], ALL, 0x8000_00FF),
            glyph,
            picture,
        ]);
        c.begin_pass(pass(t, [64, 64], Load::Clear([0.0, 0.0, 0.0, 1.0])));
        quads.record(c, list, [64, 64], atlas);
        c.end_pass();
    });
    let at = |x, y| px(&img, 64, x, y);
    // BGRA in memory.
    let checks = [
        ((10, 10), [0, 0, 255, 255], "red"),
        ((20, 20), [128, 0, 127, 255], "blend"),
        ((45, 15), [0, 255, 0, 255], "clipped green"),
        ((35, 5), [0, 0, 0, 255], "outside the clip"),
        ((45, 25), [0, 0, 0, 255], "below the clip"),
        ((1, 42), [255, 255, 255, 255], "covered texel"),
        ((3, 42), [0, 0, 0, 255], "uncovered texel"),
        ((6, 42), [0, 0, 0, 255], "past the glyph"),
        ((50, 50), [255, 0, 255, 255], "image"),
    ];
    for ((x, y), want, what) in checks {
        assert!(near(at(x, y), want), "{what} at {x},{y}: {:?}", at(x, y));
    }
    r.finish();
}

#[test]
fn frames_are_a_state_machine() {
    let Some(mut r) = rig() else { return };
    let f = r.gpu.begin_frame().unwrap();
    assert_eq!(
        r.gpu.begin_frame(),
        Err(GpuError::State("a frame is already recording"))
    );
    let stale = Frame {
        slot: f.slot,
        point: TimelinePoint(f.point.0 + 1),
    };
    assert!(matches!(r.gpu.submit(stale, &[]), Err(GpuError::State(_))));
    let mut last = r.gpu.submit(f, &[]).unwrap();
    // More frames than slots, so begin_frame has to wait on retired slots.
    for _ in 0..7 {
        let f = r.gpu.begin_frame().unwrap();
        assert_eq!(f.point.0, last.0 + 1);
        last = r.gpu.submit(f, &[]).unwrap();
    }
    assert!(r.gpu.wait(last, SECOND).unwrap());
    assert!(r.gpu.completed().unwrap() >= last);
    assert_eq!(r.gpu.last_point(), last);
    r.finish();
}

#[test]
fn misuse_is_refused_and_the_frame_can_retry() {
    let Some(mut r) = rig() else { return };
    let words = spirv_words(include_bytes!("../shaders/spv/fill.comp.spv")).unwrap();
    let fill = r
        .gpu
        .create_compute(&ComputeDesc {
            shader: Spirv {
                words: &words,
                entry: "main",
            },
        })
        .unwrap();
    let f = r.frame();
    let mut bad = Commands::default();
    bad.dispatch(&GpuPtr(0), [1, 1, 1]);
    assert!(
        matches!(r.gpu.submit(f, &[&bad]), Err(GpuError::State(_))),
        "no pipeline bound"
    );

    let mut bad = Commands::default();
    bad.copy(
        GpuRange {
            ptr: GpuPtr(8),
            bytes: 4,
        },
        r.readback_base,
    );
    assert_eq!(r.gpu.submit(f, &[&bad]), Err(GpuError::Unmapped(8)));

    let t = r.texture(&TextureDesc::d2(
        [4, 4],
        Format::Rgba8Unorm,
        Usage::COPY_DST,
    ));
    r.gpu.destroy_texture(t, r.gpu.last_point()).unwrap();
    let mut bad = Commands::default();
    bad.download(
        t,
        Region::d2([0, 0], [4, 4]),
        GpuRange {
            ptr: r.readback_base,
            bytes: 64,
        },
    );
    assert_eq!(r.gpu.submit(f, &[&bad]), Err(GpuError::Stale("texture")));
    assert_eq!(
        r.gpu.set_slot(TextureSlot(0), t),
        Err(GpuError::Stale("texture"))
    );

    // The frame is still recording, a good list goes through.
    let out = r.push(&[0u32; 4]);
    let params = r.push(&[Params {
        out: out.gpu,
        count: 4,
        add: 1,
    }]);
    let mut good = Commands::default();
    good.set_pipeline(fill);
    good.dispatch(&params.gpu, [1, 1, 1]);
    good.barrier(Stage::COMPUTE, Stage::HOST, Hazard::NONE);
    r.run(f, &good);
    let m = r.gpu.mapped_mut(r.mapped).unwrap();
    let at = out.at as usize;
    assert_eq!(&m[at..at + 8], &[1, 0, 0, 0, 4, 0, 0, 0]);
    r.finish();
}

#[test]
fn resources_are_freed_after_their_point() {
    let Some(mut r) = rig() else { return };
    let t = r.texture(&TextureDesc::d2([8, 8], Format::Rgba8Unorm, Usage::SAMPLED));
    r.gpu.set_slot(TextureSlot(0), t).unwrap();
    assert!(matches!(
        r.gpu.destroy_heap(r.textures, TimelinePoint(0)),
        Err(GpuError::State(_))
    ));
    assert!(matches!(
        r.gpu.set_slot(TextureSlot(1 << 20), t),
        Err(GpuError::SlotRange { .. })
    ));
    let f = r.frame();
    r.run(f, &Commands::default());
    r.gpu.destroy_texture(t, r.gpu.last_point()).unwrap();
    r.gpu.destroy_heap(r.textures, r.gpu.last_point()).unwrap();
    // The next frames reap them once the point has retired.
    for _ in 0..3 {
        let f = r.frame();
        r.run(f, &Commands::default());
    }
    assert_eq!(r.gpu.heap_base(r.textures), Err(GpuError::Stale("heap")));
    r.finish();
}

/// A device named by its DRM node opens with only its own driver loaded,
/// and matches through either node. Skipped where no device has one.
#[test]
fn a_drm_node_loads_only_its_driver() {
    let open = |adapter| {
        let desc = GpuDesc { adapter, validation: true, ..GpuDesc::default() };
        Gpu::open(&desc)
    };
    let Ok(any) = open(Adapter::Auto) else { return };
    let (Some(render), Some(primary)) = (any.caps().drm_render, any.caps().drm_primary) else {
        eprintln!("skipped: {} has no DRM nodes", any.caps().name);
        return;
    };
    let name = any.caps().name.clone();
    assert!(!any.caps().direct, "Auto scans every driver");
    drop(any);
    for [major, minor] in [render, primary] {
        let g = open(Adapter::Drm { major, minor }).unwrap();
        assert_eq!(g.caps().name, name);
        assert!(g.caps().direct, "{major}:{minor} loaded every driver");
        assert_eq!(g.validation_errors(), 0);
    }
}

/// The volume test kernel's root: points and results, a slot, a count.
#[derive(Clone, Copy)]
struct VolumeRoot {
    points: GpuPtr,
    values: GpuPtr,
    slot: u32,
    count: u32,
}

impl GpuLayout for VolumeRoot {
    const SIZE: usize = 24;
    const ALIGN: usize = 8;
    fn store(&self, out: &mut [u8]) {
        put(out, 0, &self.points.0.to_le_bytes());
        put(out, 8, &self.values.0.to_le_bytes());
        put(out, 16, &self.slot.to_le_bytes());
        put(out, 20, &self.count.to_le_bytes());
    }
}

const VOL: usize = 4;

/// Texel (x, y, z) of the test volume, x fastest.
fn texel_at(v: &[[u8; 4]], x: usize, y: usize, z: usize) -> [u8; 4] {
    v[(z * VOL + y) * VOL + x]
}

/// What a linear clamp sampler reads at `p` in 0..1, per channel 0..1.
fn trilinear(v: &[[u8; 4]], p: [f32; 3]) -> [f32; 4] {
    let t = p.map(|c| (c * VOL as f32 - 0.5).clamp(0.0, VOL as f32 - 1.0));
    let lo = t.map(|c| (c.floor() as usize).min(VOL - 2));
    let f = [0, 1, 2].map(|a| t[a] - lo[a] as f32);
    let mut out = [0.0; 4];
    for corner in 0..8usize {
        let pick = [corner & 1, (corner >> 1) & 1, (corner >> 2) & 1];
        let w: f32 = (0..3)
            .map(|a| if pick[a] == 1 { f[a] } else { 1.0 - f[a] })
            .product();
        let c = texel_at(v, lo[0] + pick[0], lo[1] + pick[1], lo[2] + pick[2]);
        for ch in 0..4 {
            out[ch] += w * f32::from(c[ch]) / 255.0;
        }
    }
    out
}

/// The test volume's texels: r, g, b step with x, y, z, alpha counts
/// down, and a 2x2x2 box at `BOX` is 200 throughout.
fn volume_texels() -> Vec<[u8; 4]> {
    let mut texels = vec![[0u8; 4]; VOL * VOL * VOL];
    for (i, t) in texels.iter_mut().enumerate() {
        let (x, y, z) = (i % VOL, i / VOL % VOL, i / (VOL * VOL));
        let inside = (0..3).all(|a| (BOX[a]..BOX[a] + 2).contains(&[x, y, z][a]));
        *t = match inside {
            true => [200; 4],
            false => [(x * 60) as u8, (y * 60) as u8, (z * 60) as u8, 255 - i as u8],
        };
    }
    texels
}

const BOX: [usize; 3] = [2, 0, 1];

/// Points to sample: the middle, texel centres, between two in z, a
/// corner that clamps.
const POINTS: [[f32; 4]; 6] = [
    [0.5, 0.5, 0.5, 0.0],
    [1.5 / 4.0, 1.5 / 4.0, 1.5 / 4.0, 0.0],
    [3.5 / 4.0, 1.5 / 4.0, 2.5 / 4.0, 0.0],
    [0.3, 0.7, 0.45, 0.0],
    [1.5 / 4.0, 1.5 / 4.0, 2.0 / 4.0, 0.0],
    [0.0, 1.0, 0.9, 0.0],
];

/// Upload `vol` whole from what the box leaves, then the box itself,
/// read it back and sample it at `POINTS` through `slot`.
fn record_volume(r: &mut Rig, c: &mut Commands, vol: Texture, slot: u32) -> stein_gpu::Slice<[f32; 4]> {
    let words = spirv_words(include_bytes!("../shaders/spv/volume.comp.spv")).unwrap();
    let shader = Spirv { words: &words, entry: "main" };
    let kernel = r.gpu.create_compute(&ComputeDesc { shader }).unwrap();
    let n = VOL as u32;
    let mut whole = volume_texels();
    for (i, t) in whole.iter_mut().enumerate() {
        let (x, y, z) = (i % VOL, i / VOL % VOL, i / (VOL * VOL));
        *t = [(x * 60) as u8, (y * 60) as u8, (z * 60) as u8, 255 - i as u8];
    }
    let words: Vec<u32> = whole.iter().map(|t| u32::from_le_bytes(*t)).collect();
    let src = r.push(&words);
    let patch = r.push(&[u32::from_le_bytes([200; 4]); 8]);
    let pts = r.push(&POINTS);
    let out = r.push(&[[0f32; 4]; 6]);
    c.barrier(Stage::COMPUTE, Stage::TRANSFER, Hazard::NONE);
    c.upload(src.range(), vol, Region::d3([0, 0, 0], [n, n, n]));
    c.barrier(Stage::TRANSFER, Stage::TRANSFER, Hazard::NONE);
    c.upload(patch.range(), vol, Region::d3(BOX.map(|v| v as u32), [2, 2, 2]));
    c.barrier(Stage::TRANSFER, Stage::COMPUTE | Stage::TRANSFER, Hazard::NONE);
    let bytes = u64::from(n * n * n * 4);
    c.download(vol, Region::d3([0, 0, 0], [n, n, n]), GpuRange { ptr: r.readback_base, bytes });
    c.set_pipeline(kernel);
    let root = VolumeRoot { points: pts.gpu, values: out.gpu, slot, count: POINTS.len() as u32 };
    c.dispatch(&root, [1, 1, 1]);
    c.barrier(Stage::COMPUTE | Stage::TRANSFER, Stage::HOST, Hazard::NONE);
    out
}

/// A 4x4x4 RGBA8 volume uploaded whole, then a 2x2x2 box at an offset,
/// reads back texel for texel and samples trilinearly through its slot
/// in the texture3D array, next to a 2D texture in the same slot space.
#[test]
fn volumes_upload_by_region_and_sample_trilinear() {
    let Some(mut r) = rig() else { return };
    let usage = Usage::SAMPLED.with(Usage::COPY_DST).with(Usage::COPY_SRC);
    let n = VOL as u32;
    let vol = r.texture(&TextureDesc::d3([n, n, n], Format::Rgba8Unorm, usage));
    let flat = r.texture(&TextureDesc::d2([2, 2], Format::Rgba8Unorm, Usage::SAMPLED));
    r.gpu.set_slot(TextureSlot(5), vol).unwrap();
    r.gpu.set_slot(TextureSlot(6), flat).unwrap();
    let f = r.frame();
    let mut c = Commands::default();
    let out = record_volume(&mut r, &mut c, vol, 5);
    r.run(f, &c);
    let texels = volume_texels();
    let want: Vec<u8> = texels.iter().flatten().copied().collect();
    assert_eq!(r.readback(0, want.len()), want, "the volume round trip");
    let m = r.gpu.mapped_mut(r.mapped).unwrap();
    for (i, p) in POINTS.iter().enumerate() {
        let at = out.at as usize + i * 16;
        let got: Vec<f32> = m[at..at + 16]
            .chunks(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let want = trilinear(&texels, [p[0], p[1], p[2]]);
        for ch in 0..4 {
            assert!(
                (got[ch] - want[ch]).abs() <= 2.0 / 255.0,
                "point {i} {p:?} channel {ch}: got {got:?}, want {want:?}"
            );
        }
    }
    r.finish();
}

#[test]
fn a_volume_needs_depth_and_one_layer() {
    let Some(r) = rig() else { return };
    let usage = Usage::SAMPLED;
    let flat = TextureDesc::d3([4, 4, 0], Format::Rgba8Unorm, usage);
    assert!(matches!(r.gpu.texture_footprint(&flat), Err(GpuError::BadDesc(_))));
    let layered = TextureDesc {
        layers: 2,
        ..TextureDesc::d3([4, 4, 4], Format::Rgba8Unorm, usage)
    };
    assert!(matches!(r.gpu.texture_footprint(&layered), Err(GpuError::BadDesc(_))));
    let (deep, _) = r
        .gpu
        .texture_footprint(&TextureDesc::d3([4, 4, 8], Format::Rgba8Unorm, usage))
        .unwrap();
    let (shallow, _) = r
        .gpu
        .texture_footprint(&TextureDesc::d3([4, 4, 1], Format::Rgba8Unorm, usage))
        .unwrap();
    assert!(deep >= 4 * 4 * 8 * 4 && deep > shallow, "{deep} against {shallow}");
    r.finish();
}

/// Two stamps around a dispatch come back ordered and plausible once the
/// frame retired, read at the next begin_frame without waiting.
#[test]
fn timestamps_bracket_a_dispatch() {
    let Some(mut r) = rig() else { return };
    let words = spirv_words(include_bytes!("../shaders/spv/fill.comp.spv")).unwrap();
    let shader = Spirv { words: &words, entry: "main" };
    let fill = r.gpu.create_compute(&ComputeDesc { shader }).unwrap();
    let f = r.frame();
    let out = r.push(&[0u32; 4096]);
    let params = r.push(&[Params { out: out.gpu, count: 4096, add: 1 }]);
    let mut c = Commands::default();
    c.timestamp(7);
    c.set_pipeline(fill);
    c.dispatch(&params.gpu, [64, 1, 1]);
    c.timestamp(9);
    r.run(f, &c);
    assert!(r.gpu.timestamps().is_empty(), "stamps before any frame retired");
    let f = r.frame();
    let stamps = r.gpu.timestamps().to_vec();
    r.run(f, &Commands::default());
    if stamps.is_empty() {
        eprintln!("skipped: {} has no timestamps", r.gpu.caps().name);
        r.finish();
        return;
    }
    assert_eq!(stamps.iter().map(|s| s.0).collect::<Vec<_>>(), [7, 9]);
    let took = stamps[1].1 - stamps[0].1;
    assert!(stamps[1].1 >= stamps[0].1 && took < SECOND, "{stamps:?}");
    r.finish();
}
