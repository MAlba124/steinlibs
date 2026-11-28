//! Instance, validation messages and picking a physical device.

use core::ffi::{CStr, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ash::vk;

use super::convert::err;
use crate::GpuError;
use crate::device::{Adapter, Caps, DeviceKind, Features};

const VALIDATION: &CStr = c"VK_LAYER_KHRONOS_validation";

/// The instance, its debug messenger and the window system entry points,
/// destroyed together. Owned by [`super::Vk`] and dropped after the device.
pub(super) struct Inst {
    pub(super) entry: ash::Entry,
    pub(super) instance: ash::Instance,
    debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    /// VK_KHR_surface and VK_KHR_wayland_surface, when the loader has both.
    pub(super) wsi: Option<Wsi>,
    /// The one driver loaded directly, if any. Declared after `instance`
    /// and unloaded only after Drop destroyed it.
    driver: Option<libloading::Library>,
    /// Validation errors seen, bumped by the callback through its user
    /// pointer. Boxed so the address the driver holds never moves.
    pub(super) errors: Box<AtomicU32>,
}

pub(super) struct Wsi {
    pub(super) surface: ash::khr::surface::Instance,
    pub(super) wayland: ash::khr::wayland_surface::Instance,
}

impl Inst {
    /// The instance. With `driver` candidates and a loader that supports
    /// direct driver loading, only the first of them that loads is used
    /// and no other driver is touched; otherwise the loader scans them all.
    pub(super) fn new(validation: bool, driver: &[PathBuf]) -> Result<Inst, GpuError> {
        let entry = ash::Entry::linked();
        let validation = validation && has_layer(&entry, VALIDATION);
        let loaded = if loader_has_direct(&entry) { load_driver(driver) } else { None };
        let gipa = loaded.as_ref().map(|(_, f)| *f);
        // Listing instance extensions makes the loader open every driver to
        // ask for theirs, the very cost direct loading avoids. With one
        // driver, ask for the window system and drop it if refused.
        let mut wsi = gipa.is_some() || has_wsi(&entry);
        let mut made = create(&entry, validation, gipa, wsi);
        if wsi && matches!(made, Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT)) {
            wsi = false;
            made = create(&entry, validation, gipa, false);
        }
        let instance = made.map_err(err)?;
        let wsi = wsi.then(|| Wsi {
            surface: ash::khr::surface::Instance::new(&entry, &instance),
            wayland: ash::khr::wayland_surface::Instance::new(&entry, &instance),
        });
        let mut inst = Inst {
            entry,
            instance,
            debug: None,
            wsi,
            driver: loaded.map(|(lib, _)| lib),
            errors: Box::new(AtomicU32::new(0)),
        };
        if validation {
            inst.messenger()?;
        }
        Ok(inst)
    }

    /// Whether only one driver was loaded, directly.
    pub(super) fn direct(&self) -> bool {
        self.driver.is_some()
    }

    pub(super) fn validating(&self) -> bool {
        self.debug.is_some()
    }

    fn messenger(&mut self) -> Result<(), GpuError> {
        use vk::DebugUtilsMessageSeverityFlagsEXT as S;
        use vk::DebugUtilsMessageTypeFlagsEXT as T;
        let du = ash::ext::debug_utils::Instance::new(&self.entry, &self.instance);
        let user = (&*self.errors as *const AtomicU32)
            .cast_mut()
            .cast::<c_void>();
        let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(S::WARNING | S::ERROR)
            .message_type(T::GENERAL | T::VALIDATION | T::PERFORMANCE)
            .pfn_user_callback(Some(on_message))
            .user_data(user);
        // SAFETY: the user pointer is the boxed counter, which lives as
        // long as this Inst and so as long as the messenger.
        let m = unsafe { du.create_debug_utils_messenger(&info, None) }.map_err(err)?;
        self.debug = Some((du, m));
        Ok(())
    }
}

/// Held while an instance is made or destroyed. The loader's driver scan
/// crashed in one thread while NVIDIA's driver started up in another
/// (parallel tests on a real adapter), so only one runs at a time.
static LOADER: Mutex<()> = Mutex::new(());

thread_local! {
    /// This thread holds LOADER, so a failed open dropping its Inst
    /// inside the lock does not wait on itself.
    static HELD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// LOADER, or nothing when this thread already holds it.
pub(super) struct LoaderGuard(Option<MutexGuard<'static, ()>>);

impl Drop for LoaderGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            HELD.set(false);
        }
    }
}

pub(super) fn loader_lock() -> LoaderGuard {
    if HELD.get() {
        return LoaderGuard(None);
    }
    let g = LOADER.lock().unwrap_or_else(PoisonError::into_inner);
    HELD.set(true);
    LoaderGuard(Some(g))
}

