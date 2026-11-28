//! One wl_surface and everything hanging off it: its role, its shm buffer
//! or swapchain, its size and where it is in the paint cycle.
//!
//! Every object made for a surface carries its key as user data, so an
//! event finds its surface without a lookup table.

use std::fs::File;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use memmap2::{MmapMut, MmapOptions};
use rustix::fs::{MemfdFlags, memfd_create};
use stein_ui::Input;
use stein_ui::draw::{Damage, MAX_DAMAGE_RECTS};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{
    self, ZxdgToplevelDecorationV1,
};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};

use crate::gpu;
use crate::input;
use crate::layer::{self, Keys, Layer};
use crate::multi::{Popup, Role, Side};
use crate::paint::{Atlas, Backdrop, clamp_region, clamp_regions};
use crate::plan;
use crate::selection;
use crate::shell::{Shell, ShellError};
use crate::video;

const DEFAULT_W: u32 = 800;
const DEFAULT_H: u32 = 600;
/// How long after its last size change a GPU surface is painted in
/// software. Each new size would otherwise remake the swapchain, and new
/// swapchain images cost the driver milliseconds to back on first use, so a
/// drag stutters; the shm pool is sized up front and reused. Once the size
/// holds this long the swapchain is remade once, at the size it settled on.
const SETTLE: Duration = Duration::from_millis(120);

/// Where a surface is in its paint cycle. A simple state machine per
/// surface, moved by configures, frame callbacks and wakes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    /// Committed with no buffer, waiting for the first configure.
    WaitConfigure,
    /// Configured and dirty, render on the next turn.
    NeedsPaint,
    /// Committed, waiting on wl_surface.frame done.
    WaitFrame,
    /// Nothing changed and no frame callback outstanding. Input wakes it.
    Idle,
    /// The compositor closed it. Nothing happens until the scene drops it.
    Closed,
}

/// The role objects, destroyed before the wl_surface.
pub(crate) enum RoleObj {
    Window {
        xdg: xdg_surface::XdgSurface,
        top: xdg_toplevel::XdgToplevel,
        deco: Option<ZxdgToplevelDecorationV1>,
    },
    Layer(ZwlrLayerSurfaceV1),
    Popup {
        xdg: xdg_surface::XdgSurface,
        popup: xdg_popup::XdgPopup,
    },
}

/// One shm buffer and its pool. A surface has two, see [`free_slot`].
#[derive(Default)]
pub(crate) struct Shm {
    file: Option<File>,
    map: Option<MmapMut>,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    cap: usize,
    size: [u32; 2],
    /// What the buffer was made as, XRGB or premultiplied ARGB.
    backdrop: Backdrop,
    /// Committed and not released, the compositor may still read it.
    busy: bool,
    /// Just (re)allocated, so its contents are undefined and a partial
    /// repaint would leave garbage outside the damaged region.
    fresh: bool,
    /// What frames presented through the other buffer changed since this
    /// one was painted. Painted along with the next frame's own damage.
    stale: Damage,
}

pub(crate) struct Surf {
    pub(crate) key: u32,
    /// The role asked for, compared against the scene's wants.
    pub(crate) role: Role,
    pub(crate) wl: wl_surface::WlSurface,
    pub(crate) obj: RoleObj,
    pub(crate) phase: Phase,
    pub(crate) size: [u32; 2],
    /// From the role's configure, 0 meaning the client picks.
    pub(crate) pending: [u32; 2],
    /// What "the client picks" resolves to.
    pub(crate) default: [u32; 2],
    /// The first frame has no previous one to differ from, so it presents.
    first_frame: bool,
    shm: [Shm; 2],
    pub(crate) swap: Option<stein_gpu::Surface>,
    /// Events since this surface's last build.
    pub(crate) input: Input,
    /// When it last built, for the frame's dt.
    pub(crate) built_at: Option<Instant>,
    /// What a toplevel was last told it is called.
    title: String,
    /// When a GPU surface's size last changed, while it still paints in
    /// software for it. See [`SETTLE`].
    resized_at: Option<Instant>,
    /// The last frame went out through shm in place of the swapchain.
    bridged: bool,
    /// Present the next frame even if the scene changed nothing.
    pub(crate) repaint: bool,
    /// The video layer under it, when the scene asked for one.
    pub(crate) video: Option<video::Layer>,
}

impl Surf {
    /// Mapped and able to parent a popup.
    fn is_mapped(&self) -> bool {
        !self.first_frame && self.phase != Phase::Closed
    }
}

pub(crate) fn find(shell: &mut Shell, key: u32) -> Option<&mut Surf> {
    shell.surfs.iter_mut().find(|s| s.key == key)
}

// ---------------------------------------------------------------------------
// reconcile
// ---------------------------------------------------------------------------

