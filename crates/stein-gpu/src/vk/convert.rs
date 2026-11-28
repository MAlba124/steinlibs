//! Our enums and descriptors to Vulkan's. No calls, no unsafe.

use ash::vk;

use crate::GpuError;
use crate::pipeline::{Blend, Compare, Cull, Topology};
use crate::sync::{Hazard, Stage};
use crate::texture::{Dim, Format, TextureDesc, Usage};

pub(super) fn format(f: Format) -> vk::Format {
    match f {
        Format::R8Unorm => vk::Format::R8_UNORM,
        Format::Rg8Unorm => vk::Format::R8G8_UNORM,
        Format::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        Format::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
        Format::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        Format::Bgra8Srgb => vk::Format::B8G8R8A8_SRGB,
        Format::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
        Format::R32Float => vk::Format::R32_SFLOAT,
        Format::D32Float => vk::Format::D32_SFLOAT,
        Format::D24UnormS8 => vk::Format::D24_UNORM_S8_UINT,
    }
}

/// Every aspect the format has, for layout transitions.
pub(super) fn aspects(f: Format) -> vk::ImageAspectFlags {
    match f {
        Format::D32Float => vk::ImageAspectFlags::DEPTH,
        Format::D24UnormS8 => vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL,
        _ => vk::ImageAspectFlags::COLOR,
    }
}

/// The aspect views and copies use: depth only for depth formats.
pub(super) fn view_aspect(f: Format) -> vk::ImageAspectFlags {
    if f.is_depth() {
        vk::ImageAspectFlags::DEPTH
    } else {
        vk::ImageAspectFlags::COLOR
    }
}

fn usage(u: Usage) -> vk::ImageUsageFlags {
    let pairs = [
        (Usage::SAMPLED, vk::ImageUsageFlags::SAMPLED),
        (Usage::STORAGE, vk::ImageUsageFlags::STORAGE),
        (Usage::COLOR, vk::ImageUsageFlags::COLOR_ATTACHMENT),
        (Usage::DEPTH, vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT),
        (Usage::COPY_DST, vk::ImageUsageFlags::TRANSFER_DST),
        (Usage::COPY_SRC, vk::ImageUsageFlags::TRANSFER_SRC),
    ];
    pairs
        .iter()
        .filter(|(ours, _)| u.0 & ours.0 != 0)
        .fold(vk::ImageUsageFlags::empty(), |acc, (_, theirs)| {
            acc | *theirs
        })
}

/// The create info for `d`, after checking what phase 1 supports.
pub(super) fn image_info(d: &TextureDesc) -> Result<vk::ImageCreateInfo<'static>, GpuError> {
    if d.size[0] == 0 || d.size[1] == 0 {
        return Err(GpuError::BadDesc("texture with a zero dimension"));
    }
    if d.mips == 0 || d.layers == 0 {
        return Err(GpuError::BadDesc(
            "texture needs at least one mip and layer",
        ));
    }
    if d.dim == Dim::D2 && d.layers != 1 {
        return Err(GpuError::BadDesc("a D2 texture has one layer, use D2Array"));
    }
    let volume = d.dim == Dim::D3;
    if volume && (d.layers != 1 || d.size[2] == 0 || d.format.is_depth()) {
        return Err(GpuError::BadDesc("a D3 texture is one colour layer at least one deep"));
    }
    if d.samples != 1 {
        // TODO(3d): MSAA targets and resolve.
        return Err(GpuError::BadDesc(
            "multisampled textures are not supported yet",
        ));
    }
    if d.usage.0 == 0 {
        return Err(GpuError::BadDesc("texture with no usage"));
    }
    let (ty, depth) = match volume {
        true => (vk::ImageType::TYPE_3D, d.size[2]),
        false => (vk::ImageType::TYPE_2D, 1),
    };
    Ok(vk::ImageCreateInfo::default()
        .image_type(ty)
        .format(format(d.format))
        .extent(vk::Extent3D {
            width: d.size[0],
            height: d.size[1],
            depth,
        })
        .mip_levels(u32::from(d.mips))
        .array_layers(u32::from(d.layers))
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage(d.usage))
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED))
}

