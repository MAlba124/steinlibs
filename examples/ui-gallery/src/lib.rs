//! A gallery of everything the stein UI stack draws, one page per area.
//!
//! ```text
//! Input ──► Gallery::frame ──► sidebar + page ──► end_frame ──► labels, caret ──► finish ──► damage
//! ```
//!
//! The pages are plain functions over [`Demo`], the state every page reads
//! and writes. Which page shows is a small state machine in [`Page`].

use std::fmt;

use stein_arena::Bump;
use stein_layout::{Axis, Insets, LayoutStorage, Options, Rect, SizeSpec};
use stein_shell::Scene;
use stein_ui::draw::{CustomRegion, Damage, Quad, Shape};
use stein_ui::{Input, Ui, UiError, UiStorage, WidgetState};
use stein_widgets::WidgetError;

pub mod pages;
pub mod shot;
pub mod text;

use text::Text;

const BOXES: usize = 4096;
const QUADS: usize = 32_768;
const STATE: usize = 2048;
const CUSTOM: usize = 16;
/// Bytes the tables above are carved from.
pub const ARENA: usize = 64 << 20;

/// The look, from the hum design: near black panels, one warm accent.
pub mod palette {
    pub const BG: u32 = 0xff0e_0e10;
    pub const PANEL: u32 = 0xff17_171a;
    pub const CARD: u32 = 0xff1f_1f23;
    pub const RAISED: u32 = 0xff26_262b;
    pub const CHIP: u32 = 0xff2a_2a2f;
    pub const LINE: u32 = 0xff3f_3f46;
    pub const TEXT: u32 = 0xfff4_f4f5;
    pub const SOFT: u32 = 0xffd4_d4d8;
    pub const MUTED: u32 = 0xffa1_a1aa;
    pub const ACCENT: u32 = 0xffff_8a4c;
    pub const ARTS: [u32; 6] = [
        0xff43_38ca, 0xff0f_766e, 0xffc2_410c, 0xff3f_6212, 0xff6d_28d9, 0xffbe_185d,
    ];
}

#[derive(Debug)]
pub enum GalleryError {
    Ui(UiError),
    Widget(WidgetError),
    Text(stein_text::TextError),
    Arena(stein_arena::ArenaError),
    NoFont,
}

impl fmt::Display for GalleryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GalleryError::Ui(e) => write!(f, "ui: {e:?}"),
            GalleryError::Widget(e) => write!(f, "widget: {e:?}"),
            GalleryError::Text(e) => write!(f, "text: {e}"),
            GalleryError::Arena(e) => write!(f, "arena: {e}"),
            GalleryError::NoFont => write!(f, "no font found, set UI_GALLERY_SANS and UI_GALLERY_MONO"),
        }
    }
}

impl std::error::Error for GalleryError {}

impl From<UiError> for GalleryError {
    fn from(e: UiError) -> Self {
        GalleryError::Ui(e)
    }
}
impl From<WidgetError> for GalleryError {
    fn from(e: WidgetError) -> Self {
        GalleryError::Widget(e)
    }
}
impl From<stein_text::TextError> for GalleryError {
    fn from(e: stein_text::TextError) -> Self {
        GalleryError::Text(e)
    }
}
impl From<stein_arena::ArenaError> for GalleryError {
    fn from(e: stein_arena::ArenaError) -> Self {
        GalleryError::Arena(e)
    }
}

/// Which page shows. Moves only by a click in the sidebar or a name on the
/// command line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Shapes,
    Type,
    Input,
    Motion,
    List,
}

impl Page {
    pub const ALL: [Page; 5] = [Page::Shapes, Page::Type, Page::Input, Page::Motion, Page::List];

    pub fn name(self) -> &'static str {
        match self {
            Page::Shapes => "Shapes",
            Page::Type => "Type",
            Page::Input => "Input",
            Page::Motion => "Motion",
            Page::List => "List",
        }
    }

    pub fn parse(name: &str) -> Option<Page> {
        Page::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(name))
    }
}

/// Everything the pages show and change.
pub struct Demo {
    pub page: Page,
    pub clicks: u32,
    pub toggle: bool,
    pub slider: f32,
    pub field: [u8; 96],
    pub field_len: usize,
    pub field_stops: Vec<f32>,
    pub submitted: u32,
    /// Seconds since start, advanced by each frame's dt while playing.
    pub clock: f32,
    pub playing: bool,
    pub selected: u32,
    /// Last keys pressed, newest last.
    pub keys: [u32; 8],
    pub key_count: usize,
    pub caret: Option<pages::CaretPaint>,
}

impl Default for Demo {
    fn default() -> Self {
        let mut field = [0u8; 96];
        let hello = b"Search songs, shows";
        field[..hello.len()].copy_from_slice(hello);
        Demo {
            page: Page::Shapes,
            clicks: 0,
            toggle: true,
            slider: 0.62,
            field,
            field_len: hello.len(),
            field_stops: Vec::with_capacity(97),
            submitted: 0,
            clock: 0.0,
            playing: true,
            selected: 0,
            keys: [0; 8],
            key_count: 0,
            caret: None,
        }
    }
}

