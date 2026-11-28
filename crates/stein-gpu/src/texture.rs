//! Textures, the global texture table and samplers.
//!
//! ```text
//! Texture (handle, CPU side: VkImage + default view, for passes and copies)
//! TextureSlot (u32, GPU side: index into the one bindless table)
//! ```
//!
//! Shaders never see a Texture, only slots stored in roots or in data behind
//! pointers. Rendering into a texture still needs the handle, because
//! rasterizer targets are not bindless on any current GPU.

use crate::Handle;

/// Formats we need, no more. Add as uses appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// Glyph coverage.
    R8Unorm,
    Rg8Unorm,
    Rgba8Unorm,
    Rgba8Srgb,
    /// The software painter's XRGB order, and what Wayland compositors
    /// prefer for swapchains.
    Bgra8Unorm,
    Bgra8Srgb,
    Rgba16Float,
    R32Float,
    D32Float,
    D24UnormS8,
}

impl Format {
    pub const fn is_depth(self) -> bool {
        matches!(self, Format::D32Float | Format::D24UnormS8)
    }

    /// Bytes per texel, for staging uploads.
    pub const fn texel_bytes(self) -> u32 {
        match self {
            Format::R8Unorm => 1,
            Format::Rg8Unorm => 2,
            Format::Rgba16Float => 8,
            _ => 4,
        }
    }
}

/// TODO(3d): Cube and CubeArray.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dim {
    D2,
    D2Array,
    /// A volume, `size[2]` deep, one layer. Sampled as `texture3D`, see
    /// [`TextureSlot`].
    D3,
}

/// What a texture is used for, a bit set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Usage(pub u8);

impl Usage {
    pub const SAMPLED: Usage = Usage(1 << 0);
    pub const STORAGE: Usage = Usage(1 << 1);
    pub const COLOR: Usage = Usage(1 << 2);
    pub const DEPTH: Usage = Usage(1 << 3);
    pub const COPY_DST: Usage = Usage(1 << 4);
    pub const COPY_SRC: Usage = Usage(1 << 5);

    pub const fn with(self, other: Usage) -> Usage {
        Usage(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureDesc {
    pub dim: Dim,
    pub size: [u32; 3],
    pub mips: u8,
    pub layers: u16,
    pub samples: u8,
    pub format: Format,
    pub usage: Usage,
}

impl TextureDesc {
    /// A single mip 2D texture, the common case.
    pub const fn d2(size: [u32; 2], format: Format, usage: Usage) -> TextureDesc {
        TextureDesc {
            dim: Dim::D2,
            size: [size[0], size[1], 1],
            mips: 1,
            layers: 1,
            samples: 1,
            format,
            usage,
        }
    }

    /// A single mip volume, `size[2]` texels deep.
    pub const fn d3(size: [u32; 3], format: Format, usage: Usage) -> TextureDesc {
        TextureDesc {
            dim: Dim::D3,
            size,
            mips: 1,
            layers: 1,
            samples: 1,
            format,
            usage,
        }
    }
}

/// A VkImage bound into a Textures heap at an offset from the caller's
/// allocator, with one view when its usage allows one, kept in GENERAL
/// layout from the first submit after creation. Destruction is deferred to
/// a timeline point.
///
/// TODO(3d): mip generation (a compute pass, like bgfx's cs_mipgen) and
/// MSAA resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Texture(pub(crate) Handle);

/// Index into the global texture table. The application owns the slot
/// space, like the blog's descriptor heap: it picks indices and can give a
/// material a contiguous range addressed by its first slot.
///
/// One descriptor set. Binding 0 the immutable samplers, binding 1 an
/// update-after-bind, partially bound `texture2D[]` of
/// GpuDesc::texture_slots entries and binding 2 a `texture3D[]` of as
/// many, bound once per command buffer with the one pipeline layout every
/// pipeline shares. The two arrays share one index space: a slot names an
/// entry in both, and [`crate::Gpu::set_slot`] writes the one matching the
/// texture's [`Dim`] (D3 into binding 2, the rest into binding 1). The
/// other entry keeps whatever it held, so give every texture its own
/// slot and read it through the matching array. Writing a slot is
/// vkUpdateDescriptorSets on the CPU. If descriptor_buffer or
/// descriptor_heap replace it later nothing outside `vk` changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextureSlot(pub u32);

/// The fixed sampler table, binding 0 of the same set as immutable
/// samplers. A shader picks one by this index, as a constant.
///
/// TODO(3d): anisotropic and shadow compare samplers join the table when
/// 3D needs them. It stays a small fixed enum, never user created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Sampler {
    NearestClamp = 0,
    LinearClamp = 1,
    NearestRepeat = 2,
    LinearRepeat = 3,
}

/// A box of one mip of one layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub offset: [u32; 3],
    pub size: [u32; 3],
    pub mip: u8,
    pub layer: u16,
}

impl Region {
    pub const fn d2(offset: [u32; 2], size: [u32; 2]) -> Region {
        Region {
            offset: [offset[0], offset[1], 0],
            size: [size[0], size[1], 1],
            mip: 0,
            layer: 0,
        }
    }

    /// A box of a volume's only mip.
    pub const fn d3(offset: [u32; 3], size: [u32; 3]) -> Region {
        Region {
            offset,
            size,
            mip: 0,
            layer: 0,
        }
    }
}
