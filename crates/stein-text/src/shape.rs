//! Shaping: text in, a table of glyph ids with advances and offsets out.
//!
//! A run of text goes through harfrust once and comes out as [`ShapedGlyph`]s
//! in pixels. The atlas then places them with [`crate::MonoAtlas::place`]. The
//! two steps are separate so a caller can break lines on the shaped table,
//! where the advances are known, before anything is positioned.
//!
//! # Scope
//!
//! One font, one run at a time. Pass a line without `\n`: control characters
//! are shaped like any other codepoint and come out as whatever the font maps
//! them to. Script and direction are guessed from the text, so a right to left
//! run comes back in visual order. Bidi resolution across mixed runs and font
//! fallback are the caller's, and neither exists yet.
//!
//! # Allocation
//!
//! The harfrust buffer is recycled across calls and the shape plan for each
//! (direction, script) pair is built once. A warm call touches the heap only
//! if `out` has to grow.

use harfrust::{Direction, Script, ShapeOptions, ShapePlan, ShaperData, UnicodeBuffer};
use skrifa::{
    MetadataProvider,
    instance::{LocationRef, Size},
    raw::FontRef,
};

use crate::error::TextError;

/// One glyph of a shaped run. Distances are pixels at the shaper's size.
#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub struct ShapedGlyph {
    pub glyph_id: u32,
    /// Byte offset in the source text of the grapheme this glyph came from.
    /// A base and its marks share one cluster, a ligature covers several
    /// characters under one, so clusters are monotonic but not dense.
    pub cluster: u32,
    /// Which font of the atlas chain the id belongs to. Zero from a lone
    /// [`Shaper`], set per run by [`crate::ShaperChain`].
    pub face: u16,
    /// Pen movement after this glyph.
    pub x_advance: f32,
    /// Pen to glyph origin, positive right. Non zero for positioned marks.
    pub x_offset: f32,
    /// Pen to glyph origin, positive up.
    pub y_offset: f32,
}

/// A shaper over one caller-owned font at one pixel size.
pub struct Shaper<'a> {
    font: FontRef<'a>,
    data: ShaperData,
    /// Pixels per font unit.
    scale: f32,
    /// One plan per (direction, script) seen. Two or three entries in
    /// practice, so a scan beats a map.
    plans: Vec<(Direction, Script, ShapePlan)>,
    /// Handed to harfrust for a call and taken back after, keeping its storage.
    buffer: Option<UnicodeBuffer>,
}

impl<'a> Shaper<'a> {
    /// Parses `font_bytes` in place and builds the shaping caches.
    ///
    /// This is the only function here that is expected to be slow.
    pub fn new(font_bytes: &'a [u8], px_per_em: f32) -> Result<Self, TextError> {
        Self::with_index(font_bytes, 0, px_per_em)
    }

    /// As [`Shaper::new`], for font `index` of a collection.
    pub fn with_index(font_bytes: &'a [u8], index: u32, px_per_em: f32) -> Result<Self, TextError> {
        let font = FontRef::from_index(font_bytes, index).map_err(|_| TextError::InvalidFont)?;
        let upem = font
            .metrics(Size::unscaled(), LocationRef::default())
            .units_per_em;
        if upem == 0 || !(px_per_em.is_finite() && px_per_em > 0.0) {
            return Err(TextError::InvalidFont);
        }
        let data = ShaperData::new(&font);
        Ok(Self {
            font,
            data,
            scale: px_per_em / f32::from(upem),
            plans: Vec::with_capacity(2),
            buffer: Some(UnicodeBuffer::new()),
        })
    }

    /// Pixel size the output is scaled to.
    #[must_use]
    pub fn px_per_em(&self) -> f32 {
        self.scale * f32::from(self.units_per_em())
    }

    #[must_use]
    pub fn units_per_em(&self) -> u16 {
        self.font
            .metrics(Size::unscaled(), LocationRef::default())
            .units_per_em
    }

