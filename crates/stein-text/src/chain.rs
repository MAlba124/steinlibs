//! Shaping across a font chain.
//!
//! ```text
//! text ──► face_runs ──► one Shaper per run ──► glyphs tagged with a face
//! ```
//!
//! No font covers every name a file system can hold, so text is split into
//! runs by which font of the atlas chain draws them, each run is shaped with
//! its own font, and every glyph carries its face so [`MonoAtlas::place`]
//! finds it in the right one.
//!
//! The split prefers the earliest font that maps a character, so Latin after
//! a CJK run goes back to the primary. Spaces and ASCII punctuation stay in
//! the run they are in when its font has them, and marks, joiners and
//! variation selectors never start a run, so a base keeps its marks.

use crate::atlas::MonoAtlas;
use crate::error::TextError;
use crate::shape::{ShapedGlyph, Shaper};

/// One run of text drawn by one font: bytes `start..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaceRun {
    pub face: u16,
    pub start: u32,
    pub end: u32,
}

/// Split `text` into runs over `faces` fonts, `covers(face, ch)` saying
/// whether a font maps a character. Calls `emit` per run in order.
pub fn face_runs(
    text: &str,
    faces: usize,
    covers: impl Fn(usize, char) -> bool,
    mut emit: impl FnMut(FaceRun),
) {
    let first = |ch: char| (0..faces).find(|&f| covers(f, ch));
    let mut cur: Option<usize> = None;
    let mut start = 0usize;
    for (at, ch) in text.char_indices() {
        if sticky(ch) {
            continue;
        }
        let Some(c) = cur else {
            cur = Some(first(ch).unwrap_or(0));
            continue;
        };
        if neutral(ch) && covers(c, ch) {
            continue;
        }
        // Nothing maps it: draw the primary's notdef in the current run.
        let want = first(ch).unwrap_or(c);
        if want != c {
            emit(run(c, start, at));
            cur = Some(want);
            start = at;
        }
    }
    if !text.is_empty() {
        emit(run(cur.unwrap_or(0), start, text.len()));
    }
}

fn run(face: usize, start: usize, end: usize) -> FaceRun {
    FaceRun {
        face: face as u16,
        start: start as u32,
        end: end as u32,
    }
}

/// Characters that belong to whatever precedes them.
fn sticky(ch: char) -> bool {
    matches!(ch,
        '\u{0300}'..='\u{036f}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{200c}'..='\u{200d}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe00}'..='\u{fe0f}'
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{e0100}'..='\u{e01ef}')
}

/// Characters every script shares, kept in the current run when it can.
fn neutral(ch: char) -> bool {
    ch.is_whitespace() || (ch.is_ascii_punctuation() && ch != '_')
}

/// One [`Shaper`] per font of an atlas chain, in the same order, or for
/// the atlas faces a map names.
pub struct ShaperChain<'a> {
    shapers: Vec<Shaper<'a>>,
    /// The atlas face each shaper's font is.
    faces: Vec<u16>,
    scratch: Vec<ShapedGlyph>,
}

impl<'a> ShaperChain<'a> {
    /// Fonts as bytes and collection index, primary first, the order they
    /// were given to the atlas.
    pub fn new(fonts: &[(&'a [u8], u32)], px_per_em: f32) -> Result<Self, TextError> {
        let faces: Vec<u16> = (0..fonts.len() as u16).collect();
        Self::for_faces(fonts, &faces, px_per_em)
    }

    /// Fonts that are atlas faces `faces`, in the order to consult them.
    /// For a second chain in one atlas, such as prose fonts added after a
    /// grid's, falling back to the grid's.
    pub fn for_faces(
        fonts: &[(&'a [u8], u32)],
        faces: &[u16],
        px_per_em: f32,
    ) -> Result<Self, TextError> {
        if fonts.is_empty() || fonts.len() != faces.len() {
            return Err(TextError::InvalidFont);
        }
        let mut shapers = Vec::with_capacity(fonts.len());
        for &(bytes, index) in fonts {
            shapers.push(Shaper::with_index(bytes, index, px_per_em)?);
        }
        Ok(ShaperChain {
            shapers,
            faces: faces.to_vec(),
            scratch: Vec::with_capacity(256),
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.shapers.len()
    }

    /// Room in every shaper for runs of up to `codepoints`.
    pub fn reserve(&mut self, codepoints: usize) {
        for s in &mut self.shapers {
            s.reserve(codepoints);
        }
        self.scratch.reserve(codepoints);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shapers.is_empty()
    }

    /// Shape `text` into `out`, which is cleared first. Clusters stay byte
    /// offsets into the whole of `text`.
    pub fn shape(&mut self, atlas: &MonoAtlas<'_>, text: &str, out: &mut Vec<ShapedGlyph>) {
        out.clear();
        let (shapers, faces, scratch) = (&mut self.shapers, &self.faces, &mut self.scratch);
        // A face the atlas does not hold covers nothing.
        let face = |f: usize| {
            faces
                .get(f)
                .copied()
                .filter(|&a| usize::from(a) < atlas.faces())
        };
        face_runs(
            text,
            shapers.len(),
            |f, ch| face(f).is_some_and(|a| atlas.covers(usize::from(a), ch)),
            |r| {
                let (Some(s), Some(a)) = (
                    shapers.get_mut(usize::from(r.face)),
                    face(usize::from(r.face)),
                ) else {
                    return;
                };
                s.shape(&text[r.start as usize..r.end as usize], scratch);
                out.extend(scratch.iter().map(|g| ShapedGlyph {
                    cluster: g.cluster + r.start,
                    face: a,
                    ..*g
                }));
            },
        );
    }
}

#[cfg(test)]
mod tests;
