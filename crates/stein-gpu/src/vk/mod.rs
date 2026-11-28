//! The Vulkan backend, the crate's only unsafe code.
//!
//! Everything the safe modules ask for lands here as a method on [`Vk`].
//! Each unsafe block carries a SAFETY note. The invariants they lean on:
//!
//! - Every Vulkan object in a table was created from `device` and is
//!   destroyed exactly once, either through the deferred queue once its
//!   timeline point retires or in Drop after the device is idle.
//! - A frame slot's command pool is only reset once the slot's last submit
//!   retired, which begin_frame waits for.
//! - Mapped pointers stay valid for their heap's life, and `&mut self`
//!   ties every slice handed out to a borrow of the device.

use std::sync::atomic::Ordering;

use ash::vk;

use crate::device::{Adapter, Caps, Frame, GpuDesc};
use crate::memory::MemoryKind;
use crate::sync::TimelinePoint;
use crate::table::Slots;
use crate::texture::TextureDesc;
use crate::{Commands, GpuError, Heap, MAX_FRAMES};

mod convert;
mod init;
mod instance;
mod pipeline;
mod present;
mod replay;
mod resources;
mod stamps;

use convert::err;
use init::{Global, MemTypes};
use instance::Inst;
use present::{Pending, SurfRec, SwapUse};

pub(crate) struct HeapRec {
    kind: MemoryKind,
    memory: vk::DeviceMemory,
    /// Null for texture heaps.
    buffer: vk::Buffer,
    base: u64,
    size: u64,
    /// Null unless host visible.
    map: *mut u8,
}

pub(crate) struct TexRec {
    image: vk::Image,
    view: vk::ImageView,
    desc: TextureDesc,
    heap: Heap,
}

pub(crate) struct PipeRec {
    pipeline: vk::Pipeline,
    bind: vk::PipelineBindPoint,
}

/// Destroyed once the timeline passes the point it was queued with.
enum Doomed {
    Heap(HeapRec),
    Texture(TexRec),
    Pipeline(vk::Pipeline),
}

/// One frame in flight.
///
/// ```text
/// Free ──begin_frame──► Recording ──submit──► InFlight(point) ──retired──► Free
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Free,
    Recording,
    InFlight(u64),
}

struct FrameSlot {
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    state: SlotState,
    /// Signalled by an acquire in this slot's frame, waited by its submit.
    /// Null when the device cannot present.
    acquire: vk::Semaphore,
    stamps: stamps::Stamps,
}

pub(crate) struct Vk {
    device: ash::Device,
    pdev: vk::PhysicalDevice,
    queue: vk::Queue,
    family: u32,
    swapchain_fn: Option<ash::khr::swapchain::Device>,
    incremental: bool,
    surfaces: Slots<SurfRec>,
    /// The image acquired in the recording frame, then the one waiting for
    /// present once that frame is submitted.
    pending: Option<Pending>,
    presentable: Option<Pending>,
    types: MemTypes,
    g: Global,
    timeline: vk::Semaphore,
    slots: Vec<FrameSlot>,
    next_slot: u8,
    last_point: u64,
    heaps: Slots<HeapRec>,
    textures: Slots<TexRec>,
    pipelines: Slots<PipeRec>,
    /// Images made since the last submit, still in UNDEFINED layout.
    fresh: Vec<(vk::Image, vk::ImageAspectFlags)>,
    doomed: Vec<(u64, Doomed)>,
    texture_slots: u32,
    dual_source: bool,
    /// Nanoseconds per timestamp tick, 0 without timestamps, and the
    /// valid bits of a stamp.
    stamp_period: f64,
    stamp_mask: u64,
    /// Scratch for query results, and the newest retired frame's stamps.
    stamp_raw: Vec<u64>,
    times: Vec<(u8, u64)>,
    /// Last, so it drops after Drop has destroyed the device.
    inst: Inst,
}

impl Vk {
    /// Open the device `desc` names. For a DRM node, try loading only its
    /// driver first and fall back to the loader's full scan on any failure.
    pub(crate) fn open(desc: &GpuDesc) -> Result<(Vk, Caps), GpuError> {
        if desc.frames == 0 || desc.frames as usize > MAX_FRAMES {
            return Err(GpuError::BadDesc("frames outside 1..=MAX_FRAMES"));
        }
        let only = match desc.adapter {
            Adapter::Drm { major, minor } => crate::loader::candidates(major, minor),
            _ => Vec::new(),
        };
        let _held = instance::loader_lock();
        if !only.is_empty()
            && let Ok(opened) = Vk::open_with(desc, &only)
        {
            return Ok(opened);
        }
        Vk::open_with(desc, &[])
    }

