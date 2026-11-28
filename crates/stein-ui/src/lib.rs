//! An immediate-mode API over retained tables.
//!
//! You write code that reads like immediate mode. It executes like a retained
//! toolkit: boxes are emitted into a table each frame, solved by a real multi-pass
//! layout, cached by stable key so persistent state survives, diffed so an idle
//! window costs nothing.
//!
//! Separating those two is the whole design. The jank in most immediate-mode GUIs
//! comes from conflating them: single-pass layout that cannot size a container to
//! its children, identity that shifts so state teleports, and a full repaint every
//! frame whether or not anything moved.
//!
//! # The frame
//!
//! ```text
//! ui.begin_frame(input, viewport);
//! ui.open(key, spec, fill)?;   // emit boxes, get interaction back
//! ui.close()?;
//! ui.end_frame()?;             // solve layout, build quads for styled boxes
//! ui.push_quad(..);            // whatever the application draws itself
//! let frame = ui.finish();     // diff the whole draw list, decide to present
//! if frame.needs_present { present(ui.quads()) }
//! ```
//!
//! # The one honest lag
//!
//! Hit testing reads the previous frame's rects, because a widget's geometry is not
//! known until after the code that emitted it has run. Removing that would mean
//! running application code twice per frame, which breaks the moment it has side
//! effects. In practice it is invisible: stable keys mean a click lands on the
//! right widget even when it moved, and a box that did not exist last frame
//! correctly cannot be clicked on its first.
//!
//! # The loop contract
//!
//! A driver that presents a frame **must schedule another build**. Widgets read
//! last frame's solve, so after a resize the frame that was emitted against the
//! old geometry is by definition damaged, and the build that follows is the one
//! that reads the corrected rects and settles.
//!
//! Damage implies present, present implies one more build. A loop that presents
//! without scheduling a follow-up breaks that chain silently: the window sleeps
//! holding a frame laid out against geometry that no longer exists, and nothing
//! reports an error. The Wayland driver satisfies this by requesting a
//! `wl_surface.frame` callback on every commit; a headless driver satisfies it
//! by looping while `needs_present`.
//!
//! # Memory
//!
//! `no_std`, no `alloc`. Every table is a caller-supplied slice, so the ceiling is
//! the application's decision and nothing here can allocate mid-frame.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod draw;
pub mod key;

use draw::{CustomRegion, Damage, DrawList, Quad, Shape};
use stein_core::keyed::{KeyedError, KeyedStore};
use stein_layout::{BoxSpec, BoxTable, LayoutError, LayoutStorage, Rect};

/// The fraction of the way to its target an exponential ease covers in
/// `dt` seconds at `rate` per second, `1 - exp(-rate * dt)`.
///
/// Exact rather than a per frame approximation, so one frame of 1/60 covers
/// what two of 1/120 do and an ease runs at the same speed on any display.
/// `core` has no `exp`: this is its series to the fourth power on a
/// sixteenth of the argument, squared four times, within 1e-5 of the real
/// thing for any step a frame takes.
pub fn ease_step(rate: f32, dt: f32) -> f32 {
    let x = fmax(rate, 0.0) * fmax(dt, 0.0);
    if x >= 32.0 {
        return 1.0;
    }
    let y = x / 16.0;
    // Even degree, so positive for every y.
    let mut decay = 1.0 - y + y * y / 2.0 - y * y * y / 6.0 + y * y * y * y / 24.0;
    for _ in 0..4 {
        decay *= decay;
    }
    1.0 - decay
}

/// What one mouse wheel notch adds to [`Input::wheel`]. libinput reports a
/// detent as 15 surface pixels and sway passes that on.
pub const WHEEL_NOTCH: f32 = 15.0;