pub(super) fn view_type(d: Dim) -> vk::ImageViewType {
    match d {
        Dim::D2 => vk::ImageViewType::TYPE_2D,
        Dim::D2Array => vk::ImageViewType::TYPE_2D_ARRAY,
        Dim::D3 => vk::ImageViewType::TYPE_3D,
    }
}

/// Stage bits and the reads and writes work in those stages does.
pub(super) fn stages(s: Stage) -> (vk::PipelineStageFlags2, vk::AccessFlags2, vk::AccessFlags2) {
    use vk::AccessFlags2 as A;
    use vk::PipelineStageFlags2 as P;
    let table = [
        (Stage::HOST, P::HOST, A::HOST_READ, A::HOST_WRITE),
        (
            Stage::TRANSFER,
            P::ALL_TRANSFER,
            A::TRANSFER_READ,
            A::TRANSFER_WRITE,
        ),
        (
            Stage::COMPUTE,
            P::COMPUTE_SHADER,
            A::SHADER_READ,
            A::SHADER_WRITE,
        ),
        (
            Stage::INDIRECT,
            P::DRAW_INDIRECT | P::INDEX_INPUT,
            A::INDIRECT_COMMAND_READ | A::INDEX_READ,
            A::NONE,
        ),
        (
            Stage::VERTEX,
            P::VERTEX_SHADER,
            A::SHADER_READ,
            A::SHADER_WRITE,
        ),
        (
            Stage::FRAGMENT,
            P::FRAGMENT_SHADER,
            A::SHADER_READ,
            A::SHADER_WRITE,
        ),
        (
            Stage::COLOR_OUT,
            P::COLOR_ATTACHMENT_OUTPUT,
            A::COLOR_ATTACHMENT_READ,
            A::COLOR_ATTACHMENT_WRITE,
        ),
        (
            Stage::DEPTH,
            P::EARLY_FRAGMENT_TESTS | P::LATE_FRAGMENT_TESTS,
            A::DEPTH_STENCIL_ATTACHMENT_READ,
            A::DEPTH_STENCIL_ATTACHMENT_WRITE,
        ),
    ];
    table
        .iter()
        .filter(|row| s.contains(row.0))
        .fold((P::NONE, A::NONE, A::NONE), |(p, r, w), row| {
            (p | row.1, r | row.2, w | row.3)
        })
}

/// One global barrier: the writes of `before` made visible to the reads
/// and writes of `after`.
pub(super) fn barrier(before: Stage, after: Stage, hazards: Hazard) -> vk::MemoryBarrier2<'static> {
    let (src_stage, _, src_writes) = stages(before);
    let (mut dst_stage, dst_reads, dst_writes) = stages(after);
    let mut dst_access = dst_reads | dst_writes;
    if hazards.0 & Hazard::DRAW_ARGS.0 != 0 {
        dst_stage |= vk::PipelineStageFlags2::DRAW_INDIRECT;
        dst_access |= vk::AccessFlags2::INDIRECT_COMMAND_READ;
    }
    // DESCRIPTORS and DEPTH need nothing extra with descriptor sets
    // written on the CPU. TODO(backend): revisit with descriptor buffers.
    vk::MemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_writes)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
}