    fn open_with(desc: &GpuDesc, driver: &[std::path::PathBuf]) -> Result<(Vk, Caps), GpuError> {
        let inst = Inst::new(desc.validation, driver)?;
        let picked = instance::pick(&inst, desc.adapter)?;
        // Both texture arrays count against the per stage limit.
        let fits = u64::from(desc.texture_slots) * 2 <= u64::from(picked.max_slots);
        if desc.texture_slots == 0 || !fits {
            return Err(GpuError::BadDesc(
                "texture_slots outside what the device allows",
            ));
        }
        let (device, queue) = init::device(&inst, &picked)?;
        let swapchain_fn = picked
            .swapchain
            .then(|| ash::khr::swapchain::Device::new(&inst.instance, &device));
        let mut vk = Vk {
            device,
            pdev: picked.pdev,
            queue,
            family: picked.family,
            swapchain_fn,
            incremental: picked.incremental,
            surfaces: Slots::new(desc.max_surfaces, "surface"),
            pending: None,
            presentable: None,
            types: MemTypes::default(),
            g: Global::default(),
            timeline: vk::Semaphore::null(),
            slots: Vec::with_capacity(desc.frames as usize),
            next_slot: 0,
            last_point: 0,
            heaps: Slots::new(desc.max_heaps, "heap"),
            textures: Slots::new(desc.max_textures, "texture"),
            pipelines: Slots::new(desc.max_pipelines, "pipeline"),
            fresh: Vec::with_capacity(desc.max_textures as usize),
            doomed: Vec::with_capacity(64),
            texture_slots: desc.texture_slots,
            dual_source: picked.caps.features.has(crate::Features::DUAL_SOURCE),
            stamp_period: f64::from(picked.stamp_period),
            stamp_mask: match picked.stamp_bits {
                0 => 0,
                64.. => u64::MAX,
                b => (1u64 << b) - 1,
            },
            stamp_raw: vec![0; stamps::MAX_STAMPS as usize],
            times: Vec::with_capacity(stamps::MAX_STAMPS as usize),
            inst,
        };
        vk.types = init::memory_types(&vk.inst.instance, picked.pdev, &vk.device)?;
        init::global(&vk.device, desc.texture_slots, &mut vk.g)?;
        vk.timeline = init::timeline(&vk.device)?;
        for _ in 0..desc.frames {
            let (pool, cb) = init::frame_pool(&vk.device, vk.family)?;
            let stamps = match picked.stamp_bits > 0 && picked.stamp_period > 0.0 {
                true => stamps::Stamps::new(&vk.device)?,
                false => stamps::Stamps::default(),
            };
            vk.slots.push(FrameSlot {
                pool,
                cb,
                state: SlotState::Free,
                acquire: vk::Semaphore::null(),
                stamps,
            });
        }
        for s in vk.slots.iter_mut().filter(|_| picked.swapchain) {
            s.acquire = init::binary_semaphore(&vk.device)?;
        }
        let mut caps = picked.caps;
        caps.texture_slots = desc.texture_slots;
        caps.direct = vk.inst.direct();
        Ok((vk, caps))
    }

    pub(crate) fn validation_errors(&self) -> u32 {
        self.inst.errors.load(Ordering::Relaxed)
    }

    pub(crate) fn last_point(&self) -> TimelinePoint {
        TimelinePoint(self.last_point)
    }

    pub(crate) fn completed(&self) -> Result<TimelinePoint, GpuError> {
        // SAFETY: the timeline lives as long as the device.
        let v = unsafe { self.device.get_semaphore_counter_value(self.timeline) }.map_err(err)?;
        Ok(TimelinePoint(v))
    }

