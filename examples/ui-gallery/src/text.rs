//! Proportional text for the gallery: two families, a few sizes, one atlas.
//!
//! ```text
//! label ──► shape ──► measured leaf + Label row ──► end_frame ──► paint: glyph quads at the solved rect
//! ```
//!
//! Labels are rows in a table filled while the frame is emitted, their text
//! appended to one frame buffer, so a frame allocates nothing once the
//! scratch vectors have grown. Glyphs are pushed after `end_frame`, when every
//! rect is known, and take part in the damage diff like any quad.

use stein_layout::{BoxSpec, Rect, SizeSpec};
use stein_text::{AtlasConfig, MonoAtlas, PositionedGlyph, ShapedGlyph, Shaper};
use stein_ui::draw::Quad;
use stein_ui::{Response, Ui};

use crate::GalleryError;

/// Pixel sizes the atlas holds, smallest first.
pub const SIZES: [f32; 6] = [12.0, 14.0, 16.0, 20.0, 26.0, 34.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Sans = 0,
    Mono = 1,
}

/// How a run of text looks: family, an index into [`SIZES`], colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub family: Family,
    pub size: usize,
    pub color: u32,
}

impl Style {
    pub const fn sans(size: usize, color: u32) -> Style {
        Style {
            family: Family::Sans,
            size,
            color,
        }
    }

    pub const fn mono(size: usize, color: u32) -> Style {
        Style {
            family: Family::Mono,
            size,
            color,
        }
    }
}

/// One label of this frame: the box it sits in and its bytes in the frame text.
#[derive(Clone, Copy, Debug)]
struct Label {
    box_index: u32,
    start: u32,
    end: u32,
    style: Style,
    /// Wrapped to the box width rather than drawn on one line.
    wrap: bool,
}

pub struct Text {
    atlas: MonoAtlas<'static>,
    size_ids: [u8; SIZES.len()],
    /// One shaper per family and size, family major.
    shapers: Vec<Shaper<'static>>,
    run: Vec<ShapedGlyph>,
    /// The ellipsis, shaped where a label is cut.
    dots: Vec<ShapedGlyph>,
    flags: Vec<u8>,
    glyphs: Vec<PositionedGlyph>,
    labels: Vec<Label>,
    frame_text: String,
}

const ELLIPSIS: &str = "\u{2026}";

impl Text {
    /// The sans font is the atlas primary, the mono one its second face.
    pub fn load(sans: &'static [u8], mono: &'static [u8]) -> Result<Text, GalleryError> {
        let config = AtlasConfig {
            px_per_em: SIZES[0],
            width: 1024,
            height: 1024,
            snap_cells: false,
            ..AtlasConfig::default()
        };
        let mut atlas = MonoAtlas::new(sans, config)?;
        atlas.add_fallback(mono)?;
        let mut size_ids = [0u8; SIZES.len()];
        for (id, px) in size_ids.iter_mut().zip(SIZES) {
            *id = atlas.add_size(px)?;
        }
        let mut shapers = Vec::with_capacity(SIZES.len() * 2);
        for font in [sans, mono] {
            for px in SIZES {
                shapers.push(Shaper::new(font, px)?);
            }
        }
        Ok(Text {
            atlas,
            size_ids,
            shapers,
            run: Vec::with_capacity(256),
            dots: Vec::with_capacity(4),
            flags: Vec::with_capacity(256),
            glyphs: Vec::with_capacity(1024),
            labels: Vec::with_capacity(256),
            frame_text: String::with_capacity(8192),
        })
    }

    pub fn begin_frame(&mut self) {
        self.labels.clear();
        self.frame_text.clear();
    }

    pub fn atlas(&self) -> (&[u8], [u32; 2]) {
        (self.atlas.pixels(), self.atlas.atlas_size())
    }

    pub fn clear_dirty(&mut self) {
        self.atlas.clear_dirty();
    }

    pub fn line_height(&self, style: Style) -> f32 {
        self.atlas
            .size_metrics(self.size_ids[style.size])
            .map_or(SIZES[style.size] * 1.25, |m| m.line_height.ceil())
    }

    /// Shape `text` into `self.run`.
    fn shape(&mut self, style: Style, text: &str) {
        shape_into(&mut self.shapers, style, text, &mut self.run);
    }

    pub fn width(&mut self, style: Style, text: &str) -> f32 {
        self.shape(style, text);
        stein_text::advance_of(&self.run)
    }

    /// The x of every char boundary of `text`, starting at 0, for caret
    /// placement and hit testing.
    pub fn stops(&mut self, style: Style, text: &str, out: &mut Vec<f32>) {
        self.shape(style, text);
        out.clear();
        out.push(0.0);
        let mut pen = 0.0;
        let mut glyph = 0;
        for (byte, _) in text.char_indices().skip(1).chain(std::iter::once((text.len(), ' '))) {
            while glyph < self.run.len() && (self.run[glyph].cluster as usize) < byte {
                pen += self.run[glyph].x_advance;
                glyph += 1;
            }
            out.push(pen);
        }
    }

    /// A one line label, sized to its text. Elided with an ellipsis when
    /// its box ends up narrower.
    pub fn label(&mut self, ui: &mut Ui<'_>, key: u64, text: &str, style: Style) -> Result<Response, GalleryError> {
        let w = self.width(style, text).ceil();
        let h = self.line_height(style);
        let spec = BoxSpec::leaf().measured(w, h);
        let spec = spec.w(SizeSpec::measured().soft());
        self.push_label(ui, key, spec, text, style, false)
    }

