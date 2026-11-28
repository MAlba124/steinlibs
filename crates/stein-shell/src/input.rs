//! Seat, keyboard and pointer. Keys go to the surface with keyboard focus,
//! pointer events to the one under the pointer, each into that surface's
//! pending [`Input`] until its next build.

use std::time::{Duration, Instant};

use stein_ui::{Button, ButtonEvent, Input, Mods};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};

use crate::keyboard::{Compose, Keyboard};
use crate::lock::{self, Facts, Lock, Step};
use crate::scene::Cursor;
use crate::shell::{Shell, wake};
use crate::surface::Surf;

/// evdev button codes. Not contiguous, and every wire protocol numbers them
/// differently again, so they are named once here.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// evdev keycode, wl_keyboard reports raw evdev.
const KEY_ESC: u32 = 1;

/// Most keystrokes one slow frame may turn into.
const MAX_CATCHUP: u32 = 8;

fn button_of(code: u32) -> Button {
    match code {
        BTN_LEFT => Button::Left,
        BTN_MIDDLE => Button::Middle,
        BTN_RIGHT => Button::Right,
        other => Button::Other(other as u16),
    }
}

/// A held key and when it next fires.
#[derive(Clone, Copy)]
pub(crate) struct Repeat {
    /// The evdev code, so a release of a different key does not cancel it.
    key: u32,
    sym: u32,
    mods: Mods,
    at: Instant,
    every: Duration,
}

/// The pending input of the surface with keyboard focus. Takes the fields
/// rather than the shell so the keyboard state can be borrowed beside it.
fn focused_in(surfs: &mut [Surf], kbd_key: Option<u32>) -> Option<&mut Input> {
    let key = kbd_key?;
    surfs
        .iter_mut()
        .find(|s| s.key == key)
        .map(|s| &mut s.input)
}

/// The pending input of the surface under the pointer.
fn pointed(shell: &mut Shell) -> Option<&mut Input> {
    let key = shell.ptr_key?;
    shell
        .surfs
        .iter_mut()
        .find(|s| s.key == key)
        .map(|s| &mut s.input)
}

/// Longest step one frame may take. A window that slept for a minute wakes
/// with this, so an animation resumes rather than jumping to its end.
const MAX_DT: f32 = 0.1;

/// Seconds since `last`, which becomes `now`. Wall time, so an animation
/// keeps its speed whether frames come at the display's rate or as fast as a
/// resize drives them. The first frame gets one 60th of a second.
pub(crate) fn frame_dt(last: &mut Option<std::time::Instant>, now: std::time::Instant) -> f32 {
    let dt = last.map_or(1.0 / 60.0, |t| now.saturating_duration_since(t).as_secs_f32());
    *last = Some(now);
    dt.clamp(0.0, MAX_DT)
}

/// One surface's input for its build: what arrived for it, the pointer if
/// it is over this one, and the shared modifier state.
pub(crate) fn take_input(shell: &mut Shell, i: usize) -> Input {
    let key = shell.surfs[i].key;
    let mut input = core::mem::take(&mut shell.surfs[i].input);
    input.mods = shell.mods;
    input.dt = frame_dt(&mut shell.surfs[i].built_at, std::time::Instant::now());
    input.focused = shell.kbd_key == Some(key);
    if shell.ptr_key == Some(key) {
        input.pointer = [shell.pointer_x as f32, shell.pointer_y as f32];
        input.pointer_down = shell.pointer_down;
        input.pointer_in = true;
        input.wheel = [0.0, shell.wheel];
        shell.wheel = 0.0;
    } else {
        input.pointer = [-1.0, -1.0];
    }
    input
}

// ---------------------------------------------------------------------------
// key repeat
// ---------------------------------------------------------------------------

/// Whether the held modifiers make a key a shortcut. Ctrl+C is one, not the
/// letter c, so it produces no text and goes into no compose sequence.
fn is_chord(m: Mods) -> bool {
    m.ctrl || m.alt || m.logo
}

/// Text for a repeat of a held key, which only unmodified keys produce.
fn emit_text(kb: &Keyboard, input: &mut Input, key: u32) {
    if is_chord(kb.mods()) {
        return;
    }
    let mut buf = [0u8; 8];
    if let Some(t) = kb.text(key + 8, &mut buf) {
        input.push_text(t);
    }
}

/// What a key transition came to.
#[derive(Clone, Copy)]
struct Transition {
    /// The event fit in the frame.
    kept: bool,
    /// The key stands for itself, so holding it may repeat it.
    plain: bool,
}

