//! Wayland surfaces and their swapchains.

use ash::vk;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

use super::Vk;
use super::convert::{err, format};
use super::instance::Wsi;
use crate::GpuError;
use crate::device::Frame;
use crate::surface::{Acquired, PresentMode, Surface, SurfaceDesc, SwapState};
use crate::texture::Format;

/// Most damage rects handed to the compositor per present. More are
/// dropped, which only makes the hint coarser.
const MAX_DAMAGE: usize = 16;

pub(crate) struct SurfRec {
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    desc: SurfaceDesc,
    extent: [u32; 2],
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    /// Signalled by the submit that renders image i, waited by its present.
    present: Vec<vk::Semaphore>,
    /// Whether image i was presented before, so its contents are defined
    /// and it sits in PRESENT_SRC rather than UNDEFINED.
    seen: Vec<bool>,
    state: SwapState,
    /// Keep the display and the wl_surface proxy alive for the pointers
    /// the driver holds.
    _conn: Connection,
    _wl: WlSurface,
}

/// The image acquired in the recording frame.
#[derive(Clone, Copy)]
pub(super) struct Pending {
    pub(super) surface: Surface,
    pub(super) image: u32,
    pub(super) slot: u8,
}

/// What submit needs to wait on, transition and signal for a swapchain.
pub(super) struct SwapUse {
    pub(super) image: vk::Image,
    pub(super) old: vk::ImageLayout,
    pub(super) acquire: vk::Semaphore,
    pub(super) present: vk::Semaphore,
}

/// The pieces of Vk a swapchain rebuild needs, borrowed apart from the
/// surface table so both can be held at once.
struct Ctx<'a> {
    d: &'a ash::Device,
    wsi: &'a Wsi,
    sc: &'a ash::khr::swapchain::Device,
    pdev: vk::PhysicalDevice,
}

impl Vk {
    pub(crate) fn create_surface(
        &mut self,
        conn: &Connection,
        wl: &WlSurface,
        desc: &SurfaceDesc,
    ) -> Result<Surface, GpuError> {
        let wsi = self
            .inst
            .wsi
            .as_ref()
            .ok_or(GpuError::Missing("VK_KHR_wayland_surface"))?;
        let sc = self
            .swapchain_fn
            .as_ref()
            .ok_or(GpuError::Missing("VK_KHR_swapchain"))?;
        if !self.surfaces.has_room() {
            return Err(GpuError::Full("surface"));
        }
        let display = conn.backend().display_ptr();
        let proxy = wl.id().as_ptr();
        if display.is_null() || proxy.is_null() {
            return Err(GpuError::BadDesc("wl_surface already destroyed"));
        }
        let info = vk::WaylandSurfaceCreateInfoKHR::default()
            .display(display.cast())
            .surface(proxy.cast());
        // SAFETY: both pointers are live, and the clones kept in the record
        // keep them so for the surface's life.
        let surface = unsafe { wsi.wayland.create_wayland_surface(&info, None) }.map_err(err)?;
        let mut rec = SurfRec {
            surface,
            swapchain: vk::SwapchainKHR::null(),
            desc: *desc,
            extent: [0, 0],
            images: Vec::new(),
            views: Vec::new(),
            present: Vec::new(),
            seen: Vec::new(),
            state: SwapState::Stale,
            _conn: conn.clone(),
            _wl: wl.clone(),
        };
        let ctx = Ctx {
            d: &self.device,
            wsi,
            sc,
            pdev: self.pdev,
        };
        // SAFETY: a plain query on our own surface.
        let ok = unsafe {
            wsi.surface
                .get_physical_device_surface_support(self.pdev, self.family, surface)
        };
        let built = match ok {
            Ok(true) => build(&ctx, &mut rec, desc.size),
            Ok(false) => Err(GpuError::Missing("present from the graphics queue")),
            Err(e) => Err(err(e)),
        };
        if let Err(e) = built {
            release(&ctx, rec);
            return Err(e);
        }
        self.surfaces.insert(rec).map(Surface)
    }

