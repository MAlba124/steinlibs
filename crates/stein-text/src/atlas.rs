//! The glyph atlas: rasterize once, pack into one texture, emit a quad table.
//!
//! # The grid path does no shaping
//!
//! [`MonoAtlas::layout`] assumes one codepoint is one glyph advancing by one
//! fixed cell. That assumption is wrong for most of the world's text and it is
//! not going to be patched around here. Specifically, the following are **not
//! supported** there and will render incorrectly rather than degrade gracefully:
//!
//! - **combining marks**, which need to stack on a base glyph at zero advance;
//! - **CJK and other wide characters**, which occupy two cells, not one;
//! - **emoji**, which are colour bitmaps or layered `COLR` glyphs, not coverage;
//! - **ligatures and contextual forms**, which need `GSUB`;
//! - **kerning**, which needs `GPOS` or `kern`;
//! - **bidirectional text**, which needs the Unicode bidi algorithm before any
//!   of this runs;
//! - **grapheme clusters** generally: a `char` here is a cell, not a user-visible
//!   character.
//!
//! Shaping is the fix, and it lives in [`crate::shape`]: run text through a
//! [`Shaper`] and hand the result to [`MonoAtlas::place`]. The rasterizer,
//! packer and cache underneath are shared, so both paths fill one texture.
//! Of the list above, `place` still lacks emoji and bidi across runs.
//!
//! # Allocation
//!
//! The rest of this stack is `no_std` with no `alloc`. This crate is the stated
//! exception because skrifa allocates and a crate adapting it inherits that. The
//! line held instead is *when*: the texture, the glyph table, the cache index and
//! the caller's output `Vec` are sized at setup and grown only on a cache miss.
//! Once every glyph in a string has been seen, [`MonoAtlas::layout`] touches the
//! heap zero times. `atlas_layout_is_allocation_free_once_warm` in this module's
//! tests is the proof.

use skrifa::{
    GlyphId, MetadataProvider,
    charmap::Charmap,
    instance::{LocationRef, Size},
    metrics::GlyphMetrics,
    outline::{
        DrawSettings, Engine, HintingInstance, HintingOptions, OutlineGlyphCollection, OutlinePen,
        SmoothMode, Target,
    },
    raw::FontRef,
};

use crate::cache::{CacheStats, KeyMap, glyph_key, size_bits};
use crate::error::{DrawFailure, TextError};
use crate::font::{MonoMetrics, mono_metrics};
use crate::pack::{Rect, ShelfPacker};
use crate::raster::Rasterizer;
use crate::shape::ShapedGlyph;

/// How many glyph slots the cache is sized for up front. Latin plus punctuation
/// and a little box drawing fits well under this, so a typical session never
/// grows the table.
const INITIAL_GLYPH_SLOTS: usize = 512;

/// Setup parameters for an atlas.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct AtlasConfig {
    /// Pixels per em of size 0. [`MonoAtlas::add_size`] adds more.
    pub px_per_em: f32,
    /// Texture width in pixels.
    pub width: u32,
    /// Texture height in pixels.
    pub height: u32,
    /// Blank pixels reserved right of and below each glyph, against filter bleed.
    pub padding: u32,
    /// Apply the font's embedded hints, or the autohinter where there are none.
    pub hinting: bool,
    /// Round the cell advance and line height to whole pixels.
    ///
    /// A monospace grid wants integer cells; fractional ones make every column
    /// after the first land on a different subpixel phase and blur.
    pub snap_cells: bool,
    /// Squeeze a glyph wider than the cell, Iosevka's `…` or a fallback
    /// font's, to fit it. For a grid that gives every character one cell;
    /// a terminal that gives wide ones two leaves it off.
    pub fit_cell: bool,
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self {
            px_per_em: 16.0,
            width: 512,
            height: 512,
            padding: 1,
            hinting: true,
            snap_cells: true,
            fit_cell: false,
        }
    }
}

/// One rasterized glyph's place in the texture and its bearings.
#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub struct GlyphEntry {
    pub glyph_id: u32,
    /// Atlas column of the bitmap's left edge.
    pub x: u32,
    /// Atlas row of the bitmap's top edge.
    pub y: u32,
    /// Bitmap width. Zero for a blank glyph such as space.
    pub w: u32,
    /// Bitmap height. Zero for a blank glyph.
    pub h: u32,
    /// Pen origin to the bitmap's left edge, positive right.
    pub left: i32,
    /// Baseline to the bitmap's top edge, positive up.
    pub top: i32,
    /// This glyph's own horizontal advance, which for a monospace font equals
    /// the cell advance. Kept per glyph so a broken font is visible.
    pub advance: f32,
}

/// One quad for the renderer. Laid out as two vec4s because that is what a
/// vertex buffer wants.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub struct PositionedGlyph {
    /// Destination rectangle in the caller's pixel space: x, y, w, h.
    pub dst: [f32; 4],
    /// Source rectangle in normalised texture space: u, v, w, h.
    pub uv: [f32; 4],
}

/// A monospace glyph atlas over caller-owned font bytes.
/// Most fonts one chain may hold, bounded by the room the cache key leaves
/// for a face.
const MAX_FACES: usize = 256;

/// Sizes one atlas holds at most, the config's included.
pub const MAX_SIZES: usize = 16;

/// One font in the chain.
///
/// Everything here is per font; everything on [`MonoAtlas`] below it is shared
/// between them. That split is what lets several fonts fill one texture: the
/// packer and the pixel buffer neither know nor care which face a glyph came
/// from, so a caller still hands the compositor a single atlas.
struct Face<'a> {
    font: FontRef<'a>,
    outlines: OutlineGlyphCollection<'a>,
    charmap: Charmap<'a>,
    /// Advances and hinting per size, by size id.
    sized: Vec<Sized<'a>>,
    /// Drawn at its own width even under `fit_cell`, for icons meant to
    /// spill into the cell after them.
    keep_width: bool,
}