    /// A paragraph wrapped to `width`, or to its box's last width when that
    /// is known, which settles a resize in one frame.
    pub fn paragraph(
        &mut self,
        ui: &mut Ui<'_>,
        key: u64,
        text: &str,
        style: Style,
        width: f32,
    ) -> Result<Response, GalleryError> {
        // Asking for state is what records the box's rect for next frame.
        let last = ui.state(key).map_or(0.0, |s| s.last_rect.width());
        let wrap_at = if last > 0.0 { last } else { width };
        self.shape(style, text);
        stein_text::classify(text, &self.run, &mut self.flags);
        let lines = stein_text::flow_count(&self.run, &self.flags, wrap_at).max(1);
        let h = lines as f32 * self.line_height(style);
        let spec = BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(h));
        self.push_label(ui, key, spec, text, style, true)
    }

    fn push_label(
        &mut self,
        ui: &mut Ui<'_>,
        key: u64,
        spec: BoxSpec,
        text: &str,
        style: Style,
        wrap: bool,
    ) -> Result<Response, GalleryError> {
        // Passive, so a label inside a button leaves the click to the button.
        let response = ui.leaf(key, spec.passive(), 0)?;
        let start = self.frame_text.len() as u32;
        self.frame_text.push_str(text);
        self.labels.push(Label {
            box_index: ui.last_index(),
            start,
            end: self.frame_text.len() as u32,
            style,
            wrap,
        });
        Ok(response)
    }

    /// Push every label's glyphs. Call after `end_frame`.
    pub fn paint(&mut self, ui: &mut Ui<'_>) -> Result<(), GalleryError> {
        let text = std::mem::take(&mut self.frame_text);
        let labels = std::mem::take(&mut self.labels);
        let result = labels.iter().try_for_each(|label| {
            let Some((rect, clip)) = ui.geometry_of(label.box_index) else {
                return Ok(());
            };
            let body = &text[label.start as usize..label.end as usize];
            if label.wrap {
                self.draw_wrapped(ui, body, label.style, rect, clip)
            } else {
                self.draw_line(ui, body, label.style, rect, clip)
            }
        });
        self.frame_text = text;
        self.labels = labels;
        result
    }

    /// One line at `rect`'s top left, elided when wider than `rect`.
    pub fn draw_line(&mut self, ui: &mut Ui<'_>, text: &str, style: Style, rect: Rect, clip: Rect) -> Result<(), GalleryError> {
        self.shape(style, text);
        let room = rect.width() + 0.5;
        if stein_text::advance_of(&self.run) > room {
            self.elide(style, room);
        }
        self.glyphs.clear();
        self.atlas
            .place_sized(self.size_ids[style.size], &self.run, rect.min, &mut self.glyphs)?;
        push_glyphs(ui, &self.glyphs, style.color, clip);
        Ok(())
    }

    /// Cut `self.run` to fit `room` with an ellipsis after it.
    fn elide(&mut self, style: Style, room: f32) {
        shape_into(&mut self.shapers, style, ELLIPSIS, &mut self.dots);
        let budget = room - stein_text::advance_of(&self.dots);
        let mut pen = 0.0;
        let keep = self
            .run
            .iter()
            .position(|g| {
                pen += g.x_advance;
                pen > budget
            })
            .unwrap_or(self.run.len());
        self.run.truncate(keep);
        self.run.extend_from_slice(&self.dots);
    }

    fn draw_wrapped(&mut self, ui: &mut Ui<'_>, text: &str, style: Style, rect: Rect, clip: Rect) -> Result<(), GalleryError> {
        self.shape(style, text);
        stein_text::classify(text, &self.run, &mut self.flags);
        let line_h = self.line_height(style);
        let size = self.size_ids[style.size];
        self.glyphs.clear();
        let mut y = rect.min[1];
        for line in stein_text::flow(&self.run, &self.flags, rect.width()) {
            self.atlas
                .place_sized(size, line.glyphs(&self.run), [rect.min[0], y], &mut self.glyphs)?;
            y += line_h;
        }
        push_glyphs(ui, &self.glyphs, style.color, clip);
        Ok(())
    }
}

/// Shape `text` with the family and size's shaper, tagged with the family's
/// face in the atlas chain.
fn shape_into(shapers: &mut [Shaper<'static>], style: Style, text: &str, out: &mut Vec<ShapedGlyph>) {
    match shapers.get_mut(style.family as usize * SIZES.len() + style.size) {
        Some(shaper) => shaper.shape(text, out),
        None => out.clear(),
    }
    for g in out.iter_mut() {
        g.face = style.family as u16;
    }
}

fn push_glyphs(ui: &mut Ui<'_>, glyphs: &[PositionedGlyph], color: u32, clip: Rect) {
    for g in glyphs {
        let rect = Rect {
            min: [g.dst[0], g.dst[1]],
            max: [g.dst[0] + g.dst[2], g.dst[1] + g.dst[3]],
        };
        ui.push_quad(Quad::textured(rect, clip, color, g.uv));
    }
}

/// A font file found through fontconfig, or the path in `env` when set.
pub fn find_font(env: &str, families: &[&str]) -> Option<&'static [u8]> {
    if let Some(path) = std::env::var_os(env)
        && let Ok(bytes) = std::fs::read(path)
    {
        return Some(Vec::leak(bytes));
    }
    families.iter().find_map(|family| {
        let out = std::process::Command::new("fc-match")
            .args(["--format=%{file}", family])
            .output()
            .ok()?;
        let path = String::from_utf8(out.stdout).ok()?;
        let bytes = std::fs::read(path.trim()).ok()?;
        Some(&*Vec::leak(bytes))
    })
}
