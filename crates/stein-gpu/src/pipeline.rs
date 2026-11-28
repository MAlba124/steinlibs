//! Pipelines with as little baked state as the hardware allows.
//!
//! No binding layouts and no vertex layouts: every pipeline shares one
//! VkPipelineLayout (the texture table set plus a [`crate::MAX_ROOT`] byte
//! push constant range visible to all stages), and vertex shaders fetch
//! their own data through pointers. What remains baked is what changes the
//! shader microcode: attachment formats, sample count, topology, blend.
//!
//! TODO(research): VK_EXT_shader_object is on all three local devices and
//! removes pipelines entirely (shaders plus dynamic state). Decide between
//! it and pipelines plus extended dynamic state before the backend lands.
//! The descriptor structs below work for either.

use crate::texture::Format;
use crate::{GpuError, Handle, MAX_COLOR};

/// SPIR-V words and the entry point name.
///
/// Shaders are GLSL with GL_EXT_buffer_reference in shaders/, compiled by
/// the flake's glslang into shaders/spv/ and checked in, so builds need no
/// shader compiler. tests/shaders.rs recompiles and compares when
/// glslangValidator is on PATH.
#[derive(Clone, Copy, Debug)]
pub struct Spirv<'a> {
    pub words: &'a [u32],
    pub entry: &'a str,
}

/// The first word of every SPIR-V module.
pub const SPIRV_MAGIC: u32 = 0x0723_0203;

/// SPIR-V bytes, as `include_bytes!` gives them, to words.
pub fn spirv_words(bytes: &[u8]) -> Result<Vec<u32>, GpuError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(GpuError::BadDesc("SPIR-V length not a multiple of 4"));
    }
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect();
    if words.first() != Some(&SPIRV_MAGIC) {
        return Err(GpuError::BadDesc("not SPIR-V"));
    }
    Ok(words)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Topology {
    #[default]
    Triangles,
    TriangleStrip,
    Lines,
    Points,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Cull {
    #[default]
    None,
    Back,
    Front,
}

/// A handful of named blend modes instead of the factor zoo. Colours are
/// straight alpha, like the software painter.
///
/// TODO(backend): with shader objects or extended dynamic state 3 the
/// blend can move out of the pipeline and into a command, as the blog's
/// separate blend state object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Blend {
    #[default]
    Opaque,
    /// src * a + dst * (1 - a). Everything in 2D today.
    Alpha,
    Premultiplied,
    Additive,
    /// Two source colours, per channel coverage. For LCD subpixel text if
    /// stein-text ever produces it. dualSrcBlend is on all three devices.
    DualSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Compare {
    Never,
    Less,
    Equal,
    #[default]
    LessEqual,
    Greater,
    GreaterEqual,
    Always,
}

/// TODO(3d): test and write become dynamic state (core 1.3 extended
/// dynamic state) so they stop multiplying pipelines, as the blog's
/// separate depth stencil object. Stencil joins when something needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DepthDesc {
    pub format: Format,
    pub test: Compare,
    pub write: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RasterDesc<'a> {
    pub vertex: Spirv<'a>,
    pub fragment: Spirv<'a>,
    pub topology: Topology,
    pub cull: Cull,
    pub blend: Blend,
    /// Formats of the colour attachments, `colors` of them used.
    pub color: [Format; MAX_COLOR],
    pub colors: u8,
    pub depth: Option<DepthDesc>,
    pub samples: u8,
}

/// TODO(3d): static constants. The blog wants one C struct matching a
/// shader side struct. Vulkan specialization constants are per scalar, so
/// map GpuLayout fields onto constant ids 0..n in order, generated from the
/// same type the shader mirrors.
#[derive(Clone, Copy, Debug)]
pub struct ComputeDesc<'a> {
    pub shader: Spirv<'a>,
}

/// A VkPipeline made for dynamic rendering, with dynamic viewport and
/// scissor, on the shared layout.
///
/// TODO(3d): mesh pipelines behind Features::MESH (llvmpipe has
/// VK_EXT_mesh_shader, the two real GPUs here do not).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pipeline(pub(crate) Handle);