/// Everything the frame needs to know about the outside world.
///
/// The pointer delta is computed here rather than supplied, so a platform layer
/// only has to report where the pointer is. Nothing else can compute it
/// correctly anyway: it has to be the movement since the frame that was drawn,
/// not since the last motion event, or dragging stutters whenever several
/// motion events land between frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub pointer: [f32; 2],
    pub pointer_down: bool,
    /// Scroll since the last frame in the compositor's surface pixels,
    /// positive toward the start. A mouse wheel notch is [`WHEEL_NOTCH`] of
    /// them, a touchpad sends any amount.
    pub wheel: [f32; 2],
    /// Seconds since the previous frame, measured. Drives animation.
    pub dt: f32,
    /// Key events since the last frame.
    ///
    /// A fixed array rather than a slice, so `Input` stays `Copy` and carries
    /// no lifetime. Eight events per frame is 480 a second, well past what a
    /// keyboard produces; the platform layer drops the excess and says so.
    pub keys: [KeyEvent; MAX_KEYS],
    pub key_count: u8,
    /// Text committed this frame, UTF-8. Separate from `keys` because what a
    /// key means as text depends on layout, modifiers and dead keys, which is
    /// the platform's job to resolve rather than a shortcut table's.
    pub text: [u8; MAX_TEXT],
    pub text_len: u8,
    pub mods: Mods,
    /// Pointer button transitions since the last frame.
    ///
    /// Separate from `pointer_down`, which is only the left button's held
    /// state and is what a widget wants. A terminal reporting the mouse to a
    /// program needs which button and which edge, and it needs the middle and
    /// right buttons that `pointer_down` throws away.
    pub buttons: [ButtonEvent; MAX_BUTTONS],
    pub button_count: u8,
    /// Whether the pointer is over the window at all. A position alone cannot
    /// say: the last one before it left is still inside.
    pub pointer_in: bool,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Relative pointer motion since the last frame, surface pixels, only
    /// while the pointer is locked. Zero otherwise.
    pub motion: [f32; 2],
}

pub const MAX_KEYS: usize = 8;
pub const MAX_TEXT: usize = 32;
pub const MAX_BUTTONS: usize = 8;

/// Which pointer button. Named rather than numbered, because the evdev codes
/// are not contiguous and the wire encodings number them differently again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
    /// Anything else, kept so a caller can ignore it deliberately.
    Other(u16),
}

/// One pointer button transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ButtonEvent {
    pub button: Button,
    pub pressed: bool,
    /// Where the pointer was, in the same space as [`Input::pointer`]. Carried
    /// on the event because the pointer may have moved on by the time the
    /// frame is built.
    pub at: [i32; 2],
    pub mods: Mods,
}

/// Held modifiers. Named for what they do rather than where they sit, except
/// `logo`, which has no agreed name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

/// One key transition.
///
/// `sym` is an xkb keysym, not a scancode: a scancode says which switch moved,
/// a keysym says what it means under the user's layout. A shortcut table keyed
/// on scancodes is wrong on every non-US keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyEvent {
    pub sym: u32,
    pub pressed: bool,
    /// Generated by auto-repeat rather than a physical press. Navigation wants
    /// these, a toggle does not.
    pub repeat: bool,
    pub mods: Mods,
}

impl Input {
    pub fn keys(&self) -> &[KeyEvent] {
        &self.keys[..(self.key_count as usize).min(MAX_KEYS)]
    }

    pub fn buttons(&self) -> &[ButtonEvent] {
        &self.buttons[..(self.button_count as usize).min(MAX_BUTTONS)]
    }

    /// Record a button transition, dropping it when the frame is already full.
    /// Returns whether it was kept.
    pub fn push_button(&mut self, e: ButtonEvent) -> bool {
        let at = self.button_count as usize;
        if at >= MAX_BUTTONS {
            return false;
        }
        self.buttons[at] = e;
        self.button_count += 1;
        true
    }

    /// Text committed this frame, empty when none.
    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(MAX_TEXT);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }

    /// Whether `sym` went down this frame, repeats included.
    pub fn pressed(&self, sym: u32) -> bool {
        self.keys().iter().any(|k| k.pressed && k.sym == sym)
    }

    /// A chord: `sym` pressed with exactly these modifiers. Exact, so ctrl+s
    /// does not fire on ctrl+shift+s.
    pub fn chord(&self, sym: u32, mods: Mods) -> bool {
        self.keys()
            .iter()
            .any(|k| k.pressed && k.sym == sym && k.mods == mods)
    }

    pub fn push_key(&mut self, ev: KeyEvent) -> bool {
        let n = self.key_count as usize;
        if n >= MAX_KEYS {
            return false;
        }
        self.keys[n] = ev;
        self.key_count += 1;
        true
    }

    /// Append committed text, truncating on a char boundary rather than
    /// splitting a code point.
    pub fn push_text(&mut self, s: &str) -> bool {
        let start = self.text_len as usize;
        let room = MAX_TEXT.saturating_sub(start);
        let mut n = s.len().min(room);
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        self.text[start..start + n].copy_from_slice(&s.as_bytes()[..n]);
        self.text_len = (start + n) as u8;
        n == s.len()
    }
}

