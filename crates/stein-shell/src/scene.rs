//! What the shell needs from whatever it is showing.

use std::os::fd::BorrowedFd;

/// What the pointer looks like.
///
/// A small set rather than the protocol's full list: these are the shapes an
/// application actually chooses between, and a name nobody passes is a name
/// nobody has to maintain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    /// An ordinary arrow.
    #[default]
    Default,
    /// Over text that can be selected.
    Text,
    /// Over something that can be activated, such as a link.
    Pointer,
    /// Work is in progress but the window still responds.
    Progress,
    /// Over a divider that moves left and right.
    ColResize,
    /// Over a divider that moves up and down.
    RowResize,
    /// No cursor at all, for mouse look while the pointer is locked.
    Hidden,
}

use crate::gpu::GpuCtx;
use crate::layer::Layer;
use crate::paint::{Backdrop, Images};
use crate::video::VideoLayer;
use stein_ui::Input;
use stein_ui::draw::{Damage, Quad};

/// What a scene put on the clipboard, for [`Scene::take_clip`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clip {
    Text,
    /// A list of local files, offered as file lists and as text.
    Files,
}

/// One form of the clipboard's contents, as the shell writes or reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    Text,
    /// `text/uri-list`.
    UriList,
    /// `x-special/gnome-copied-files`: a `copy` or `cut` line, then uris.
    GnomeFiles,
}