    pub(crate) fn resize_surface(&mut self, s: Surface, size: [u32; 2]) -> Result<(), GpuError> {
        if self.pending.is_some_and(|p| p.surface == s) {
            return Err(GpuError::State("resize while an image is acquired"));
        }
        let (wsi, sc) = wsi_of(&self.inst, self.swapchain_fn.as_ref())?;
        let ctx = Ctx {
            d: &self.device,
            wsi,
            sc,
            pdev: self.pdev,
        };
        // SAFETY: waiting for idle means no image of the old swapchain is
        // in use when build destroys it.
        unsafe { ctx.d.device_wait_idle() }.map_err(err)?;
        let rec = self.surfaces.get_mut(s.0)?;
        build(&ctx, rec, size)?;
        // An image submitted and not yet presented belonged to the old
        // swapchain, there is nothing left to present it to.
        if self.presentable.is_some_and(|p| p.surface == s) {
            self.presentable = None;
        }
        Ok(())
    }

    pub(crate) fn surface_state(&self, s: Surface) -> Result<(SwapState, [u32; 2]), GpuError> {
        let rec = self.surfaces.get(s.0)?;
        Ok((rec.state, rec.extent))
    }

    pub(crate) fn surface_desc(&self, s: Surface) -> Result<SurfaceDesc, GpuError> {
        Ok(self.surfaces.get(s.0)?.desc)
    }

    pub(crate) fn acquire(&mut self, s: Surface, frame: Frame) -> Result<Acquired, GpuError> {
        self.check_recording(frame)?;
        if self.pending.is_some() {
            return Err(GpuError::State("one acquire per frame"));
        }
        let sem = self.slots[frame.slot as usize].acquire;
        let sc = self
            .swapchain_fn
            .as_ref()
            .ok_or(GpuError::Missing("VK_KHR_swapchain"))?;
        let rec = self.surfaces.get_mut(s.0)?;
        match rec.state {
            SwapState::Ready => {}
            SwapState::Stale => return Err(GpuError::OutOfDate),
            SwapState::Acquired => return Err(GpuError::State("image acquired and not presented")),
        }
        // SAFETY: the slot's semaphore is unsignalled, its last wait was in
        // the slot's previous submit, which begin_frame saw retire.
        match unsafe { sc.acquire_next_image(rec.swapchain, u64::MAX, sem, vk::Fence::null()) } {
            Ok((image, suboptimal)) => {
                rec.state = SwapState::Acquired;
                self.pending = Some(Pending {
                    surface: s,
                    image,
                    slot: frame.slot,
                });
                Ok(Acquired {
                    surface: s,
                    image,
                    size: rec.extent,
                    suboptimal,
                })
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                rec.state = SwapState::Stale;
                Err(GpuError::OutOfDate)
            }
            Err(e) => Err(err(e)),
        }
    }

    /// The acquired image's wait, signal and transition for the submit of
    /// the frame in `slot`.
    pub(super) fn swap_use(&self, slot: u8) -> Result<Option<SwapUse>, GpuError> {
        let Some(p) = self.pending else {
            return Ok(None);
        };
        if p.slot != slot {
            return Err(GpuError::State("image acquired in another frame"));
        }
        let rec = self.surfaces.get(p.surface.0)?;
        let i = p.image as usize;
        let seen = rec.seen.get(i).copied().unwrap_or(false);
        Ok(Some(SwapUse {
            image: rec.images[i],
            old: if seen {
                vk::ImageLayout::PRESENT_SRC_KHR
            } else {
                vk::ImageLayout::UNDEFINED
            },
            acquire: self.slots[slot as usize].acquire,
            present: rec.present[i],
        }))
    }