/// What a face needs at one pixel size.
struct Sized<'a> {
    glyph_metrics: GlyphMetrics<'a>,
    hinter: Option<HintingInstance>,
}

/// A size's cell, snapped the way the atlas's own is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeMetrics {
    pub px_per_em: f32,
    pub cell_advance: f32,
    pub line_height: f32,
    pub baseline: f32,
}

/// One size the atlas holds: its cell, the primary's metrics at it, and its
/// cache key bits.
#[derive(Clone, Copy, Debug)]
struct SizeData {
    cell: SizeMetrics,
    mono: MonoMetrics,
    bits: u32,
}

pub struct MonoAtlas<'a> {
    config: AtlasConfig,
    /// The primary font, then fallbacks in the order they were added.
    faces: Vec<Face<'a>>,
    metrics: MonoMetrics,
    cell_advance: f32,
    line_height: f32,
    ascent: f32,
    /// Every size by id, the config's first.
    sizes: Vec<SizeData>,
    pixels: Vec<u8>,
    packer: ShelfPacker,
    entries: Vec<GlyphEntry>,
    index: KeyMap,
    raster: Rasterizer,
    stats: CacheStats,
    dirty: Option<Rect>,
}

impl<'a> Face<'a> {
    /// A face ready at every size in `sizes`, in their order.
    fn new(font: &FontRef<'a>, sizes: &[f32], hinting: bool) -> Result<Face<'a>, TextError> {
        let outlines = font.outline_glyphs();
        if outlines.format().is_none() {
            return Err(TextError::NoOutlines);
        }
        let mut face = Face {
            font: font.clone(),
            outlines,
            charmap: font.charmap(),
            sized: Vec::with_capacity(sizes.len()),
            keep_width: false,
        };
        for &px in sizes {
            face.add_size(px, hinting)?;
        }
        Ok(face)
    }

    fn add_size(&mut self, px: f32, hinting: bool) -> Result<(), TextError> {
        let size = Size::new(px);
        let hinter = build_hinter(&self.outlines, size, hinting)?;
        self.sized.push(Sized {
            glyph_metrics: self.font.glyph_metrics(size, LocationRef::default()),
            hinter,
        });
        Ok(())
    }
}

impl<'a> MonoAtlas<'a> {
    /// Parses `font_bytes` in place and allocates the texture and caches.
    ///
    /// This is the only function here that is expected to be slow.
    pub fn new(font_bytes: &'a [u8], config: AtlasConfig) -> Result<Self, TextError> {
        Self::with_index(font_bytes, 0, config)
    }

    /// As [`MonoAtlas::new`], for font `index` of a collection. A plain font
    /// file is index 0.
    pub fn with_index(
        font_bytes: &'a [u8],
        index: u32,
        config: AtlasConfig,
    ) -> Result<Self, TextError> {
        let font = FontRef::from_index(font_bytes, index).map_err(|_| TextError::InvalidFont)?;
        let face = Face::new(&font, &[config.px_per_em], config.hinting)?;
        let metrics = mono_metrics(&font, config.px_per_em)?;
        let (cell_advance, line_height, ascent) = snap(&metrics, config.snap_cells);
        let first = SizeData {
            cell: SizeMetrics {
                px_per_em: config.px_per_em,
                cell_advance,
                line_height,
                baseline: ascent,
            },
            mono: metrics,
            bits: size_bits(config.px_per_em),
        };
        let texels = config.width as usize * config.height as usize;
        Ok(Self {
            config,
            faces: vec![face],
            metrics,
            cell_advance,
            line_height,
            ascent,
            sizes: vec![first],
            pixels: vec![0u8; texels],
            packer: ShelfPacker::new(config.width, config.height, config.padding),
            entries: Vec::with_capacity(INITIAL_GLYPH_SLOTS),
            index: KeyMap::with_capacity(INITIAL_GLYPH_SLOTS),
            raster: Rasterizer::new(),
            stats: CacheStats::default(),
            dirty: None,
        })
    }

    #[must_use]
    pub fn config(&self) -> AtlasConfig {
        self.config
    }

    #[must_use]
    pub fn metrics(&self) -> MonoMetrics {
        self.metrics
    }

    /// Horizontal distance between cell origins, snapped if configured.
    #[must_use]
    pub fn cell_advance(&self) -> f32 {
        self.cell_advance
    }

    /// Vertical distance between baselines, snapped if configured.
    #[must_use]
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Distance from the top of a line box down to its baseline.
    #[must_use]
    pub fn baseline(&self) -> f32 {
        self.ascent
    }

    #[must_use]
    pub fn atlas_size(&self) -> [u32; 2] {
        [self.config.width, self.config.height]
    }

