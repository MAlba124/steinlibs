//! Device creation and the objects that live as long as the device.

use ash::vk;

use super::convert::{err, samplers};
use super::instance::{Inst, Picked};
use crate::{GpuError, MAX_ROOT};

/// Every usage a buffer heap needs, so a pointer into it works anywhere.
pub(super) const BUFFER_USAGE: vk::BufferUsageFlags = vk::BufferUsageFlags::from_raw(
    vk::BufferUsageFlags::STORAGE_BUFFER.as_raw()
        | vk::BufferUsageFlags::INDEX_BUFFER.as_raw()
        | vk::BufferUsageFlags::INDIRECT_BUFFER.as_raw()
        | vk::BufferUsageFlags::TRANSFER_SRC.as_raw()
        | vk::BufferUsageFlags::TRANSFER_DST.as_raw()
        | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS.as_raw(),
);

/// The device with the floor enabled and nothing else, plus its queue.
pub(super) fn device(inst: &Inst, p: &Picked) -> Result<(ash::Device, vk::Queue), GpuError> {
    let prio = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(p.family)
        .queue_priorities(&prio)];
    let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
        .buffer_device_address(true)
        .timeline_semaphore(true)
        .descriptor_indexing(true)
        .runtime_descriptor_array(true)
        .descriptor_binding_partially_bound(true)
        .descriptor_binding_sampled_image_update_after_bind(true)
        .shader_sampled_image_array_non_uniform_indexing(true)
        .storage_buffer8_bit_access(true);
    let mut v13 = vk::PhysicalDeviceVulkan13Features::default()
        .dynamic_rendering(true)
        .synchronization2(true)
        .maintenance4(true);
    let dual = p.caps.features.has(crate::Features::DUAL_SOURCE);
    let base = vk::PhysicalDeviceFeatures::default().dual_src_blend(dual);
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .features(base)
        .push_next(&mut v12)
        .push_next(&mut v13);
    let mut exts = Vec::with_capacity(2);
    if p.swapchain {
        exts.push(ash::khr::swapchain::NAME.as_ptr());
    }
    if p.incremental {
        exts.push(ash::khr::incremental_present::NAME.as_ptr());
    }
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&exts)
        .push_next(&mut f2);
    // SAFETY: the features were all checked supported in `floor`, and every
    // pointer in `info` outlives the call.
    let device = unsafe { inst.instance.create_device(p.pdev, &info, None) }.map_err(err)?;
    // SAFETY: family and index 0 are the ones just created.
    let queue = unsafe { device.get_device_queue(p.family, 0) };
    Ok((device, queue))
}

/// Memory type indices per heap kind, picked once.
#[derive(Default)]
pub(super) struct MemTypes {
    pub(super) mapped: u32,
    /// Plain host memory for when the mapped VRAM window is spent.
    pub(super) mapped_fallback: Option<u32>,
    pub(super) gpu_only: u32,
    pub(super) readback: u32,
    pub(super) textures: u32,
}

pub(super) fn memory_types(
    i: &ash::Instance,
    pdev: vk::PhysicalDevice,
    d: &ash::Device,
) -> Result<MemTypes, GpuError> {
    use vk::MemoryPropertyFlags as M;
    // SAFETY: plain queries, the create infos describe nothing that exists.
    let (props, buf_bits, img_bits) = unsafe {
        let props = i.get_physical_device_memory_properties(pdev);
        let buf = vk::BufferCreateInfo::default()
            .size(1 << 16)
            .usage(BUFFER_USAGE);
        let mut out = vk::MemoryRequirements2::default();
        d.get_device_buffer_memory_requirements(
            &vk::DeviceBufferMemoryRequirements::default().create_info(&buf),
            &mut out,
        );
        let buf_bits = out.memory_requirements.memory_type_bits;
        let img = probe_image();
        let mut out = vk::MemoryRequirements2::default();
        d.get_device_image_memory_requirements(
            &vk::DeviceImageMemoryRequirements::default().create_info(&img),
            &mut out,
        );
        (props, buf_bits, out.memory_requirements.memory_type_bits)
    };
    let types = props.memory_types_as_slice();
    let find = |bits: u32, want: M| {
        (0..types.len() as u32)
            .find(|&t| bits & (1 << t) != 0 && types[t as usize].property_flags.contains(want))
    };
    let host = M::HOST_VISIBLE | M::HOST_COHERENT;
    let missing = GpuError::Missing("a memory type for one of the heap kinds");
    let mapped_fallback = find(buf_bits, host);
    Ok(MemTypes {
        mapped: find(buf_bits, host | M::DEVICE_LOCAL)
            .or(mapped_fallback)
            .ok_or(missing)?,
        mapped_fallback,
        gpu_only: find(buf_bits, M::DEVICE_LOCAL).ok_or(missing)?,
        readback: find(buf_bits, host | M::HOST_CACHED)
            .or(mapped_fallback)
            .ok_or(missing)?,
        textures: find(img_bits, M::DEVICE_LOCAL).ok_or(missing)?,
    })
}