    /// After a successful submit the acquired image waits for present.
    pub(super) fn submitted(&mut self) {
        let Some(p) = self.pending.take() else {
            return;
        };
        if let Ok(rec) = self.surfaces.get_mut(p.surface.0)
            && let Some(seen) = rec.seen.get_mut(p.image as usize)
        {
            *seen = true;
        }
        self.presentable = Some(p);
    }

    /// The view a pass renders into for `Target::Swapchain(s)`.
    pub(super) fn swap_view(&self, s: Surface) -> Result<vk::ImageView, GpuError> {
        let p = self.pending.filter(|p| p.surface == s);
        let p = p.ok_or(GpuError::State(
            "render to a swapchain with no image acquired",
        ))?;
        let rec = self.surfaces.get(s.0)?;
        Ok(rec.views[p.image as usize])
    }

    pub(crate) fn present(&mut self, a: Acquired, damage: &[[u32; 4]]) -> Result<(), GpuError> {
        let p = self.presentable.take();
        let p = p.filter(|p| p.surface == a.surface && p.image == a.image);
        let p = p.ok_or(GpuError::State(
            "present of an image whose frame was not submitted",
        ))?;
        let sc = self
            .swapchain_fn
            .as_ref()
            .ok_or(GpuError::Missing("VK_KHR_swapchain"))?;
        let rec = self.surfaces.get_mut(p.surface.0)?;
        let mut rects = [vk::RectLayerKHR::default(); MAX_DAMAGE];
        let n = damage_rects(damage, rec.extent, &mut rects);
        let region = [vk::PresentRegionKHR::default().rectangles(&rects[..n])];
        let mut regions = vk::PresentRegionsKHR::default().regions(&region);
        let waits = [rec.present[p.image as usize]];
        let chains = [rec.swapchain];
        let images = [p.image];
        let mut info = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&chains)
            .image_indices(&images);
        if self.incremental && n > 0 {
            info = info.push_next(&mut regions);
        }
        // SAFETY: the image was rendered by a submitted frame that signals
        // its present semaphore; every array outlives the call.
        match unsafe { sc.queue_present(self.queue, &info) } {
            Ok(suboptimal) => {
                rec.state = after_present(suboptimal || a.suboptimal);
                Ok(())
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                rec.state = SwapState::Stale;
                Err(GpuError::OutOfDate)
            }
            Err(e) => Err(err(e)),
        }
    }

    pub(crate) fn destroy_surface(&mut self, s: Surface) -> Result<(), GpuError> {
        let (wsi, sc) = wsi_of(&self.inst, self.swapchain_fn.as_ref())?;
        let ctx = Ctx {
            d: &self.device,
            wsi,
            sc,
            pdev: self.pdev,
        };
        // SAFETY: idle, so no image of this swapchain is in use.
        unsafe { ctx.d.device_wait_idle() }.map_err(err)?;
        let rec = self.surfaces.remove(s.0)?;
        release(&ctx, rec);
        if self.pending.is_some_and(|p| p.surface == s) {
            self.pending = None;
        }
        if self.presentable.is_some_and(|p| p.surface == s) {
            self.presentable = None;
        }
        Ok(())
    }

    /// Destroy every surface, for Drop. The device is idle.
    pub(super) fn release_surfaces(&mut self) {
        let (Some(wsi), Some(sc)) = (self.inst.wsi.as_ref(), self.swapchain_fn.as_ref()) else {
            return;
        };
        let ctx = Ctx {
            d: &self.device,
            wsi,
            sc,
            pdev: self.pdev,
        };
        self.surfaces.drain().for_each(|rec| release(&ctx, rec));
    }
}

/// The window system entry points, from fields so the surface table can
/// be borrowed alongside.
fn wsi_of<'a>(
    inst: &'a super::instance::Inst,
    sc: Option<&'a ash::khr::swapchain::Device>,
) -> Result<(&'a Wsi, &'a ash::khr::swapchain::Device), GpuError> {
    let wsi = inst
        .wsi
        .as_ref()
        .ok_or(GpuError::Missing("VK_KHR_wayland_surface"))?;
    let sc = sc.ok_or(GpuError::Missing("VK_KHR_swapchain"))?;
    Ok((wsi, sc))
}