    pub(crate) fn wait(&self, point: TimelinePoint, timeout_ns: u64) -> Result<bool, GpuError> {
        let sems = [self.timeline];
        let values = [point.0];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&sems)
            .values(&values);
        // SAFETY: waiting on our own timeline.
        match unsafe { self.device.wait_semaphores(&info, timeout_ns) } {
            Ok(()) => Ok(true),
            Err(vk::Result::TIMEOUT) => Ok(false),
            Err(e) => Err(err(e)),
        }
    }

    pub(crate) fn begin_frame(&mut self) -> Result<Frame, GpuError> {
        if self.slots.iter().any(|s| s.state == SlotState::Recording) {
            return Err(GpuError::State("a frame is already recording"));
        }
        let slot = self.next_slot;
        let s = self
            .slots
            .get(slot as usize)
            .ok_or(GpuError::State("no frame slots"))?;
        if let SlotState::InFlight(p) = s.state {
            self.wait(TimelinePoint(p), u64::MAX)?;
        }
        self.reap()?;
        self.read_stamps()?;
        self.last_point += 1;
        if let Some(s) = self.slots.get_mut(slot as usize) {
            s.state = SlotState::Recording;
        }
        Ok(Frame {
            slot,
            point: TimelinePoint(self.last_point),
        })
    }

    /// The slot of `frame` if it is the one recording.
    fn check_recording(&self, frame: Frame) -> Result<usize, GpuError> {
        let slot = frame.slot as usize;
        let ok = self.slots.get(slot).is_some_and(|s| s.state == SlotState::Recording);
        if !ok || frame.point.0 != self.last_point {
            return Err(GpuError::State("a frame that is not the one recording"));
        }
        Ok(slot)
    }

    /// Replay `lists` into the frame's command buffer and submit it,
    /// signalling the frame's point. On a replay error nothing is submitted
    /// and the frame stays recording, so the caller may fix and resubmit.
    pub(crate) fn submit(&mut self, frame: Frame, lists: &[&Commands]) -> Result<TimelinePoint, GpuError> {
        let slot = self.check_recording(frame)?;
        let swap = self.swap_use(frame.slot)?;
        let (pool, cb) = (self.slots[slot].pool, self.slots[slot].cb);
        // SAFETY: the slot's previous submit retired before begin_frame
        // handed it out, so its pool is idle.
        unsafe {
            self.device.reset_command_pool(pool, vk::CommandPoolResetFlags::empty()).map_err(err)?;
            let begin = vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device.begin_command_buffer(cb, &begin).map_err(err)?;
        }
        self.prologue(cb, swap.as_ref());
        let mut stamps = core::mem::take(&mut self.slots[slot].stamps);
        stamps.reset(&self.device, cb);
        let replayed = lists.iter().try_for_each(|c| replay::replay(self, cb, c, &mut stamps));
        if replayed.is_err() {
            stamps.ids.clear();
        }
        self.slots[slot].stamps = stamps;
        if let Some(u) = swap.as_ref() {
            self.image_barrier(cb, swap_barrier(u, true));
        }
        // SAFETY: cb is recording, end it whether or not replay failed.
        let ended = unsafe { self.device.end_command_buffer(cb) }.map_err(err);
        replayed?;
        ended?;
        self.queue_submit(cb, frame.point.0, swap.as_ref())?;
        self.fresh.clear();
        self.submitted();
        self.slots[slot].state = SlotState::InFlight(frame.point.0);
        self.next_slot = ((slot + 1) % self.slots.len()) as u8;
        Ok(frame.point)
    }

    /// Bind the global set, move new images to GENERAL, and the acquired
    /// swapchain image too.
    fn prologue(&self, cb: vk::CommandBuffer, swap: Option<&SwapUse>) {
        let inits: Vec<vk::ImageMemoryBarrier2> = self
            .fresh
            .iter()
            .map(|&(image, aspect)| {
                vk::ImageMemoryBarrier2::default()
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(image)
                    .subresource_range(whole(aspect))
            })
            .collect();
        let sets = [self.g.set];
        // SAFETY: cb is recording; the set and layout live as long as the
        // device, the images are live and not yet used by any submit.
        unsafe {
            for bind in [vk::PipelineBindPoint::GRAPHICS, vk::PipelineBindPoint::COMPUTE] {
                self.device.cmd_bind_descriptor_sets(cb, bind, self.g.layout, 0, &sets, &[]);
            }
            if !inits.is_empty() {
                let dep = vk::DependencyInfo::default().image_memory_barriers(&inits);
                self.device.cmd_pipeline_barrier2(cb, &dep);
            }
        }
        if let Some(u) = swap {
            self.image_barrier(cb, swap_barrier(u, false));
        }
    }

    fn image_barrier(&self, cb: vk::CommandBuffer, b: vk::ImageMemoryBarrier2<'static>) {
        let bs = [b];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&bs);
        // SAFETY: cb is recording and the image is live.
        unsafe { self.device.cmd_pipeline_barrier2(cb, &dep) };
    }

    fn queue_submit(&self, cb: vk::CommandBuffer, point: u64, swap: Option<&SwapUse>) -> Result<(), GpuError> {
        let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let timeline = vk::SemaphoreSubmitInfo::default()
            .semaphore(self.timeline)
            .value(point)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let mut signal = [timeline; 2];
        let mut wait = [vk::SemaphoreSubmitInfo::default(); 1];
        let (waits, signals) = match swap {
            Some(u) => {
                wait[0] = wait[0]
                    .semaphore(u.acquire)
                    .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT);
                signal[1] = timeline.semaphore(u.present).value(0);
                (1, 2)
            }
            None => (0, 1),
        };
        let submit = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait[..waits])
            .command_buffer_infos(&cbs)
            .signal_semaphore_infos(&signal[..signals])];
        // SAFETY: cb is executable and only this queue uses it; the
        // acquire semaphore was signalled by this frame's acquire.
        unsafe { self.device.queue_submit2(self.queue, &submit, vk::Fence::null()) }.map_err(err)
    }

    /// Destroy what the GPU has finished with.
    fn reap(&mut self) -> Result<(), GpuError> {
        if self.doomed.is_empty() {
            return Ok(());
        }
        let done = self.completed()?.0;
        let dev = &self.device;
        self.doomed
            .extract_if(.., |(p, _)| *p <= done)
            .for_each(|(_, d)| destroy(dev, d));
        Ok(())
    }

    fn defer(&mut self, after: TimelinePoint, d: Doomed) {
        self.doomed.push((after.0, d));
    }
}

