//! Commands to Vulkan calls, in one pass.
//!
//! Replay is also the command list's checker, a small state machine:
//!
//! ```text
//! Outside ──BeginPass──► InPass ──EndPass──► Outside
//! ```
//!
//! Draws only inside a pass with a graphics pipeline bound, dispatches,
//! copies and barriers only outside one with (for dispatches) a compute
//! pipeline bound. Anything else is [`GpuError::State`] and nothing is
//! submitted. Validation layers catch the rest.

use ash::vk;

use super::Vk;
use super::stamps::Stamps;
use super::convert::{barrier, view_aspect};
use crate::cmd::{Attachment, Commands, Load, Op, Pass, RootRef, Store, Target};
use crate::memory::GpuRange;
use crate::texture::{Region, Texture, Usage};
use crate::{GpuError, MAX_COLOR};

struct State {
    in_pass: bool,
    bound: Option<vk::PipelineBindPoint>,
}

pub(super) fn replay(
    vk: &Vk,
    cb: vk::CommandBuffer,
    cmds: &Commands,
    stamps: &mut Stamps,
) -> Result<(), GpuError> {
    let mut st = State {
        in_pass: false,
        bound: None,
    };
    for op in cmds.ops() {
        match op {
            Op::BeginPass(_)
            | Op::EndPass
            | Op::SetPipeline(_)
            | Op::SetViewport(_)
            | Op::SetScissor(_) => state_op(vk, cb, cmds, &mut st, op)?,
            Op::Draw { .. } | Op::DrawIndexed { .. } | Op::DrawIndirect { .. } => {
                draw_op(vk, cb, cmds, &st, op)?
            }
            Op::Dispatch { .. } | Op::DispatchIndirect { .. } => {
                dispatch_op(vk, cb, cmds, &st, op)?
            }
            Op::Copy { .. } | Op::Upload { .. } | Op::Download { .. } | Op::Barrier { .. } => {
                transfer_op(vk, cb, &st, op)?
            }
            Op::Timestamp(id) => stamps.write(&vk.device, cb, *id),
        }
    }
    if st.in_pass {
        return Err(GpuError::State("a pass was never ended"));
    }
    Ok(())
}

fn state_op(
    vk: &Vk,
    cb: vk::CommandBuffer,
    cmds: &Commands,
    st: &mut State,
    op: &Op,
) -> Result<(), GpuError> {
    let d = &vk.device;
    match *op {
        Op::BeginPass(i) => {
            if st.in_pass {
                return Err(GpuError::State("pass begun inside a pass"));
            }
            let pass = cmds
                .pass(i)
                .ok_or(GpuError::State("pass index out of range"))?;
            begin_pass(vk, cb, pass)?;
            st.in_pass = true;
        }
        Op::EndPass => {
            if !st.in_pass {
                return Err(GpuError::State("end of a pass never begun"));
            }
            // SAFETY: cb is inside the rendering begun above.
            unsafe { d.cmd_end_rendering(cb) };
            st.in_pass = false;
        }
        Op::SetPipeline(p) => {
            let rec = vk.pipeline(p)?;
            // SAFETY: a live pipeline on a recording cb.
            unsafe { d.cmd_bind_pipeline(cb, rec.bind, rec.pipeline) };
            st.bound = Some(rec.bind);
        }
        Op::SetViewport([x, y, width, height]) => {
            let v = [vk::Viewport {
                x,
                y,
                width,
                height,
                min_depth: 0.0,
                max_depth: 1.0,
            }];
            // SAFETY: dynamic state on a recording cb.
            unsafe { d.cmd_set_viewport(cb, 0, &v) };
        }
        Op::SetScissor([x, y, w, h]) => {
            let offset = vk::Offset2D {
                x: x as i32,
                y: y as i32,
            };
            let s = [vk::Rect2D {
                offset,
                extent: vk::Extent2D {
                    width: w,
                    height: h,
                },
            }];
            // SAFETY: dynamic state on a recording cb.
            unsafe { d.cmd_set_scissor(cb, 0, &s) };
        }
        _ => {}
    }
    Ok(())
}

fn push(vk: &Vk, cb: vk::CommandBuffer, cmds: &Commands, root: RootRef) {
    let bytes = cmds.root(root);
    if !bytes.is_empty() {
        // SAFETY: the range is within the layout's MAX_ROOT push range,
        // enforced when the root was recorded.
        unsafe {
            vk.device
                .cmd_push_constants(cb, vk.g.layout, vk::ShaderStageFlags::ALL, 0, bytes)
        };
    }
}