/// Stale when either acquire or present said suboptimal.
fn after_present(stale: bool) -> SwapState {
    if stale {
        SwapState::Stale
    } else {
        SwapState::Ready
    }
}

/// (Re)create `rec`'s swapchain at `size`, from its old one when there is
/// one. The device is idle or the old swapchain never rendered.
fn build(ctx: &Ctx<'_>, rec: &mut SurfRec, size: [u32; 2]) -> Result<(), GpuError> {
    if size[0] == 0 || size[1] == 0 {
        return Err(GpuError::BadDesc("surface with a zero size"));
    }
    let info_parts = plan(ctx, rec, size)?;
    let (f, mode, count, extent, usage, caps) = info_parts;
    let info = vk::SwapchainCreateInfoKHR::default()
        .surface(rec.surface)
        .min_image_count(count)
        .image_format(f.format)
        .image_color_space(f.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(usage)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(composite(caps.supported_composite_alpha, rec.desc.alpha))
        .present_mode(mode)
        .clipped(true)
        .old_swapchain(rec.swapchain);
    // SAFETY: the surface is live and supported by this device's queue.
    let fresh = unsafe { ctx.sc.create_swapchain(&info, None) }.map_err(err)?;
    drop_images(ctx, rec);
    // SAFETY: the old swapchain is retired by the new one and idle.
    unsafe { ctx.sc.destroy_swapchain(rec.swapchain, None) };
    rec.swapchain = fresh;
    rec.extent = [extent.width, extent.height];
    // SAFETY: a plain query on the swapchain just made.
    rec.images = unsafe { ctx.sc.get_swapchain_images(fresh) }.map_err(err)?;
    for &image in &rec.images {
        rec.views.push(view(ctx.d, image, f.format)?);
        rec.present.push(super::init::binary_semaphore(ctx.d)?);
    }
    rec.seen = vec![false; rec.images.len()];
    rec.state = SwapState::Ready;
    Ok(())
}

type Plan = (
    vk::SurfaceFormatKHR,
    vk::PresentModeKHR,
    u32,
    vk::Extent2D,
    vk::ImageUsageFlags,
    vk::SurfaceCapabilitiesKHR,
);

/// Everything the swapchain create info needs, from what the surface
/// supports and what `rec.desc` asks for.
fn plan(ctx: &Ctx<'_>, rec: &SurfRec, size: [u32; 2]) -> Result<Plan, GpuError> {
    let q = &ctx.wsi.surface;
    // SAFETY: plain queries on a live surface.
    let (caps, formats, modes) = unsafe {
        (
            q.get_physical_device_surface_capabilities(ctx.pdev, rec.surface)
                .map_err(err)?,
            q.get_physical_device_surface_formats(ctx.pdev, rec.surface)
                .map_err(err)?,
            q.get_physical_device_surface_present_modes(ctx.pdev, rec.surface)
                .map_err(err)?,
        )
    };
    let f = pick_format(&rec.desc.formats, &formats)
        .ok_or(GpuError::Missing("a wanted surface format"))?;
    let wanted = vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST;
    let usage = wanted & caps.supported_usage_flags;
    if !usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT) {
        return Err(GpuError::Missing("colour attachment swapchain images"));
    }
    let count = image_count(caps.min_image_count, caps.max_image_count, rec.desc.images);
    let extent = extent(&caps, size);
    Ok((
        f,
        pick_mode(rec.desc.mode, &modes),
        count,
        extent,
        usage,
        caps,
    ))
}

fn view(d: &ash::Device, image: vk::Image, f: vk::Format) -> Result<vk::ImageView, GpuError> {
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(f)
        .subresource_range(range);
    // SAFETY: a view of a live swapchain image.
    unsafe { d.create_image_view(&info, None) }.map_err(err)
}