/// Keysyms the UI itself reacts to. The full set lives in xkbcommon; these are
/// the ones worth naming here.
pub mod keys {
    pub const ESCAPE: u32 = 0xff1b;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const BACKSPACE: u32 = 0xff08;
    pub const DELETE: u32 = 0xffff;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const HOME: u32 = 0xff50;
    pub const END: u32 = 0xff57;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const SPACE: u32 = 0x20;

    /// Keysym of an unmodified letter key.
    ///
    /// X11 numbers the Latin block at its ASCII value, and a control modifier
    /// does not shift the level, so `Ctrl+A` arrives as lowercase `a`. Matching
    /// on `letter('a')` rather than a shifted constant is therefore what makes
    /// a shortcut fire with caps lock on.
    pub const fn letter(c: char) -> u32 {
        (c as u32) | 0x20
    }

    pub const A: u32 = letter('a');
    pub const C: u32 = letter('c');
    pub const F: u32 = letter('f');
    pub const G: u32 = letter('g');
    pub const N: u32 = letter('n');
    pub const P: u32 = letter('p');
    pub const S: u32 = letter('s');
    pub const V: u32 = letter('v');
    pub const W: u32 = letter('w');
    pub const X: u32 = letter('x');
    pub const Z: u32 = letter('z');
}

/// What a widget being dragged needs to know.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drag {
    /// Pointer movement since the previous frame. Zero unless active.
    pub delta: [f32; 2],
    /// Pointer movement since the press started. Survives the pointer leaving
    /// the widget, which is the point of a drag.
    pub total: [f32; 2],
    /// Where the press started, in the same space as `Input::pointer`.
    pub origin: [f32; 2],
}

/// What happened to one widget this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Response {
    pub hovered: bool,
    /// The press started here, this frame.
    pub pressed: bool,
    /// Held since a press that started here. Survives the pointer wandering off,
    /// which is what makes dragging work.
    pub active: bool,
    /// Released over the widget the press started on. This is a click; `pressed`
    /// is not, because a press the user drags away from should do nothing.
    pub clicked: bool,
    /// Movement while this widget is being dragged. Meaningful only when
    /// `active`, and zero otherwise, so a caller can add it unconditionally.
    pub drag: Drag,
}

/// Per-widget state that has to outlive the frame that drew it.
///
/// The geometry fields are written by [`Ui::end_frame`] for every key that
/// already has an entry, which is what makes last frame's solve readable while
/// this frame's widgets are still being emitted. Without it a widget that needs
/// its own extent, to size a scrollbar thumb or clamp a pane, has nothing to
/// work from: the table is reset at `begin_frame` and each emitted box zeroes
/// its rect.
///
/// Only keys that already have state are tracked, so asking for state is how a
/// widget opts in and a large UI does not fill the table with entries nobody
/// reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WidgetState {
    /// Solved rect from the previous frame. Zero until laid out once.
    pub last_rect: Rect,
    pub last_clip: Rect,
    /// What the children wanted last frame, padding included. `last_content`
    /// minus the rect's extent is the scrollable overflow.
    pub last_content: [f32; 2],
    pub scroll: [f32; 2],
    /// Current animation value, moved toward `anim_target` by [`Ui::animate`].
    pub anim: f32,
    pub anim_target: f32,
    /// Opaque to this crate. A widget owns these outright and nothing here
    /// reads or writes them, so a widget needing a `u64` or a handful of flags
    /// does not have to smuggle them through the float fields.
    pub user: [u64; 4],
    /// Frame the entry was created on. Lets a widget tell a fresh entry from a
    /// zeroed one without reserving a sentinel inside its own data.
    pub born: u32,
}

impl WidgetState {
    /// Whether this entry was created on `frame`, meaning its fields are
    /// defaults rather than anything a previous frame chose.
    #[inline]
    pub fn is_fresh(&self, frame: u32) -> bool {
        self.born == frame
    }
}

/// How many frames a widget can go unseen before its state is dropped. Without
/// this a long-running UI leaks state for every widget it ever showed.
pub const STATE_MAX_AGE: u32 = 120;

/// Frames one full sweep of the state table takes.
///
/// Staleness lives in the table's slots, so finding it costs a pass over
/// capacity, not over live widgets. Doing that every frame makes the cost of an
/// idle frame a function of how big the table is rather than how much is in it,
/// which measured as nearly half of a 100-widget frame at 4096 slots and would
/// be the largest single line item in any larger table.
///
/// Sweeping a slice per frame makes it proportional to capacity divided by this,
/// at the cost of state outliving [`STATE_MAX_AGE`] by up to one sweep. At 60fps
/// this is a second, against a two second age limit, and nothing observes the
/// difference.
const STATE_SWEEP_FRAMES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiError {
    Layout(LayoutError),
    State(KeyedError),
    /// Nested deeper than the key stack allows.
    TooDeep,
    /// More custom regions than the storage holds.
    TooManyCustom,
    /// `close` without a matching `open`, or `end_frame` with boxes still open.
    Unbalanced,
    /// The quad and previous-quad buffers must match, since one is diffed
    /// against the other.
    MismatchedQuadBuffers,
}