fn whole(aspect: vk::ImageAspectFlags) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: aspect,
        base_mip_level: 0,
        level_count: vk::REMAINING_MIP_LEVELS,
        base_array_layer: 0,
        layer_count: vk::REMAINING_ARRAY_LAYERS,
    }
}

/// The acquired image into GENERAL after the acquire wait, which is at
/// COLOR_ATTACHMENT_OUTPUT, or back to PRESENT_SRC once everything in the
/// frame is done with it.
fn swap_barrier(u: &SwapUse, to_present: bool) -> vk::ImageMemoryBarrier2<'static> {
    use vk::AccessFlags2 as A;
    use vk::PipelineStageFlags2 as P;
    let b = vk::ImageMemoryBarrier2::default()
        .image(u.image)
        .subresource_range(whole(vk::ImageAspectFlags::COLOR));
    if to_present {
        b.src_stage_mask(P::ALL_COMMANDS)
            .src_access_mask(A::MEMORY_WRITE)
            .dst_stage_mask(P::NONE)
            .dst_access_mask(A::NONE)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
    } else {
        b.src_stage_mask(P::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(A::NONE)
            .dst_stage_mask(P::ALL_COMMANDS)
            .dst_access_mask(A::MEMORY_READ | A::MEMORY_WRITE)
            .old_layout(u.old)
            .new_layout(vk::ImageLayout::GENERAL)
    }
}

/// Free one object. Its timeline point has retired or the device is idle.
fn destroy(d: &ash::Device, doomed: Doomed) {
    // SAFETY: see the callers, nothing still in flight uses the object.
    unsafe {
        match doomed {
            Doomed::Heap(h) => {
                d.destroy_buffer(h.buffer, None);
                d.free_memory(h.memory, None);
            }
            Doomed::Texture(t) => {
                d.destroy_image_view(t.view, None);
                d.destroy_image(t.image, None);
            }
            Doomed::Pipeline(p) => d.destroy_pipeline(p, None),
        }
    }
}

impl Drop for Vk {
    fn drop(&mut self) {
        // Device teardown unloads driver state too, never beside a scan.
        // Released before the fields drop, Inst's drop takes it again.
        let _held = instance::loader_lock();
        // SAFETY: after wait idle nothing is in flight, so every object can
        // go. Destroying null handles is allowed, which covers a partly
        // built Vk from a failed open.
        let _ = unsafe { self.device.device_wait_idle() };
        self.release_surfaces();
        let d = &self.device;
        let doomed = self.doomed.drain(..).map(|(_, x)| x);
        let pipes = self.pipelines.drain().map(|p| Doomed::Pipeline(p.pipeline));
        let texes = self.textures.drain().map(Doomed::Texture);
        let heaps = self.heaps.drain().map(Doomed::Heap);
        doomed
            .chain(pipes)
            .chain(texes)
            .chain(heaps)
            .for_each(|x| destroy(d, x));
        let g = &self.g;
        // SAFETY: as above, the device is idle and these are ours.
        unsafe {
            self.slots.iter().for_each(|s| {
                d.destroy_query_pool(s.stamps.pool, None);
                d.destroy_command_pool(s.pool, None);
                d.destroy_semaphore(s.acquire, None);
            });
            d.destroy_semaphore(self.timeline, None);
            d.destroy_pipeline_layout(g.layout, None);
            d.destroy_descriptor_pool(g.pool, None);
            d.destroy_descriptor_set_layout(g.set_layout, None);
            g.samplers.iter().for_each(|&s| d.destroy_sampler(s, None));
            d.destroy_device(None);
        }
    }
}
