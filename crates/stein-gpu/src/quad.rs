//! The 2D path: stein-ui's quads, one pipeline, one draw.
//!
//! ```text
//! &[stein_ui::draw::Quad] ──stein-shell converts──► ring: [GpuQuad] ──► draw(6 verts, n instances)
//! ```
//!
//! The vertex shader (shaders/quad.vert) reads `quads[gl_InstanceIndex]`
//! through the root's pointer and turns it into a pixel rect, clipped in
//! the shader, so per quad scissoring costs nothing and needs no state
//! changes. The fragment shader (shaders/quad.frag) picks by `kind`: flat
//! colour, coverage bytes from the atlas behind [`QuadRoot::atlas`] times
//! colour, or an RGBA image from the texture table. A solid or image quad
//! with a `radius` or `stroke` is shaped: its coverage is the distance of
//! each pixel centre from a rounded outline, stein-shell's round.rs to the
//! arithmetic.
//!
//! Solid and coverage quads reproduce stein-shell's software painter to the
//! pixel, so a frame can move between the two backends unseen:
//!
//! - Solid rects are the clipped rect truncated to whole pixels.
//! - Glyphs map one atlas texel to one pixel from rounded origins, with the
//!   clip's near edges ceiled and far edges floored, and never read past
//!   the atlas. The colour's alpha is ignored, coverage is the alpha.
//! - A colour alpha of 0 or 255 is opaque (callers that predate alpha
//!   write plain XRGB), 1..254 blends.
//! - Blending is straight alpha on the Bgra8Unorm values with no sRGB
//!   decode, which is blend.rs's `(dst * (255 - a) + fg * a + 127) / 255`
//!   up to the hardware's rounding.
//!
//! The atlas is plain bytes in a Mapped heap, read a word at a time, so a
//! glyph needs no texture, no upload command and no barrier. stein-shell's
//! crate::gpu is the backend that uses all this and the pixel diff test
//! against the software painter lives there.
//!
//! Painter's order is the quad order. Nothing is sorted or batched.
//!
//! This crate does not depend on stein-ui. stein-shell owns the mapping
//! from its Quad to GpuQuad, written straight into the ring mapping.

use crate::cmd::Commands;
use crate::memory::{GpuLayout, GpuPtr, Slice, put};
use crate::pipeline::{Blend, Cull, Pipeline, RasterDesc, Spirv, Topology, spirv_words};
use crate::texture::{Format, TextureSlot};
use crate::{Gpu, GpuError, MAX_COLOR};

/// What the fragment shader does with a quad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum QuadKind {
    #[default]
    Solid = 0,
    /// Atlas bytes are coverage, times colour.
    Coverage = 1,
    /// RGBA texture times colour.
    Image = 2,
}

/// One quad as the shader reads it. 64 bytes, 16 aligned.
///
/// ```text
/// 0  rect   x0 y0 x1 y1
/// 16 clip   x0 y0 x1 y1
/// 32 uv     u v w h, normalised
/// 48 color  ARGB, top byte alpha
/// 52 slot   texture table index
/// 56 kind   QuadKind in the low byte, stroke in the next
/// 60 radius corners as bytes, top left lowest
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuQuad {
    pub rect: [f32; 4],
    pub clip: [f32; 4],
    pub uv: [f32; 4],
    pub color: u32,
    pub slot: TextureSlot,
    pub kind: QuadKind,
    /// Corner radii in pixels, top left, top right, bottom right, bottom left.
    pub radius: [u8; 4],
    /// Ring width in pixels, 0 for a filled shape.
    pub stroke: u8,
}

impl GpuLayout for GpuQuad {
    const SIZE: usize = 64;
    const ALIGN: usize = 16;
    #[inline]
    fn store(&self, out: &mut [u8]) {
        self.rect.store(out.get_mut(0..16).unwrap_or(&mut []));
        self.clip.store(out.get_mut(16..32).unwrap_or(&mut []));
        self.uv.store(out.get_mut(32..48).unwrap_or(&mut []));
        put(out, 48, &self.color.to_le_bytes());
        put(out, 52, &self.slot.0.to_le_bytes());
        put(out, 56, &(self.kind as u32 | u32::from(self.stroke) << 8).to_le_bytes());
        put(out, 60, &self.radius);
    }
}

/// The quad pass's push constants. 32 bytes.
///
/// ```text
/// 0  quads       buffer reference to GpuQuad[]
/// 8  atlas       buffer reference to the coverage bytes, row major
/// 16 size        target width and height in pixels, as floats
/// 24 atlas_size  atlas width and height in texels
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuadRoot {
    pub quads: GpuPtr,
    pub atlas: GpuPtr,
    pub size: [f32; 2],
    pub atlas_size: [u32; 2],
}

impl GpuLayout for QuadRoot {
    const SIZE: usize = 32;
    const ALIGN: usize = 8;
    #[inline]
    fn store(&self, out: &mut [u8]) {
        put(out, 0, &self.quads.0.to_le_bytes());
        put(out, 8, &self.atlas.0.to_le_bytes());
        self.size.store(out.get_mut(16..24).unwrap_or(&mut []));
        self.atlas_size.store(out.get_mut(24..32).unwrap_or(&mut []));
    }
}

/// Where the coverage atlas lives: `size[0] * size[1]` bytes at `ptr`,
/// row major, in a heap whose length is rounded up to 4 bytes so the
/// shader's word reads stay inside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuadAtlas {
    pub ptr: GpuPtr,
    pub size: [u32; 2],
}

impl QuadAtlas {
    /// No atlas. Coverage quads then draw nothing, as in paint.rs.
    pub const NONE: QuadAtlas = QuadAtlas {
        ptr: GpuPtr::NULL,
        size: [0, 0],
    };
}

/// The pipeline and what it was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuadPass {
    pub pipeline: Pipeline,
    pub format: Format,
}

impl QuadPass {
    /// The pipeline from the checked in SPIR-V of shaders/quad.vert and
    /// quad.frag. Straight alpha blending, no depth, one colour target of
    /// `format`, no culling.
    pub fn new(gpu: &mut Gpu, format: Format) -> Result<QuadPass, GpuError> {
        let vs = spirv_words(include_bytes!("../shaders/spv/quad.vert.spv"))?;
        let fs = spirv_words(include_bytes!("../shaders/spv/quad.frag.spv"))?;
        let color = [format; MAX_COLOR];
        let pipeline = gpu.create_pipeline(&RasterDesc {
            vertex: Spirv {
                words: &vs,
                entry: "main",
            },
            fragment: Spirv {
                words: &fs,
                entry: "main",
            },
            topology: Topology::Triangles,
            cull: Cull::None,
            blend: Blend::Alpha,
            color,
            colors: 1,
            depth: None,
            samples: 1,
        })?;
        Ok(QuadPass { pipeline, format })
    }

    /// Record the draw. The caller has begun a pass on the target and
    /// placed the quads in the ring.
    pub fn record(&self, cmds: &mut Commands, quads: Slice<GpuQuad>, size: [u32; 2], atlas: QuadAtlas) {
        if quads.len == 0 || size[0] == 0 || size[1] == 0 {
            return;
        }
        let root = QuadRoot {
            quads: quads.gpu,
            atlas: atlas.ptr,
            size: [size[0] as f32, size[1] as f32],
            atlas_size: atlas.size,
        };
        cmds.set_pipeline(self.pipeline);
        cmds.set_viewport([0.0, 0.0, size[0] as f32, size[1] as f32]);
        cmds.set_scissor([0, 0, size[0], size[1]]);
        cmds.draw(&root, 6, quads.len);
    }
}