impl From<LayoutError> for UiError {
    fn from(e: LayoutError) -> Self {
        UiError::Layout(e)
    }
}
impl From<KeyedError> for UiError {
    fn from(e: KeyedError) -> Self {
        UiError::State(e)
    }
}

/// Borrowed tables. `layout` is [`stein_layout::LayoutStorage`]; the rest belong
/// to this crate.
pub struct UiStorage<'a> {
    pub layout: LayoutStorage<'a>,
    pub state_keys: &'a mut [u64],
    pub state_values: &'a mut [Option<WidgetState>],
    pub state_touched: &'a mut [u32],
    pub quads: &'a mut [Quad],
    /// Last frame's quads, to diff against. Same length as `quads`.
    pub prev_quads: &'a mut [Quad],
    pub custom: &'a mut [CustomRegion],
    pub key_stack: &'a mut [u64],
    /// Fill colour per box, parallel to emission order.
    pub fill: &'a mut [u32],
    /// Outline per box, parallel to `fill`. Shorter than the box table leaves
    /// the boxes past its end plain.
    pub shape: &'a mut [Shape],
}

/// What `end_frame` concluded.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameResult {
    /// The regions that changed, empty when the frame is identical to the last
    /// one. Several rather than one bounding rect, so two changes at opposite
    /// ends of the window do not dirty everything between them.
    pub damage: Damage,
    /// Whether to present at all. False means the window can stay asleep.
    pub needs_present: bool,
    /// An animation is still in flight, so another frame is owed even if this one
    /// changed nothing. Missing this is how a UI that sleeps when idle freezes
    /// halfway through a transition.
    pub animating: bool,
    /// Quads refused for want of buffer space. Nonzero means the frame on screen
    /// is incomplete.
    pub dropped_quads: u32,
    /// The application asked for another frame via [`Ui::request_repaint`].
    pub repaint_requested: bool,
}

pub struct Ui<'a> {
    boxes: BoxTable<'a>,
    state: KeyedStore<'a, WidgetState>,
    draw: DrawList<'a>,
    prev_quads: &'a mut [Quad],
    prev_len: u32,
    custom: &'a mut [CustomRegion],
    custom_len: u32,
    /// Box index each custom region belongs to, so its rect can be filled in once
    /// layout has solved.
    custom_box: [u32; MAX_CUSTOM_TRACKED],
    key_stack: &'a mut [u64],
    fill: &'a mut [u32],
    shape: &'a mut [Shape],
    /// The box emitted last, which [`Ui::shape`] applies to.
    last_box: u32,
    depth: u32,
    frame: u32,
    input: Input,
    viewport: [f32; 2],
    hot: Option<u64>,
    active: Option<u64>,
    pressed: Option<u64>,
    clicked: Option<u64>,
    was_down: bool,
    /// Pointer at the previous frame, so the delta is per frame rather than
    /// per motion event.
    last_pointer: [f32; 2],
    drag_origin: [f32; 2],
    drag: Drag,
    animating: bool,
    repaint_requested: bool,
    /// Where the incremental state sweep resumes next frame.
    sweep_cursor: usize,
    /// Which widget keyboard input goes to. Survives frames, like any other
    /// identity in this design.
    focused: Option<u64>,
    /// Keys emitted this frame in emission order, so Tab can move to the next
    /// one without the caller maintaining a list.
    focusables: [u64; MAX_FOCUSABLES],
    focusable_count: u32,
    focus_moved: bool,
}

/// Custom regions per frame are a handful, not thousands: one per flame graph, one
/// per timeline. A fixed side array avoids another caller-supplied slice.
const MAX_CUSTOM_TRACKED: usize = 64;

/// Focusable widgets per frame. A form with more than this many fields is a
/// design problem before it is a capacity one.
const MAX_FOCUSABLES: usize = 128;