    /// The grayscale coverage texture, one byte per texel, row major.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Region written since the last [`MonoAtlas::clear_dirty`], for partial
    /// texture uploads. `None` means nothing changed.
    #[must_use]
    pub fn dirty_rect(&self) -> Option<Rect> {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = None;
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Number of distinct glyphs currently resident.
    #[must_use]
    pub fn resident(&self) -> usize {
        self.entries.len()
    }

    /// Fraction of the texture handed out.
    #[must_use]
    pub fn occupancy(&self) -> f32 {
        self.packer.occupancy()
    }

    /// Throws away every cached glyph and frees the whole texture for reuse.
    /// Keeps the allocations.
    pub fn reset_cache(&mut self) {
        self.packer.reset();
        self.entries.clear();
        self.index.clear();
        self.stats = CacheStats::default();
        self.pixels.fill(0);
        self.dirty = Some(Rect {
            x: 0,
            y: 0,
            w: self.config.width,
            h: self.config.height,
        });
    }

    /// Width and height of `text`, without rasterizing anything.
    ///
    /// Feeds `stein_layout`'s `measured` column. Width is the longest line in
    /// cells times the cell advance; height is the line count times the line
    /// height. Empty text measures zero on both axes so an empty box collapses.
    #[must_use]
    pub fn measure(&self, text: &str) -> [f32; 2] {
        if text.is_empty() {
            return [0.0, 0.0];
        }
        let (cols, lines) = grid_extent(text);
        [
            cols as f32 * self.cell_advance,
            lines as f32 * self.line_height,
        ]
    }

    /// Rasterizes every glyph in `text` that is not already cached.
    ///
    /// Call this once off the frame path so a later [`MonoAtlas::layout`] is a
    /// pure lookup.
    pub fn prepare(&mut self, text: &str) -> Result<(), TextError> {
        for ch in text.chars().filter(|c| !is_break(*c)) {
            self.glyph(ch)?;
        }
        Ok(())
    }

    /// Add a font to consult for characters the ones before it lack.
    ///
    /// No font covers everything. A terminal draws box drawing, arrows like
    /// `U+23F5` and whatever else a program decides to use, and the usual
    /// monospace fonts each miss a different part of that. Without a chain,
    /// a missing character is a blank cell and nothing reports it.
    ///
    /// Metrics stay the primary's. A fallback glyph is drawn into the cell the
    /// primary defines, because a grid with two column widths is not a grid;
    /// a fallback whose glyphs are much wider will overhang.
    pub fn add_fallback(&mut self, font_bytes: &'a [u8]) -> Result<(), TextError> {
        self.add_fallback_at(font_bytes, 0)
    }

    /// As [`MonoAtlas::add_fallback`], for font `index` of a collection.
    pub fn add_fallback_at(&mut self, font_bytes: &'a [u8], index: u32) -> Result<(), TextError> {
        if self.faces.len() >= MAX_FACES {
            return Err(TextError::InvalidFont);
        }
        let font = FontRef::from_index(font_bytes, index).map_err(|_| TextError::InvalidFont)?;
        let px: Vec<f32> = self.sizes.iter().map(|s| s.cell.px_per_em).collect();
        self.faces.push(Face::new(&font, &px, self.config.hinting)?);
        Ok(())
    }

    /// Draw face `face` at its glyphs' own widths even under
    /// [`AtlasConfig::fit_cell`]: an icon font whose glyphs are meant to
    /// take the room after them.
    pub fn keep_width(&mut self, face: usize) {
        if let Some(f) = self.faces.get_mut(face) {
            f.keep_width = true;
        }
    }

    /// Hold glyphs at `px_per_em` as well, in the same texture. Returns the
    /// size's id, the one it already has when the size is held. Size 0 is
    /// the config's. A size that is not finite and positive is refused, as is
    /// one past [`MAX_SIZES`].
    pub fn add_size(&mut self, px_per_em: f32) -> Result<u8, TextError> {
        if !(px_per_em.is_finite() && px_per_em > 0.0) {
            return Err(TextError::InvalidFont);
        }
        let bits = size_bits(px_per_em);
        if let Some(i) = self.sizes.iter().position(|s| s.bits == bits) {
            return Ok(i as u8);
        }
        if self.sizes.len() >= MAX_SIZES {
            return Err(TextError::AtlasFull {
                width: self.config.width,
                height: self.config.height,
            });
        }
        let primary = self
            .faces
            .first()
            .ok_or(TextError::InvalidFont)?
            .font
            .clone();
        let mono = mono_metrics(&primary, px_per_em)?;
        for f in &mut self.faces {
            f.add_size(px_per_em, self.config.hinting)?;
        }
        let (cell_advance, line_height, baseline) = snap(&mono, self.config.snap_cells);
        self.sizes.push(SizeData {
            cell: SizeMetrics {
                px_per_em,
                cell_advance,
                line_height,
                baseline,
            },
            mono,
            bits,
        });
        Ok((self.sizes.len() - 1) as u8)
    }

    /// Size `size`'s cell, or `None` for a size the atlas does not hold.
    #[must_use]
    pub fn size_metrics(&self, size: u8) -> Option<SizeMetrics> {
        self.sizes.get(usize::from(size)).map(|s| s.cell)
    }

    /// As [`MonoAtlas::glyph`], at size `size`.
    pub fn glyph_sized(&mut self, size: u8, ch: char) -> Result<GlyphEntry, TextError> {
        let found = self
            .faces
            .iter()
            .enumerate()
            .find_map(|(i, f)| f.charmap.map(ch).map(|g| (i, g)));
        let (face, gid) = found.unwrap_or((0, GlyphId::new(0)));
        self.glyph_at(size, face, gid)
    }

    /// As [`MonoAtlas::glyph_of`], at size `size`.
    pub fn glyph_of_sized(
        &mut self,
        size: u8,
        face: usize,
        ch: char,
    ) -> Option<Result<GlyphEntry, TextError>> {
        let gid = self.faces.get(face)?.charmap.map(ch)?;
        Some(self.glyph_at(size, face, gid))
    }

    /// How many fonts are in the chain, the primary included.
    #[must_use]
    pub fn faces(&self) -> usize {
        self.faces.len()
    }

    /// Which font in the chain would draw `ch`, or `None` if none of them can.
    ///
    /// Answering without rasterising anything, so a caller can ask what its
    /// coverage is without filling the atlas to find out.
    #[must_use]
    pub fn face_for(&self, ch: char) -> Option<usize> {
        self.faces.iter().position(|f| f.charmap.map(ch).is_some())
    }

    /// Whether font `face` of the chain maps `ch`.
    #[must_use]
    pub fn covers(&self, face: usize, ch: char) -> bool {
        self.faces
            .get(face)
            .is_some_and(|f| f.charmap.map(ch).is_some())
    }

    /// The cached entry for `ch`, rasterizing on a miss.
    ///
    /// Characters no font in the chain maps fall back to glyph 0 of the
    /// primary, the `.notdef` box.
    pub fn glyph(&mut self, ch: char) -> Result<GlyphEntry, TextError> {
        let found = self
            .faces
            .iter()
            .enumerate()
            .find_map(|(i, f)| f.charmap.map(ch).map(|g| (i, g)));
        let (face, gid) = found.unwrap_or((0, GlyphId::new(0)));
        self.glyph_in(face, gid)
    }

    /// The cached entry for `ch` from font `face` of the chain alone, or
    /// `None` when that font does not map it. For a symbol font whose
    /// codepoints another font in the chain also claims.
    pub fn glyph_of(&mut self, face: usize, ch: char) -> Option<Result<GlyphEntry, TextError>> {
        let gid = self.faces.get(face)?.charmap.map(ch)?;
        Some(self.glyph_in(face, gid))
    }

    /// The cached entry for a glyph id in the primary font.
    pub fn glyph_by_id(&mut self, gid: GlyphId) -> Result<GlyphEntry, TextError> {
        self.glyph_in(0, gid)
    }

    /// The cached entry for a glyph id in a named font.
    pub fn glyph_in(&mut self, face: usize, gid: GlyphId) -> Result<GlyphEntry, TextError> {
        self.glyph_at(0, face, gid)
    }

    /// The cached entry for a glyph id in a named font at a size.
    fn glyph_at(&mut self, size: u8, face: usize, gid: GlyphId) -> Result<GlyphEntry, TextError> {
        let bits = self
            .sizes
            .get(usize::from(size))
            .ok_or(TextError::InvalidFont)?
            .bits;
        let key = glyph_key(face as u32, gid.to_u32(), bits);
        if let Some(i) = self.index.get(key) {
            self.stats.hits += 1;
            return Ok(self.entries[i as usize]);
        }
        self.stats.misses += 1;
        let entry = self.rasterize(size, face, gid)?;
        self.stats.rasterized += 1;
        let row = self.entries.len() as u32;
        self.entries.push(entry);
        self.index.insert(key, row);
        Ok(entry)
    }

    /// Emits one quad per visible glyph of `text` into `out`.
    ///
    /// `origin` is the top left of the text block in the caller's pixel space, so
    /// the first baseline sits at `origin[1] + baseline()`. `out` is cleared and
    /// refilled, which keeps its capacity: pass the same `Vec` every frame.
    ///
    /// Blank glyphs emit nothing but still advance the pen.
    pub fn layout(
        &mut self,
        text: &str,
        origin: [f32; 2],
        out: &mut Vec<PositionedGlyph>,
    ) -> Result<(), TextError> {
        out.clear();
        let mut baseline = origin[1] + self.ascent;
        for line in text.split('\n') {
            self.layout_line(line, origin[0], baseline, out)?;
            baseline += self.line_height;
        }
        Ok(())
    }

    /// Places a shaped run along one baseline, appending one quad per visible
    /// glyph to `out`.
    ///
    /// `origin` is the top left of the line box, as for [`MonoAtlas::layout`],
    /// so the baseline sits at `origin[1] + baseline()`. Unlike `layout` this
    /// appends, because a paragraph is placed one broken line at a time. Glyph
    /// ids are looked up in the font each glyph names, so the chain the atlas
    /// holds must be in the order the run was shaped with.
    ///
    /// Each glyph origin is rounded to a whole pixel. The atlas holds one
    /// bitmap per glyph, not one per subpixel phase, and a bitmap drawn at a
    /// fractional position blurs. The pen keeps its fractional advance so the
    /// rounding does not accumulate along the line.
    pub fn place(
        &mut self,
        run: &[ShapedGlyph],
        origin: [f32; 2],
        out: &mut Vec<PositionedGlyph>,
    ) -> Result<(), TextError> {
        self.place_sized(0, run, origin, out)
    }

    /// As [`MonoAtlas::place`], for a run shaped at size `size` from
    /// [`MonoAtlas::add_size`]. The baseline is that size's.
    pub fn place_sized(
        &mut self,
        size: u8,
        run: &[ShapedGlyph],
        origin: [f32; 2],
        out: &mut Vec<PositionedGlyph>,
    ) -> Result<(), TextError> {
        let atlas = [self.config.width as f32, self.config.height as f32];
        let ascent = self
            .sizes
            .get(usize::from(size))
            .ok_or(TextError::InvalidFont)?
            .cell
            .baseline;
        let baseline = origin[1] + ascent;
        let mut pen_x = origin[0];
        for g in run {
            let e = self.glyph_at(size, usize::from(g.face), GlyphId::new(g.glyph_id))?;
            let pen = [
                (pen_x + g.x_offset).round(),
                (baseline - g.y_offset).round(),
            ];
            push_quad(out, e, pen, atlas);
            pen_x += g.x_advance;
        }
        Ok(())
    }

    /// One line of cells along a fixed baseline.
    fn layout_line(
        &mut self,
        line: &str,
        x0: f32,
        baseline: f32,
        out: &mut Vec<PositionedGlyph>,
    ) -> Result<(), TextError> {
        let atlas = [self.config.width as f32, self.config.height as f32];
        let mut pen_x = x0;
        for ch in line.chars().filter(|c| !is_break(*c)) {
            let e = self.glyph(ch)?;
            push_quad(out, e, [pen_x, baseline], atlas);
            pen_x += self.cell_advance;
        }
        Ok(())
    }

    /// Draws, packs and fills one glyph. Only reached on a cache miss.
    fn rasterize(&mut self, size: u8, face: usize, gid: GlyphId) -> Result<GlyphEntry, TextError> {
        let at = *self
            .sizes
            .get(usize::from(size))
            .ok_or(TextError::InvalidFont)?;
        let f = self.faces.get(face).ok_or(TextError::InvalidFont)?;
        let sized = f
            .sized
            .get(usize::from(size))
            .ok_or(TextError::InvalidFont)?;
        let outline = f
            .outlines
            .get(gid)
            .ok_or(TextError::MissingGlyph(gid.to_u32()))?;
        self.raster.reset();
        let px = Size::new(at.cell.px_per_em);
        let own = sized.glyph_metrics.advance_width(gid);
        let squeeze = own
            .filter(|&a| self.config.fit_cell && !f.keep_width && a > at.mono.advance * WIDE)
            .map(|a| at.mono.advance / a);
        let drawn = match (squeeze, sized.hinter.as_ref()) {
            // Hints fit the glyph to the grid at its own width, so a
            // squeezed one is drawn from the plain outline.
            (Some(k), _) => outline.draw(
                DrawSettings::unhinted(px, LocationRef::default()),
                &mut Squeeze {
                    pen: &mut self.raster,
                    k,
                },
            ),
            (None, Some(h)) => outline.draw(DrawSettings::hinted(h, false), &mut self.raster),
            (None, None) => outline.draw(
                DrawSettings::unhinted(px, LocationRef::default()),
                &mut self.raster,
            ),
        };
        let adjusted = drawn.map_err(|e| TextError::Draw(DrawFailure::from(&e)))?;
        let bounds = self.raster.bounds();
        let rect = self.packer.alloc(bounds.width, bounds.height)?;
        if bounds.width != 0 && bounds.height != 0 {
            let stride = self.config.width as usize;
            self.raster
                .fill(bounds, &mut self.pixels, stride, rect.x, rect.y);
            self.dirty = Some(union(self.dirty, rect));
        }
        // From the face the glyph came from, not the primary: a fallback
        // glyph's own advance is the only honest number to report, even
        // though a grid will use the cell width instead.
        let advance = match squeeze {
            Some(_) => at.mono.advance,
            None => adjusted.advance_width.or(own).unwrap_or(at.mono.advance),
        };
        Ok(GlyphEntry {
            glyph_id: gid.to_u32(),
            x: rect.x,
            y: rect.y,
            w: bounds.width,
            h: bounds.height,
            left: bounds.left,
            top: bounds.top,
            advance,
        })
    }
}

/// How much wider than the cell a glyph may be before `fit_cell` squeezes
/// it, so a font's rounding never does.
const WIDE: f32 = 1.05;

/// A pen that draws an outline `k` times as wide, toward its origin.
struct Squeeze<'a> {
    pen: &'a mut Rasterizer,
    k: f32,
}

impl OutlinePen for Squeeze<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pen.move_to(x * self.k, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pen.line_to(x * self.k, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.pen.quad_to(cx0 * self.k, cy0, x * self.k, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let k = self.k;
        self.pen.curve_to(cx0 * k, cy0, cx1 * k, cy1, x * k, y);
    }
    fn close(&mut self) {
        self.pen.close();
    }
}

/// Builds the hinting instance, or `None` when hinting is off.
///
/// `AutoFallback` uses the TrueType interpreter when the font ships `fpgm` or
/// `prep` and the autohinter otherwise, which is what FreeType does by default.
/// `symmetric_rendering` is off because this crate fills with an analytical area
/// rasterizer, the case skrifa documents as being blurred by the wider stems
/// symmetric rendering asks for. `preserve_linear_metrics` is on so grid fitting
/// never moves a cell boundary.
fn build_hinter(
    outlines: &OutlineGlyphCollection<'_>,
    size: Size,
    enabled: bool,
) -> Result<Option<HintingInstance>, TextError> {
    if !enabled {
        return Ok(None);
    }
    let options = HintingOptions {
        engine: Engine::AutoFallback,
        target: Target::Smooth {
            mode: SmoothMode::Normal,
            symmetric_rendering: false,
            preserve_linear_metrics: true,
        },
    };
    HintingInstance::new(outlines, size, LocationRef::default(), options)
        .map(Some)
        .map_err(|e| TextError::Hinting(DrawFailure::from(&e)))
}

/// True for characters that are line structure rather than cells.
#[inline]
fn is_break(c: char) -> bool {
    c == '\n' || c == '\r'
}

/// Longest line in cells and the line count.
fn grid_extent(text: &str) -> (usize, usize) {
    let mut widest = 0usize;
    let mut lines = 0usize;
    for line in text.split('\n') {
        lines += 1;
        widest = widest.max(line.chars().filter(|c| !is_break(*c)).count());
    }
    (widest, lines)
}

/// Emits the quad for one glyph at a pen position. Blank glyphs emit nothing.
fn push_quad(out: &mut Vec<PositionedGlyph>, e: GlyphEntry, pen: [f32; 2], atlas: [f32; 2]) {
    if e.w == 0 || e.h == 0 {
        return;
    }
    out.push(PositionedGlyph {
        dst: [
            pen[0] + e.left as f32,
            pen[1] - e.top as f32,
            e.w as f32,
            e.h as f32,
        ],
        uv: [
            e.x as f32 / atlas[0],
            e.y as f32 / atlas[1],
            e.w as f32 / atlas[0],
            e.h as f32 / atlas[1],
        ],
    });
}

/// Cell advance, line height and baseline, optionally snapped to whole pixels.
fn snap(m: &MonoMetrics, on: bool) -> (f32, f32, f32) {
    if !on {
        return (m.advance, m.line_height, m.ascent);
    }
    let advance = m.advance.round().max(1.0);
    let ascent = m.ascent.ceil();
    let line_height = m.line_height.round().max(1.0);
    (advance, line_height, ascent)
}

fn union(current: Option<Rect>, r: Rect) -> Rect {
    let Some(c) = current else { return r };
    let x = c.x.min(r.x);
    let y = c.y.min(r.y);
    let right = (c.x + c.w).max(r.x + r.w);
    let bottom = (c.y + c.h).max(r.y + r.h);
    Rect {
        x,
        y,
        w: right - x,
        h: bottom - y,
    }
}

#[cfg(test)]
mod size_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font;

    const SAMPLE: &str = "The quick brown fox jumps over 13 lazy dogs.\nSecond line here.";

    /// Sums the coverage bytes of a glyph's rectangle in the atlas.
    fn coverage(a: &MonoAtlas<'_>, e: GlyphEntry) -> u64 {
        let stride = a.atlas_size()[0] as usize;
        let mut total = 0u64;
        for row in 0..e.h as usize {
            let base = (e.y as usize + row) * stride + e.x as usize;
            let span = &a.pixels()[base..base + e.w as usize];
            total += span.iter().map(|p| u64::from(*p)).sum::<u64>();
        }
        total
    }

    /// A second, different font, for the fallback tests. `None` when the
    /// machine has only one, which is not a failure.
    fn other_font(primary: &[u8]) -> Option<Vec<u8>> {
        [
            "AdwaitaMono",
            "DejaVuSansMono",
            "LiberationMono",
            "FreeMono",
        ]
        .into_iter()
        .filter_map(by_family)
        .find(|b| b != primary)
    }

    /// Characters a terminal really draws, for probing coverage.
    const PROBE: [char; 5] = ['\u{23f5}', '\u{23bf}', '\u{e0b0}', '\u{273b}', '\u{2500}'];

    /// A character the primary lacks and `second` has, if there is one. With
    /// none, there is nothing to test on this machine's fonts.
    fn only_in_second(primary: &MonoAtlas<'_>, second: &[u8]) -> Option<char> {
        let mut b = MonoAtlas::new(second, AtlasConfig::default()).ok()?;
        PROBE.into_iter().find(|ch| {
            primary.face_for(*ch).is_none()
                && b.glyph(*ch).map(|g| g.glyph_id != 0).unwrap_or(false)
        })
    }

    fn by_family(family: &str) -> Option<Vec<u8>> {
        let out = std::process::Command::new("fc-match")
            .args(["--format=%{file}", family])
            .output()
            .ok()?;
        std::fs::read(String::from_utf8(out.stdout).ok()?.trim()).ok()
    }

    /// A character the primary lacks is drawn by whichever font in the chain
    /// has it, instead of coming out as the `.notdef` box.
    #[test]
    fn a_fallback_draws_what_the_primary_cannot() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let Some(second) = other_font(&primary) else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        let Some(ch) = only_in_second(&a, &second) else {
            return;
        };

        assert!(
            a.glyph(ch).expect("glyph").glyph_id == 0,
            "the primary should not have {ch:?} before a fallback is added"
        );
        a.add_fallback(&second).expect("fallback");
        assert_eq!(a.faces(), 2);
        assert_eq!(
            a.face_for(ch),
            Some(1),
            "{ch:?} should come from the fallback"
        );
        let e = a.glyph(ch).expect("glyph");
        assert_ne!(e.glyph_id, 0, "{ch:?} still came out as notdef");
        assert!(coverage(&a, e) > 0, "the fallback glyph drew nothing");
    }

