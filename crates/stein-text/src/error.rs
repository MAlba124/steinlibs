//! Error types for font loading, rasterization and packing.
//!
//! Every variant is `Copy`. Nothing here allocates, so an error can be returned
//! from the per-frame path without touching the heap.

use core::fmt;

use skrifa::outline::DrawError;

/// Everything that can go wrong in this crate.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TextError {
    /// The byte slice handed in is not a font this parser understands.
    InvalidFont,
    /// The font has no `glyf`, `CFF`, `CFF2` or `VARC` outlines to scale.
    NoOutlines,
    /// The font has no usable horizontal advance, so no monospace cell exists.
    NoCellAdvance,
    /// The glyph id is absent from the outline collection.
    MissingGlyph(u32),
    /// skrifa refused to draw the outline.
    Draw(DrawFailure),
    /// Building the hinting instance failed.
    Hinting(DrawFailure),
    /// The atlas has no free shelf left for this rectangle.
    AtlasFull { width: u32, height: u32 },
    /// The glyph cannot fit in an empty atlas of this size, so no eviction helps.
    GlyphTooLarge { width: u32, height: u32 },
}

/// A flattened copy of [`DrawError`].
///
/// skrifa's error carries a `GlyphId` and nested error payloads. Collapsing it to
/// a fieldless enum keeps [`TextError`] `Copy` and comparable in tests.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DrawFailure {
    NoSources,
    GlyphNotFound,
    InsufficientMemory,
    RecursionLimit,
    TooManyPoints,
    HintingFailed,
    InvalidAnchorPoint,
    PostScript,
    ToPath,
    Read,
    Unsupported,
}

impl From<&DrawError> for DrawFailure {
    fn from(e: &DrawError) -> Self {
        match e {
            DrawError::NoSources => Self::NoSources,
            DrawError::GlyphNotFound(_) => Self::GlyphNotFound,
            DrawError::InsufficientMemory => Self::InsufficientMemory,
            DrawError::RecursionLimitExceeded(_) => Self::RecursionLimit,
            DrawError::TooManyPoints(_) => Self::TooManyPoints,
            DrawError::HintingFailed(_) => Self::HintingFailed,
            DrawError::InvalidAnchorPoint(..) => Self::InvalidAnchorPoint,
            DrawError::PostScript(_) => Self::PostScript,
            DrawError::ToPath(_) => Self::ToPath,
            DrawError::Read(_) => Self::Read,
            DrawError::HarfBuzzHintingUnsupported => Self::Unsupported,
        }
    }
}

impl fmt::Display for DrawFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::NoSources => "no glyph sources",
            Self::GlyphNotFound => "glyph not found",
            Self::InsufficientMemory => "scaler memory exhausted",
            Self::RecursionLimit => "composite recursion limit",
            Self::TooManyPoints => "too many outline points",
            Self::HintingFailed => "hinting failed",
            Self::InvalidAnchorPoint => "invalid composite anchor point",
            Self::PostScript => "postscript charstring error",
            Self::ToPath => "outline to path conversion failed",
            Self::Read => "font data read error",
            Self::Unsupported => "unsupported draw configuration",
        };
        f.write_str(s)
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFont => f.write_str("not a parseable font"),
            Self::NoOutlines => f.write_str("font contains no scalable outlines"),
            Self::NoCellAdvance => f.write_str("font exposes no horizontal advance"),
            Self::MissingGlyph(gid) => write!(f, "glyph {gid} is not in the font"),
            Self::Draw(e) => write!(f, "draw failed: {e}"),
            Self::Hinting(e) => write!(f, "hinting instance failed: {e}"),
            Self::AtlasFull { width, height } => {
                write!(f, "atlas full for a {width}x{height} rectangle")
            }
            Self::GlyphTooLarge { width, height } => {
                write!(f, "a {width}x{height} glyph never fits this atlas")
            }
        }
    }
}

impl std::error::Error for TextError {}