pub struct Gallery {
    ui: Ui<'static>,
    text: Text,
    pub demo: Demo,
    needs_present: bool,
    damage: Damage,
}

impl Gallery {
    pub fn new(bump: &'static Bump, text: Text) -> Result<Gallery, GalleryError> {
        Ok(Gallery {
            ui: Ui::new(ui_storage(bump)?)?,
            text,
            demo: Demo::default(),
            needs_present: true,
            damage: Damage::new(),
        })
    }

    /// One frame: emit, solve, paint text and the caret, diff.
    pub fn frame(&mut self, input: Input, viewport: [f32; 2]) -> Result<(), GalleryError> {
        self.demo.note_keys(&input);
        if self.demo.playing {
            self.demo.clock += input.dt;
        }
        self.ui.begin_frame(input, viewport);
        self.text.begin_frame();
        self.demo.caret = None;
        pages::root(&mut self.ui, &mut self.text, &mut self.demo)?;
        self.ui.end_frame()?;
        self.text.paint(&mut self.ui)?;
        pages::paint_caret(&mut self.ui, &mut self.text, &mut self.demo)?;
        let result = self.ui.finish();
        self.needs_present = result.needs_present;
        self.damage.absorb(&result.damage);
        if self.demo.page == Page::Motion && self.demo.playing {
            self.ui.request_repaint();
            self.needs_present = true;
        }
        Ok(())
    }

    pub fn quads(&self) -> &[Quad] {
        self.ui.quads()
    }

    pub fn atlas(&self) -> (&[u8], [u32; 2]) {
        self.text.atlas()
    }

    pub fn needs_frame(&self) -> bool {
        self.needs_present
    }
}

impl Demo {
    fn note_keys(&mut self, input: &Input) {
        for k in input.keys().iter().filter(|k| k.pressed) {
            if self.key_count == self.keys.len() {
                self.keys.rotate_left(1);
                self.key_count -= 1;
            }
            self.keys[self.key_count] = k.sym;
            self.key_count += 1;
        }
    }

    pub fn field_text(&self) -> &str {
        std::str::from_utf8(&self.field[..self.field_len]).unwrap_or("")
    }
}

impl Scene for Gallery {
    fn build(&mut self, input: Input, viewport: [f32; 2]) {
        if let Err(e) = self.frame(input, viewport) {
            eprintln!("ui-gallery: frame: {e}");
        }
    }

    fn needs_present(&self) -> bool {
        self.needs_present
    }

    fn damage(&self) -> Damage {
        self.damage
    }

    fn presented(&mut self) {
        self.damage = Damage::new();
        self.text.clear_dirty();
    }

    fn quads(&self) -> &[Quad] {
        self.ui.quads()
    }

    fn atlas(&self) -> (&[u8], [u32; 2]) {
        self.text.atlas()
    }

    fn title(&self) -> &str {
        "ui gallery"
    }

    fn app_id(&self) -> &str {
        "dev.stein.ui-gallery"
    }
}

/// The frame's tables, carved from the bump.
fn ui_storage(b: &'static Bump) -> Result<UiStorage<'static>, stein_arena::ArenaError> {
    let blank = Quad::solid(Rect::default(), Rect::default(), 0);
    let region = CustomRegion {
        key: 0,
        rect: Rect::default(),
        clip: Rect::default(),
    };
    Ok(UiStorage {
        layout: LayoutStorage {
            key: b.zeroed(BOXES)?,
            parent: b.zeroed(BOXES)?,
            subtree_end: b.zeroed(BOXES)?,
            size: b.alloc_slice(BOXES, [SizeSpec::px(0.0); 2])?,
            axis: b.alloc_slice(BOXES, Axis::X)?,
            measured: b.alloc_slice(BOXES, [0.0f32; 2])?,
            pad: b.alloc_slice(BOXES, Insets::ZERO)?,
            opts: b.alloc_slice(BOXES, Options::default())?,
            computed: b.alloc_slice(BOXES, [0.0f32; 2])?,
            content: b.alloc_slice(BOXES, [0.0f32; 2])?,
            rect: b.alloc_slice(BOXES, Rect::default())?,
            clip: b.alloc_slice(BOXES, Rect::default())?,
            stack: b.zeroed(BOXES)?,
        },
        state_keys: b.zeroed(STATE)?,
        state_values: b.alloc_slice(STATE, None::<WidgetState>)?,
        state_touched: b.zeroed(STATE)?,
        quads: b.alloc_slice(QUADS, blank)?,
        prev_quads: b.alloc_slice(QUADS, blank)?,
        custom: b.alloc_slice(CUSTOM, region)?,
        key_stack: b.zeroed(BOXES)?,
        fill: b.zeroed(BOXES)?,
        shape: b.alloc_slice(BOXES, Shape::NONE)?,
    })
}

#[cfg(test)]
mod tests;