/// Make the surfaces match the scene's wants: drop what is no longer wanted
/// or changed role, make what is new. A popup whose parent is not mapped yet
/// waits for a later turn.
pub(crate) fn reconcile(shell: &mut Shell, qh: &QueueHandle<Shell>) -> Result<(), ShellError> {
    shell.wants.clear();
    shell.wants.extend_from_slice(shell.scene.surfaces());
    shell.have.clear();
    shell
        .have
        .extend(shell.surfs.iter().map(|s| (s.key, s.role)));
    let (mut drop, mut make) = (
        core::mem::take(&mut shell.plan_drop),
        core::mem::take(&mut shell.plan_make),
    );
    plan::plan(&shell.have, &shell.wants, &mut drop, &mut make);
    for &i in &drop {
        destroy(shell, i);
    }
    for &j in &make {
        let w = shell.wants[j];
        create(shell, qh, w.key, w.role)?;
    }
    shell.plan_drop = drop;
    shell.plan_make = make;
    Ok(())
}

fn create(
    shell: &mut Shell,
    qh: &QueueHandle<Shell>,
    key: u32,
    role: Role,
) -> Result<(), ShellError> {
    let Some(compositor) = shell.compositor.clone() else {
        return Ok(());
    };
    // Checked before anything is made, so a skipped want leaves nothing.
    let obj_ready = match role {
        Role::Popup(p) => shell
            .surfs
            .iter()
            .any(|s| s.key == p.parent && s.is_mapped()),
        Role::Layer {
            output: Some(id), ..
        } => shell.outputs.iter().any(|o| o.global == id),
        _ => true,
    };
    if !obj_ready {
        return Ok(());
    }
    if let Role::Layer { layer, .. } = role {
        layer::check(&layer).map_err(ShellError::Layer)?;
    }
    let wl = compositor.create_surface(qh, key);
    let made = match role {
        Role::Window { fixed, preferred } => window(shell, qh, &wl, key, fixed, preferred)?,
        Role::Layer { layer, output } => match shell.layer_shell.clone() {
            Some(ls) => layer_surface(shell, qh, &wl, key, &ls, layer, output),
            None => {
                // No layer shell: a window of the layer's size, which is
                // better than nothing on a compositor without one.
                let fixed = (layer.size[0] > 0 && layer.size[1] > 0).then_some(layer.size);
                window(shell, qh, &wl, key, fixed, None)?
            }
        },
        Role::Popup(p) => match popup(shell, qh, &wl, key, &p) {
            Some(m) => m,
            None => {
                wl.destroy();
                return Ok(());
            }
        },
    };
    let (obj, size) = made;
    wl.commit();
    trace!("surface {key} made as {role:?}");
    shell.surfs.push(Surf {
        key,
        role,
        wl,
        obj,
        phase: Phase::WaitConfigure,
        size,
        pending: [0, 0],
        default: size,
        first_frame: true,
        shm: [Shm::default(), Shm::default()],
        swap: None,
        input: Input::default(),
        built_at: None,
        title: String::new(),
        resized_at: None,
        bridged: false,
        repaint: false,
        video: None,
    });
    Ok(())
}

type Made = (RoleObj, [u32; 2]);

fn window(
    shell: &mut Shell,
    qh: &QueueHandle<Shell>,
    wl: &wl_surface::WlSurface,
    key: u32,
    fixed: Option<[u32; 2]>,
    preferred: Option<[u32; 2]>,
) -> Result<Made, ShellError> {
    let wm = shell
        .wm_base
        .clone()
        .ok_or(ShellError::MissingGlobal("xdg_wm_base"))?;
    let xdg = wm.get_xdg_surface(wl, qh, key);
    let top = xdg.get_toplevel(qh, key);
    // Before the first commit, as the protocol requires.
    let deco = shell.decoration_manager.as_ref().map(|m| {
        let d = m.get_toplevel_decoration(&top, qh, ());
        d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        d
    });
    top.set_title(shell.scene.title(key).to_string());
    top.set_app_id(shell.scene.app_id().to_string());
    let size = match fixed {
        // Equal min and max pins the size, and is also what makes a tiling
        // compositor float and centre the window instead of tiling it.
        Some([w, h]) => {
            let (w, h) = (w.max(1), h.max(1));
            top.set_min_size(w as i32, h as i32);
            top.set_max_size(w as i32, h as i32);
            [w, h]
        }
        None => {
            top.set_min_size(320, 240);
            preferred.map_or([DEFAULT_W, DEFAULT_H], |[w, h]| [w.max(320), h.max(240)])
        }
    };
    Ok((RoleObj::Window { xdg, top, deco }, size))
}

