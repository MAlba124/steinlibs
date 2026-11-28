//! Several surfaces from one scene: a bar per output, popups off them.
//!
//! A [`MultiScene`] declares the surfaces it wants as a list of [`Want`]s,
//! keyed by numbers it picks, and the shell makes the list true after every
//! turn: new keys get a surface, dropped or changed ones lose theirs. Every
//! drawing call names the key it is for. Outputs come in through
//! [`MultiScene::outputs`] whenever the set changes, so a scene that wants
//! one bar per output follows monitors being plugged in and out.
//!
//! [`Scene`] is the one surface case, run through [`Single`].

use std::os::fd::BorrowedFd;
use std::path::PathBuf;

use stein_ui::Input;
use stein_ui::draw::{Damage, Quad};

use crate::layer::Layer;
use crate::paint::{Backdrop, Images};
use crate::video::VideoLayer;
use crate::gpu::GpuCtx;
use crate::scene::{Clip, Cursor, Flavour, Scene};

/// An output the compositor has named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output<'a> {
    /// Stable while the output exists, what [`Role::Layer`] names it by.
    pub id: u32,
    /// The connector, "DP-3".
    pub name: &'a str,
}

/// Which side of its anchor a popup opens on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Side {
    #[default]
    Below,
    Above,
    Right,
    Left,
}

/// A popup: a surface over its parent, placed next to a rect of it and
/// moved by the compositor to stay on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Popup {
    /// The key of the surface it hangs off.
    pub parent: u32,
    /// x, y, w, h in the parent's coordinates, what it opens next to.
    pub anchor: [i32; 4],
    pub size: [u32; 2],
    pub side: Side,
    /// Take the pointer and keyboard until dismissed, so a click elsewhere
    /// closes it. Made on the last input event's serial.
    pub grab: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A toplevel window. `fixed` pins its size, `preferred` is asked for
    /// first.
    Window {
        fixed: Option<[u32; 2]>,
        preferred: Option<[u32; 2]>,
    },
    /// A layer surface on output `output`, or the one the compositor picks.
    Layer {
        layer: Layer,
        output: Option<u32>,
    },
    Popup(Popup),
}

/// One surface a scene wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Want {
    pub key: u32,
    pub role: Role,
}

/// A scene drawn onto any number of surfaces. See the module docs.
pub trait MultiScene: 'static {
    /// The outputs, whenever one appears, goes or is named.
    fn outputs(&mut self, outputs: &[Output<'_>]) {
        let _ = outputs;
    }

    /// The surfaces wanted now, parents before their popups.
    fn surfaces(&self) -> &[Want];

    /// The compositor closed `key`: a popup dismissed, a layer's output
    /// gone, a window's close asked for. It stays closed until the scene
    /// drops the key and asks for it again.
    fn closed(&mut self, key: u32);

    fn build(&mut self, key: u32, input: Input, viewport: [f32; 2]);
    fn needs_present(&self, key: u32) -> bool;
    fn damage(&self, key: u32) -> Damage;
    fn presented(&mut self, key: u32);
    fn quads(&self, key: u32) -> &[Quad];

    /// As [`Scene::draw_gpu`], for surface `key`.
    fn draw_gpu(&mut self, key: u32, ctx: &mut GpuCtx<'_>) -> bool {
        let _ = (key, ctx);
        false
    }

    /// The coverage atlas, shared by every surface.
    fn atlas(&self) -> (&[u8], [u32; 2]);

    /// The image atlas, shared by every surface.
    fn images(&self) -> Images<'_> {
        Images::NONE
    }

    fn title(&self, key: u32) -> &str {
        let _ = key;
        "stein"
    }

    fn app_id(&self) -> &str {
        "dev.stein.app"
    }

    fn cursor(&self, key: u32) -> Cursor {
        let _ = key;
        Cursor::Default
    }

    /// As [`Scene::pointer_lock`], for surface `key`.
    fn pointer_lock(&self, key: u32) -> bool {
        let _ = key;
        false
    }

    fn running(&self) -> bool {
        true
    }

    /// Whether presents wait on a frame callback. A scene that redraws a
    /// few times a second, like a bar, can say no: it presents as soon as
    /// it changed, paced by the buffer coming back, and a present then
    /// makes no new protocol object, which in wayland-client allocates.
    /// Such a scene is painted in software into shm buffers and opens no
    /// GPU: a swapchain waits on frame callbacks of its own, and a surface
    /// hidden under a fullscreen window gets none.
    fn paced(&self) -> bool {
        true
    }

    /// As [`Scene::wakeup`]: a descriptor whose readiness wakes every
    /// surface.
    fn wakeup(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    fn drain(&mut self) -> bool {
        false
    }

    fn take_copy(&mut self) -> Option<String> {
        None
    }

    fn wants_paste(&mut self) -> bool {
        false
    }

    fn paste(&mut self, text: &str) {
        let _ = text;
    }

    fn take_clip(&mut self) -> Option<Clip> {
        None
    }

    fn write_clip(&self, flavour: Flavour, out: &mut dyn FnMut(&[u8])) {
        let _ = (flavour, out);
    }

    fn clip_lost(&mut self) {}

    fn wants_paste_files(&mut self) -> bool {
        false
    }

    fn paste_files(&mut self, flavour: Flavour, bytes: &[u8]) {
        let _ = (flavour, bytes);
    }

    fn dropped(&mut self, key: u32, paths: &[PathBuf]) {
        let _ = (key, paths);
    }

    /// Escape closes the whole program. Only the one window case does it.
    fn escape_quits(&self) -> bool {
        false
    }

    /// As [`Scene::backdrop`], for surface `key`.
    fn backdrop(&self, key: u32) -> Backdrop {
        let _ = key;
        Backdrop::Opaque
    }

    /// As [`Scene::video`], for surface `key`.
    fn video(&mut self, key: u32) -> Option<VideoLayer<'_>> {
        let _ = key;
        None
    }

    /// As [`Scene::video_released`], for a frame of surface `key`.
    fn video_released(&mut self, key: u32, id: u64) {
        let _ = (key, id);
    }
}