/// Views and semaphores of the current images. The device is idle.
fn drop_images(ctx: &Ctx<'_>, rec: &mut SurfRec) {
    // SAFETY: nothing in flight uses them, see the callers.
    unsafe {
        rec.views
            .drain(..)
            .for_each(|v| ctx.d.destroy_image_view(v, None));
        rec.present
            .drain(..)
            .for_each(|s| ctx.d.destroy_semaphore(s, None));
    }
    rec.images.clear();
    rec.seen.clear();
}

fn release(ctx: &Ctx<'_>, mut rec: SurfRec) {
    drop_images(ctx, &mut rec);
    // SAFETY: the device is idle and the surface is ours. Null handles are
    // allowed, which covers a failed build.
    unsafe {
        ctx.sc.destroy_swapchain(rec.swapchain, None);
        ctx.wsi.surface.destroy_surface(rec.surface, None);
    }
}

/// Wanted formats in order, in the sRGB non linear colour space every
/// compositor takes.
fn pick_format(want: &[Format], have: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    want.iter().find_map(|w| {
        have.iter()
            .find(|h| h.format == format(*w) && h.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
            .copied()
    })
}

fn pick_mode(want: PresentMode, have: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    let m = match want {
        PresentMode::Fifo => vk::PresentModeKHR::FIFO,
        PresentMode::Mailbox => vk::PresentModeKHR::MAILBOX,
        PresentMode::Immediate => vk::PresentModeKHR::IMMEDIATE,
    };
    if have.contains(&m) {
        m
    } else {
        vk::PresentModeKHR::FIFO
    }
}

/// `want` clamped to the surface's range. A max of 0 means no limit.
fn image_count(min: u32, max: u32, want: u8) -> u32 {
    let n = u32::from(want).max(min);
    if max == 0 { n } else { n.min(max) }
}

/// The surface's own extent when it has one, else `size` clamped to its
/// limits. Wayland surfaces never dictate one.
fn extent(caps: &vk::SurfaceCapabilitiesKHR, size: [u32; 2]) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        return caps.current_extent;
    }
    let (lo, hi) = (caps.min_image_extent, caps.max_image_extent);
    vk::Extent2D {
        width: size[0].clamp(lo.width, hi.width.max(lo.width)),
        height: size[1].clamp(lo.height, hi.height.max(lo.height)),
    }
}

/// Opaque when the compositor allows it and the window has no alpha to
/// blend. With `alpha` the image's own premultiplied alpha first, which on
/// Wayland is an ARGB buffer, so what is under the surface shows through.
fn composite(supported: vk::CompositeAlphaFlagsKHR, alpha: bool) -> vk::CompositeAlphaFlagsKHR {
    use vk::CompositeAlphaFlagsKHR as C;
    let order = match alpha {
        false => [C::OPAQUE, C::INHERIT, C::PRE_MULTIPLIED, C::POST_MULTIPLIED],
        true => [C::PRE_MULTIPLIED, C::INHERIT, C::POST_MULTIPLIED, C::OPAQUE],
    };
    order.into_iter().find(|c| supported.contains(*c)).unwrap_or(C::OPAQUE)
}

/// Damage rects clipped to the image, empty ones dropped, at most
/// MAX_DAMAGE of them.
fn damage_rects(
    damage: &[[u32; 4]],
    extent: [u32; 2],
    out: &mut [vk::RectLayerKHR; MAX_DAMAGE],
) -> usize {
    let mut n = 0;
    for &[x, y, w, h] in damage {
        let w = w.min(extent[0].saturating_sub(x));
        let h = h.min(extent[1].saturating_sub(y));
        if w == 0 || h == 0 || n == MAX_DAMAGE {
            continue;
        }
        out[n] = vk::RectLayerKHR {
            offset: vk::Offset2D {
                x: x as i32,
                y: y as i32,
            },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
            layer: 0,
        };
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests;