/// Everything here is double buffered and lands with the first commit.
fn layer_surface(
    shell: &Shell,
    qh: &QueueHandle<Shell>,
    wl: &wl_surface::WlSurface,
    key: u32,
    ls: &ZwlrLayerShellV1,
    spec: Layer,
    output: Option<u32>,
) -> Made {
    let output = output
        .and_then(|id| shell.outputs.iter().find(|o| o.global == id))
        .map(|o| &o.proxy);
    let level = match spec.level {
        layer::Level::Background => zwlr_layer_shell_v1::Layer::Background,
        layer::Level::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        layer::Level::Top => zwlr_layer_shell_v1::Layer::Top,
        layer::Level::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    };
    let keys = match layer::keys_for(spec.keys, ls.version()) {
        Keys::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        Keys::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        Keys::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    };
    let l = ls.get_layer_surface(wl, output, level, spec.namespace.to_string(), qh, key);
    l.set_size(spec.size[0], spec.size[1]);
    l.set_anchor(zwlr_layer_surface_v1::Anchor::from_bits_truncate(
        u32::from(spec.anchor.0),
    ));
    l.set_exclusive_zone(spec.exclusive);
    let [top, right, bottom, left] = spec.margin;
    l.set_margin(top, right, bottom, left);
    l.set_keyboard_interactivity(keys);
    (RoleObj::Layer(l), layer::configured(&spec, 0, 0))
}

fn popup(
    shell: &mut Shell,
    qh: &QueueHandle<Shell>,
    wl: &wl_surface::WlSurface,
    key: u32,
    p: &Popup,
) -> Option<Made> {
    let wm = shell.wm_base.clone()?;
    let parent = shell.surfs.iter().find(|s| s.key == p.parent)?;
    let pos = wm.create_positioner(qh, ());
    let size = [p.size[0].max(1), p.size[1].max(1)];
    pos.set_size(size[0] as i32, size[1] as i32);
    let [x, y, w, h] = p.anchor;
    pos.set_anchor_rect(x, y, w.max(1), h.max(1));
    let (anchor, gravity) = placement(p.side);
    pos.set_anchor(anchor);
    pos.set_gravity(gravity);
    use xdg_positioner::ConstraintAdjustment as C;
    pos.set_constraint_adjustment(C::SlideX | C::SlideY | C::FlipX | C::FlipY);
    let xdg = wm.get_xdg_surface(wl, qh, key);
    let popup = match &parent.obj {
        RoleObj::Window { xdg: px, .. } | RoleObj::Popup { xdg: px, .. } => {
            xdg.get_popup(Some(px), &pos, qh, key)
        }
        RoleObj::Layer(l) => {
            let popup = xdg.get_popup(None, &pos, qh, key);
            l.get_popup(&popup);
            popup
        }
    };
    pos.destroy();
    if p.grab
        && let Some(seat) = shell.seat.as_ref()
    {
        popup.grab(seat, shell.clip.serial);
    }
    Some((RoleObj::Popup { xdg, popup }, size))
}

/// Which corner of the anchor rect a popup hangs from and which way it
/// grows, for each side.
fn placement(side: Side) -> (xdg_positioner::Anchor, xdg_positioner::Gravity) {
    use xdg_positioner::{Anchor as A, Gravity as G};
    match side {
        Side::Below => (A::BottomLeft, G::BottomRight),
        Side::Above => (A::TopLeft, G::TopRight),
        Side::Right => (A::TopRight, G::BottomRight),
        Side::Left => (A::TopLeft, G::BottomLeft),
    }
}

/// Destroy surface `i`: its swapchain, buffer, role objects, then itself.
pub(crate) fn destroy(shell: &mut Shell, i: usize) {
    // The lock goes before the surface it names.
    if shell
        .locked
        .as_ref()
        .is_some_and(|(k, _)| *k == shell.surfs[i].key)
    {
        input::unlock(shell);
    }
    let mut s = shell.surfs.remove(i);
    // The scene is told nothing: it asked for the surface to go.
    if let Some(layer) = s.video.take() {
        video::destroy(layer).for_each(drop);
    }
    if let (Some(g), Some(swap)) = (shell.gpu.as_mut(), s.swap)
        && let Err(e) = g.forget(swap)
    {
        eprintln!("stein-shell: dropping a swapchain failed: {e}");
    }
    for shm in s.shm {
        if let Some(b) = shm.buffer {
            b.destroy();
        }
        if let Some(p) = shm.pool {
            p.destroy();
        }
    }
    match s.obj {
        RoleObj::Window { xdg, top, deco } => {
            if let Some(d) = deco {
                d.destroy();
            }
            top.destroy();
            xdg.destroy();
        }
        RoleObj::Layer(l) => l.destroy(),
        RoleObj::Popup { xdg, popup } => {
            popup.destroy();
            xdg.destroy();
        }
    }
    s.wl.destroy();
    if shell.ptr_key == Some(s.key) {
        shell.ptr_key = None;
    }
    if shell.kbd_key == Some(s.key) {
        shell.kbd_key = None;
    }
    trace!("surface {} destroyed", s.key);
}