/// One key transition: the event and its text into `input`, and the
/// modifier state the next build is handed into `mods`.
///
/// A press goes through the compose table first. A dead key produces no
/// text, and the key that finishes its sequence is reported as what the
/// sequence made.
fn key_transition(
    kb: &mut Keyboard,
    mods: &mut Mods,
    input: &mut Input,
    key: u32,
    pressed: bool,
) -> Transition {
    let now = kb.mods();
    let mut sym = kb.sym(key + 8);
    let mut buf = [0u8; 8];
    let mut text = None;
    let mut plain = true;
    if pressed && !is_chord(now) {
        let step = kb.feed(key + 8);
        plain = step == Compose::Plain;
        match step {
            Compose::Plain => text = kb.text(key + 8, &mut buf),
            Compose::Done => {
                let (made, t) = kb.take_composed(&mut buf);
                sym = made.unwrap_or(sym);
                text = t;
            }
            Compose::Pending | Compose::Dropped => {}
        }
    }
    let kept = input.push_key(stein_ui::KeyEvent {
        sym,
        pressed,
        repeat: false,
        mods: now,
    });
    if let Some(t) = text {
        input.push_text(t);
    }
    *mods = now;
    Transition { kept, plain }
}

/// A `wl_keyboard.modifiers` event.
///
/// It follows the key event for the modifier itself, so the release of Ctrl
/// reaches [`key_transition`] with Ctrl still down and `mods` keeps it.
/// Refreshing here is what stops the next plain key from being handed a
/// phantom Ctrl, which is how a terminal loses the first letter typed after
/// a chord.
fn modifiers_event(
    kb: &mut Keyboard,
    mods: &mut Mods,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) {
    kb.update(depressed, latched, locked, group);
    *mods = kb.mods();
}

/// Start or stop repeating on a key transition.
///
/// A press replaces whatever was repeating, because typing a second key while
/// holding the first is how everyone types and the new key is the one meant.
/// A release only cancels if it is the key that was held.
fn arm_repeat(shell: &mut Shell, key: u32, pressed: bool) {
    if !pressed {
        if shell.repeat.is_some_and(|r| r.key == key) {
            shell.repeat = None;
        }
        return;
    }
    if shell.repeat_rate == 0 {
        return;
    }
    let Some(kb) = shell.xkb.as_ref() else {
        return;
    };
    if !kb.repeats(key + 8) {
        // The keymap says this one does not repeat, which is how a held Shift
        // avoids becoming a stream of events.
        shell.repeat = None;
        return;
    }
    shell.repeat = Some(Repeat {
        key,
        sym: kb.sym(key + 8),
        mods: kb.mods(),
        at: Instant::now() + Duration::from_millis(u64::from(shell.repeat_delay)),
        every: interval(shell.repeat_rate),
    });
}

/// Milliseconds between repeats for a rate in keys per second.
///
/// Floored at one millisecond. A compositor reporting a rate above a thousand
/// would otherwise give an interval of zero, and a zero interval means every
/// poll returns instantly forever.
fn interval(rate: u32) -> Duration {
    let ms = 1000 / u64::from(rate).max(1);
    Duration::from_millis(ms.max(1))
}

/// How long the loop may sleep before a repeat is due.
pub(crate) fn repeat_timeout(shell: &Shell) -> Option<Duration> {
    let r = shell.repeat?;
    Some(r.at.saturating_duration_since(Instant::now()))
}

/// Emit any repeats that have come due.
///
/// Catches up rather than firing once, so a loop that was busy for longer than
/// the interval produces the keystrokes the user actually held the key for.
/// Bounded, because a stall of several seconds should not deliver a hundred.
pub(crate) fn fire_repeats(shell: &mut Shell) -> bool {
    let Some(mut r) = shell.repeat else {
        return false;
    };
    let now = Instant::now();
    if now < r.at {
        return false;
    }
    let (fired, next) = catch_up(r.at, r.every, now, MAX_CATCHUP);
    for _ in 0..fired {
        let Some(kb) = shell.xkb.as_ref() else {
            break;
        };
        let Some(input) = focused_in(&mut shell.surfs, shell.kbd_key) else {
            break;
        };
        input.push_key(stein_ui::KeyEvent {
            sym: r.sym,
            pressed: true,
            repeat: true,
            mods: r.mods,
        });
        emit_text(kb, input, r.key);
    }
    r.at = next;
    shell.repeat = Some(r);
    fired > 0
}