/// An application the shell can run.
///
/// Deliberately narrow. The shell owns the window, the buffer and the event
/// loop; the scene owns the tables, the font and what anything looks like. The
/// only things crossing the line are input going in and quads coming out.
///
/// `'static` because the Wayland event queue owns its dispatch state, so the
/// scene is boxed and handed over rather than borrowed.
pub trait Scene: 'static {
    /// Build one frame. Called before anything is painted.
    fn build(&mut self, input: Input, viewport: [f32; 2]);

    /// Whether this frame has to reach the screen. False lets the window sleep.
    fn needs_present(&self) -> bool;

    /// Regions that differ from the last presented frame, empty to repaint
    /// nothing. Carried across frames that were built but never presented, so
    /// the scene, not the shell, decides when it is cleared.
    ///
    /// Several rects rather than one bounding box: a frame that changes in two
    /// distant places would otherwise dirty everything between them, which for
    /// a terminal is the whole window on every keystroke.
    fn damage(&self) -> Damage;

    /// Called once a frame has actually reached the screen.
    fn presented(&mut self);

    /// This frame's draw list, in painter order.
    fn quads(&self) -> &[Quad];

    /// Record GPU passes into `ctx.target` ahead of the quads, which then
    /// draw over them, a 3D view under a HUD. True when it drew, so the
    /// quads keep the image rather than clear it.
    ///
    /// Only called when the frame is presented through the GPU. Painted
    /// in software the frame is the quads alone, so a scene that draws
    /// here still says something with them.
    fn draw_gpu(&mut self, ctx: &mut GpuCtx<'_>) -> bool {
        let _ = ctx;
        false
    }

    /// Coverage atlas that textured quads sample: pixels and size.
    fn atlas(&self) -> (&[u8], [u32; 2]);

    /// RGBA atlas that image quads sample. Bump its version when the
    /// pixels change, and damage what shows them: the quads alone do not
    /// differ.
    fn images(&self) -> Images<'_> {
        Images::NONE
    }

    fn title(&self) -> &str {
        "stein"
    }

    fn app_id(&self) -> &str {
        "dev.stein.app"
    }

    /// A size the window should be and stay, in pixels. `None` means
    /// resizable, with the shell's defaults.
    ///
    /// Asked once, before the window exists. Equal minimum and maximum size
    /// is also a signal tiling compositors read: sway floats a fixed-size
    /// window and centres it, which is what a launcher or a dialog wants
    /// without any window rule.
    fn fixed_size(&self) -> Option<[u32; 2]> {
        None
    }

    /// Return false to close the window. Checked once per frame, so a scene can
    /// quit itself rather than only being quit from outside.
    fn running(&self) -> bool {
        true
    }

    /// A second descriptor the loop should wake on.
    ///
    /// Without this the loop waits on the display socket alone, which is only
    /// correct for a scene whose every change comes from the compositor. A
    /// terminal's output arrives on a pty instead, and would sit unread until
    /// the user happened to move the mouse.
    ///
    /// The loop only watches it while the window is otherwise idle, so a scene
    /// that does not drain the descriptor in `build` stalls rather than spins.
    /// What the pointer should look like over this scene.
    ///
    /// Asked every frame. A client that never sets a cursor gets whatever the
    /// compositor last used, which is why dragging a window edge in sway and
    /// releasing leaves the resize arrow sitting over the window: nothing ever
    /// told it otherwise.
    fn cursor(&self) -> Cursor {
        Cursor::Default
    }

    /// Lock the pointer in place and report relative motion in
    /// [`Input::motion`], for mouse look. Asked after every build. The
    /// lock happens once the window has focus and the pointer is over it,
    /// and the cursor is hidden while it holds. A compositor without
    /// pointer constraints and relative pointer gives no lock and zero
    /// motion.
    fn pointer_lock(&self) -> bool {
        false
    }

    fn wakeup(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    /// Consume whatever is on [`Scene::wakeup`] without building a frame.
    ///
    /// Reading and drawing happen at different rates and must not share one.
    /// A terminal can read far faster than a screen refreshes, so tying the
    /// two together throttles the program on the other end to the frame rate:
    /// one pty buffer per frame is under four megabytes a second, whatever the
    /// machine could actually do.
    ///
    /// So the loop calls this whenever the descriptor is readable, including
    /// while a frame is in flight, and paints separately when it can. It must
    /// actually consume the descriptor, or the poll it returns to will report
    /// readable again immediately and spin.
    ///
    /// Returns whether anything changed, so the loop knows a paint is owed.
    fn drain(&mut self) -> bool {
        false
    }

    /// Text the scene wants put on the clipboard, taken once.
    ///
    /// Polled per frame rather than pushed, because only the shell knows the
    /// input serial the compositor demands before it will accept a selection.
    fn take_copy(&mut self) -> Option<String> {
        None
    }

    /// Whether the scene is asking to be handed the clipboard this frame.
    ///
    /// Reading it costs a round trip to whichever application owns it, so it
    /// happens on request rather than every frame.
    fn wants_paste(&mut self) -> bool {
        false
    }

    /// The clipboard contents, in answer to [`Scene::wants_paste`]. Not called
    /// when there was nothing to paste.
    fn paste(&mut self, text: &str) {
        let _ = text;
    }

    /// Contents the scene keeps and wants on the clipboard, taken once.
    ///
    /// Unlike [`Scene::take_copy`] nothing is handed over: the shell asks
    /// for the bytes through [`Scene::write_clip`] whenever another
    /// application pastes, until [`Scene::clip_lost`].
    fn take_clip(&mut self) -> Option<Clip> {
        None
    }

    /// Write the clipboard contents as `flavour`, a piece at a time.
    fn write_clip(&self, flavour: Flavour, out: &mut dyn FnMut(&[u8])) {
        let _ = (flavour, out);
    }

    /// Another application took the clipboard.
    fn clip_lost(&mut self) {}

    /// Whether the scene is asking for a file list from the clipboard.
    fn wants_paste_files(&mut self) -> bool {
        false
    }

    /// The clipboard in answer to [`Scene::wants_paste_files`], in the
    /// best flavour it had. Empty when it held nothing readable.
    fn paste_files(&mut self, flavour: Flavour, bytes: &[u8]) {
        let _ = (flavour, bytes);
    }

    /// Files dropped on the window, from a file manager drag. A frame
    /// follows, so the scene can act on them in its next `build`.
    fn dropped(&mut self, paths: &[std::path::PathBuf]) {
        let _ = paths;
    }

    /// The Escape key closes the program. A scene with a use of its own
    /// for the key says no and gets it as any other.
    fn escape_quits(&self) -> bool {
        true
    }

    /// What the quads are painted over. [`Backdrop::Clear`] leaves the
    /// surface transparent where nothing is drawn, so a video layer
    /// under it shows. Asked every frame.
    fn backdrop(&self) -> Backdrop {
        Backdrop::Opaque
    }

    /// The video layer under the surface, a subsurface showing client
    /// dmabufs, or none. Asked after every build and every drain. A frame
    /// handed out is shown at once, without waiting for the surface, and
    /// is the scene's to keep until [`Scene::video_released`] names it.
    fn video(&mut self) -> Option<VideoLayer<'_>> {
        None
    }

    /// The compositor is done with the frame `id`, which may be reused.
    fn video_released(&mut self, id: u64) {
        let _ = id;
    }

    /// The size a resizable window asks for before the compositor decides,
    /// such as the size it had when it last closed. Asked once.
    fn preferred_size(&self) -> Option<[u32; 2]> {
        None
    }

    /// Put the scene on a layer surface instead of a window. Asked once.
    ///
    /// A compositor without zwlr_layer_shell_v1 gets a window instead, sized
    /// by [`Scene::fixed_size`] when there is one.
    fn layer(&self) -> Option<Layer> {
        None
    }

    /// The output a layer surface goes on, by name ("DP-3"). `None` lets
    /// the compositor pick, which is the focused one on sway.
    fn output(&self) -> Option<&str> {
        None
    }
}