// ---------------------------------------------------------------------------
// shm
// ---------------------------------------------------------------------------

/// The buffer to paint the next frame into, the first one the compositor
/// does not hold. `None` while it holds both, and the release of either
/// lets the frame in.
///
/// A compositor that uploads on commit releases at once, so the first
/// buffer is free again by the next frame and the second is never made.
/// One that reads the pixels in place, the pixman renderer, keeps a buffer
/// until another replaces it. With a single buffer that is a stall with no
/// end, since the next frame waits on a release that waits on the next
/// frame. There the two take turns.
fn free_slot(shm: &[Shm; 2]) -> Option<usize> {
    shm.iter().position(|b| !b.busy)
}

/// What buffer `b` has to repaint to show a frame whose own damage is
/// `frame`, and the books moved on for it having been presented.
///
/// A frame's damage is its difference from the frame before, which the
/// buffer last shown holds. The other buffer is older by every frame it
/// sat out, so each of those is added to what it owes.
fn owed(shm: &mut [Shm; 2], b: usize, frame: &Damage) -> Damage {
    let mut all = core::mem::take(&mut shm[b].stale);
    all.absorb(frame);
    for (i, other) in shm.iter_mut().enumerate() {
        if i != b {
            other.stale.absorb(frame);
        }
    }
    all
}

// Grows the memfd + wl_shm_pool + wl_buffer to fit w*h. Old objects are
// destroyed explicitly and the old mapping is dropped, which munmaps, and the
// old File is dropped, which closes the fd. No leaks.
fn ensure_buffer(
    shm: &mut Shm,
    key: u32,
    qh: &QueueHandle<Shell>,
    global: &wl_shm::WlShm,
    [w, h]: [u32; 2],
    bd: Backdrop,
) -> Result<(), ShellError> {
    if shm.buffer.is_some() && shm.size == [w, h] && shm.backdrop == bd {
        return Ok(());
    }
    let over = ShellError::SizeOverflow {
        width: w,
        height: h,
    };
    let stride = (w as usize).checked_mul(4).ok_or(over)?;
    let needed = stride
        .checked_mul(h as usize)
        .ok_or(ShellError::SizeOverflow {
            width: w,
            height: h,
        })?;
    // tear down the old buffer first, the pool may be about to go away
    if let Some(buf) = shm.buffer.take() {
        buf.destroy();
    }
    shm.busy = false;
    if needed > shm.cap {
        grow_pool(shm, qh, global, needed)?;
    }
    if let Some(pool) = shm.pool.as_ref() {
        let format = match bd {
            Backdrop::Opaque => wl_shm::Format::Xrgb8888,
            Backdrop::Clear => wl_shm::Format::Argb8888,
        };
        shm.backdrop = bd;
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, key);
        shm.buffer = Some(buffer);
        shm.fresh = true;
        shm.stale.clear();
        shm.size = [w, h];
    }
    Ok(())
}