/// How many repeats are due by `now`, and when the next one is.
///
/// Pure, so the catch-up rule can be tested without a compositor.
///
/// Capped, and whatever is still behind the cap is abandoned rather than
/// carried: a loop stalled for two seconds should not then deliver two
/// seconds of keystrokes, which is what makes a hitch feel like a stuck key.
fn catch_up(at: Instant, every: Duration, now: Instant, max: u32) -> (u32, Instant) {
    if now < at {
        return (0, at);
    }
    let mut next = at;
    let mut fired = 0;
    while next <= now && fired < max {
        next += every;
        fired += 1;
    }
    if next <= now {
        // Whatever is still behind the cap is abandoned. `every` is floored by
        // `interval`, but not by the type, so the max keeps this total.
        next = now + every.max(Duration::from_millis(1));
    }
    (fired, next)
}

// ---------------------------------------------------------------------------
// seat and keyboard
// ---------------------------------------------------------------------------

/// What a capabilities event means for one of the seat's devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Device {
    Keep,
    Get,
    Drop,
}

/// The compositor takes a capability away when the last such device goes,
/// which is every device across a suspend or a VT switch. The object held
/// is dead from then on and says nothing when the capability returns, so
/// it is dropped and a new one is asked for.
const fn device_step(held: bool, capable: bool) -> Device {
    match (held, capable) {
        (false, true) => Device::Get,
        (true, false) => Device::Drop,
        _ => Device::Keep,
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Shell {
    fn event(
        shell: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(caps),
            } => capabilities(shell, seat, caps, qh),
            wl_seat::Event::Name { name } => trace!("wl_seat.name {name}"),
            _ => {}
        }
    }
}

/// Bring the keyboard and pointer in line with what the seat now has.
fn capabilities(
    shell: &mut Shell,
    seat: &wl_seat::WlSeat,
    caps: wl_seat::Capability,
    qh: &QueueHandle<Shell>,
) {
    trace!("wl_seat.capabilities {caps:?}");
    let keyboard = caps.contains(wl_seat::Capability::Keyboard);
    match device_step(shell.keyboard.is_some(), keyboard) {
        Device::Get => shell.keyboard = Some(seat.get_keyboard(qh, ())),
        Device::Drop => drop_keyboard(shell),
        Device::Keep => {}
    }
    let pointer = caps.contains(wl_seat::Capability::Pointer);
    match device_step(shell.pointer.is_some(), pointer) {
        Device::Get => {
            shell.pointer = Some(seat.get_pointer(qh, ()));
            // Made now rather than on the first shape change, so a
            // scene that allocates nothing once running stays so.
            let _ = cursor_device(shell, qh);
            ensure_relative(shell, qh);
        }
        Device::Drop => drop_pointer(shell),
        Device::Keep => {}
    }
}

/// The seat lost its keyboards. Focus goes with the object.
fn drop_keyboard(shell: &mut Shell) {
    if let Some(k) = shell.keyboard.take() {
        k.release();
        trace!("keyboard dropped");
    }
    keyboard_left(shell);
}

/// The seat lost its pointers. Everything made from the pointer goes
/// first, since none of it outlives the object it was made from.
fn drop_pointer(shell: &mut Shell) {
    unlock(shell);
    if let Some(r) = shell.relative.take() {
        r.destroy();
    }
    if let Some(d) = shell.cursor_device.take() {
        d.destroy();
    }
    if let Some(p) = shell.pointer.take() {
        p.release();
        trace!("pointer dropped");
    }
    shell.wheel = 0.0;
    pointer_left(shell);
}

/// Keyboard focus is gone.
///
/// A held modifier is not held any more as far as this window knows, and
/// keeping it would leave the next keystroke with a phantom Ctrl. The same
/// goes for a key held while switching away, which would otherwise repeat
/// forever.
fn keyboard_left(shell: &mut Shell) {
    shell.kbd_key = None;
    shell.mods = Mods::default();
    shell.repeat = None;
    forget_compose(shell);
    wake(shell);
}

