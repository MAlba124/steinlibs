//! Zero-copy pull parser for XML. no_std, never allocates.
//! State lives in caller-supplied tables with hard capacity. No DTDs or external entities.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod entity;
pub mod ns;
pub mod scan;

pub use entity::decode;
pub use ns::{Binding, Names};
pub use scan::{Event, Reader};

/// Offset and length into the document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    pub const EMPTY: Span = Span { at: 0, len: 0 };

    #[inline]
    pub fn of(src: &[u8], at: usize, end: usize) -> Span {
        let at = at.min(src.len());
        let end = end.min(src.len()).max(at);
        Span {
            at: at as u32,
            len: (end - at) as u32,
        }
    }

    /// The bytes, or empty when the span does not fit the document.
    #[inline]
    pub fn bytes<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        let at = self.at as usize;
        src.get(at..at + self.len as usize).unwrap_or(&[])
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// One attribute of the current element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    /// As written, prefix included.
    pub name: Span,
    /// Quotes removed, references left raw. See [`decode`].
    pub value: Span,
}

/// Error kind and byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlError {
    pub kind: ErrorKind,
    pub at: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Document ended mid construct.
    Truncated,
    /// A `<` that begins nothing valid.
    BadTag,
    BadName,
    /// Attribute without `=`, or unquoted value.
    BadAttr,
    /// Same attribute name twice on one element.
    DuplicateAttr,
    /// Close tag not matching the innermost open element.
    Mismatched,
    /// Close tag with nothing open.
    Unbalanced,
    /// Not a name or number, or number out of range.
    BadReference,
    /// Element stack capacity exceeded.
    TooDeep,
    /// Attribute table capacity exceeded.
    TooManyAttrs,
    /// Binding table capacity exceeded.
    TooManyBindings,
    /// Refused construct, such as a DTD internal subset defining entities.
    Unsupported,
}

impl XmlError {
    #[inline]
    pub fn new(kind: ErrorKind, at: usize) -> XmlError {
        XmlError {
            kind,
            at: at.min(u32::MAX as usize) as u32,
        }
    }
}

impl core::fmt::Display for XmlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self.kind {
            ErrorKind::Truncated => "the document ended mid construct",
            ErrorKind::BadTag => "not a tag",
            ErrorKind::BadName => "not a name",
            ErrorKind::BadAttr => "malformed attribute",
            ErrorKind::DuplicateAttr => "the same attribute twice",
            ErrorKind::Mismatched => "close tag does not match the open one",
            ErrorKind::Unbalanced => "close tag with nothing open",
            ErrorKind::BadReference => "malformed reference",
            ErrorKind::TooDeep => "nested deeper than the element stack allows",
            ErrorKind::TooManyAttrs => "more attributes than the table allows",
            ErrorKind::TooManyBindings => "more namespaces than the table allows",
            ErrorKind::Unsupported => "this parser refuses that construct",
        };
        write!(f, "{what} at byte {}", self.at)
    }
}

/// ASCII name start or any byte >= 0x80.
#[inline]
pub fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':' || b >= 0x80
}

#[inline]
pub fn is_name(b: u8) -> bool {
    is_name_start(b) || b.is_ascii_digit() || b == b'-' || b == b'.'
}

/// XML whitespace, narrower than `is_ascii_whitespace`.
#[inline]
pub fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}