fn grow_pool(
    shm: &mut Shm,
    qh: &QueueHandle<Shell>,
    global: &wl_shm::WlShm,
    needed: usize,
) -> Result<(), ShellError> {
    if let Some(pool) = shm.pool.take() {
        pool.destroy();
    }
    shm.map = None;
    shm.file = None;
    // over-allocate so a drag-resize does not thrash the fd
    let cap = needed.next_power_of_two().max(4096);
    let fd = memfd_create(
        "stein-shell-shm",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(ShellError::Memfd)?;
    rustix::fs::ftruncate(&fd, cap as u64).map_err(ShellError::Truncate)?;
    let file = File::from(fd);
    // SAFETY: memfd is private to this process and only this mapping
    // touches it, the compositor gets its own read-only view.
    let map = unsafe { MmapOptions::new().len(cap).map_mut(&file) }.map_err(ShellError::Mmap)?;
    let pool = global.create_pool(file.as_fd(), cap as i32, qh, ());
    trace!("shm: new pool, cap {cap} bytes");
    shm.file = Some(file);
    shm.map = Some(map);
    shm.pool = Some(pool);
    shm.cap = cap;
    Ok(())
}

fn pixels_mut(map: &mut MmapMut) -> &mut [u32] {
    let bytes: &mut [u8] = map.as_mut();
    let len = bytes.len() / 4;
    // SAFETY: an mmap base is page aligned so the u32 alignment holds, and the
    // length is truncated to whole u32s.
    unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u32>(), len) }
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Build surface `i` and, if anything differs, present it.
pub(crate) fn render(
    shell: &mut Shell,
    i: usize,
    qh: &QueueHandle<Shell>,
    conn: &Connection,
) -> Result<(), ShellError> {
    let Some(b) = free_slot(&shell.surfs[i].shm) else {
        // compositor still owns the pixels, wait for wl_buffer.release
        trace!("surface {} busy, waiting for release", shell.surfs[i].key);
        return Ok(());
    };
    let t0 = Instant::now();
    let Some(global) = shell.shm.clone() else {
        return Ok(());
    };
    let key = shell.surfs[i].key;
    let size = shell.surfs[i].size;
    // The swapchain stands in for the shm buffer while the GPU presents,
    // except while a resize is still moving.
    let gpu = gpu::presents(&shell.gpu);
    let bridge = gpu
        && shell.surfs[i]
            .resized_at
            .is_some_and(|t| t.elapsed() < SETTLE);
    let bd = shell.scene.backdrop(key);
    if !gpu || bridge {
        ensure_buffer(&mut shell.surfs[i].shm[b], key, qh, &global, size, bd)?;
    }
    // Build before touching the buffer. Layout is a pure transform over the
    // configured size, so a resize needs nothing here but a different viewport.
    let input = input::take_input(shell, i);
    shell
        .scene
        .build(key, input, [size[0] as f32, size[1] as f32]);
    trace!(
        "surface {key} built {}x{} in {:?}",
        size[0],
        size[1],
        t0.elapsed()
    );
    // The layer is placed by the build, and a move rides this frame's commit.
    video::sync(shell, i, qh);
    // After the build, because that is when the scene knows whether anything
    // was copied and whether it is asking for a paste.
    selection::pump_clipboard(shell, qh, conn);
    // The scene knows now whether it wants the pointer locked.
    input::drive_lock(shell, i, qh);
    // And when it knows what is under the pointer, so a shape that depends on
    // what is hovered follows the pointer rather than lagging a frame.
    if shell.ptr_key == Some(key) {
        let want = input::cursor_for(shell, key);
        input::apply_cursor(shell, qh, want);
    }
    // Nothing changed and nothing is animating, so do not attach, do not
    // commit and do not ask for another frame callback. The surface sleeps
    // until something wakes it. This is the whole point of diffing the draw
    // list, and it is what an immediate-mode toolkit usually cannot do.
    let s = &mut shell.surfs[i];
    if !shell.scene.needs_present(key) && !s.first_frame && !s.repaint {
        s.phase = Phase::Idle;
        return Ok(());
    }
    s.first_frame = false;
    s.repaint = false;
    s.bridged = bridge;
    if bridge {
        // With explicit sync on, a buffer of our own on a surface the WSI
        // presents to is a protocol error, so the swapchain goes first, its
        // Vulkan surface with it. Settling makes a new one.
        let t = Instant::now();
        if let (Some(g), Some(swap)) = (shell.gpu.as_mut(), s.swap.take())
            && let Err(e) = g.forget(swap)
        {
            eprintln!("stein-shell: dropping a swapchain for a resize failed: {e}");
        }
        trace!("surface {key} swapchain dropped in {:?}", t.elapsed());
        let t = Instant::now();
        present_shm(shell, i, qh, true, b);
        trace!("surface {key} bridged paint in {:?}", t.elapsed());
        return Ok(());
    }
    if gpu {
        let t = Instant::now();
        if present_gpu(shell, i, qh, conn) {
            trace!("surface {key} gpu present in {:?}", t.elapsed());
            return Ok(());
        }
        // The GPU is gone. Paint this frame in software into a new buffer.
        let s = &mut shell.surfs[i];
        ensure_buffer(&mut s.shm[b], key, qh, &global, size, bd)?;
    }
    present_shm(shell, i, qh, false, b);
    Ok(())
}

/// Present surface `i` through its swapchain. False when the GPU failed
/// and was dropped, every swapchain with it.
fn present_gpu(shell: &mut Shell, i: usize, qh: &QueueHandle<Shell>, conn: &Connection) -> bool {
    let paced = shell.scene.paced();
    let s = &mut shell.surfs[i];
    let key = s.key;
    let [w, h] = s.size;
    if let RoleObj::Window { xdg, .. } | RoleObj::Popup { xdg, .. } = &s.obj {
        xdg.set_window_geometry(0, 0, w as i32, h as i32);
    }
    // Presenting commits the surface, so the frame request and the
    // geometry ride on that commit. The whole frame is redrawn, the
    // damage is only the compositor's hint.
    if paced {
        s.wl.frame(qh, key);
    }
    let mut hint = [[0u32; 4]; MAX_DAMAGE_RECTS];
    let hints = clamp_regions(&shell.scene.damage(key), w, h, &mut hint);
    let ok = gpu::present_frame(
        &mut shell.gpu,
        conn,
        &s.wl,
        &mut s.swap,
        [w, h],
        &mut *shell.scene,
        key,
        &hint[..hints],
    );
    if !ok {
        for s in &mut shell.surfs {
            s.swap = None;
        }
        return false;
    }
    s.phase = if paced { Phase::WaitFrame } else { Phase::Idle };
    // This frame is in no shm buffer, so none of them is one diff behind.
    for shm in &mut s.shm {
        shm.fresh = true;
    }
    shell.scene.presented(key);
    retitle(shell, i);
    shell.frame_count = shell.frame_count.wrapping_add(1);
    if shell.frame_count == 1 || shell.frame_count.is_multiple_of(60) {
        trace!(
            "gpu frame {} of surface {key} at {w}x{h}",
            shell.frame_count
        );
    }
    true
}

