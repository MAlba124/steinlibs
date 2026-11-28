//! Glyph rasterization and atlas packing.
//!
//! Font bytes in, one grayscale texture plus a table of positioned quads out.
//! This crate knows nothing about layout, windows, GPUs or files. The caller
//! owns the font bytes and the output `Vec`; this crate owns the texture, the
//! glyph cache and the scratch buffers behind them.
//!
//! ```no_run
//! use stein_text::{AtlasConfig, MonoAtlas, PositionedGlyph};
//!
//! # fn run(font_bytes: &[u8]) -> Result<(), stein_text::TextError> {
//! let mut atlas = MonoAtlas::new(font_bytes, AtlasConfig::default())?;
//!
//! // Measurement is free and feeds stein_layout's `measured` column.
//! let [w, h] = atlas.measure("hello");
//!
//! // Reused across frames. Nothing here allocates once the glyphs are cached.
//! let mut quads: Vec<PositionedGlyph> = Vec::with_capacity(4096);
//! atlas.layout("hello", [0.0, 0.0], &mut quads)?;
//! # let _ = (w, h); Ok(())
//! # }
//! ```
//!
//! # Scope
//!
//! Two paths share one atlas. [`MonoAtlas::layout`] puts one codepoint in one
//! cell with no shaping, for terminals and tables. [`Shaper`] runs text through
//! harfrust and [`MonoAtlas::place`] positions the result, for proportional
//! text. See the [`atlas`] module docs for what the grid path does not handle.
//!
//! # Allocation
//!
//! The rest of this workspace is `no_std` with no `alloc` and library crates do
//! not allocate. This crate is the stated exception: skrifa allocates, and a
//! crate that adapts a third-party library inherits its behaviour. The rule that
//! survives is *when* rather than *whether*. Allocation happens at setup and on
//! a cache miss. The per-frame path allocates nothing, and there is a test that
//! says so.

// Test assertions are written with `expect`, which the workspace denies in
// library code. `cargo clippy` does not build test targets by default, so this
// only matters under `--all-targets`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod atlas;
pub mod chain;
pub mod cache;
pub mod elide;
pub mod error;
pub mod flow;
pub mod font;
pub mod pack;
pub mod raster;
pub mod shape;
pub mod wrap;

#[cfg(test)]
mod test_font;

pub use atlas::{AtlasConfig, GlyphEntry, MonoAtlas, PositionedGlyph};
pub use cache::CacheStats;
pub use chain::{FaceRun, ShaperChain, face_runs};
pub use elide::{Elide, elide_into, fits};
pub use error::{DrawFailure, TextError};
pub use flow::{Flow, classify, flow, flow_count};
pub use font::MonoMetrics;
pub use pack::{Rect, ShelfPacker};
pub use raster::{Bounds, Rasterizer};
pub use shape::{ShapedGlyph, Shaper, advance_of};
pub use wrap::{TAB_WIDTH, Wrap, columns, wrap, wrap_count};