/// The pointer is over no surface. Parked outside so nothing stays hovered.
fn pointer_left(shell: &mut Shell) {
    shell.pointer_x = -1.0;
    shell.pointer_y = -1.0;
    shell.pointer_down = false;
    shell.ptr_key = None;
    // The serial only names this surface, and the pointer has gone.
    shell.cursor_serial = 0;
    shell.cursor_shape = None;
    wake(shell);
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Shell {
    fn event(
        shell: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                shell.clip.serial = serial;
                shell.kbd_key = surface.data::<u32>().copied();
                trace!(
                    "wl_keyboard.enter serial {serial} surface {:?}",
                    shell.kbd_key
                );
                wake(shell);
            }
            wl_keyboard::Event::Leave { serial, .. } => {
                trace!("wl_keyboard.leave serial {serial}");
                keyboard_left(shell);
            }
            wl_keyboard::Event::Key {
                serial, key, state, ..
            } => {
                // A copy made from the keyboard needs a serial as fresh as
                // one made with the pointer, or the compositor may refuse it
                // as older than the selection it already has.
                shell.clip.serial = serial;
                key_event(shell, key, state);
            }
            wl_keyboard::Event::Keymap { format, fd, size } => {
                trace!("wl_keyboard.keymap format {format:?} size {size}");
                if matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                    match Keyboard::from_fd(fd, size as usize) {
                        Ok(kb) => shell.xkb = Some(kb),
                        Err(e) => eprintln!("warn: keymap unusable, keys will not work: {e}"),
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(kb) = shell.xkb.as_mut() {
                    modifiers_event(
                        kb,
                        &mut shell.mods,
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group,
                    );
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                trace!("wl_keyboard.repeat_info rate {rate} delay {delay}");
                // A rate of zero means the user turned repeating off.
                shell.repeat_rate = rate.max(0) as u32;
                shell.repeat_delay = delay.max(0) as u32;
                if shell.repeat_rate == 0 {
                    shell.repeat = None;
                }
            }
            _ => {}
        }
    }
}

/// Drop a dead key typed before focus left, so it cannot compose with the
/// first key typed after it comes back.
fn forget_compose(shell: &mut Shell) {
    if let Some(kb) = shell.xkb.as_mut() {
        kb.reset_compose();
    }
}

fn key_event(shell: &mut Shell, key: u32, state: WEnum<wl_keyboard::KeyState>) {
    let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
    if key == KEY_ESC && pressed && shell.scene.escape_quits() {
        shell.quit = true;
    }
    // wl_keyboard reports evdev codes; xkb wants them offset by 8.
    // Getting this wrong shifts the entire keyboard by one row and
    // is the classic first bug here.
    let mut plain = true;
    if let Some(kb) = shell.xkb.as_mut() {
        match focused_in(&mut shell.surfs, shell.kbd_key) {
            Some(input) => {
                let t = key_transition(kb, &mut shell.mods, input, key, pressed);
                plain = t.plain;
                if !t.kept {
                    trace!("key buffer full, dropping key {key}");
                }
            }
            None => shell.mods = kb.mods(),
        }
    }
    if plain {
        arm_repeat(shell, key, pressed);
    } else {
        // A key that went into a sequence is not text to repeat.
        shell.repeat = None;
    }
    wake(shell);
}

// ---------------------------------------------------------------------------
// pointer and cursor
// ---------------------------------------------------------------------------

/// Ask the compositor for a pointer shape.
///
/// Cheap to call every frame: it returns immediately unless the shape actually
/// changed, so the common case costs a comparison and no request.
pub(crate) fn apply_cursor(shell: &mut Shell, qh: &QueueHandle<Shell>, want: Cursor) {
    if shell.cursor_shape == Some(want) || shell.ptr_key.is_none() || shell.cursor_serial == 0 {
        return;
    }
    let serial = shell.cursor_serial;
    let Some(shape) = shape_of(want) else {
        // Hidden is a null cursor surface, which needs no shape device.
        if let Some(p) = shell.pointer.as_ref() {
            p.set_cursor(serial, None, 0, 0);
            shell.cursor_shape = Some(want);
        }
        return;
    };
    let Some(device) = cursor_device(shell, qh) else {
        // No cursor-shape-v1. The alternative is loading a cursor theme and
        // scaling its images, which is a great deal of work to change an
        // arrow, so the pointer keeps whatever it had.
        return;
    };
    device.set_shape(serial, shape);
    shell.cursor_shape = Some(want);
}

/// The shape device, made on first use.
fn cursor_device<'a>(
    shell: &'a mut Shell,
    qh: &QueueHandle<Shell>,
) -> Option<&'a WpCursorShapeDeviceV1> {
    if shell.cursor_device.is_none() {
        let manager = shell.cursor_manager.as_ref()?;
        let pointer = shell.pointer.as_ref()?;
        shell.cursor_device = Some(manager.get_pointer(pointer, qh, ()));
    }
    shell.cursor_device.as_ref()
}