/// Paint surface `i` into its shm buffer `b` and commit, on the CPU alone
/// when `cpu`, else through the GPU's offscreen canvas if there is one.
fn present_shm(shell: &mut Shell, i: usize, qh: &QueueHandle<Shell>, cpu: bool, b: usize) {
    let paced = shell.scene.paced();
    // With no GPU to hand it, paint_frame paints on the CPU.
    let mut none = None;
    let g = if cpu { &mut none } else { &mut shell.gpu };
    let s = &mut shell.surfs[i];
    let key = s.key;
    let [w, h] = s.size;
    let frame = shell.scene.damage(key);
    let paint = owed(&mut s.shm, b, &frame);
    // A fresh buffer holds nothing, so the whole surface is painted. Any
    // other is painted where it is behind, and the compositor is told only
    // what differs from the frame it is showing.
    let mut regions = [[0u32; 4]; MAX_DAMAGE_RECTS];
    let mut hint = [[0u32; 4]; MAX_DAMAGE_RECTS];
    let (count, hints) = if s.shm[b].fresh {
        regions[0] = clamp_region(None, w, h);
        hint[0] = regions[0];
        (1, 1)
    } else {
        (
            clamp_regions(&paint, w, h, &mut regions),
            clamp_regions(&frame, w, h, &mut hint),
        )
    };
    if let Some(map) = s.shm[b].map.as_mut() {
        let px = pixels_mut(map);
        let used = w as usize * h as usize;
        if used <= px.len() {
            let (pixels, asize) = shell.scene.atlas();
            let atlas = Atlas {
                pixels,
                size: asize,
                images: shell.scene.images(),
            };
            let (px, quads, regions) = (&mut px[..used], shell.scene.quads(key), &regions[..count]);
            let t = Instant::now();
            let n = quads.len();
            gpu::paint_frame(
                g,
                shell.blend,
                shell.scene.backdrop(key),
                px,
                w as usize,
                [w, h],
                quads,
                &atlas,
                regions,
            );
            trace!("surface {key} painted {n} quads in {:?}", t.elapsed());
        }
    }
    s.shm[b].fresh = false;
    let Some(buffer) = s.shm[b].buffer.as_ref() else {
        return;
    };
    if let RoleObj::Window { xdg, .. } | RoleObj::Popup { xdg, .. } = &s.obj {
        xdg.set_window_geometry(0, 0, w as i32, h as i32);
    }
    s.wl.attach(Some(buffer), 0, 0);
    // Tell the compositor what actually changed, so it can limit its own
    // composite and, on a tiled or remote setup, what it transfers.
    for [rx, ry, rw, rh] in hint[..hints].iter().copied() {
        s.wl.damage_buffer(rx as i32, ry as i32, rw as i32, rh as i32);
    }
    if paced {
        s.wl.frame(qh, key);
    }
    s.wl.commit();
    s.shm[b].busy = true;
    // Unpaced, the buffer coming back is what lets the next frame in.
    s.phase = if paced { Phase::WaitFrame } else { Phase::Idle };
    shell.scene.presented(key);
    retitle(shell, i);
    shell.frame_count = shell.frame_count.wrapping_add(1);
}

/// Time until the first bridged surface's size has held for [`SETTLE`].
pub(crate) fn settle_timeout(shell: &Shell) -> Option<Duration> {
    shell
        .surfs
        .iter()
        .filter(|s| s.bridged)
        .filter_map(|s| s.resized_at)
        .map(|t| SETTLE.saturating_sub(t.elapsed()))
        .min()
}

/// Surfaces whose size has held: back to the GPU, remaking the swapchain
/// once at the size they settled on.
pub(crate) fn settle(shell: &mut Shell) {
    for s in &mut shell.surfs {
        let held = s.resized_at.is_some_and(|t| t.elapsed() >= SETTLE);
        if !held {
            continue;
        }
        s.resized_at = None;
        if !s.bridged {
            continue;
        }
        s.repaint = true;
        if s.phase == Phase::Idle {
            s.phase = Phase::NeedsPaint;
        }
    }
}