impl Drop for Inst {
    fn drop(&mut self) {
        let _held = loader_lock();
        // SAFETY: the device and everything made from it is gone by now,
        // Vk's Drop runs before its fields drop.
        unsafe {
            if let Some((du, m)) = self.debug.take() {
                du.destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// Create the instance, with only `gipa`'s driver when given one.
fn create(entry: &ash::Entry, validation: bool, gipa: Option<GetInstanceProcAddr>, wsi: bool) -> Result<ash::Instance, vk::Result> {
    let mut exts = Vec::with_capacity(4);
    if gipa.is_some() {
        exts.push(ash::lunarg::direct_driver_loading::NAME.as_ptr());
    }
    if validation {
        exts.push(ash::ext::debug_utils::NAME.as_ptr());
    }
    if wsi {
        exts.push(ash::khr::surface::NAME.as_ptr());
        exts.push(ash::khr::wayland_surface::NAME.as_ptr());
    }
    let layers = [VALIDATION.as_ptr()];
    let app = vk::ApplicationInfo::default()
        .application_name(c"stein")
        .api_version(vk::API_VERSION_1_3);
    let mut info = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&exts);
    if validation {
        info = info.enabled_layer_names(&layers);
    }
    let drivers = [vk::DirectDriverLoadingInfoLUNARG::default().pfn_get_instance_proc_addr(gipa)];
    let mut only = vk::DirectDriverLoadingListLUNARG::default()
        .mode(vk::DirectDriverLoadingModeLUNARG::EXCLUSIVE)
        .drivers(&drivers);
    if gipa.is_some() {
        info = info.push_next(&mut only);
    }
    // SAFETY: every pointer in `info` outlives the call, and the caller
    // keeps the driver library loaded for the instance's life.
    unsafe { entry.create_instance(&info, None) }
}

/// VK_LUNARG_direct_driver_loading came with loader 1.3.238. Asking the
/// version is answered by the loader alone, no driver is opened.
fn loader_has_direct(entry: &ash::Entry) -> bool {
    // SAFETY: a plain query on a loaded entry.
    let v = unsafe { entry.try_enumerate_instance_version() };
    v.ok().flatten().is_some_and(|v| v >= vk::make_api_version(0, 1, 3, 238))
}

/// Whether the loader and its drivers offer Wayland surfaces. This opens
/// every driver, so it is only asked when they are all loaded anyway.
fn has_wsi(entry: &ash::Entry) -> bool {
    // SAFETY: a plain query on a loaded entry.
    let available = unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
    let has = |n: &CStr| available.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
    has(ash::khr::surface::NAME) && has(ash::khr::wayland_surface::NAME)
}

/// The first candidate that loads and exports the driver entry point.
fn load_driver(candidates: &[PathBuf]) -> Option<(libloading::Library, GetInstanceProcAddr)> {
    candidates.iter().find_map(|path| {
        // SAFETY: a Vulkan driver's initialisers only set up its own state,
        // as the loader relies on when it dlopens the same file.
        let lib = unsafe { libloading::Library::new(path) }.ok()?;
        // SAFETY: the loader driver interface fixes this symbol's signature.
        let f = unsafe { lib.get::<GetInstanceProcAddr>(b"vk_icdGetInstanceProcAddr\0") }.ok().map(|s| *s)?;
        Some((lib, f))
    })
}

type GetInstanceProcAddr = unsafe extern "system" fn(vk::Instance, *const core::ffi::c_char) -> vk::PFN_vkVoidFunction;

fn has_layer(entry: &ash::Entry, name: &CStr) -> bool {
    // SAFETY: a plain query on a loaded entry.
    let layers = unsafe { entry.enumerate_instance_layer_properties() }.unwrap_or_default();
    let found = layers.iter().any(|l| l.layer_name_as_c_str() == Ok(name));
    if !found {
        eprintln!("stein-gpu: validation asked for but {name:?} is not installed");
    }
    found
}

unsafe extern "system" fn on_message(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    user: *mut c_void,
) -> vk::Bool32 {
    let error = severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR);
    // SAFETY: the loader hands us valid callback data or null, and `user`
    // is the AtomicU32 set up in `messenger`.
    let msg = unsafe { data.as_ref().and_then(|d| d.message_as_c_str()) };
    // SAFETY: as above, the counter outlives the messenger.
    let counter = unsafe { user.cast::<AtomicU32>().as_ref() };
    if error && let Some(c) = counter {
        c.fetch_add(1, Ordering::Relaxed);
    }
    let level = if error { "error" } else { "warning" };
    let text = msg.map(CStr::to_string_lossy).unwrap_or_default();
    eprintln!("vulkan {level}: {text}");
    vk::FALSE
}

/// A device that meets the floor, and what it offers beyond it.
pub(super) struct Picked {
    pub(super) pdev: vk::PhysicalDevice,
    pub(super) family: u32,
    pub(super) caps: Caps,
    /// Most sampled images one stage may index in an update-after-bind set.
    pub(super) max_slots: u32,
    /// VK_KHR_swapchain, and VK_KHR_incremental_present with it.
    pub(super) swapchain: bool,
    pub(super) incremental: bool,
    /// Nanoseconds per timestamp tick, 0 without timestamps, and the
    /// queue's valid timestamp bits.
    pub(super) stamp_period: f32,
    pub(super) stamp_bits: u32,
}

pub(super) fn pick(inst: &Inst, adapter: Adapter) -> Result<Picked, GpuError> {
    // SAFETY: a plain query on a live instance.
    let devs = unsafe { inst.instance.enumerate_physical_devices() }.map_err(err)?;
    match adapter {
        Adapter::Index(i) => {
            let d = devs.get(i as usize).ok_or(GpuError::NoAdapter)?;
            probe(inst, *d)
        }
        Adapter::Software => first(inst, &devs, |c| c.kind == DeviceKind::Software),
        Adapter::Drm { major, minor } => first(inst, &devs, |c| {
            c.drm_render == Some([major, minor]) || c.drm_primary == Some([major, minor])
        }),

        Adapter::Auto => {
            let mut ok: Vec<Picked> = devs.iter().filter_map(|d| probe(inst, *d).ok()).collect();
            ok.sort_by_key(|p| rank(p.caps.kind));
            ok.into_iter().next().ok_or(GpuError::NoAdapter)
        }
    }
}

/// Integrated first, it drives the display on a hybrid laptop and costs
/// less power. TODO(backend): match the compositor's DRM device instead.
fn rank(k: DeviceKind) -> u8 {
    match k {
        DeviceKind::Integrated => 0,
        DeviceKind::Discrete => 1,
        DeviceKind::Software => 2,
    }
}

fn first(
    inst: &Inst,
    devs: &[vk::PhysicalDevice],
    want: impl Fn(&Caps) -> bool,
) -> Result<Picked, GpuError> {
    devs.iter()
        .filter_map(|d| probe(inst, *d).ok())
        .find(|p| want(&p.caps))
        .ok_or(GpuError::NoAdapter)
}

/// Check `pdev` against the floor and fill its caps.
fn probe(inst: &Inst, pdev: vk::PhysicalDevice) -> Result<Picked, GpuError> {
    let i = &inst.instance;
    // SAFETY: plain queries on a live instance and one of its devices.
    let exts = unsafe { i.enumerate_device_extension_properties(pdev) }.map_err(err)?;
    let has = |name: &CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));