    /// A named face draws only what it maps, even where the primary has it.
    #[test]
    fn glyph_of_asks_one_face() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let Some(second) = other_font(&primary) else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        a.add_fallback(&second).expect("fallback");
        assert!(a.glyph_of(2, 'M').is_none(), "no third face");
        let from_primary = a.glyph_of(0, 'M').expect("mapped").expect("glyph");
        assert_eq!(from_primary, a.glyph('M').expect("glyph"));
        if a.covers(1, 'M') {
            let e = a.glyph_of(1, 'M').expect("mapped").expect("glyph");
            assert_ne!(e, from_primary, "the second face's M is its own");
        }
    }

    /// The primary still wins for what it has, or a fallback would quietly
    /// change how ordinary text looks.
    #[test]
    fn the_primary_is_preferred_for_what_it_has() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let Some(second) = other_font(&primary) else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        let before = a.glyph('M').expect("glyph");
        a.add_fallback(&second).expect("fallback");
        assert_eq!(a.face_for('M'), Some(0));
        assert_eq!(a.glyph('M').expect("glyph"), before, "M moved fonts");
    }

    /// Metrics are the primary's, whatever the fallbacks say. A grid with two
    /// column widths is not a grid.
    #[test]
    fn a_fallback_does_not_move_the_cell() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let Some(second) = other_font(&primary) else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        let (adv, line, base) = (a.cell_advance(), a.line_height(), a.baseline());
        a.add_fallback(&second).expect("fallback");
        assert_eq!(a.cell_advance(), adv);
        assert_eq!(a.line_height(), line);
        assert_eq!(a.baseline(), base);
    }

    /// Two fonts in one texture. Glyph 42 of each is a different picture, and
    /// the cache must not serve one for the other.
    #[test]
    fn glyphs_from_two_fonts_do_not_collide_in_the_cache() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let Some(second) = other_font(&primary) else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        a.add_fallback(&second).expect("fallback");
        let from_first = a.glyph_in(0, GlyphId::new(42)).expect("glyph");
        let from_second = a.glyph_in(1, GlyphId::new(42)).expect("glyph");
        assert_ne!(
            (from_first.x, from_first.y),
            (from_second.x, from_second.y),
            "the same slot was served for two different fonts"
        );
    }

    /// A character nothing in the chain has is still the notdef box, not an
    /// error and not a blank.
    #[test]
    fn what_no_font_has_is_still_notdef() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        // A private use codepoint no ordinary font maps.
        let ch = '\u{f8ff}';
        if a.face_for(ch).is_some() {
            return;
        }
        assert_eq!(a.glyph(ch).expect("glyph").glyph_id, 0);
    }

    #[test]
    fn rubbish_is_not_accepted_as_a_fallback() {
        let Some(primary) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&primary, AtlasConfig::default()).expect("atlas");
        assert!(a.add_fallback(b"not a font").is_err());
        assert_eq!(a.faces(), 1, "a refused fallback was still added");
    }

    #[test]
    fn an_m_rasterizes_with_coverage_and_sane_bounds() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let e = a.glyph('M').expect("glyph M");
        assert!(e.w > 0 && e.h > 0, "M rasterized empty: {e:?}");
        // An 'M' is a cap-height glyph: never taller than the line, never wider
        // than two cells, and its top is above the baseline.
        assert!(e.h as f32 <= a.line_height());
        assert!(e.w as f32 <= a.cell_advance() * 2.0);
        assert!(e.top > 0, "M should sit above the baseline");
        assert!(e.left.abs() < e.w as i32 + 4, "absurd left bearing");
        assert!(coverage(&a, e) > 0, "M is blank");
        assert!(a.dirty_rect().is_some());
    }

    #[test]
    fn a_blank_glyph_takes_no_texture() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let e = a.glyph(' ').expect("glyph space");
        assert_eq!((e.w, e.h), (0, 0));
        assert!(a.occupancy() == 0.0, "space consumed atlas space");
    }

    #[test]
    fn every_ascii_glyph_shares_one_advance() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        if !a.metrics().is_monospace {
            return;
        }
        let first = a.glyph('M').expect("glyph").advance;
        for ch in "iWl.0_|@#$%^&*()".chars() {
            let e = a.glyph(ch).expect("glyph");
            assert!(
                (e.advance - first).abs() < 0.01,
                "{ch:?} advances {} not {first}",
                e.advance
            );
        }
    }

    #[test]
    fn a_cache_hit_does_not_rasterize_again() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let first = a.glyph('g').expect("glyph");
        let occupancy = a.occupancy();
        for _ in 0..50 {
            assert_eq!(a.glyph('g').expect("glyph"), first);
        }
        let s = a.stats();
        assert_eq!((s.misses, s.rasterized), (1, 1));
        assert_eq!(s.hits, 50);
        assert!((s.hit_rate() - 50.0 / 51.0).abs() < 1e-5);
        assert_eq!(a.occupancy(), occupancy, "a hit repacked the glyph");
        assert_eq!(a.resident(), 1);
    }

    #[test]
    fn the_same_glyph_at_two_sizes_is_two_entries() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let small = AtlasConfig {
            px_per_em: 12.0,
            ..AtlasConfig::default()
        };
        let big = AtlasConfig {
            px_per_em: 48.0,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&bytes, small).expect("atlas");
        let mut b = MonoAtlas::new(&bytes, big).expect("atlas");
        let ea = a.glyph('H').expect("glyph");
        let eb = b.glyph('H').expect("glyph");
        assert_eq!(ea.glyph_id, eb.glyph_id);
        assert!(eb.h > ea.h, "48px H is not taller than 12px H");
    }

    #[test]
    fn measure_is_the_sum_of_advances() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let cell = a.cell_advance();
        assert_eq!(a.measure(""), [0.0, 0.0]);
        assert_eq!(a.measure("abcde"), [cell * 5.0, a.line_height()]);
        // Longest line wins on width, line count on height.
        assert_eq!(
            a.measure("abc\nabcdefg\nx"),
            [cell * 7.0, a.line_height() * 3.0]
        );
        assert_eq!(a.measure("ab\r\ncd"), [cell * 2.0, a.line_height() * 2.0]);
    }

    #[test]
    fn measure_costs_nothing_and_touches_no_glyph() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let _ = a.measure(SAMPLE);
        assert_eq!(a.resident(), 0, "measure rasterized something");
        assert_eq!(a.stats(), CacheStats::default());
    }

    #[test]
    fn layout_places_quads_on_the_grid() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let mut out = Vec::new();
        a.layout("AB\nC", [10.0, 20.0], &mut out).expect("layout");
        assert_eq!(out.len(), 3);
        let cell = a.cell_advance();
        // Column two sits one cell right of column one, with the same bearing.
        let ea = a.glyph('A').expect("glyph");
        let eb = a.glyph('B').expect("glyph");
        assert!((out[0].dst[0] - (10.0 + ea.left as f32)).abs() < 1e-4);
        assert!((out[1].dst[0] - (10.0 + cell + eb.left as f32)).abs() < 1e-4);
        // Row two sits one line below row one.
        assert!((out[2].dst[1] - out[0].dst[1] - a.line_height()).abs() < 1e-4);
        for q in &out {
            assert!(q.uv[0] >= 0.0 && q.uv[0] + q.uv[2] <= 1.0, "u out of range");
            assert!(q.uv[1] >= 0.0 && q.uv[1] + q.uv[3] <= 1.0, "v out of range");
        }
    }

    #[test]
    fn blank_cells_advance_without_emitting_a_quad() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let mut out = Vec::new();
        a.layout("a b", [0.0, 0.0], &mut out).expect("layout");
        assert_eq!(out.len(), 2, "the space emitted a quad");
        assert!((out[1].dst[0] - out[0].dst[0] - a.cell_advance() * 2.0).abs() < 1.0);
    }

    #[test]
    fn a_tiny_atlas_reports_failure_instead_of_overflowing() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let cfg = AtlasConfig {
            px_per_em: 64.0,
            width: 48,
            height: 48,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&bytes, cfg).expect("atlas");
        let mut out = Vec::new();
        let mut failures = 0;
        for ch in ('A'..='Z').chain('a'..='z') {
            let r = a.layout(&ch.to_string(), [0.0, 0.0], &mut out);
            failures += u32::from(r.is_err());
        }
        assert!(failures > 0, "a 48x48 atlas swallowed 52 glyphs at 64px");
        assert!(a.occupancy() <= 1.0);
    }

    #[test]
    fn atlas_layout_is_allocation_free_once_warm() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let mut out: Vec<PositionedGlyph> = Vec::new();
        a.layout(SAMPLE, [0.0, 0.0], &mut out).expect("layout");
        let before = (
            out.capacity(),
            a.entries.capacity(),
            a.index.capacity(),
            a.pixels.capacity(),
            a.raster.capacities(),
        );
        let misses = a.stats().misses;
        for frame in 0..400u32 {
            let y = frame as f32;
            a.layout(SAMPLE, [0.0, y], &mut out).expect("layout");
        }
        let after = (
            out.capacity(),
            a.entries.capacity(),
            a.index.capacity(),
            a.pixels.capacity(),
            a.raster.capacities(),
        );
        assert_eq!(before, after, "per-frame reallocation crept in");
        assert_eq!(a.stats().misses, misses, "a warm frame rasterized");
    }

    #[test]
    fn reset_cache_frees_the_texture_without_freeing_memory() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        a.prepare(SAMPLE).expect("prepare");
        assert!(a.resident() > 10);
        let cap = a.pixels.capacity();
        a.reset_cache();
        assert_eq!(a.resident(), 0);
        assert_eq!(a.occupancy(), 0.0);
        assert_eq!(a.stats(), CacheStats::default());
        assert_eq!(a.pixels.capacity(), cap);
        assert!(a.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn an_unmapped_codepoint_falls_back_to_notdef() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let e = a.glyph('\u{10FFFD}').expect("notdef");
        assert_eq!(e.glyph_id, 0);
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        let bytes = [0u8; 64];
        assert_eq!(
            MonoAtlas::new(&bytes, AtlasConfig::default()).err(),
            Some(TextError::InvalidFont)
        );
    }

    #[test]
    fn snapped_cells_are_whole_pixels() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        assert_eq!(a.cell_advance().fract(), 0.0);
        assert_eq!(a.line_height().fract(), 0.0);
        assert!((a.cell_advance() - a.metrics().advance).abs() <= 0.5);

        let loose = AtlasConfig {
            snap_cells: false,
            ..AtlasConfig::default()
        };
        let b = MonoAtlas::new(&bytes, loose).expect("atlas");
        assert_eq!(b.cell_advance(), b.metrics().advance);
    }

    #[test]
    fn unhinted_and_hinted_both_produce_glyphs() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let off = AtlasConfig {
            hinting: false,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&bytes, off).expect("atlas");
        let mut b = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let ea = a.glyph('E').expect("glyph");
        let eb = b.glyph('E').expect("glyph");
        assert!(ea.w > 0 && ea.h > 0);
        assert!(eb.w > 0 && eb.h > 0);
        // Grid fitting moves edges by at most a pixel or two at this size.
        assert!(ea.h.abs_diff(eb.h) <= 2, "{} vs {}", ea.h, eb.h);
    }

    #[test]
    fn an_h_has_two_stems_and_a_gap_between_them() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let cfg = AtlasConfig {
            px_per_em: 32.0,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&bytes, cfg).expect("atlas");
        let e = a.glyph('H').expect("glyph H");
        assert!(e.w >= 6 && e.h >= 6, "H too small to inspect: {e:?}");
        // The top row of an H is two stems with a hole between them. A rasterizer
        // that lost its winding or its per-row reset would fill the hole.
        let stride = a.atlas_size()[0] as usize;
        let w = e.w as usize;
        let read = |r: usize| {
            let base = (e.y as usize + r) * stride + e.x as usize;
            a.pixels()[base..base + w].to_vec()
        };
        let top = read(0);
        let ink = |s: &[u8]| s.iter().copied().max().unwrap_or(0);
        assert!(ink(&top[..w / 3]) > 200, "left stem missing");
        assert!(ink(&top[w * 2 / 3..]) > 200, "right stem missing");
        assert_eq!(top[w / 2], 0, "the gap between the stems is filled");
        // The crossbar is solid all the way across at mid height.
        let bar = read(e.h as usize / 2);
        assert!(bar.iter().all(|&p| p > 0), "crossbar has holes: {bar:?}");
        assert_eq!(ink(&bar[w / 3..w * 2 / 3]), 255, "crossbar is not solid");
    }

    #[test]
    fn place_follows_shaped_advances() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let mut s = crate::Shaper::new(&bytes, a.config().px_per_em).expect("shaper");
        let mut run = Vec::new();
        let mut out = Vec::new();
        s.shape("Wide", &mut run);
        a.place(&run, [3.0, 5.0], &mut out).expect("place");
        assert_eq!(out.len(), run.len());
        let mut pen = 3.0f32;
        for (q, g) in out.iter().zip(&run) {
            let e = a.glyph_by_id(GlyphId::new(g.glyph_id)).expect("glyph");
            assert_eq!(q.dst[0], (pen + g.x_offset).round() + e.left as f32);
            let top = (5.0 + a.baseline() - g.y_offset).round() - e.top as f32;
            assert_eq!(q.dst[1], top);
            pen += g.x_advance;
        }
        // Appends rather than clearing, one broken line at a time.
        a.place(&run, [0.0, 40.0], &mut out).expect("place");
        assert_eq!(out.len(), 2 * run.len());
    }

    #[test]
    fn place_is_allocation_free_once_warm() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut a = MonoAtlas::new(&bytes, AtlasConfig::default()).expect("atlas");
        let mut s = crate::Shaper::new(&bytes, a.config().px_per_em).expect("shaper");
        let mut run = Vec::new();
        let mut out: Vec<PositionedGlyph> = Vec::new();
        s.shape(SAMPLE, &mut run);
        a.place(&run, [0.0, 0.0], &mut out).expect("place");
        let before = (
            out.capacity(),
            run.capacity(),
            a.entries.capacity(),
            a.index.capacity(),
            a.pixels.capacity(),
            a.raster.capacities(),
        );
        let misses = a.stats().misses;
        for frame in 0..400u32 {
            out.clear();
            s.shape(SAMPLE, &mut run);
            a.place(&run, [0.0, frame as f32], &mut out).expect("place");
        }
        let after = (
            out.capacity(),
            run.capacity(),
            a.entries.capacity(),
            a.index.capacity(),
            a.pixels.capacity(),
            a.raster.capacities(),
        );
        assert_eq!(before, after, "per-frame reallocation crept in");
        assert_eq!(a.stats().misses, misses, "a warm frame rasterized");
    }
}
