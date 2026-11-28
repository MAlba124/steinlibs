//! Heaps, textures and the texture table.

use ash::vk;

use super::convert::{aspects, err, image_info, oom, view_aspect, view_type};
use super::init::BUFFER_USAGE;
use super::{Doomed, HeapRec, TexRec, Vk};
use crate::GpuError;
use crate::memory::{GpuPtr, Heap, HeapDesc, MemoryKind};
use crate::sync::TimelinePoint;
use crate::texture::{Dim, Texture, TextureDesc, TextureSlot, Usage};

impl Vk {
    pub(crate) fn create_heap(&mut self, d: &HeapDesc) -> Result<Heap, GpuError> {
        if d.bytes == 0 {
            return Err(GpuError::BadDesc("empty heap"));
        }
        if !self.heaps.has_room() {
            return Err(GpuError::Full("heap"));
        }
        let rec = match d.kind {
            MemoryKind::Textures => {
                let memory = self.allocate(d.kind, d.bytes, 1 << self.types.textures, false)?;
                HeapRec {
                    kind: d.kind,
                    memory,
                    buffer: vk::Buffer::null(),
                    base: 0,
                    size: d.bytes,
                    map: core::ptr::null_mut(),
                }
            }
            _ => self.buffer_heap(d)?,
        };
        self.heaps.insert(rec).map(Heap)
    }