    let mut v12p = vk::PhysicalDeviceVulkan12Properties::default();
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut v12p);
    let drm_ok = has(ash::ext::physical_device_drm::NAME);
    if drm_ok {
        props = props.push_next(&mut drm);
    }
    // SAFETY: the chain only holds structs of extensions the device has.
    unsafe { i.get_physical_device_properties2(pdev, &mut props) };
    let p = props.properties;
    if p.api_version < vk::API_VERSION_1_3 {
        return Err(GpuError::Missing("Vulkan 1.3"));
    }
    let (dual, draw_count) = floor(i, pdev)?;
    let family = queue_family(i, pdev).ok_or(GpuError::Missing("a graphics and compute queue"))?;
    let max_slots = v12p.max_per_stage_descriptor_update_after_bind_sampled_images;
    let drm_render = (drm_ok && drm.has_render == vk::TRUE)
        .then_some([drm.render_major as u32, drm.render_minor as u32]);
    let drm_primary = (drm_ok && drm.has_primary == vk::TRUE)
        .then_some([drm.primary_major as u32, drm.primary_minor as u32]);
    let caps = Caps {
        name: p
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: kind(p.device_type),
        api: [
            vk::api_version_major(p.api_version),
            vk::api_version_minor(p.api_version),
            vk::api_version_patch(p.api_version),
        ],
        drm_render,
        drm_primary,
        mapped_vram: mapped_vram(i, pdev),
        max_root: p.limits.max_push_constants_size.min(crate::MAX_ROOT as u32),
        texture_slots: 0,
        features: features(&has, dual, draw_count),
        validation: inst.validating(),
        present: false,
        direct: false,
    };
    let swapchain = inst.wsi.is_some() && has(ash::khr::swapchain::NAME);
    let incremental = swapchain && has(ash::khr::incremental_present::NAME);
    let caps = Caps { present: swapchain, ..caps };
    // SAFETY: a plain query.
    let fams = unsafe { i.get_physical_device_queue_family_properties(pdev) };
    let stamp_bits = fams.get(family as usize).map_or(0, |f| f.timestamp_valid_bits);
    let stamps = p.limits.timestamp_compute_and_graphics == vk::TRUE;
    Ok(Picked {
        pdev,
        family,
        caps,
        max_slots,
        swapchain,
        incremental,
        stamp_period: if stamps { p.limits.timestamp_period } else { 0.0 },
        stamp_bits,
    })
}