/// Push a title change to a toplevel.
///
/// Compared rather than set every frame: `set_title` is a round trip through
/// the compositor and most frames do not change it.
fn retitle(shell: &mut Shell, i: usize) {
    let s = &mut shell.surfs[i];
    let RoleObj::Window { top, .. } = &s.obj else {
        return;
    };
    let t = shell.scene.title(s.key);
    if t == s.title {
        return;
    }
    s.title.clear();
    s.title.push_str(t);
    top.set_title(s.title.clone());
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// A configure landed: size from what the role said, 0 meaning ours.
fn configured(shell: &mut Shell, key: u32) {
    let Some(s) = find(shell, key) else {
        return;
    };
    let pick = |got: u32, ours: u32, have: u32| if got == 0 { have.max(ours) } else { got };
    let size = [
        pick(s.pending[0], s.default[0], s.size[0]),
        pick(s.pending[1], s.default[1], s.size[1]),
    ];
    if size != s.size && (s.swap.is_some() || s.resized_at.is_some()) {
        s.resized_at = Some(Instant::now());
    }
    // A new size needs a buffer of that size even when the scene draws the
    // same thing, a window resized by less than a text cell say. The
    // compositor holds the resize until it has one.
    if size != s.size {
        s.repaint = true;
    }
    s.size = size;
    if s.phase != Phase::Closed {
        s.phase = Phase::NeedsPaint;
    }
    trace!("surface {key} configured {}x{}", size[0], size[1]);
}

/// The compositor closed `key`. Told once: a dismissed popup can hear
/// popup_done more than once before it is destroyed.
fn close(shell: &mut Shell, key: u32) {
    let Some(s) = find(shell, key) else {
        return;
    };
    if s.phase == Phase::Closed {
        return;
    }
    s.phase = Phase::Closed;
    shell.scene.closed(key);
}

// xdg_surface.configure is the commit barrier. Every configure must be acked,
// and the ack must be followed by a commit carrying the new state.
impl Dispatch<xdg_surface::XdgSurface, u32> for Shell {
    fn event(
        shell: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            configured(shell, *key);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, u32> for Shell {
    fn event(
        shell: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // arrives before xdg_surface.configure, only stages the size
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(s) = find(shell, *key) {
                    s.pending = [width.max(0) as u32, height.max(0) as u32];
                }
            }
            xdg_toplevel::Event::Close => close(shell, *key),
            _ => {}
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, u32> for Shell {
    fn event(
        shell: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure { width, height, .. } => {
                if let Some(s) = find(shell, *key) {
                    s.pending = [width.max(0) as u32, height.max(0) as u32];
                }
            }
            // A click outside a grabbing popup, or the compositor gave up
            // on it.
            xdg_popup::Event::PopupDone => close(shell, *key),
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &xdg_positioner::XdgPositioner,
        _: xdg_positioner::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The positioner has no events.
    }
}

// The layer surface's configure is its own commit barrier, acked the same
// way as xdg_surface's.
impl Dispatch<ZwlrLayerSurfaceV1, u32> for Shell {
    fn event(
        shell: &mut Self,
        l: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                l.ack_configure(serial);
                if let Some(s) = find(shell, *key)
                    && let Role::Layer { layer, .. } = s.role
                {
                    s.pending = layer::configured(&layer, width, height);
                }
                configured(shell, *key);
            }
            // The output went away or the user dismissed it.
            zwlr_layer_surface_v1::Event::Closed => close(shell, *key),
            _ => {}
        }
    }
}

// wl_surface.frame throttles each surface to the compositor's repaint cycle
impl Dispatch<wl_callback::WlCallback, u32> for Shell {
    fn event(
        shell: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && let Some(s) = find(shell, *key)
            && s.phase == Phase::WaitFrame
        {
            trace!("surface {key} frame done");
            s.phase = Phase::NeedsPaint;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, u32> for Shell {
    fn event(
        shell: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(s) = find(shell, *key)
            && let Some(shm) = s.shm.iter_mut().find(|b| b.buffer.as_ref() == Some(buffer))
        {
            trace!("surface {key} buffer released");
            shm.busy = false;
        }
    }
}

impl Dispatch<wl_surface::WlSurface, u32> for Shell {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => trace!("surface {key} entered an output"),
            wl_surface::Event::PreferredBufferScale { factor } => {
                // hidpi lands here, ignored for now, we stay at scale 1
                trace!("surface {key} preferred scale {factor}");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor may answer with client side anyway; there is no
        // frame of our own to fall back to, so the window is simply bare.
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            trace!("zxdg_toplevel_decoration.configure {mode:?}");
        }
    }
}

#[cfg(test)]
mod shm_tests {
    use stein_layout::Rect;

    use super::*;

    const W: usize = 16;
    const H: usize = 12;
    /// A buffer as a grid of cells, each holding the frame it was painted in.
    type Grid = [u32; W * H];

    fn slots() -> [Shm; 2] {
        [Shm::default(), Shm::default()]
    }

    /// A small deterministic generator, so a failure repeats.
    fn next(seed: &mut u32) -> usize {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 16) as usize
    }

    /// A random rect on whole cells, as `[x0, y0, x1, y1]`.
    fn cells(seed: &mut u32) -> [usize; 4] {
        let x0 = next(seed) % W;
        let y0 = next(seed) % H;
        let x1 = x0 + 1 + next(seed) % (W - x0);
        let y1 = y0 + 1 + next(seed) % (H - y0);
        [x0, y0, x1, y1]
    }

    /// Whether a damage rect touches cell `i`.
    fn touches(r: &Rect, i: usize) -> bool {
        let (x, y) = ((i % W) as f32, (i / W) as f32);
        x + 1.0 > r.min[0] && x < r.max[0] && y + 1.0 > r.min[1] && y < r.max[1]
    }

    /// Repaint the cells of `to` that `paint` covers.
    fn repaint(from: &Grid, to: &mut Grid, paint: &Damage) {
        for (i, cell) in to.iter_mut().enumerate() {
            if paint.rects().iter().any(|r| touches(r, i)) {
                *cell = from[i];
            }
        }
    }

    /// Change a random rect of the frame and report it as damage.
    fn change(truth: &mut Grid, damage: &mut Damage, seed: &mut u32, frame: u32) {
        let [x0, y0, x1, y1] = cells(seed);
        for y in y0..y1 {
            truth[y * W + x0..y * W + x1].fill(frame);
        }
        damage.add(Rect {
            min: [x0 as f32, y0 as f32],
            max: [x1 as f32, y1 as f32],
        });
    }

    #[test]
    fn the_first_buffer_is_reused_while_it_comes_back() {
        let mut shm = slots();
        assert_eq!(free_slot(&shm), Some(0));
        shm[1].busy = true;
        assert_eq!(free_slot(&shm), Some(0));
    }

    /// A compositor that keeps the buffer it shows must not stop the next
    /// frame, which is the only thing that would make it let go.
    #[test]
    fn a_held_buffer_leaves_the_other_to_paint_into() {
        let mut shm = slots();
        shm[0].busy = true;
        assert_eq!(free_slot(&shm), Some(1));
        shm[1].busy = true;
        assert_eq!(free_slot(&shm), None);
        shm[0].busy = false;
        assert_eq!(free_slot(&shm), Some(0));
    }

    /// One frame through each buffer in turn, the second owes both diffs.
    #[test]
    fn a_buffer_owes_the_frames_it_sat_out() {
        let rect = |x: f32| Rect {
            min: [x, 0.0],
            max: [x + 1.0, 1.0],
        };
        let mut shm = slots();
        let (mut a, mut b) = (Damage::new(), Damage::new());
        a.add(rect(0.0));
        b.add(rect(8.0));
        assert_eq!(owed(&mut shm, 0, &a).rects(), a.rects());
        let both = owed(&mut shm, 1, &b);
        assert_eq!(both.len(), 2, "the frame it missed and its own");
        // The first buffer missed only the second frame.
        assert_eq!(owed(&mut shm, 0, &Damage::new()).rects(), b.rects());
        assert!(owed(&mut shm, 0, &Damage::new()).is_empty());
    }

    /// Frames go out through either buffer in any order, each painting only
    /// what it is told it owes. Whichever is shown must hold the whole
    /// frame, or stale pixels from two frames back show through.
    #[test]
    fn either_buffer_shows_the_whole_frame_after_a_partial_paint() {
        let mut seed = 7;
        let mut shm = slots();
        let mut truth: Grid = [0; W * H];
        let mut held: [Grid; 2] = [[u32::MAX; W * H]; 2];
        let mut painted = [false; 2];
        for frame in 1..=600u32 {
            let mut damage = Damage::new();
            // Some frames are presented with nothing changed, a resize say.
            for _ in 0..next(&mut seed) % 4 {
                change(&mut truth, &mut damage, &mut seed, frame);
            }
            // Runs on one buffer and switches between the two, both happen.
            let b = usize::from(next(&mut seed) % 3 == 0);
            let paint = owed(&mut shm, b, &damage);
            // A new buffer is painted whole.
            match painted[b] {
                true => repaint(&truth, &mut held[b], &paint),
                false => held[b] = truth,
            }
            painted[b] = true;
            assert!(held[b] == truth, "frame {frame} left buffer {b} stale");
        }
    }
}