impl<'a> Ui<'a> {
    pub fn new(s: UiStorage<'a>) -> Result<Self, UiError> {
        if s.quads.len() != s.prev_quads.len() {
            return Err(UiError::MismatchedQuadBuffers);
        }
        let boxes = BoxTable::new(s.layout)?;
        let state = KeyedStore::new(s.state_keys, s.state_values, s.state_touched)?;
        Ok(Ui {
            boxes,
            state,
            draw: DrawList::new(s.quads),
            prev_quads: s.prev_quads,
            prev_len: 0,
            custom: s.custom,
            custom_len: 0,
            custom_box: [0; MAX_CUSTOM_TRACKED],
            key_stack: s.key_stack,
            fill: s.fill,
            shape: s.shape,
            last_box: u32::MAX,
            depth: 0,
            frame: 0,
            input: Input::default(),
            viewport: [0.0, 0.0],
            hot: None,
            active: None,
            pressed: None,
            clicked: None,
            was_down: false,
            last_pointer: [0.0, 0.0],
            drag_origin: [0.0, 0.0],
            drag: Drag::default(),
            animating: false,
            repaint_requested: false,
            sweep_cursor: 0,
            focused: None,
            focusables: [0; MAX_FOCUSABLES],
            focusable_count: 0,
            focus_moved: false,
        })
    }

    /// Resolve input against the previous frame's geometry, then clear for a new
    /// one. Interaction is decided here, once, rather than per widget.
    pub fn begin_frame(&mut self, input: Input, viewport: [f32; 2]) {
        // The box table still holds the previous frame, which is the only
        // geometry that exists at this point. See the module note on the lag.
        self.hot = self.boxes.hit(input.pointer);

        let down = input.pointer_down;
        self.pressed = None;
        self.clicked = None;
        let delta = [
            input.pointer[0] - self.last_pointer[0],
            input.pointer[1] - self.last_pointer[1],
        ];
        if down && !self.was_down {
            self.active = self.hot;
            self.pressed = self.hot;
            // The press point is the anchor, so `total` is movement since the
            // press rather than since the widget happened to become active.
            self.drag_origin = input.pointer;
            self.drag = Drag {
                delta: [0.0, 0.0],
                total: [0.0, 0.0],
                origin: input.pointer,
            };
        } else if down {
            self.drag = Drag {
                delta,
                total: [
                    input.pointer[0] - self.drag_origin[0],
                    input.pointer[1] - self.drag_origin[1],
                ],
                origin: self.drag_origin,
            };
        } else {
            self.drag = Drag::default();
        }
        if !down && self.was_down {
            // A release only counts where the press started. Dragging off and
            // letting go is a cancel, which is what users expect.
            if self.active.is_some() && self.active == self.hot {
                self.clicked = self.active;
            }
            self.active = None;
        }
        self.was_down = down;
        self.last_pointer = input.pointer;

        self.input = input;
        self.viewport = viewport;
        self.frame = self.frame.wrapping_add(1);
        self.animating = false;
        self.repaint_requested = false;
        self.boxes.reset();
        self.draw.clear();
        self.custom_len = 0;
        self.depth = 0;
        self.last_box = u32::MAX;
        self.focusable_count = 0;
        self.focus_moved = false;
    }

    /// Register a widget as able to take keyboard focus, and report whether it
    /// currently has it.
    ///
    /// Call during emission, in the order Tab should visit. Registration is per
    /// frame, so a widget that stops being drawn stops being reachable, and a
    /// focus left pointing at it is dropped by [`Ui::resolve_focus`].
    pub fn focusable(&mut self, key: u64) -> bool {
        let n = self.focusable_count as usize;
        if n < MAX_FOCUSABLES {
            self.focusables[n] = key;
            self.focusable_count += 1;
        }
        self.focused == Some(key)
    }

    pub fn focused(&self) -> Option<u64> {
        self.focused
    }

    pub fn is_focused(&self, key: u64) -> bool {
        self.focused == Some(key)
    }

    /// Move focus explicitly, as a click on a field should.
    pub fn focus(&mut self, key: Option<u64>) {
        if self.focused != key {
            self.focused = key;
            self.focus_moved = true;
            self.repaint_requested = true;
        }
    }

    /// Whether this frame's keys are for `key`.
    ///
    /// The guard every focusable widget needs: without it, two text fields both
    /// consume the same keystroke.
    pub fn keys_for(&self, key: u64) -> bool {
        self.focused == Some(key)
    }