/// The feature floor. Returns the optional dualSrcBlend and
/// drawIndirectCount on the way.
fn floor(i: &ash::Instance, pdev: vk::PhysicalDevice) -> Result<(bool, bool), GpuError> {
    let mut v12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut v13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut f = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut v12)
        .push_next(&mut v13);
    // SAFETY: core 1.2 and 1.3 structs on a 1.3 device.
    unsafe { i.get_physical_device_features2(pdev, &mut f) };
    let dual = f.features.dual_src_blend == vk::TRUE;
    let need = [
        (v12.buffer_device_address, "bufferDeviceAddress"),
        (v12.timeline_semaphore, "timelineSemaphore"),
        (v12.descriptor_indexing, "descriptorIndexing"),
        (v12.runtime_descriptor_array, "runtimeDescriptorArray"),
        (
            v12.descriptor_binding_partially_bound,
            "descriptorBindingPartiallyBound",
        ),
        (
            v12.descriptor_binding_sampled_image_update_after_bind,
            "descriptorBindingSampledImageUpdateAfterBind",
        ),
        (
            v12.shader_sampled_image_array_non_uniform_indexing,
            "shaderSampledImageArrayNonUniformIndexing",
        ),
        (v12.storage_buffer8_bit_access, "storageBuffer8BitAccess"),
        (v13.dynamic_rendering, "dynamicRendering"),
        (v13.synchronization2, "synchronization2"),
        (v13.maintenance4, "maintenance4"),
    ];
    match need.iter().find(|(on, _)| *on != vk::TRUE) {
        Some((_, name)) => Err(GpuError::Missing(name)),
        None => Ok((dual, v12.draw_indirect_count == vk::TRUE)),
    }
}

fn queue_family(i: &ash::Instance, pdev: vk::PhysicalDevice) -> Option<u32> {
    let want = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE;
    // SAFETY: a plain query.
    let fams = unsafe { i.get_physical_device_queue_family_properties(pdev) };
    fams.iter()
        .position(|f| f.queue_flags.contains(want))
        .map(|p| p as u32)
}

fn kind(t: vk::PhysicalDeviceType) -> DeviceKind {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => DeviceKind::Discrete,
        vk::PhysicalDeviceType::CPU => DeviceKind::Software,
        _ => DeviceKind::Integrated,
    }
}

/// The largest heap with a DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT type.
fn mapped_vram(i: &ash::Instance, pdev: vk::PhysicalDevice) -> u64 {
    let want = vk::MemoryPropertyFlags::DEVICE_LOCAL
        | vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT;
    // SAFETY: a plain query.
    let m = unsafe { i.get_physical_device_memory_properties(pdev) };
    m.memory_types_as_slice()
        .iter()
        .filter(|t| t.property_flags.contains(want))
        .filter_map(|t| m.memory_heaps_as_slice().get(t.heap_index as usize))
        .map(|h| h.size)
        .max()
        .unwrap_or(0)
}

fn features(has: &impl Fn(&CStr) -> bool, dual: bool, draw_count: bool) -> Features {
    let exts = [
        (c"VK_EXT_descriptor_buffer", Features::DESCRIPTOR_BUFFER),
        (c"VK_EXT_descriptor_heap", Features::DESCRIPTOR_HEAP),
        (c"VK_EXT_shader_object", Features::SHADER_OBJECT),
        (c"VK_EXT_mesh_shader", Features::MESH),
        (c"VK_EXT_host_image_copy", Features::HOST_IMAGE_COPY),
        (c"VK_KHR_incremental_present", Features::INCREMENTAL_PRESENT),
        (c"VK_KHR_unified_image_layouts", Features::UNIFIED_LAYOUTS),
        (c"VK_KHR_present_wait", Features::PRESENT_WAIT),
        (c"VK_EXT_memory_budget", Features::MEMORY_BUDGET),
    ];
    let mut f = exts
        .iter()
        .filter(|(n, _)| has(n))
        .fold(0, |acc, (_, bit)| acc | bit.0);
    if dual {
        f |= Features::DUAL_SOURCE.0;
    }
    if draw_count {
        f |= Features::DRAW_INDIRECT_COUNT.0;
    }
    Features(f)
}
