//! Wayland: globals, outputs and the event loop. Surfaces live in
//! crate::surface, input in crate::input, the selection in crate::selection.

use std::fmt;
use std::os::fd::BorrowedFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec};
use stein_ui::Mods;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::shell::client::xdg_wm_base;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};

use crate::blend::Blend;
use crate::clipboard::Clipboard;
use crate::gpu::{self, GpuPaint};
use crate::input::{self, Repeat};
use crate::keyboard::Keyboard;
use crate::layer::{self, LayerError};
use crate::multi::{MultiScene, Output as OutputInfo, Role, Single, Want};
use crate::scene::{Cursor, Scene};
use crate::selection;
use crate::surface::{self, Phase, Surf};
use crate::video;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ShellError {
    Connect(wayland_client::ConnectError),
    Dispatch(wayland_client::DispatchError),
    Memfd(rustix::io::Errno),
    Truncate(rustix::io::Errno),
    Mmap(std::io::Error),
    MissingGlobal(&'static str),
    GlobalTooOld {
        name: &'static str,
        have: u32,
        need: u32,
    },
    SizeOverflow {
        width: u32,
        height: u32,
    },
    /// The display socket itself failed, as opposed to a message on it.
    Protocol(WaylandError),
    /// Waiting on the display and the scene's descriptor failed.
    Poll(rustix::io::Errno),
    /// The scene's layer spec would be a protocol error.
    Layer(LayerError),
    /// No output by the name the scene asked for.
    UnknownOutput(String),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "wayland connect failed: {e}"),
            Self::Dispatch(e) => write!(f, "wayland dispatch failed: {e}"),
            Self::Memfd(e) => write!(f, "memfd_create failed: {e}"),
            Self::Truncate(e) => write!(f, "ftruncate failed: {e}"),
            Self::Mmap(e) => write!(f, "mmap failed: {e}"),
            Self::MissingGlobal(n) => write!(f, "compositor did not advertise {n}"),
            Self::GlobalTooOld { name, have, need } => {
                write!(f, "{name} version {have} too old, need {need}")
            }
            Self::SizeOverflow { width, height } => {
                write!(f, "window size {width}x{height} overflows buffer math")
            }
            Self::Protocol(e) => write!(f, "wayland connection failed: {e}"),
            Self::Poll(e) => write!(f, "waiting for events failed: {e}"),
            Self::Layer(e) => write!(f, "{e}"),
            Self::UnknownOutput(n) => write!(f, "no output named {n:?}"),
        }
    }
}

impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(e) => Some(e),
            Self::Dispatch(e) => Some(e),
            Self::Memfd(e) | Self::Truncate(e) => Some(e),
            Self::Mmap(e) => Some(e),
            Self::Protocol(e) => Some(e),
            Self::Poll(e) => Some(e),
            Self::Layer(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

/// A bound wl_output and the registry name it came from.
pub(crate) struct Output {
    pub(crate) global: u32,
    pub(crate) proxy: wl_output::WlOutput,
    /// Empty until the name event arrives.
    pub(crate) name: String,
}

pub(crate) struct Shell {
    pub(crate) quit: bool,

    // globals
    pub(crate) compositor: Option<wl_compositor::WlCompositor>,
    pub(crate) shm: Option<wl_shm::WlShm>,
    pub(crate) wm_base: Option<xdg_wm_base::XdgWmBase>,
    pub(crate) seat: Option<wl_seat::WlSeat>,
    pub(crate) layer_shell: Option<ZwlrLayerShellV1>,
    /// Asks the compositor to draw the frame. Without the request a
    /// tiling compositor assumes the client decorates and draws no border.
    pub(crate) decoration_manager: Option<ZxdgDecorationManagerV1>,
    /// Sets the pointer shape without this having to load a cursor theme,
    /// decode its images and pick a size for the output's scale.
    pub(crate) cursor_manager: Option<WpCursorShapeManagerV1>,
    /// Pointer lock and relative motion, for mouse look. Both optional.
    pub(crate) constraints: Option<ZwpPointerConstraintsV1>,
    pub(crate) relative_manager: Option<ZwpRelativePointerManagerV1>,
    /// What a video layer is made of, a subsurface, a viewport to scale
    /// it and dmabuf import. Each optional, a scene without them gets no
    /// layer.
    pub(crate) subcompositor: Option<WlSubcompositor>,
    pub(crate) viewporter: Option<WpViewporter>,
    pub(crate) dmabuf: Option<ZwpLinuxDmabufV1>,

    pub(crate) outputs: Vec<Output>,
    /// An output came, went or was named since the scene last heard.
    outputs_dirty: bool,

    // surfaces, and scratch for reconciling them with the scene's wants
    pub(crate) surfs: Vec<Surf>,
    pub(crate) wants: Vec<Want>,
    pub(crate) have: Vec<(u32, Role)>,
    pub(crate) plan_drop: Vec<usize>,
    pub(crate) plan_make: Vec<usize>,

    // input
    pub(crate) keyboard: Option<wl_keyboard::WlKeyboard>,
    pub(crate) pointer: Option<wl_pointer::WlPointer>,
    pub(crate) cursor_device: Option<WpCursorShapeDeviceV1>,
    /// Made once both the pointer and the manager exist.
    pub(crate) relative: Option<ZwpRelativePointerV1>,
    /// The lock and the surface key it was asked for on.
    pub(crate) locked: Option<(u32, ZwpLockedPointerV1)>,
    /// Between the lock's locked and unlocked events. Motion only counts
    /// while this holds.
    pub(crate) lock_active: bool,
    /// The serial of the last pointer enter.
    ///
    /// Setting a shape needs one, and it has to be an *enter* serial: the
    /// compositor uses it to check the pointer is actually over this surface,
    /// so a motion or button serial is refused.
    pub(crate) cursor_serial: u32,
    /// What was last asked for, so an unchanged shape costs no requests.
    pub(crate) cursor_shape: Cursor,
    /// The surface under the pointer and the one with keyboard focus.
    pub(crate) ptr_key: Option<u32>,
    pub(crate) kbd_key: Option<u32>,
    pub(crate) pointer_x: f64,
    pub(crate) pointer_y: f64,
    pub(crate) pointer_down: bool,
    pub(crate) wheel: f32,
    pub(crate) mods: Mods,
    /// Keyboard state, once the compositor has sent a keymap. `None` until
    /// then, which is normal for the first events after focus.
    pub(crate) xkb: Option<Keyboard>,
    /// The key being held, if it is one that repeats.
    pub(crate) repeat: Option<Repeat>,
    /// What the compositor asked for. Wayland leaves repeating to the client
    /// and only says how fast, so without this nothing repeats at all.
    pub(crate) repeat_rate: u32,
    pub(crate) repeat_delay: u32,
    pub(crate) clip: Clipboard,
    /// The surface a drag is over.
    pub(crate) drag_key: Option<u32>,

    // The scene drives what gets painted. Boxed because the Wayland event
    // queue owns its dispatch state, so the scene is handed over, not borrowed.
    pub(crate) scene: Box<dyn MultiScene>,
    /// The blend kernel, with the CPU level probed once here.
    pub(crate) blend: Blend,
    /// The GPU painter, on unless STEIN_GPU=0 or no device opens.
    /// Dropped for good the first time it fails, software takes over.
    pub(crate) gpu: Option<GpuPaint>,
    pub(crate) frame_count: u32,
}

impl Shell {
    fn new(scene: Box<dyn MultiScene>) -> Self {
        Self {
            quit: false,
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            layer_shell: None,
            decoration_manager: None,
            cursor_manager: None,
            constraints: None,
            relative_manager: None,
            subcompositor: None,
            viewporter: None,
            dmabuf: None,
            outputs: Vec::new(),
            outputs_dirty: true,
            surfs: Vec::new(),
            wants: Vec::new(),
            have: Vec::new(),
            plan_drop: Vec::new(),
            plan_make: Vec::new(),
            keyboard: None,
            pointer: None,
            cursor_device: None,
            relative: None,
            locked: None,
            lock_active: false,
            cursor_serial: 0,
            cursor_shape: Cursor::Default,
            ptr_key: None,
            kbd_key: None,
            pointer_x: -1.0,
            pointer_y: -1.0,
            pointer_down: false,
            wheel: 0.0,
            mods: Mods::default(),
            xkb: None,
            repeat: None,
            // What most compositors default to, used until one says otherwise.
            repeat_rate: 25,
            repeat_delay: 600,
            clip: Clipboard::default(),
            drag_key: None,
            scene,
            blend: Blend::detect(),
            gpu: None,
            frame_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

/// Connect, bind globals, create the window, run the scene until it quits.
pub fn run(scene: Box<dyn Scene>) -> Result<(), ShellError> {
    if let Some(spec) = scene.layer() {
        layer::check(&spec).map_err(ShellError::Layer)?;
    }
    let single = Single::new(scene);
    let named = single.output_name().map(str::to_string);
    run_with(Box::new(single), named)
}

/// Run a scene with any number of surfaces until it quits.
pub fn run_multi(scene: Box<dyn MultiScene>) -> Result<(), ShellError> {
    run_with(scene, None)
}

fn run_with(scene: Box<dyn MultiScene>, named: Option<String>) -> Result<(), ShellError> {
    let conn = Connection::connect_to_env().map_err(ShellError::Connect)?;
    let mut queue = conn.new_event_queue::<Shell>();
    let qh = queue.handle();
    let mut shell = Shell::new(scene);
    let _registry = conn.display().get_registry(&qh, ());
    // The first roundtrip collects the globals, the second what the bound
    // ones say about themselves, output names among it.
    queue.roundtrip(&mut shell).map_err(ShellError::Dispatch)?;
    queue.roundtrip(&mut shell).map_err(ShellError::Dispatch)?;
    check_globals(&shell)?;
    if let Some(name) = named
        && !shell.outputs.iter().any(|o| o.name == name)
    {
        return Err(ShellError::UnknownOutput(name));
    }
    selection::bind_data_device(&mut shell, &qh);

    notify_outputs(&mut shell);
    surface::reconcile(&mut shell, &qh)?;
    // The GPU opens while the compositor works out the first configures. It
    // asks the compositor which device it renders on and loads only that
    // driver.
    let _ = conn.flush();
    // An unpaced scene stays on shm. A swapchain paces itself on frame
    // callbacks, which a surface under a fullscreen window never gets, so
    // its second present would block the loop and every surface with it.
    if shell.scene.paced() {
        shell.gpu = gpu::open_from_env(&conn);
    }

    while !shell.quit {
        turn(&mut shell, &qh, &conn)?;
        if shell.quit {
            break;
        }
        pump(&mut queue, &mut shell)?;
    }
    teardown(&mut shell);
    let _ = conn.flush();
    trace!("clean exit after {} frames", shell.frame_count);
    Ok(())
}

fn check_globals(shell: &Shell) -> Result<(), ShellError> {
    let compositor = shell
        .compositor
        .as_ref()
        .ok_or(ShellError::MissingGlobal("wl_compositor"))?;
    shell
        .shm
        .as_ref()
        .ok_or(ShellError::MissingGlobal("wl_shm"))?;
    if shell.seat.is_none() {
        eprintln!("warn: no wl_seat, input will be dead");
    }
    // damage_buffer is wl_surface v4
    if compositor.version() < 4 {
        return Err(ShellError::GlobalTooOld {
            name: "wl_compositor",
            have: compositor.version(),
            need: 4,
        });
    }
    Ok(())
}

/// One turn: tell the scene about outputs, make its surfaces, paint what is
/// due, and make any surface a build asked for, a popup opened by a click.
fn turn(shell: &mut Shell, qh: &QueueHandle<Shell>, conn: &Connection) -> Result<(), ShellError> {
    if shell.outputs_dirty {
        notify_outputs(shell);
    }
    surface::reconcile(shell, qh)?;
    let mut i = 0;
    while i < shell.surfs.len() {
        if shell.surfs[i].phase == Phase::NeedsPaint {
            surface::render(shell, i, qh, conn)?;
        }
        i += 1;
    }
    // A scene can quit itself. Checked once per turn.
    if !shell.scene.running() {
        shell.quit = true;
        return Ok(());
    }
    surface::reconcile(shell, qh)
}

/// Hand the scene the named outputs.
fn notify_outputs(shell: &mut Shell) {
    shell.outputs_dirty = false;
    let list: Vec<OutputInfo<'_>> = shell
        .outputs
        .iter()
        .filter(|o| !o.name.is_empty())
        .map(|o| OutputInfo {
            id: o.global,
            name: &o.name,
        })
        .collect();
    shell.scene.outputs(&list);
}

/// Explicit teardown: swapchains, surfaces children first, then input.
fn teardown(shell: &mut Shell) {
    input::unlock(shell);
    if let Some(r) = shell.relative.take() {
        r.destroy();
    }
    while !shell.surfs.is_empty() {
        surface::destroy(shell, shell.surfs.len() - 1);
    }
    shell.gpu = None;
    if let Some(k) = shell.keyboard.take() {
        k.release();
    }
    if let Some(p) = shell.pointer.take() {
        p.release();
    }
    for o in shell.outputs.drain(..) {
        o.proxy.release();
    }
}

// ---------------------------------------------------------------------------
// the wait
// ---------------------------------------------------------------------------

/// One turn of the event loop.
///
/// `EventQueue::blocking_dispatch` waits on the display socket alone, which is
/// only right for a scene whose every change comes from the compositor. A
/// terminal's output arrives on a pty, and would sit unread until the user
/// happened to move the mouse. So the wait is a poll over both descriptors.
fn pump(queue: &mut EventQueue<Shell>, shell: &mut Shell) -> Result<(), ShellError> {
    let qh = queue.handle();
    let qh = &qh;
    if queue
        .dispatch_pending(shell)
        .map_err(ShellError::Dispatch)?
        > 0
    {
        return Ok(());
    }
    queue.flush().map_err(ShellError::Protocol)?;

    // prepare_read has to happen before the poll, otherwise another reader
    // could take the events between the two and this would block on a socket
    // that has already been drained.
    let Some(guard) = queue.prepare_read() else {
        queue
            .dispatch_pending(shell)
            .map_err(ShellError::Dispatch)?;
        return Ok(());
    };
    // Watched in every phase, not only when idle. The scene consumes it in
    // `drain` rather than only in `build`, so a frame being in flight does not
    // stop bytes being read, and consuming it is what keeps this from spinning.
    let extra = shell.scene.wakeup();
    let deadline = match (input::repeat_timeout(shell), surface::settle_timeout(shell)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let (display, scene) = wait(guard.connection_fd(), extra, deadline)?;
    if display {
        // WouldBlock here is normal: poll said readable, another thread got
        // there first.
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(ShellError::Protocol(e)),
        }
    } else {
        drop(guard);
    }
    if input::fire_repeats(shell) {
        trace!("woken by a key repeat");
        wake(shell);
    }
    surface::settle(shell);
    if scene {
        if shell.scene.drain() {
            trace!("woken by the scene");
            wake(shell);
        }
        video::sync_all(shell, qh);
    }
    queue
        .dispatch_pending(shell)
        .map_err(ShellError::Dispatch)?;
    Ok(())
}

/// Something changed, so every surface owes a frame.
///
/// Only an idle surface paints now. One waiting on a frame callback paints
/// when that arrives, and one waiting on its first configure has nowhere to
/// paint to yet. All of them, since shared state changed through one of them
/// shows on the others; a surface that did not change builds, finds no
/// difference and goes back to sleep.
pub(crate) fn wake(shell: &mut Shell) {
    for s in &mut shell.surfs {
        if s.phase == Phase::Idle {
            s.phase = Phase::NeedsPaint;
        }
    }
}

/// Block until either descriptor has something. Returns which.
///
/// A hangup or an error counts as ready, so the read that follows reports the
/// real reason rather than this looping on a dead descriptor.
fn wait(
    display: BorrowedFd<'_>,
    extra: Option<BorrowedFd<'_>>,
    deadline: Option<Duration>,
) -> Result<(bool, bool), ShellError> {
    let want = PollFlags::IN | PollFlags::ERR | PollFlags::HUP;
    // The second slot is the display again when there is no scene descriptor,
    // so the array is always fully initialised and only `n` decides what is
    // actually waited on.
    let second = extra.unwrap_or(display);
    let mut fds = [
        PollFd::from_borrowed_fd(display, PollFlags::IN),
        PollFd::from_borrowed_fd(second, PollFlags::IN),
    ];
    let n = if extra.is_some() { 2 } else { 1 };
    // A pending key repeat is a deadline, not an event, so the wait has to end
    // on its own rather than only when something arrives.
    let ts = deadline.map(|d| Timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
        tv_nsec: d.subsec_nanos() as i64,
    });
    loop {
        match rustix::event::poll(&mut fds[..n], ts.as_ref()) {
            Ok(_) => {
                let ready = |i: usize| i < n && fds[i].revents().intersects(want);
                return Ok((ready(0), ready(1)));
            }
            // A signal arriving mid-wait is not something the loop can act on.
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(ShellError::Poll(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// globals
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for Shell {
    fn event(
        shell: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                trace!("global: {interface} v{version} (name {name})");
                bind(shell, registry, qh, name, &interface, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                trace!("global removed: name {name}");
                if let Some(i) = shell.outputs.iter().position(|o| o.global == name) {
                    let o = shell.outputs.swap_remove(i);
                    o.proxy.release();
                    shell.outputs_dirty = true;
                }
            }
            _ => {}
        }
    }
}

fn bind(
    shell: &mut Shell,
    registry: &wl_registry::WlRegistry,
    qh: &QueueHandle<Shell>,
    name: u32,
    interface: &str,
    version: u32,
) {
    match interface {
        "wl_compositor" => shell.compositor = Some(registry.bind(name, version.min(6), qh, ())),
        "wl_shm" => shell.shm = Some(registry.bind(name, version.min(1), qh, ())),
        "xdg_wm_base" => shell.wm_base = Some(registry.bind(name, version.min(6), qh, ())),
        "wl_seat" => shell.seat = Some(registry.bind(name, version.min(8), qh, ())),
        "zxdg_decoration_manager_v1" => {
            shell.decoration_manager = Some(registry.bind(name, version.min(1), qh, ()));
        }
        "wp_cursor_shape_manager_v1" => {
            shell.cursor_manager = Some(registry.bind(name, version.min(1), qh, ()));
        }
        "zwp_pointer_constraints_v1" => {
            shell.constraints = Some(registry.bind(name, version.min(1), qh, ()));
        }
        "zwp_relative_pointer_manager_v1" => {
            shell.relative_manager = Some(registry.bind(name, version.min(1), qh, ()));
            input::ensure_relative(shell, qh);
        }
        "wl_subcompositor" => shell.subcompositor = Some(registry.bind(name, 1, qh, ())),
        "wp_viewporter" => shell.viewporter = Some(registry.bind(name, 1, qh, ())),
        // create_immed is version 2, and from 4 on nothing comes unasked.
        "zwp_linux_dmabuf_v1" if version >= 2 => {
            shell.dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
        }
        "wl_data_device_manager" => {
            shell.clip.manager = Some(registry.bind(name, version.min(3), qh, ()));
        }
        // on_demand keyboard focus is v4, nothing past it is used
        "zwlr_layer_shell_v1" => {
            shell.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
        }
        // v4 is the first to say its name
        "wl_output" if version >= 4 => {
            let proxy = registry.bind(name, 4, qh, ());
            shell.outputs.push(Output {
                global: name,
                proxy,
                name: String::new(),
            });
        }
        _ => {}
    }
}

// xdg_wm_base.ping must be answered or the compositor decides we hung and
// kills the window
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Shell {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Shell {
    fn event(
        shell: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Name { name } => {
                if let Some(o) = shell.outputs.iter_mut().find(|o| &o.proxy == output) {
                    trace!("wl_output.name {name}");
                    o.name = name;
                }
            }
            // The end of a burst of output state, one note to the scene.
            wl_output::Event::Done => shell.outputs_dirty = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // only logged, we hardcode Xrgb8888 which is mandatory
        if let wl_shm::Event::Format {
            format: WEnum::Value(f),
        } = event
        {
            trace!("wl_shm.format {f:?}");
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The shell has no events.
    }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZxdgDecorationManagerV1,
        _: <ZxdgDecorationManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

delegate_noop!(Shell: ignore wl_compositor::WlCompositor);
delegate_noop!(Shell: ignore wl_shm_pool::WlShmPool);
