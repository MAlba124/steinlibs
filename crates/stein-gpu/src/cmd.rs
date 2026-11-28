//! Command recording as plain data.
//!
//! ```text
//! Commands { ops: [Op], roots: bytes, passes: [Pass] }  ──submit──►  vk replay, one pass
//! ```
//!
//! Recording touches no driver, so it is safe code, runs on any thread and
//! is testable without a GPU. bgfx works the same way (a frame of render
//! items replayed by the backend). The cost is one extra walk over a
//! compact array at submit, which is noise next to the Vulkan calls.
//!
//! Reuse a `Commands` across frames: `clear` keeps the capacity, so steady
//! state recording allocates nothing.
//!
//! Replay checks the list as it goes, a small state machine (see vk/replay):
//! draws only inside a pass with a graphics pipeline, dispatches, copies
//! and barriers only outside one. A bad list is [`crate::GpuError::State`]
//! and nothing is submitted.
//!
//! TODO(3d): multi-threaded recording is already possible (one Commands per
//! thread, submitted together). If replay itself gets too slow, replay
//! each Commands into its own secondary command buffer on its own thread.

use crate::memory::{GpuLayout, GpuPtr, GpuRange};
use crate::pipeline::Pipeline;
use crate::surface::Surface;
use crate::sync::{Hazard, Stage};
use crate::texture::{Region, Texture};
use crate::{MAX_COLOR, MAX_ROOT};

/// Where a pass renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Texture(Texture),
    /// The image [`crate::Gpu::acquire`] handed out this frame.
    Swapchain(Surface),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Load {
    /// Keep what the target holds. What damage driven repaint needs.
    #[default]
    Keep,
    /// Colour, or depth in the first lane.
    Clear([f32; 4]),
    DontCare,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Store {
    #[default]
    Store,
    DontCare,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attachment {
    pub target: Target,
    pub load: Load,
    pub store: Store,
}

/// A dynamic rendering pass. No render pass or framebuffer objects.
///
/// vkCmdBeginRendering on the targets' views, all in GENERAL. No implicit
/// barriers at pass boundaries, as the blog asks: callers place them, which
/// lets a depth prepass and the main pass overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pass {
    pub color: [Option<Attachment>; MAX_COLOR],
    pub depth: Option<Attachment>,
    pub size: [u32; 2],
}

/// Where a root's bytes sit in [`Commands`]. `len` 0 means no root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RootRef {
    pub at: u32,
    pub len: u16,
}

/// One recorded command.
///
/// TODO(cmd): keep this at 64 bytes or less, a test pins it. Upload is the
/// big one, move its Region to a side table if another variant grows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    BeginPass(u32),
    EndPass,
    SetPipeline(Pipeline),
    /// x, y, w, h in pixels.
    SetViewport([f32; 4]),
    SetScissor([u32; 4]),
    Draw {
        root: RootRef,
        vertices: u32,
        instances: u32,
    },
    /// u32 indices only. TODO(3d): u16 when meshes want it.
    DrawIndexed {
        root: RootRef,
        indices: GpuRange,
        count: u32,
        instances: u32,
    },
    /// `draws` VkDrawIndirectCommand records at `args`.
    /// TODO(3d): count from a GpuPtr (drawIndirectCount, on all three).
    DrawIndirect {
        root: RootRef,
        args: GpuRange,
        draws: u32,
    },
    Dispatch {
        root: RootRef,
        groups: [u32; 3],
    },
    DispatchIndirect {
        root: RootRef,
        args: GpuPtr,
    },
    Copy {
        src: GpuRange,
        dst: GpuPtr,
    },
    /// Linear texels at `src` into `region` of `dst`.
    Upload {
        src: GpuRange,
        dst: Texture,
        region: Region,
    },
    /// `region` of `src` as linear texels at `dst`, for readback.
    Download {
        src: Texture,
        region: Region,
        dst: GpuRange,
    },
    Barrier {
        before: Stage,
        after: Stage,
        hazards: Hazard,
    },
    /// A GPU timestamp once everything recorded before it finished, read
    /// back with [`crate::Gpu::timestamps`]. Inside or outside a pass.
    Timestamp(u8),
}

/// A recorded command list. See the module docs.
#[derive(Debug, Default)]
pub struct Commands {
    ops: Vec<Op>,
    roots: Vec<u8>,
    passes: Vec<Pass>,
}