    fn buffer_heap(&self, d: &HeapDesc) -> Result<HeapRec, GpuError> {
        let dev = &self.device;
        let info = vk::BufferCreateInfo::default()
            .size(d.bytes)
            .usage(BUFFER_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: plain create and query on a live device.
        let (buffer, reqs) = unsafe {
            let b = dev
                .create_buffer(&info, None)
                .map_err(|e| oom(e, d.kind, d.bytes))?;
            (b, dev.get_buffer_memory_requirements(b))
        };
        let mut rec = HeapRec {
            kind: d.kind,
            memory: vk::DeviceMemory::null(),
            buffer,
            base: 0,
            size: d.bytes,
            map: core::ptr::null_mut(),
        };
        if let Err(e) = self.back_buffer(&mut rec, reqs) {
            // Frees whatever was made, null handles are skipped.
            super::destroy(dev, Doomed::Heap(rec));
            return Err(e);
        }
        Ok(rec)
    }

    /// Memory, binding, address and mapping for a fresh heap buffer.
    fn back_buffer(&self, rec: &mut HeapRec, reqs: vk::MemoryRequirements) -> Result<(), GpuError> {
        let dev = &self.device;
        rec.memory = self.allocate(rec.kind, reqs.size, reqs.memory_type_bits, true)?;
        // SAFETY: a fresh buffer and memory of a type it accepts, offset 0.
        unsafe { dev.bind_buffer_memory(rec.buffer, rec.memory, 0) }.map_err(err)?;
        let addr = vk::BufferDeviceAddressInfo::default().buffer(rec.buffer);
        // SAFETY: the buffer is bound and was made with SHADER_DEVICE_ADDRESS.
        rec.base = unsafe { dev.get_buffer_device_address(&addr) };
        if rec.kind != MemoryKind::GpuOnly {
            // SAFETY: host visible memory, mapped once for the heap's life
            // and unmapped implicitly when it is freed.
            let p = unsafe {
                dev.map_memory(rec.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            };
            rec.map = p.map_err(err)?.cast();
        }
        Ok(())
    }

    /// Device memory for a heap of `kind`, trying the kind's memory types
    /// in order of preference, the next one when one is out of room.
    fn allocate(
        &self,
        kind: MemoryKind,
        size: u64,
        bits: u32,
        address: bool,
    ) -> Result<vk::DeviceMemory, GpuError> {
        let t = &self.types;
        let order = match kind {
            MemoryKind::Mapped => [Some(t.mapped), t.mapped_fallback],
            MemoryKind::GpuOnly => [Some(t.gpu_only), None],
            MemoryKind::Readback => [Some(t.readback), None],
            MemoryKind::Textures => [Some(t.textures), None],
        };
        let mut last = GpuError::BadDesc("no memory type fits this heap");
        for ty in order
            .into_iter()
            .flatten()
            .filter(|ty| bits & (1 << ty) != 0)
        {
            match self.allocate_type(ty, size, address) {
                Ok(m) => return Ok(m),
                Err(e) => last = oom(e, kind, size),
            }
        }
        Err(last)
    }

    fn allocate_type(
        &self,
        ty: u32,
        size: u64,
        address: bool,
    ) -> Result<vk::DeviceMemory, vk::Result> {
        let mut flags =
            vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(ty);
        if address {
            info = info.push_next(&mut flags);
        }
        // SAFETY: a plain allocation on a live device.
        unsafe { self.device.allocate_memory(&info, None) }
    }

    pub(crate) fn heap_base(&self, heap: Heap) -> Result<GpuPtr, GpuError> {
        let h = self.heaps.get(heap.0)?;
        if h.buffer == vk::Buffer::null() {
            return Err(GpuError::BadDesc("texture heaps have no gpu address"));
        }
        Ok(GpuPtr(h.base))
    }

    pub(crate) fn mapped_mut(&mut self, heap: Heap) -> Result<&mut [u8], GpuError> {
        let h = self.heaps.get(heap.0)?;
        if h.map.is_null() {
            return Err(GpuError::BadDesc("heap is not host visible"));
        }
        let len = usize::try_from(h.size)
            .map_err(|_| GpuError::BadDesc("heap larger than the address space"))?;
        // SAFETY: the mapping covers the heap's `size` bytes for its whole
        // life, and `&mut self` keeps the heap alive and this the only
        // slice for as long as it is borrowed. The GPU may read or write
        // these bytes concurrently, the same contract every mapped buffer
        // has: sync with the timeline before reading GPU output.
        Ok(unsafe { core::slice::from_raw_parts_mut(h.map, len) })
    }

    pub(crate) fn destroy_heap(
        &mut self,
        heap: Heap,
        after: TimelinePoint,
    ) -> Result<(), GpuError> {
        if self.textures.iter().any(|t| t.heap == heap) {
            return Err(GpuError::State("textures still live in this heap"));
        }
        let rec = self.heaps.remove(heap.0)?;
        self.defer(after, Doomed::Heap(rec));
        Ok(())
    }

    /// The buffer and offset behind `bytes` at `ptr`.
    pub(super) fn locate(&self, ptr: GpuPtr, bytes: u64) -> Result<(vk::Buffer, u64), GpuError> {
        self.heaps
            .iter()
            .filter(|h| h.buffer != vk::Buffer::null())
            .find_map(|h| {
                let off = ptr.0.checked_sub(h.base)?;
                (off.checked_add(bytes)? <= h.size).then_some((h.buffer, off))
            })
            .ok_or(GpuError::Unmapped(ptr.0))
    }

    /// Size, alignment and memory type bits of a texture like `d`.
    fn image_reqs(&self, info: &vk::ImageCreateInfo<'_>) -> vk::MemoryRequirements {
        let q = vk::DeviceImageMemoryRequirements::default().create_info(info);
        let mut out = vk::MemoryRequirements2::default();
        // SAFETY: a plain query, nothing is created.
        unsafe {
            self.device
                .get_device_image_memory_requirements(&q, &mut out)
        };
        out.memory_requirements
    }

    pub(crate) fn texture_footprint(&self, d: &TextureDesc) -> Result<(u64, u64), GpuError> {
        let r = self.image_reqs(&image_info(d)?);
        Ok((r.size, r.alignment))
    }

    pub(crate) fn create_texture(
        &mut self,
        d: &TextureDesc,
        heap: Heap,
        at: u64,
    ) -> Result<Texture, GpuError> {
        if !self.textures.has_room() {
            return Err(GpuError::Full("texture"));
        }
        let info = image_info(d)?;
        let r = self.image_reqs(&info);
        let h = self.heaps.get(heap.0)?;
        if h.kind != MemoryKind::Textures {
            return Err(GpuError::BadDesc("textures go in a Textures heap"));
        }
        if r.memory_type_bits & (1 << self.types.textures) == 0 {
            return Err(GpuError::BadDesc(
                "texture needs a memory type its heap lacks",
            ));
        }
        if r.alignment != 0 && !at.is_multiple_of(r.alignment) {
            return Err(GpuError::BadDesc(
                "texture offset not aligned to its footprint",
            ));
        }
        if at.checked_add(r.size).is_none_or(|end| end > h.size) {
            return Err(GpuError::BadDesc("texture runs past the end of its heap"));
        }
        let (image, view) = self.image(d, &info, h.memory, at)?;
        self.fresh.push((image, aspects(d.format)));
        self.textures
            .insert(TexRec {
                image,
                view,
                desc: *d,
                heap,
            })
            .map(Texture)
    }

    fn image(
        &self,
        d: &TextureDesc,
        info: &vk::ImageCreateInfo<'_>,
        memory: vk::DeviceMemory,
        at: u64,
    ) -> Result<(vk::Image, vk::ImageView), GpuError> {
        let dev = &self.device;
        // SAFETY: plain create on a live device.
        let image =
            unsafe { dev.create_image(info, None) }.map_err(|e| oom(e, MemoryKind::Textures, 0))?;
        let range = vk::ImageSubresourceRange {
            aspect_mask: view_aspect(d.format),
            base_mip_level: 0,
            level_count: vk::REMAINING_MIP_LEVELS,
            base_array_layer: 0,
            layer_count: vk::REMAINING_ARRAY_LAYERS,
        };
        let vinfo = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(view_type(d.dim))
            .format(info.format)
            .subresource_range(range);
        // Copy only textures get no view, Vulkan forbids one.
        let viewed = Usage::SAMPLED.0 | Usage::STORAGE.0 | Usage::COLOR.0 | Usage::DEPTH.0;
        let wants_view = d.usage.0 & viewed != 0;
        // SAFETY: `at` was checked aligned and in bounds of memory of a
        // type the image accepts. On failure the image was never used.
        unsafe {
            let made = dev
                .bind_image_memory(image, memory, at)
                .and_then(|()| match wants_view {
                    true => dev.create_image_view(&vinfo, None),
                    false => Ok(vk::ImageView::null()),
                });
            match made {
                Ok(view) => Ok((image, view)),
                Err(e) => {
                    dev.destroy_image(image, None);
                    Err(err(e))
                }
            }
        }
    }

    pub(crate) fn set_slot(&mut self, slot: TextureSlot, texture: Texture) -> Result<(), GpuError> {
        if slot.0 >= self.texture_slots {
            return Err(GpuError::SlotRange {
                slot: slot.0,
                slots: self.texture_slots,
            });
        }
        let t = self.textures.get(texture.0)?;
        if t.desc.usage.0 & Usage::SAMPLED.0 == 0 {
            return Err(GpuError::BadDesc("slotted textures need SAMPLED usage"));
        }
        let images = [vk::DescriptorImageInfo::default()
            .image_view(t.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        // Volumes go in the texture3D array, see TextureSlot.
        let binding = if t.desc.dim == Dim::D3 { 2 } else { 1 };
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.g.set)
            .dst_binding(binding)
            .dst_array_element(slot.0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&images);
        // SAFETY: the binding is UPDATE_AFTER_BIND, so writing it while
        // earlier frames that do not read this slot are in flight is fine.
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };
        Ok(())
    }

    pub(crate) fn destroy_texture(
        &mut self,
        texture: Texture,
        after: TimelinePoint,
    ) -> Result<(), GpuError> {
        let rec = self.textures.remove(texture.0)?;
        self.fresh.retain(|&(i, _)| i != rec.image);
        self.defer(after, Doomed::Texture(rec));
        Ok(())
    }

    /// The image, view and description behind a handle, for replay.
    pub(super) fn texture(&self, t: Texture) -> Result<&TexRec, GpuError> {
        self.textures.get(t.0)
    }
}