    /// Shapes one run of `text` into `out`, which is cleared first.
    pub fn shape(&mut self, text: &str, out: &mut Vec<ShapedGlyph>) {
        out.clear();
        if text.is_empty() {
            return;
        }
        let mut buf = self.buffer.take().unwrap_or_default();
        buf.clear();
        buf.push_str(text);
        buf.guess_segment_properties();
        let (dir, script) = (buf.direction(), buf.script());
        let shaper = self.data.shaper(&self.font).build();
        let found = self.plans.iter().position(|p| p.0 == dir && p.1 == script);
        let plan = match found {
            Some(i) => i,
            None => {
                let plan = ShapePlan::new(&shaper, dir, Some(script), None, &[]);
                self.plans.push((dir, script, plan));
                self.plans.len() - 1
            }
        };
        let options = ShapeOptions::new().plan(Some(&self.plans[plan].2));
        let glyphs = shaper.shape(buf, options);
        let scale = self.scale;
        out.extend(
            glyphs
                .glyph_infos()
                .iter()
                .zip(glyphs.glyph_positions())
                .map(|(info, pos)| ShapedGlyph {
                    glyph_id: info.glyph_id,
                    cluster: info.cluster,
                    face: 0,
                    x_advance: pos.x_advance as f32 * scale,
                    x_offset: pos.x_offset as f32 * scale,
                    y_offset: pos.y_offset as f32 * scale,
                }),
        );
        self.buffer = Some(glyphs.clear());
    }

    /// Room for runs of up to `codepoints` without growing, so a caller
    /// with a known bound shapes allocation free from the first call.
    pub fn reserve(&mut self, codepoints: usize) {
        if let Some(b) = self.buffer.as_mut() {
            b.reserve(codepoints);
        }
    }

    /// How many (direction, script) plans have been built so far.
    #[must_use]
    pub fn plans(&self) -> usize {
        self.plans.len()
    }
}

/// Total pen movement of a run, which is its width for horizontal text.
#[must_use]
pub fn advance_of(run: &[ShapedGlyph]) -> f32 {
    run.iter().map(|g| g.x_advance).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font;

    const PX: f32 = 16.0;

    fn width(s: &mut Shaper<'_>, text: &str) -> f32 {
        let mut out = Vec::new();
        s.shape(text, &mut out);
        advance_of(&out)
    }

    #[test]
    fn empty_text_shapes_to_nothing() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        let mut out = vec![ShapedGlyph::default()];
        s.shape("", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn garbage_is_not_a_font() {
        assert!(matches!(
            Shaper::new(b"not a font", PX).map(|_| ()),
            Err(TextError::InvalidFont)
        ));
    }

    #[test]
    fn a_proportional_font_is_not_a_grid() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        assert!(width(&mut s, "i") < width(&mut s, "M"));
    }

    #[test]
    fn kerning_pulls_pairs_together() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        let apart = width(&mut s, "A") + width(&mut s, "V");
        let together = width(&mut s, "AV");
        assert!(together < apart, "{together} vs {apart}");
    }

    #[test]
    fn clusters_are_byte_offsets() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        let mut out = Vec::new();
        s.shape("a\u{e9}b", &mut out);
        let clusters: Vec<u32> = out.iter().map(|g| g.cluster).collect();
        assert_eq!(clusters, [0, 1, 3]);
    }

    #[test]
    fn a_mark_takes_no_advance() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        let mut out = Vec::new();
        s.shape("x\u{301}", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].x_advance, 0.0);
        // Default cluster level: a mark joins its base's cluster.
        assert_eq!(out[1].cluster, 0);
    }

    #[test]
    fn scale_follows_pixel_size() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut small = Shaper::new(&bytes, PX).expect("shaper");
        let mut big = Shaper::new(&bytes, PX * 2.0).expect("shaper");
        let a = width(&mut small, "Wide text");
        let b = width(&mut big, "Wide text");
        assert!((b - 2.0 * a).abs() < 1e-3, "{a} {b}");
    }

    #[test]
    fn warm_shaping_does_not_grow() {
        let Some(bytes) = test_font::load_proportional() else {
            return;
        };
        let mut s = Shaper::new(&bytes, PX).expect("shaper");
        let mut out = Vec::new();
        s.shape("The quick brown fox jumps over 13 lazy dogs.", &mut out);
        let (cap, plans) = (out.capacity(), s.plans());
        let first: Vec<ShapedGlyph> = out.clone();
        for _ in 0..200 {
            s.shape("The quick brown fox jumps over 13 lazy dogs.", &mut out);
        }
        assert_eq!(out.capacity(), cap);
        assert_eq!(s.plans(), plans);
        assert_eq!(out, first, "shaping is not deterministic");
    }

    #[test]
    fn advance_of_sums() {
        let run = [
            ShapedGlyph {
                x_advance: 1.5,
                ..Default::default()
            },
            ShapedGlyph {
                x_advance: 2.0,
                ..Default::default()
            },
        ];
        assert_eq!(advance_of(&run), 3.5);
        assert_eq!(advance_of(&[]), 0.0);
    }
}