fn shape_of(c: Cursor) -> Option<Shape> {
    Some(match c {
        Cursor::Default => Shape::Default,
        Cursor::Text => Shape::Text,
        Cursor::Pointer => Shape::Pointer,
        Cursor::Progress => Shape::Progress,
        Cursor::ColResize => Shape::ColResize,
        Cursor::RowResize => Shape::RowResize,
        Cursor::Hidden => return None,
    })
}

/// The cursor for surface `key`: hidden while it holds the lock, else
/// what the scene asks for.
pub(crate) fn cursor_for(shell: &Shell, key: u32) -> Cursor {
    if shell.locked.as_ref().is_some_and(|(k, _)| *k == key) {
        Cursor::Hidden
    } else {
        shell.scene.cursor(key)
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeManagerV1,
        _: <WpCursorShapeManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        _: <WpCursorShapeDeviceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Nor does the device.
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Shell {
    fn event(
        shell: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                shell.pointer_x = surface_x;
                shell.pointer_y = surface_y;
                shell.ptr_key = surface.data::<u32>().copied();
                shell.cursor_serial = serial;
                // The compositor hands the pointer over with whatever shape it
                // was last using, a resize arrow after dragging an edge for
                // instance. Saying nothing here is what leaves it there, so
                // no shape counts as set until one is sent for this enter.
                shell.cursor_shape = None;
                if let Some(key) = shell.ptr_key {
                    let want = cursor_for(shell, key);
                    apply_cursor(shell, qh, want);
                }
                trace!(
                    "wl_pointer.enter serial {serial} surface {:?}",
                    shell.ptr_key
                );
                wake(shell);
            }
            wl_pointer::Event::Leave { serial, .. } => {
                trace!("wl_pointer.leave serial {serial}");
                pointer_left(shell);
            }
            // surface-local, already scaled. this is the hit testing input
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                shell.pointer_x = surface_x;
                shell.pointer_y = surface_y;
                // Hover is a visible change, so ask for a frame. The scene still
                // decides whether anything actually differs.
                wake(shell);
            }
            wl_pointer::Event::Button {
                button,
                state,
                serial,
                ..
            } => button_event(shell, button, state, serial),
            wl_pointer::Event::Axis { axis, value, .. } => {
                if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                    // Wayland scrolls positive downward; the scroll offset grows
                    // downward too, so the sign is flipped where it is consumed.
                    shell.wheel -= value as f32;
                    wake(shell);
                }
            }
            _ => {}
        }
    }
}

fn button_event(
    shell: &mut Shell,
    button: u32,
    state: WEnum<wl_pointer::ButtonState>,
    serial: u32,
) {
    shell.clip.serial = serial;
    let pressed = matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed));
    // BTN_LEFT alone drives `pointer_down`, which is the held
    // state a widget asks about. Every button is reported as an
    // event, because a terminal has to pass them to the program.
    if button == BTN_LEFT {
        shell.pointer_down = pressed;
    }
    let e = ButtonEvent {
        button: button_of(button),
        pressed,
        at: [shell.pointer_x as i32, shell.pointer_y as i32],
        mods: shell.mods,
    };
    if pointed(shell).is_some_and(|i| !i.push_button(e)) {
        trace!("dropped a button event, frame full");
    }
    wake(shell);
}

// ---------------------------------------------------------------------------
// pointer lock and relative motion
// ---------------------------------------------------------------------------

/// Make the relative pointer once the seat's pointer and the manager both
/// exist. Called from whichever of the two arrives last.
pub(crate) fn ensure_relative(shell: &mut Shell, qh: &QueueHandle<Shell>) {
    if shell.relative.is_some() {
        return;
    }
    let (Some(manager), Some(pointer)) = (shell.relative_manager.as_ref(), shell.pointer.as_ref())
    else {
        return;
    };
    shell.relative = Some(manager.get_relative_pointer(pointer, qh, ()));
    trace!("relative pointer made");
}