fn draw_op(
    vk: &Vk,
    cb: vk::CommandBuffer,
    cmds: &Commands,
    st: &State,
    op: &Op,
) -> Result<(), GpuError> {
    if !st.in_pass || st.bound != Some(vk::PipelineBindPoint::GRAPHICS) {
        return Err(GpuError::State(
            "draw outside a pass or without a graphics pipeline",
        ));
    }
    let d = &vk.device;
    match *op {
        Op::Draw {
            root,
            vertices,
            instances,
        } => {
            push(vk, cb, cmds, root);
            // SAFETY: inside a pass with a graphics pipeline bound.
            unsafe { d.cmd_draw(cb, vertices, instances, 0, 0) };
        }
        Op::DrawIndexed {
            root,
            indices,
            count,
            instances,
        } => {
            let (buf, off) = vk.locate(indices.ptr, indices.bytes)?;
            push(vk, cb, cmds, root);
            // SAFETY: as above, the index range lies inside a heap buffer.
            unsafe {
                d.cmd_bind_index_buffer(cb, buf, off, vk::IndexType::UINT32);
                d.cmd_draw_indexed(cb, count, instances, 0, 0, 0);
            }
        }
        Op::DrawIndirect { root, args, draws } => {
            let (buf, off) = vk.locate(args.ptr, args.bytes)?;
            push(vk, cb, cmds, root);
            let stride = size_of::<vk::DrawIndirectCommand>() as u32;
            // SAFETY: as above, the argument range lies inside a heap buffer.
            unsafe { d.cmd_draw_indirect(cb, buf, off, draws, stride) };
        }
        _ => {}
    }
    Ok(())
}

fn dispatch_op(
    vk: &Vk,
    cb: vk::CommandBuffer,
    cmds: &Commands,
    st: &State,
    op: &Op,
) -> Result<(), GpuError> {
    if st.in_pass || st.bound != Some(vk::PipelineBindPoint::COMPUTE) {
        return Err(GpuError::State(
            "dispatch inside a pass or without a compute pipeline",
        ));
    }
    let d = &vk.device;
    match *op {
        Op::Dispatch {
            root,
            groups: [x, y, z],
        } => {
            push(vk, cb, cmds, root);
            // SAFETY: outside a pass with a compute pipeline bound.
            unsafe { d.cmd_dispatch(cb, x, y, z) };
        }
        Op::DispatchIndirect { root, args } => {
            let (buf, off) = vk.locate(args, size_of::<vk::DispatchIndirectCommand>() as u64)?;
            push(vk, cb, cmds, root);
            // SAFETY: as above, the arguments lie inside a heap buffer.
            unsafe { d.cmd_dispatch_indirect(cb, buf, off) };
        }
        _ => {}
    }
    Ok(())
}

fn transfer_op(vk: &Vk, cb: vk::CommandBuffer, st: &State, op: &Op) -> Result<(), GpuError> {
    if st.in_pass {
        return Err(GpuError::State("transfers and barriers go outside passes"));
    }
    let d = &vk.device;
    match *op {
        Op::Copy { src, dst } if src.bytes > 0 => {
            let (sb, so) = vk.locate(src.ptr, src.bytes)?;
            let (db, dof) = vk.locate(dst, src.bytes)?;
            let region = [vk::BufferCopy {
                src_offset: so,
                dst_offset: dof,
                size: src.bytes,
            }];
            // SAFETY: both ranges lie inside heap buffers, outside a pass.
            unsafe { d.cmd_copy_buffer(cb, sb, db, &region) };
        }
        Op::Upload { src, dst, region } => {
            let (buf, region) = image_copy(vk, src, dst, region)?;
            let image = vk.texture(dst)?.image;
            // SAFETY: the image is live in GENERAL, the buffer range was
            // checked large enough for the region.
            unsafe {
                d.cmd_copy_buffer_to_image(cb, buf, image, vk::ImageLayout::GENERAL, &[region])
            };
        }
        Op::Download { src, region, dst } => {
            let (buf, region) = image_copy(vk, dst, src, region)?;
            let image = vk.texture(src)?.image;
            // SAFETY: as for Upload, in the other direction.
            unsafe {
                d.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::GENERAL, buf, &[region])
            };
        }
        Op::Barrier {
            before,
            after,
            hazards,
        } => {
            let b = [barrier(before, after, hazards)];
            let dep = vk::DependencyInfo::default().memory_barriers(&b);
            // SAFETY: one global memory barrier on a recording cb.
            unsafe { d.cmd_pipeline_barrier2(cb, &dep) };
        }
        _ => {}
    }
    Ok(())
}

