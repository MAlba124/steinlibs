//! JSON, RFC 8259, as a flat node table over the caller's bytes.
//!
//! ```text
//! bytes -> parse -> Node[] -> get(root, "name") -> text -> unescape
//! ```
//!
//! A parse walks the input once and writes one [`Node`] per value into a
//! slice the caller supplies, or with the `alloc` feature a `Vec` it
//! grows and reuses ([`parse_vec`]), in document order, so a subtree is a
//! contiguous run and a sibling is one jump. Nothing is copied or decoded
//! up front: a string is a span of its source between the quotes, a
//! number the span of its literal. A string with no escapes reads back as
//! a `&str` into the input; one with escapes is decoded on request into a
//! buffer the caller passes, or compared in place without one.
//!
//! Strict: the whole grammar is checked, strings are UTF-8 with no raw
//! control bytes and well formed escapes, surrogate pairs included, and
//! anything after the root value is an error. Nesting is a fixed stack of
//! [`MAX_DEPTH`], so hostile input is an error rather than a stack overflow.
//!
//! [`write`](mod@write) escapes strings the other way, for building small documents.
//!
//! Strings, whitespace and number arrays are scanned 32 bytes at a time
//! through `fearless_simd`, at the level detected once per parse, which
//! puts a document of several megabytes through in a few milliseconds.
//!
//! `no_std`, no `unsafe`, and no allocation without the `alloc` feature.

#![no_std]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

#[cfg(test)]
extern crate std;

#[cfg(feature = "alloc")]
extern crate alloc;

use core::fmt;

pub mod doc;
/// The byte at a time parser the lane one must agree with, for the tests and
/// the fuzz target.
#[cfg(any(test, feature = "oracle"))]
#[doc(hidden)]
pub mod oracle;
pub mod parse;
pub mod text;
pub mod write;

pub use doc::{Children, Doc, Entries, ROOT};
#[cfg(feature = "alloc")]
pub use parse::parse_vec;
pub use parse::{MAX_DEPTH, count, parse};
pub use text::Text;

/// A range of the source. `u32` keeps a [`Node`] at sixteen bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    pub const EMPTY: Span = Span { at: 0, len: 0 };

    /// The bytes. Out of range reads empty rather than panicking.
    #[inline]
    pub fn of<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        let at = self.at as usize;
        src.get(at..at + self.len as usize).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Null,
    False,
    True,
    Number,
    String,
    Array,
    Object,
}

/// One value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    /// A string holds at least one escape, so it is not its own text.
    pub escaped: bool,
    /// [`Kind::String`]: between the quotes. [`Kind::Number`] and the
    /// literals: the token. Containers: from the bracket through its match.
    pub span: Span,
    /// One past the last node of this subtree. The children are
    /// `nodes[i + 1..end]` and the next sibling is at `end`.
    pub end: u32,
}

impl Node {
    pub const EMPTY: Node = Node {
        kind: Kind::Null,
        escaped: false,
        span: Span::EMPTY,
        end: 0,
    };
}

/// What went wrong, and the byte it went wrong at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub kind: ErrorKind,
    pub at: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The input ended inside a value.
    Truncated,
    /// A byte that starts no value, or is not the punctuation expected.
    BadByte,
    /// `tru`, `nul` and the like.
    BadLiteral,
    /// A number that breaks the grammar: `01`, `1.`, `-`, `1e`.
    BadNumber,
    /// A raw control byte in a string, which must be escaped.
    Control,
    /// An unknown escape, bad hex, or a surrogate without its pair.
    BadEscape,
    /// A string that is not UTF-8.
    Utf8,
    /// An object key that is not a string.
    BadKey,
    /// Bytes after the root value.
    Trailing,
    /// Nesting past [`MAX_DEPTH`].
    TooDeep,
    /// More values than the node slice holds.
    TooManyNodes,
    /// Input past what a `u32` span can address.
    TooLarge,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Truncated => "input ends inside a value",
            ErrorKind::BadByte => "unexpected byte",
            ErrorKind::BadLiteral => "bad literal",
            ErrorKind::BadNumber => "bad number",
            ErrorKind::Control => "raw control byte in a string",
            ErrorKind::BadEscape => "bad escape",
            ErrorKind::Utf8 => "string is not UTF-8",
            ErrorKind::BadKey => "object key is not a string",
            ErrorKind::Trailing => "bytes after the value",
            ErrorKind::TooDeep => "nesting too deep",
            ErrorKind::TooManyNodes => "more values than nodes",
            ErrorKind::TooLarge => "input too large",
        };
        write!(f, "json: {what} at byte {}", self.at)
    }
}