/// Run the lock machine for surface `i` after its build.
pub(crate) fn drive_lock(shell: &mut Shell, i: usize, qh: &QueueHandle<Shell>) {
    let key = shell.surfs[i].key;
    let state = match shell.locked.as_ref() {
        Some((k, _)) if *k == key => Lock::Held,
        // Another surface holds it and settles it in its own build.
        Some(_) => return,
        None => Lock::Free,
    };
    let facts = Facts {
        wanted: shell.scene.pointer_lock(key),
        focused: shell.kbd_key == Some(key),
        pointer_in: shell.ptr_key == Some(key),
        have_protocol: shell.constraints.is_some() && shell.relative.is_some(),
    };
    match lock::step(state, facts) {
        Step::Lock => {
            let (Some(c), Some(p)) = (shell.constraints.as_ref(), shell.pointer.as_ref()) else {
                return;
            };
            let wl = &shell.surfs[i].wl;
            let locked = c.lock_pointer(wl, p, None, Lifetime::Persistent, qh, key);
            shell.locked = Some((key, locked));
            trace!("pointer lock asked for on surface {key}");
        }
        Step::Unlock => unlock(shell),
        Step::Nothing => {}
    }
}

/// Drop the lock, if one is held. The compositor unlocks with it.
pub(crate) fn unlock(shell: &mut Shell) {
    if let Some((key, locked)) = shell.locked.take() {
        locked.destroy();
        shell.lock_active = false;
        trace!("pointer lock dropped on surface {key}");
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZwpPointerConstraintsV1,
        _: <ZwpPointerConstraintsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwpRelativePointerManagerV1, ()> for Shell {
    fn event(
        _: &mut Self,
        _: &ZwpRelativePointerManagerV1,
        _: <ZwpRelativePointerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Nor does this one.
    }
}

impl Dispatch<ZwpLockedPointerV1, u32> for Shell {
    fn event(
        shell: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                trace!("zwp_locked_pointer_v1.locked surface {key}");
                shell.lock_active = true;
            }
            zwp_locked_pointer_v1::Event::Unlocked => {
                // Persistent, so the object stays and locks again on return.
                trace!("zwp_locked_pointer_v1.unlocked surface {key}");
                shell.lock_active = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Shell {
    fn event(
        shell: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event else {
            return;
        };
        let active = shell.lock_active;
        if let Some(input) = pointed(shell) {
            lock::accumulate(&mut input.motion, active, dx, dy);
        }
        if active {
            wake(shell);
        }
    }
}

#[cfg(test)]
mod seat_tests {
    use super::*;

    /// The seat as the shell tracks it, driven through capability changes.
    fn run(held: &mut bool, capable: bool) -> Device {
        let step = device_step(*held, capable);
        match step {
            Device::Get => *held = true,
            Device::Drop => *held = false,
            Device::Keep => {}
        }
        step
    }

    /// Every device goes across a suspend and comes back after it. The
    /// object from before is dead, so the return has to ask for a new one.
    #[test]
    fn a_capability_that_returns_gets_a_new_device() {
        let mut held = false;
        assert_eq!(run(&mut held, true), Device::Get);
        assert_eq!(run(&mut held, false), Device::Drop);
        assert!(!held);
        assert_eq!(run(&mut held, true), Device::Get);
        assert!(held);
    }

    /// A second mouse plugged in repeats the capability, and the device
    /// in use must survive it.
    #[test]
    fn a_repeated_capability_keeps_the_device() {
        let mut held = false;
        assert_eq!(run(&mut held, false), Device::Keep);
        assert_eq!(run(&mut held, true), Device::Get);
        assert_eq!(run(&mut held, true), Device::Keep);
        assert!(held);
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    // evdev codes
    const LEFTCTRL: u32 = 29;
    const A: u32 = 30;
    const B: u32 = 48;

    struct Wire {
        kb: Keyboard,
        mods: Mods,
        input: Input,
    }

    impl Wire {
        fn key(&mut self, key: u32, pressed: bool) {
            key_transition(&mut self.kb, &mut self.mods, &mut self.input, key, pressed);
        }

        fn modifiers(&mut self, depressed: u32) {
            modifiers_event(&mut self.kb, &mut self.mods, depressed, 0, 0, 0);
        }
    }

    fn wire() -> Option<Wire> {
        Some(Wire {
            kb: Keyboard::us()?,
            mods: Mods::default(),
            input: Input::default(),
        })
    }

    // More evdev codes, the ones a Norwegian keyboard types a tilde with.
    const DIAERESIS: u32 = 27;
    const N: u32 = 49;
    const SPACE: u32 = 57;

    /// A Norwegian layout with AltGr held, or `None` without the xkb data
    /// or a compose table.
    fn norwegian() -> Option<(Wire, u32)> {
        let w = Wire {
            kb: Keyboard::named("no")?,
            mods: Mods::default(),
            input: Input::default(),
        };
        let altgr = w.kb.mask("Mod5");
        w.kb.composes().then_some((w, altgr))
    }

    /// AltGr and the diaeresis key is `dead_tilde`, the only tilde the
    /// layout has. Space after it is the tilde alone.
    #[test]
    fn a_dead_tilde_and_space_type_a_tilde() {
        let Some((mut w, altgr)) = norwegian() else {
            eprintln!("no xkb data on this machine, skipping");
            return;
        };
        w.modifiers(altgr);
        w.key(DIAERESIS, true);
        w.key(DIAERESIS, false);
        w.modifiers(0);
        assert_eq!(w.input.text(), "", "the dead key is not text by itself");

        w.key(SPACE, true);
        assert_eq!(w.input.text(), "~");
        let last = w.input.keys().last().copied();
        assert_eq!(last.map(|k| k.sym), Some(u32::from('~')));
    }

    #[test]
    fn a_dead_tilde_and_a_letter_compose() {
        let Some((mut w, altgr)) = norwegian() else {
            return;
        };
        w.modifiers(altgr);
        w.key(DIAERESIS, true);
        w.key(DIAERESIS, false);
        w.modifiers(0);
        w.key(N, true);
        assert_eq!(w.input.text(), "\u{f1}");
        // The sequence is over, so the next key stands for itself.
        w.key(N, false);
        w.key(N, true);
        assert_eq!(w.input.text(), "\u{f1}n");
    }

    /// Shift going down between the dead key and the letter is how a capital
    /// is composed, and must not break the sequence.
    #[test]
    fn a_modifier_inside_a_sequence_does_not_break_it() {
        const LEFTSHIFT: u32 = 42;
        let Some((mut w, altgr)) = norwegian() else {
            return;
        };
        let shift = w.kb.mask("Shift");
        w.modifiers(altgr);
        w.key(DIAERESIS, true);
        w.key(DIAERESIS, false);
        w.modifiers(0);
        w.key(LEFTSHIFT, true);
        w.modifiers(shift);
        w.key(N, true);
        assert_eq!(w.input.text(), "\u{d1}");
    }

    /// Losing focus mid sequence must not leave the next window's first key
    /// composed with a dead key typed in this one.
    #[test]
    fn a_reset_forgets_the_dead_key() {
        let Some((mut w, altgr)) = norwegian() else {
            return;
        };
        w.modifiers(altgr);
        w.key(DIAERESIS, true);
        w.key(DIAERESIS, false);
        w.modifiers(0);
        w.kb.reset_compose();
        w.key(N, true);
        assert_eq!(w.input.text(), "n");
    }

    /// A key that went into a sequence is not one to repeat.
    #[test]
    fn a_composing_key_is_not_plain() {
        let Some((mut w, altgr)) = norwegian() else {
            return;
        };
        w.modifiers(altgr);
        let dead = key_transition(&mut w.kb, &mut w.mods, &mut w.input, DIAERESIS, true);
        assert!(!dead.plain);
        w.modifiers(0);
        let done = key_transition(&mut w.kb, &mut w.mods, &mut w.input, SPACE, true);
        assert!(!done.plain);
        let after = key_transition(&mut w.kb, &mut w.mods, &mut w.input, N, true);
        assert!(after.plain);
    }

    /// The compositor sends a modifier's own key event first and the
    /// modifiers event after it, both on press and on release. After the
    /// chord, the state handed to the next build must be clean and the next
    /// letter must arrive as text.
    #[test]
    fn a_letter_typed_after_a_ctrl_chord_keeps_its_text() {
        let Some(mut w) = wire() else {
            eprintln!("no xkb data on this machine, skipping");
            return;
        };
        let ctrl = w.kb.mask("Control");

        w.key(LEFTCTRL, true);
        w.modifiers(ctrl);
        w.key(B, true);
        assert!(w.input.keys().last().is_some_and(|k| k.mods.ctrl));
        assert_eq!(w.input.text(), "", "Ctrl+B is a chord, not text");
        w.key(B, false);
        w.key(LEFTCTRL, false);
        assert!(w.mods.ctrl, "the release key event still sees Ctrl held");
        w.modifiers(0);
        assert_eq!(w.mods, Mods::default(), "the modifiers event clears it");

        w.key(A, true);
        assert_eq!(w.input.text(), "a");
        assert_eq!(w.mods, Mods::default());
        assert!(w.input.keys().last().is_some_and(|k| !k.mods.ctrl));
    }

    /// Ctrl going down and up in one build, then a letter in the same build:
    /// the letter is text and the modifiers handed over are clean.
    #[test]
    fn a_chord_and_a_letter_in_one_build() {
        let Some(mut w) = wire() else {
            return;
        };
        let ctrl = w.kb.mask("Control");
        w.key(LEFTCTRL, true);
        w.modifiers(ctrl);
        w.key(LEFTCTRL, false);
        w.modifiers(0);
        w.key(A, true);
        assert_eq!(w.input.text(), "a");
        assert_eq!(w.mods, Mods::default());
    }
}

#[cfg(test)]
mod repeat_tests {
    use super::*;

    const EVERY: Duration = Duration::from_millis(40);

    #[test]
    fn nothing_fires_before_the_delay_is_up() {
        let now = Instant::now();
        let (n, next) = catch_up(now + Duration::from_millis(100), EVERY, now, 8);
        assert_eq!(n, 0);
        assert_eq!(next, now + Duration::from_millis(100));
    }

    #[test]
    fn one_fires_when_the_deadline_has_just_passed() {
        let now = Instant::now();
        let (n, next) = catch_up(now, EVERY, now, 8);
        assert_eq!(n, 1);
        assert_eq!(next, now + EVERY);
    }

    /// A frame that took longer than the interval owes the keystrokes the user
    /// actually held the key for, not one.
    #[test]
    fn a_slow_frame_catches_up() {
        let start = Instant::now();
        let now = start + Duration::from_millis(130);
        let (n, _) = catch_up(start, EVERY, now, 8);
        assert_eq!(n, 4, "40ms apart across 130ms is four");
    }

    /// But a long stall is abandoned rather than carried, or a hitch delivers
    /// seconds of keystrokes and feels like a stuck key.
    #[test]
    fn a_long_stall_is_capped_and_not_carried() {
        let start = Instant::now();
        let now = start + Duration::from_secs(2);
        let (n, next) = catch_up(start, EVERY, now, 8);
        assert_eq!(n, 8, "the cap did not hold");
        assert!(next > now, "it stayed behind and would fire again at once");
        assert_eq!(next, now + EVERY);
    }

    /// Firing repeatedly must stay on the original cadence rather than drift
    /// by however long each turn of the loop took.
    #[test]
    fn repeated_firing_does_not_drift() {
        let start = Instant::now();
        let mut at = start;
        let mut total = 0;
        for turn in 1..=10 {
            // The loop wakes a little late every time, as a real one does.
            let now = start + EVERY * turn + Duration::from_millis(3);
            let (n, next) = catch_up(at, EVERY, now, 8);
            total += n;
            at = next;
        }
        // Eleven, not ten: the deadlines are at zero through ten intervals,
        // and the first one is the moment the delay expired.
        assert_eq!(total, 11, "fired {total} times, so it drifted");
    }

    /// A zero interval must never leave the next deadline in the past, or the
    /// poll returns instantly forever and the loop spins at full tilt.
    #[test]
    fn a_zero_interval_still_moves_forward() {
        let now = Instant::now();
        let (n, next) = catch_up(now, Duration::ZERO, now, 4);
        assert_eq!(n, 4);
        assert!(next > now, "the deadline stayed in the past");
    }

    /// Which is why the rate is turned into an interval that cannot be zero.
    #[test]
    fn an_absurd_repeat_rate_still_gives_a_usable_interval() {
        assert!(interval(0) >= Duration::from_millis(1));
        assert!(interval(5000) >= Duration::from_millis(1));
        assert_eq!(interval(25), Duration::from_millis(40));
        assert_eq!(interval(1000), Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_DT, frame_dt};
    use std::time::{Duration, Instant};

    #[test]
    fn dt_is_wall_time_between_builds_however_fast_they_come() {
        let start = Instant::now();
        let mut last = None;
        assert_eq!(frame_dt(&mut last, start), 1.0 / 60.0, "the first frame");
        // A resize builds far faster than the display refreshes.
        let quick = frame_dt(&mut last, start + Duration::from_millis(2));
        assert!((quick - 0.002).abs() < 1e-4, "{quick}");
        let slept = frame_dt(&mut last, start + Duration::from_secs(60));
        assert_eq!(slept, MAX_DT, "a long sleep is one capped step");
        let back = frame_dt(&mut last, start + Duration::from_secs(59));
        assert_eq!(back, 0.0, "a clock that steps back is no time");
    }
}
