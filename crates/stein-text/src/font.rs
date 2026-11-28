//! Font metrics for a monospace grid.
//!
//! The caller owns the font bytes. Nothing here opens a file, consults a config
//! directory or asks a font manager anything. Hand in a `&[u8]` that outlives the
//! atlas and this crate parses it in place.

use skrifa::{
    MetadataProvider,
    charmap::Charmap,
    instance::{LocationRef, Size},
    metrics::GlyphMetrics,
    raw::FontRef,
};

use crate::error::TextError;

/// Characters probed for the cell advance, in preference order.
///
/// A monospace font gives the same answer for all of them. The list exists only
/// so a font missing one of them still yields a cell.
const ADVANCE_PROBES: [char; 4] = ['M', '0', 'x', ' '];

/// Everything a monospace grid needs from a font at one pixel size.
///
/// All fields are in pixels at `px_per_em`. `descent` is a positive distance
/// below the baseline, unlike skrifa's signed value.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MonoMetrics {
    /// Design units per em, from `head`.
    pub units_per_em: u16,
    /// Pixel size these metrics were computed at.
    pub px_per_em: f32,
    /// Horizontal advance of one cell.
    pub advance: f32,
    /// Baseline to the top of the alignment box.
    pub ascent: f32,
    /// Baseline to the bottom of the alignment box, positive downwards.
    pub descent: f32,
    /// Recommended extra space between lines.
    pub leading: f32,
    /// `ascent + descent + leading`.
    pub line_height: f32,
    /// The font's own claim, from `post.isFixedPitch`. Not enforced.
    pub is_monospace: bool,
}

/// Reads the grid metrics for `font` at `px_per_em`.
pub fn mono_metrics(font: &FontRef<'_>, px_per_em: f32) -> Result<MonoMetrics, TextError> {
    let size = Size::new(px_per_em);
    let m = font.metrics(size, LocationRef::default());
    let advance = cell_advance(font, size).ok_or(TextError::NoCellAdvance)?;
    let descent = -m.descent;
    Ok(MonoMetrics {
        units_per_em: m.units_per_em,
        px_per_em,
        advance,
        ascent: m.ascent,
        descent,
        leading: m.leading,
        line_height: m.ascent + descent + m.leading,
        is_monospace: m.is_monospace,
    })
}

/// Advance of the widest probe character, falling back to the font's average and
/// maximum widths for fonts with an unusual character set.
fn cell_advance(font: &FontRef<'_>, size: Size) -> Option<f32> {
    let charmap: Charmap<'_> = font.charmap();
    let metrics: GlyphMetrics<'_> = font.glyph_metrics(size, LocationRef::default());
    for ch in ADVANCE_PROBES {
        if let Some(gid) = charmap.map(ch)
            && let Some(a) = metrics.advance_width(gid)
            && a > 0.0
        {
            return Some(a);
        }
    }
    let global = font.metrics(size, LocationRef::default());
    global
        .average_width
        .filter(|a| *a > 0.0)
        .or(global.max_width.filter(|a| *a > 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font;

    #[test]
    fn metrics_scale_linearly_with_pixel_size() {
        let Some(bytes) = test_font::load() else {
            return;
        };
        let font = FontRef::new(&bytes).expect("parses");
        let a = mono_metrics(&font, 16.0).expect("metrics");
        let b = mono_metrics(&font, 32.0).expect("metrics");
        assert!((b.advance - a.advance * 2.0).abs() < 0.01);
        assert!((b.ascent - a.ascent * 2.0).abs() < 0.01);
        assert!(a.line_height > a.ascent);
        assert!(a.descent > 0.0, "descent should be a positive magnitude");
    }

    #[test]
    fn rejects_garbage_bytes() {
        assert!(FontRef::new(&[0u8; 32]).is_err());
    }
}