    /// Advance focus, forwards or backwards, wrapping.
    ///
    /// Call after emitting, once the frame's focusables are known. Returns the
    /// new focus.
    pub fn cycle_focus(&mut self, backwards: bool) -> Option<u64> {
        let n = self.focusable_count as usize;
        if n == 0 {
            return None;
        }
        let list = &self.focusables[..n];
        let at = self.focused.and_then(|f| list.iter().position(|k| *k == f));
        let next = match (at, backwards) {
            (None, false) => 0,
            (None, true) => n - 1,
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
        };
        self.focus(Some(list[next]));
        self.focused
    }

    /// Drop a focus pointing at a widget that no longer exists.
    ///
    /// Called by `end_frame`. Without it, focus survives on a widget that has
    /// been scrolled away or whose panel was closed, and keystrokes vanish into
    /// nothing with no way to get them back.
    fn resolve_focus(&mut self) {
        let Some(f) = self.focused else {
            return;
        };
        let n = self.focusable_count as usize;
        if self.focusables[..n].iter().any(|k| *k == f) {
            return;
        }
        // Only forget it once the frame actually offered somewhere to go, so a
        // frame that simply drew nothing focusable does not clear it.
        if n > 0 {
            self.focused = None;
            self.repaint_requested = true;
        }
    }

    pub fn frame_index(&self) -> u32 {
        self.frame
    }
    pub fn input(&self) -> Input {
        self.input
    }

    /// The key of the currently open box, which is what child keys hang off.
    pub fn parent_key(&self) -> u64 {
        if self.depth == 0 {
            0
        } else {
            self.key_stack[self.depth as usize - 1]
        }
    }

    /// Emit a container and descend into it.
    pub fn open(&mut self, key: u64, spec: BoxSpec, fill: u32) -> Result<Response, UiError> {
        let i = self.boxes.open(key, spec)?;
        if self.depth as usize >= self.key_stack.len() {
            return Err(UiError::TooDeep);
        }
        self.key_stack[self.depth as usize] = key;
        self.depth += 1;
        self.record(i, fill);
        Ok(self.interact(key))
    }

    /// Emit a childless box.
    pub fn leaf(&mut self, key: u64, spec: BoxSpec, fill: u32) -> Result<Response, UiError> {
        let i = self.boxes.leaf(key, spec)?;
        self.record(i, fill);
        Ok(self.interact(key))
    }

    pub fn close(&mut self) -> Result<(), UiError> {
        if self.depth == 0 {
            return Err(UiError::Unbalanced);
        }
        self.depth -= 1;
        self.boxes.close()?;
        Ok(())
    }

    /// Reserve a rect the application will paint itself.
    ///
    /// This is the escape hatch. A box per visual element does not scale to a
    /// flame graph over millions of samples, and that geometry comes from data
    /// rather than from a constraint solve. After `end_frame`, read
    /// [`Ui::custom_regions`] and push quads with [`Ui::push_quad`], culling
    /// against the region's clip.
    pub fn custom(&mut self, key: u64, spec: BoxSpec) -> Result<Response, UiError> {
        let i = self.boxes.leaf(key, spec)?;
        if self.custom_len as usize >= self.custom.len()
            || self.custom_len as usize >= MAX_CUSTOM_TRACKED
        {
            return Err(UiError::TooManyCustom);
        }
        // No fill: the application owns every pixel inside.
        self.record(i, 0);
        self.custom_box[self.custom_len as usize] = i;
        self.custom[self.custom_len as usize] = CustomRegion {
            key,
            rect: Rect::default(),
            clip: Rect::default(),
        };
        self.custom_len += 1;
        Ok(self.interact(key))
    }

    fn record(&mut self, i: u32, fill: u32) {
        if let Some(slot) = self.fill.get_mut(i as usize) {
            *slot = fill;
        }
        if let Some(slot) = self.shape.get_mut(i as usize) {
            *slot = Shape::NONE;
        }
        self.last_box = i;
    }

    /// Give the box emitted last a rounded or stroked outline. Its fill paints
    /// in that shape. Does nothing before the first box of a frame, or when the
    /// shape table is shorter than the box table.
    pub fn shape(&mut self, shape: Shape) {
        if let Some(slot) = self.shape.get_mut(self.last_box as usize) {
            *slot = shape;
        }
    }

    /// Ask for another frame even if this one changed nothing.
    ///
    /// Damage can only see what changed *this* frame. When application state
    /// changes in response to a [`Response`], the widget that reads that state
    /// has usually already been emitted, so the change does not reach the screen
    /// until the next frame. Without this the window goes to sleep holding a
    /// frame that is already stale, and the update appears only when some
    /// unrelated event happens to wake it.
    ///
    /// Call it whenever a response causes a state change that anything visible
    /// depends on.
    pub fn request_repaint(&mut self) {
        self.repaint_requested = true;
    }