impl Commands {
    /// Sized up front so a frame's recording never grows it.
    pub fn with_capacity(ops: usize, root_bytes: usize, passes: usize) -> Commands {
        Commands {
            ops: Vec::with_capacity(ops),
            roots: Vec::with_capacity(root_bytes),
            passes: Vec::with_capacity(passes),
        }
    }

    pub fn clear(&mut self) {
        self.ops.clear();
        self.roots.clear();
        self.passes.clear();
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    pub fn root(&self, r: RootRef) -> &[u8] {
        let at = r.at as usize;
        self.roots.get(at..at + r.len as usize).unwrap_or(&[])
    }

    pub fn pass(&self, i: u32) -> Option<&Pass> {
        self.passes.get(i as usize)
    }

    pub fn begin_pass(&mut self, pass: Pass) {
        self.ops.push(Op::BeginPass(self.passes.len() as u32));
        self.passes.push(pass);
    }

    pub fn end_pass(&mut self) {
        self.ops.push(Op::EndPass);
    }

    pub fn set_pipeline(&mut self, p: Pipeline) {
        self.ops.push(Op::SetPipeline(p));
    }

    pub fn set_viewport(&mut self, xywh: [f32; 4]) {
        self.ops.push(Op::SetViewport(xywh));
    }

    pub fn set_scissor(&mut self, xywh: [u32; 4]) {
        self.ops.push(Op::SetScissor(xywh));
    }

    /// Non-indexed draw. Vertex shaders pull their own data through the
    /// root's pointers, `vertices` is only how many invocations to run.
    pub fn draw<R: GpuLayout>(&mut self, root: &R, vertices: u32, instances: u32) {
        let root = self.push_root(root);
        self.ops.push(Op::Draw {
            root,
            vertices,
            instances,
        });
    }

    pub fn draw_indexed<R: GpuLayout>(
        &mut self,
        root: &R,
        indices: GpuRange,
        count: u32,
        instances: u32,
    ) {
        let root = self.push_root(root);
        self.ops.push(Op::DrawIndexed {
            root,
            indices,
            count,
            instances,
        });
    }

    pub fn draw_indirect<R: GpuLayout>(&mut self, root: &R, args: GpuRange, draws: u32) {
        let root = self.push_root(root);
        self.ops.push(Op::DrawIndirect { root, args, draws });
    }

    pub fn dispatch<R: GpuLayout>(&mut self, root: &R, groups: [u32; 3]) {
        let root = self.push_root(root);
        self.ops.push(Op::Dispatch { root, groups });
    }

    pub fn dispatch_indirect<R: GpuLayout>(&mut self, root: &R, args: GpuPtr) {
        let root = self.push_root(root);
        self.ops.push(Op::DispatchIndirect { root, args });
    }

    pub fn copy(&mut self, src: GpuRange, dst: GpuPtr) {
        self.ops.push(Op::Copy { src, dst });
    }

    pub fn upload(&mut self, src: GpuRange, dst: Texture, region: Region) {
        self.ops.push(Op::Upload { src, dst, region });
    }

    pub fn download(&mut self, src: Texture, region: Region, dst: GpuRange) {
        self.ops.push(Op::Download { src, region, dst });
    }

    /// Stamp the GPU clock as `id` once all earlier work is done.
    pub fn timestamp(&mut self, id: u8) {
        self.ops.push(Op::Timestamp(id));
    }

    pub fn barrier(&mut self, before: Stage, after: Stage, hazards: Hazard) {
        self.ops.push(Op::Barrier {
            before,
            after,
            hazards,
        });
    }

    /// Copy a root's bytes in. Oversized roots fail to compile.
    fn push_root<R: GpuLayout>(&mut self, root: &R) -> RootRef {
        const {
            assert!(
                R::SIZE <= MAX_ROOT && R::SIZE % 4 == 0,
                "root over MAX_ROOT or not a multiple of 4"
            )
        };
        let at = self.roots.len();
        self.roots.resize(at + R::SIZE, 0);
        if let Some(out) = self.roots.get_mut(at..) {
            root.store(out);
        }
        RootRef {
            at: at as u32,
            len: R::SIZE as u16,
        }
    }
}