/// The buffer and copy region for moving `region` of `tex` through the
/// tightly packed texels at `range`.
fn image_copy(
    vk: &Vk,
    range: GpuRange,
    tex: Texture,
    region: Region,
) -> Result<(vk::Buffer, vk::BufferImageCopy), GpuError> {
    let t = vk.texture(tex)?;
    let [w, h, depth] = region.size;
    let need = u64::from(w)
        * u64::from(h)
        * u64::from(depth.max(1))
        * u64::from(t.desc.format.texel_bytes());
    if range.bytes < need {
        return Err(GpuError::BadDesc(
            "buffer range smaller than the texture region",
        ));
    }
    let (buf, off) = vk.locate(range.ptr, need)?;
    let copy = vk::BufferImageCopy {
        buffer_offset: off,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers {
            aspect_mask: view_aspect(t.desc.format),
            mip_level: u32::from(region.mip),
            base_array_layer: u32::from(region.layer),
            layer_count: 1,
        },
        image_offset: vk::Offset3D {
            x: region.offset[0] as i32,
            y: region.offset[1] as i32,
            z: region.offset[2] as i32,
        },
        image_extent: vk::Extent3D {
            width: w,
            height: h,
            depth: depth.max(1),
        },
    };
    Ok((buf, copy))
}

fn begin_pass(vk: &Vk, cb: vk::CommandBuffer, pass: &Pass) -> Result<(), GpuError> {
    if pass.size[0] == 0 || pass.size[1] == 0 {
        return Err(GpuError::BadDesc("pass with a zero size"));
    }
    let mut colors = [vk::RenderingAttachmentInfo::default(); MAX_COLOR];
    let mut n = 0;
    for a in pass.color.iter().flatten() {
        colors[n] = attachment(vk, a, false)?;
        n += 1;
    }
    let depth = pass.depth.map(|a| attachment(vk, &a, true)).transpose()?;
    let area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: pass.size[0],
            height: pass.size[1],
        },
    };
    let mut info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&colors[..n]);
    if let Some(z) = depth.as_ref() {
        info = info.depth_attachment(z);
    }
    // SAFETY: every view is a live texture's in GENERAL layout.
    unsafe { vk.device.cmd_begin_rendering(cb, &info) };
    Ok(())
}

fn attachment(vk: &Vk, a: &Attachment, depth: bool) -> Result<vk::RenderingAttachmentInfo<'static>, GpuError> {
    let view = match a.target {
        Target::Texture(t) => texture_view(vk, t, depth)?,
        Target::Swapchain(s) if !depth => vk.swap_view(s)?,
        Target::Swapchain(_) => return Err(GpuError::BadDesc("a swapchain is not a depth target")),
    };
    let (load, clear) = match a.load {
        Load::Keep => (vk::AttachmentLoadOp::LOAD, [0.0; 4]),
        Load::Clear(c) => (vk::AttachmentLoadOp::CLEAR, c),
        Load::DontCare => (vk::AttachmentLoadOp::DONT_CARE, [0.0; 4]),
    };
    let clear_value = if depth {
        vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: clear[0],
                stencil: 0,
            },
        }
    } else {
        vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
        }
    };
    let store = match a.store {
        Store::Store => vk::AttachmentStoreOp::STORE,
        Store::DontCare => vk::AttachmentStoreOp::DONT_CARE,
    };
    Ok(vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::GENERAL)
        .load_op(load)
        .store_op(store)
        .clear_value(clear_value))
}

fn texture_view(vk: &Vk, t: Texture, depth: bool) -> Result<vk::ImageView, GpuError> {
    let t = vk.texture(t)?;
    if t.desc.format.is_depth() != depth {
        return Err(GpuError::BadDesc("colour and depth attachments swapped"));
    }
    let need = if depth { Usage::DEPTH } else { Usage::COLOR };
    if t.desc.usage.0 & need.0 == 0 {
        return Err(GpuError::BadDesc("attachment texture lacks COLOR or DEPTH usage"));
    }
    Ok(t.view)
}