    /// Whether a key is hovered, asked *before* the widget is emitted.
    ///
    /// Legitimate because interaction was resolved at `begin_frame` from the
    /// previous frame's geometry, so it does not depend on anything this frame
    /// has done yet. This is what lets a fill colour depend on hover without a
    /// second pass over the tree.
    pub fn peek_hovered(&self, key: u64) -> bool {
        self.hot == Some(key)
    }

    pub fn peek_active(&self, key: u64) -> bool {
        self.active == Some(key)
    }

    fn interact(&self, key: u64) -> Response {
        let active = self.active == Some(key);
        Response {
            hovered: self.hot == Some(key),
            pressed: self.pressed == Some(key),
            active,
            clicked: self.clicked == Some(key),
            // Only the dragged widget sees movement. Handing it to everyone
            // would make every hovered widget think it was being dragged.
            drag: if active { self.drag } else { Drag::default() },
        }
    }

    /// Persistent state for a widget, created on first sight. `None` only when the
    /// state table is full.
    pub fn state(&mut self, key: u64) -> Option<&mut WidgetState> {
        let frame = self.frame;
        self.state.get_or_insert_with(key, frame, || WidgetState {
            born: frame,
            ..WidgetState::default()
        })
    }

    /// Last frame's solved rect for a key, readable while this frame is still
    /// being emitted. `None` when the key has no state, or has never been laid
    /// out.
    pub fn last_rect(&self, key: u64) -> Option<Rect> {
        self.state.get(key).map(|s| s.last_rect)
    }

    pub fn last_clip(&self, key: u64) -> Option<Rect> {
        self.state.get(key).map(|s| s.last_clip)
    }

    /// Last frame's content extent, padding included. Subtract the rect's
    /// extent for the scrollable overflow.
    pub fn last_content(&self, key: u64) -> Option<[f32; 2]> {
        self.state.get(key).map(|s| s.last_content)
    }

    /// This frame's content extent by box index, valid after `end_frame`.
    pub fn content_of(&self, i: u32) -> Option<[f32; 2]> {
        self.boxes.content(i)
    }

    /// The drag in progress, by key, readable before the widget is emitted.
    ///
    /// Same justification as [`Ui::peek_hovered`]: it was resolved at
    /// `begin_frame`. Without it a widget that moves itself has to re-derive
    /// the delta from the pointer and a cached anchor, duplicating what this
    /// already computed.
    pub fn peek_drag(&self, key: u64) -> Drag {
        if self.active == Some(key) {
            self.drag
        } else {
            Drag::default()
        }
    }

    /// Move a widget's animation toward `target` and return where it got to.
    ///
    /// The step is [`ease_step`], so the motion is the same at any refresh
    /// rate and never overshoots.
    pub fn animate(&mut self, key: u64, target: f32, rate: f32) -> f32 {
        let dt = self.input.dt;
        let frame = self.frame;
        let Some(st) = self.state.get_or_insert_with(key, frame, || WidgetState {
            born: frame,
            ..WidgetState::default()
        }) else {
            return target;
        };
        st.anim_target = target;
        st.anim += (target - st.anim) * ease_step(rate, dt);
        let v = st.anim;
        if fabs(target - v) > 0.001 {
            // Owe another frame, or the transition stops halfway when the window
            // is otherwise idle.
            self.animating = true;
        }
        v
    }

    /// Solve layout, turn boxes into quads, and resolve custom regions.
    ///
    /// Deliberately does not conclude the frame. Anything the application
    /// paints itself, custom regions and text, is pushed after this and has to
    /// take part in the damage diff, so the diff lives in [`Ui::finish`]. Doing
    /// it here would leave every application-drawn pixel invisible to damage,
    /// and a window that sleeps on damage would sleep through it.
    pub fn end_frame(&mut self) -> Result<(), UiError> {
        if self.depth != 0 {
            return Err(UiError::Unbalanced);
        }
        self.boxes.solve(self.viewport)?;
        self.resolve_custom_regions();
        self.record_geometry();
        self.resolve_focus();
        self.build_quads();
        Ok(())
    }