/// A [`Scene`] as a one surface [`MultiScene`], key 0.
pub(crate) struct Single {
    scene: Box<dyn Scene>,
    want: [Want; 1],
    /// The output the scene named, found by name as outputs arrive.
    output: Option<String>,
    closed: bool,
}

impl Single {
    pub(crate) fn new(scene: Box<dyn Scene>) -> Self {
        let role = match scene.layer() {
            Some(layer) => Role::Layer {
                layer,
                output: None,
            },
            None => Role::Window {
                fixed: scene.fixed_size(),
                preferred: scene.preferred_size(),
            },
        };
        let output = scene.output().map(str::to_string);
        Self {
            scene,
            want: [Want { key: 0, role }],
            output,
            closed: false,
        }
    }

    /// The output name the scene asked for, if any.
    pub(crate) fn output_name(&self) -> Option<&str> {
        self.output.as_deref()
    }
}

impl MultiScene for Single {
    fn outputs(&mut self, outputs: &[Output<'_>]) {
        let Some(name) = self.output.as_deref() else {
            return;
        };
        let id = outputs.iter().find(|o| o.name == name).map(|o| o.id);
        if let Role::Layer { output, .. } = &mut self.want[0].role {
            *output = id;
        }
    }

    fn surfaces(&self) -> &[Want] {
        &self.want
    }

    fn closed(&mut self, _: u32) {
        self.closed = true;
    }

    fn build(&mut self, _: u32, input: Input, viewport: [f32; 2]) {
        self.scene.build(input, viewport);
    }

    fn needs_present(&self, _: u32) -> bool {
        self.scene.needs_present()
    }

    fn damage(&self, _: u32) -> Damage {
        self.scene.damage()
    }

    fn presented(&mut self, _: u32) {
        self.scene.presented();
    }

    fn quads(&self, _: u32) -> &[Quad] {
        self.scene.quads()
    }

    fn draw_gpu(&mut self, _: u32, ctx: &mut GpuCtx<'_>) -> bool {
        self.scene.draw_gpu(ctx)
    }

    fn atlas(&self) -> (&[u8], [u32; 2]) {
        self.scene.atlas()
    }

    fn images(&self) -> Images<'_> {
        self.scene.images()
    }

    fn title(&self, _: u32) -> &str {
        self.scene.title()
    }

    fn app_id(&self) -> &str {
        self.scene.app_id()
    }

    fn cursor(&self, _: u32) -> Cursor {
        self.scene.cursor()
    }

    fn pointer_lock(&self, _: u32) -> bool {
        self.scene.pointer_lock()
    }

    fn running(&self) -> bool {
        !self.closed && self.scene.running()
    }

    fn wakeup(&self) -> Option<BorrowedFd<'_>> {
        self.scene.wakeup()
    }

    fn drain(&mut self) -> bool {
        self.scene.drain()
    }

    fn take_copy(&mut self) -> Option<String> {
        self.scene.take_copy()
    }

    fn wants_paste(&mut self) -> bool {
        self.scene.wants_paste()
    }

    fn paste(&mut self, text: &str) {
        self.scene.paste(text);
    }

    fn take_clip(&mut self) -> Option<Clip> {
        self.scene.take_clip()
    }

    fn write_clip(&self, flavour: Flavour, out: &mut dyn FnMut(&[u8])) {
        self.scene.write_clip(flavour, out);
    }

    fn clip_lost(&mut self) {
        self.scene.clip_lost();
    }

    fn wants_paste_files(&mut self) -> bool {
        self.scene.wants_paste_files()
    }

    fn paste_files(&mut self, flavour: Flavour, bytes: &[u8]) {
        self.scene.paste_files(flavour, bytes);
    }

    fn dropped(&mut self, _: u32, paths: &[PathBuf]) {
        self.scene.dropped(paths);
    }

    fn escape_quits(&self) -> bool {
        self.scene.escape_quits()
    }

    fn backdrop(&self, _: u32) -> Backdrop {
        self.scene.backdrop()
    }

    fn video(&mut self, _: u32) -> Option<VideoLayer<'_>> {
        self.scene.video()
    }

    fn video_released(&mut self, _: u32, id: u64) {
        self.scene.video_released(id);
    }
}
