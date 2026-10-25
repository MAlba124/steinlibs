//! SHA-1, one message at a time or eight side by side.
//!
//! no_std, never allocates. Feature `sha1` is the scalar hasher, `sha1x8` adds the
//! eight lane one on fearless_simd.

#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(feature = "sha1")]
mod block;
#[cfg(feature = "sha1")]
pub mod sha1;

#[cfg(feature = "sha1x8")]
pub use sha1::{LANES, Sha1x8, sha1_many};
#[cfg(feature = "sha1")]
pub use sha1::{Sha1, sha1};
