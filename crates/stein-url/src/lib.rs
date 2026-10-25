//! URL parsing into spans over the input bytes.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod url;

pub use url::{Parts, Scheme, Span, Url, UrlError, decoded_len, parse, percent_decode, resolve};