/// A typical texture, to learn which memory types textures accept.
fn probe_image() -> vk::ImageCreateInfo<'static> {
    use vk::ImageUsageFlags as U;
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D {
            width: 64,
            height: 64,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(U::SAMPLED | U::COLOR_ATTACHMENT | U::TRANSFER_SRC | U::TRANSFER_DST)
}

/// The one descriptor set and pipeline layout every pipeline shares:
/// binding 0 the samplers, 1 `texture2D[]`, 2 `texture3D[]`.
#[derive(Default)]
pub(super) struct Global {
    pub(super) samplers: [vk::Sampler; 4],
    pub(super) set_layout: vk::DescriptorSetLayout,
    pub(super) pool: vk::DescriptorPool,
    pub(super) set: vk::DescriptorSet,
    pub(super) layout: vk::PipelineLayout,
}

/// Fill `g` step by step. Whatever was made before a failure stays in `g`
/// for the caller's Drop to destroy.
pub(super) fn global(d: &ash::Device, slots: u32, g: &mut Global) -> Result<(), GpuError> {
    for (s, info) in g.samplers.iter_mut().zip(samplers()) {
        // SAFETY: a plain create on a live device.
        *s = unsafe { d.create_sampler(&info, None) }.map_err(err)?;
    }
    let bindings = [
        vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .stage_flags(vk::ShaderStageFlags::ALL)
            .immutable_samplers(&g.samplers),
        vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(slots)
            .stage_flags(vk::ShaderStageFlags::ALL),
        // Volumes, the same slot index space as binding 1.
        vk::DescriptorSetLayoutBinding::default()
            .binding(2)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(slots)
            .stage_flags(vk::ShaderStageFlags::ALL),
    ];
    let table = vk::DescriptorBindingFlags::PARTIALLY_BOUND
        | vk::DescriptorBindingFlags::UPDATE_AFTER_BIND;
    let flags = [vk::DescriptorBindingFlags::empty(), table, table];
    let mut bf = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&flags);
    let info = vk::DescriptorSetLayoutCreateInfo::default()
        .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
        .bindings(&bindings)
        .push_next(&mut bf);
    // SAFETY: the samplers referenced by `bindings` were just created.
    g.set_layout = unsafe { d.create_descriptor_set_layout(&info, None) }.map_err(err)?;
    let sizes = [
        vk::DescriptorPoolSize {
            ty: vk::DescriptorType::SAMPLER,
            descriptor_count: 4,
        },
        vk::DescriptorPoolSize {
            ty: vk::DescriptorType::SAMPLED_IMAGE,
            descriptor_count: slots * 2,
        },
    ];
    let info = vk::DescriptorPoolCreateInfo::default()
        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
        .max_sets(1)
        .pool_sizes(&sizes);
    // SAFETY: plain creates on a live device from live objects.
    unsafe {
        g.pool = d.create_descriptor_pool(&info, None).map_err(err)?;
        let layouts = [g.set_layout];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(g.pool)
            .set_layouts(&layouts);
        g.set = d
            .allocate_descriptor_sets(&alloc)
            .map_err(err)?
            .first()
            .copied()
            .unwrap_or_default();
        let push = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::ALL,
            offset: 0,
            size: MAX_ROOT as u32,
        }];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        g.layout = d.create_pipeline_layout(&info, None).map_err(err)?;
    }
    Ok(())
}

pub(super) fn timeline(d: &ash::Device) -> Result<vk::Semaphore, GpuError> {
    let mut ty = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let info = vk::SemaphoreCreateInfo::default().push_next(&mut ty);
    // SAFETY: a plain create on a live device.
    unsafe { d.create_semaphore(&info, None) }.map_err(err)
}

pub(super) fn binary_semaphore(d: &ash::Device) -> Result<vk::Semaphore, GpuError> {
    // SAFETY: a plain create on a live device.
    unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }.map_err(err)
}

/// A command pool and its one primary buffer, for one frame slot.
pub(super) fn frame_pool(
    d: &ash::Device,
    family: u32,
) -> Result<(vk::CommandPool, vk::CommandBuffer), GpuError> {
    let info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::TRANSIENT)
        .queue_family_index(family);
    // SAFETY: plain creates on a live device. The pool is destroyed again if
    // its buffer cannot be allocated.
    unsafe {
        let pool = d.create_command_pool(&info, None).map_err(err)?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        match d.allocate_command_buffers(&alloc) {
            Ok(v) if !v.is_empty() => Ok((pool, v[0])),
            Ok(_) => {
                d.destroy_command_pool(pool, None);
                Err(GpuError::Vk(vk::Result::ERROR_UNKNOWN.as_raw()))
            }
            Err(e) => {
                d.destroy_command_pool(pool, None);
                Err(err(e))
            }
        }
    }
}