    /// Conclude the frame: diff the complete draw list against the previous one,
    /// and decide whether anything has to reach the screen.
    ///
    /// Call after pushing whatever the application draws itself.
    pub fn finish(&mut self) -> FrameResult {
        let damage = draw::damage(
            &self.prev_quads[..self.prev_len as usize],
            self.draw.quads(),
        );
        let dropped = self.draw.dropped();
        self.remember_frame();
        self.sweep_state();

        FrameResult {
            damage,
            needs_present: !damage.is_empty() || self.animating || self.repaint_requested,
            animating: self.animating,
            dropped_quads: dropped,
            repaint_requested: self.repaint_requested,
        }
    }

    /// Copy this frame's solve into the state of any key that has some, so the
    /// next frame can read it while emitting.
    fn record_geometry(&mut self) {
        for i in 0..self.boxes.len() as u32 {
            let Some(key) = self.boxes.key(i) else {
                continue;
            };
            let (rect, clip, content) = (
                self.boxes.rect(i),
                self.boxes.clip(i),
                self.boxes.content(i),
            );
            let Some(st) = self.state.get_mut(key) else {
                continue;
            };
            if let (Some(r), Some(c)) = (rect, clip) {
                st.last_rect = r;
                st.last_clip = c;
            }
            if let Some(ct) = content {
                st.last_content = ct;
            }
        }
    }

    fn resolve_custom_regions(&mut self) {
        for n in 0..self.custom_len as usize {
            let i = self.custom_box[n];
            if let (Some(rect), Some(clip)) = (self.boxes.rect(i), self.boxes.clip(i)) {
                self.custom[n].rect = rect;
                self.custom[n].clip = clip;
            }
        }
    }

    fn build_quads(&mut self) {
        let custom = &self.custom_box[..self.custom_len as usize];
        for i in 0..self.boxes.len() as u32 {
            if custom.contains(&i) {
                continue;
            }
            let fill = self.fill.get(i as usize).copied().unwrap_or(0);
            // Zero alpha means "layout only", which containers usually are. They
            // still take part in layout and hit testing, they just never paint.
            if fill >> 24 == 0 {
                continue;
            }
            let (Some(rect), Some(clip)) = (self.boxes.rect(i), self.boxes.clip(i)) else {
                continue;
            };
            let shape = self.shape.get(i as usize).copied().unwrap_or(Shape::NONE);
            self.draw.push(Quad::solid(rect, clip, fill).shaped(shape));
        }
    }

    /// Evict a slice of the state table. Bounded work, so a bigger table costs
    /// more sweeps rather than a more expensive frame.
    fn sweep_state(&mut self) {
        let budget = self.state.capacity().div_ceil(STATE_SWEEP_FRAMES).max(1);
        let (_, next) = self
            .state
            .prune_from(self.frame, STATE_MAX_AGE, self.sweep_cursor, budget);
        self.sweep_cursor = next;
    }

    fn remember_frame(&mut self) {
        let n = self.draw.len();
        self.prev_quads[..n].copy_from_slice(self.draw.quads());
        self.prev_len = n as u32;
    }

    /// Regions the application still has to paint. Valid after `end_frame`.
    pub fn custom_regions(&self) -> &[CustomRegion] {
        &self.custom[..self.custom_len as usize]
    }

    /// Append to this frame's draw list. For filling custom regions after
    /// `end_frame`; these land above every styled box, so a popup over a custom
    /// region has to be drawn by the application too. A layer index per quad with
    /// a counting sort is the fix when that starts to bite.
    pub fn push_quad(&mut self, q: Quad) -> bool {
        self.draw.push(q)
    }

    pub fn quads(&self) -> &[Quad] {
        self.draw.quads()
    }

    /// Solved geometry, for anything that needs it after the fact.
    pub fn rect_of(&self, i: u32) -> Option<Rect> {
        self.boxes.rect(i)
    }

    /// Rect and clip of a box, by index. What a caller needs to paint into it
    /// after `end_frame`, which is how text gets drawn over its background.
    pub fn geometry_of(&self, i: u32) -> Option<(Rect, Rect)> {
        Some((self.boxes.rect(i)?, self.boxes.clip(i)?))
    }

    /// Index of the box emitted most recently. Record it at emission time when
    /// you will need the solved rect later; a key lookup would be a linear scan.
    pub fn last_index(&self) -> u32 {
        self.boxes.len().saturating_sub(1) as u32
    }
    pub fn box_count(&self) -> usize {
        self.boxes.len()
    }
    pub fn live_state(&self) -> usize {
        self.state.len()
    }
    pub fn state_capacity(&self) -> usize {
        self.state.capacity()
    }
}

#[inline]
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
#[inline]
fn fabs(a: f32) -> f32 {
    if a < 0.0 { -a } else { a }
}

#[cfg(test)]
mod tests;