pub(super) fn blend(b: Blend) -> vk::PipelineColorBlendAttachmentState {
    use vk::BlendFactor as F;
    let on = |sc: F, dc: F, sa: F, da: F| {
        vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(sc)
            .dst_color_blend_factor(dc)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(sa)
            .dst_alpha_blend_factor(da)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    };
    match b {
        Blend::Opaque => vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA),
        Blend::Alpha => on(
            F::SRC_ALPHA,
            F::ONE_MINUS_SRC_ALPHA,
            F::ONE,
            F::ONE_MINUS_SRC_ALPHA,
        ),
        Blend::Premultiplied => on(
            F::ONE,
            F::ONE_MINUS_SRC_ALPHA,
            F::ONE,
            F::ONE_MINUS_SRC_ALPHA,
        ),
        Blend::Additive => on(F::ONE, F::ONE, F::ONE, F::ONE),
        Blend::DualSource => on(
            F::ONE,
            F::ONE_MINUS_SRC1_COLOR,
            F::ONE,
            F::ONE_MINUS_SRC1_ALPHA,
        ),
    }
}

pub(super) fn compare(c: Compare) -> vk::CompareOp {
    match c {
        Compare::Never => vk::CompareOp::NEVER,
        Compare::Less => vk::CompareOp::LESS,
        Compare::Equal => vk::CompareOp::EQUAL,
        Compare::LessEqual => vk::CompareOp::LESS_OR_EQUAL,
        Compare::Greater => vk::CompareOp::GREATER,
        Compare::GreaterEqual => vk::CompareOp::GREATER_OR_EQUAL,
        Compare::Always => vk::CompareOp::ALWAYS,
    }
}

pub(super) fn topology(t: Topology) -> vk::PrimitiveTopology {
    match t {
        Topology::Triangles => vk::PrimitiveTopology::TRIANGLE_LIST,
        Topology::TriangleStrip => vk::PrimitiveTopology::TRIANGLE_STRIP,
        Topology::Lines => vk::PrimitiveTopology::LINE_LIST,
        Topology::Points => vk::PrimitiveTopology::POINT_LIST,
    }
}

pub(super) fn cull(c: Cull) -> vk::CullModeFlags {
    match c {
        Cull::None => vk::CullModeFlags::NONE,
        Cull::Back => vk::CullModeFlags::BACK,
        Cull::Front => vk::CullModeFlags::FRONT,
    }
}

/// The fixed sampler table, in [`crate::Sampler`] order.
pub(super) fn samplers() -> [vk::SamplerCreateInfo<'static>; 4] {
    let s = |filter: vk::Filter, mip: vk::SamplerMipmapMode, wrap: vk::SamplerAddressMode| {
        vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .mipmap_mode(mip)
            .address_mode_u(wrap)
            .address_mode_v(wrap)
            .address_mode_w(wrap)
            .max_lod(vk::LOD_CLAMP_NONE)
    };
    let (near, lin) = (vk::Filter::NEAREST, vk::Filter::LINEAR);
    let (mnear, mlin) = (
        vk::SamplerMipmapMode::NEAREST,
        vk::SamplerMipmapMode::LINEAR,
    );
    let (clamp, rep) = (
        vk::SamplerAddressMode::CLAMP_TO_EDGE,
        vk::SamplerAddressMode::REPEAT,
    );
    [
        s(near, mnear, clamp),
        s(lin, mlin, clamp),
        s(near, mnear, rep),
        s(lin, mlin, rep),
    ]
}

/// Map a failed call. Out of memory needs the caller's context, see
/// [`oom`].
pub(super) fn err(r: vk::Result) -> GpuError {
    match r {
        vk::Result::ERROR_DEVICE_LOST => GpuError::DeviceLost,
        vk::Result::ERROR_OUT_OF_DATE_KHR => GpuError::OutOfDate,
        vk::Result::ERROR_SURFACE_LOST_KHR => GpuError::SurfaceLost,
        other => GpuError::Vk(other.as_raw()),
    }
}

pub(super) fn oom(r: vk::Result, kind: crate::MemoryKind, asked: u64) -> GpuError {
    match r {
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY | vk::Result::ERROR_OUT_OF_HOST_MEMORY => {
            GpuError::OutOfMemory { kind, asked }
        }
        other => err(other),
    }
}

#[cfg(test)]
mod tests;
