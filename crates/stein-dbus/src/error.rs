//! What can go wrong reading or writing the wire format.

use core::fmt;

use crate::sig::SigError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The output buffer is full.
    Full,
    /// Input ended inside a value.
    Truncated,
    /// Padding that is not zero, which the spec forbids.
    Padding,
    /// A boolean other than 0 or 1.
    Bool(u32),
    /// A string that is not UTF-8, holds a NUL, or lacks its terminator.
    Str,
    Path,
    Sig(SigError),
    /// An array over 64 MiB.
    ArrayTooLong(u32),
    /// An element ran past the array's stated length.
    ArrayOverrun,
    /// Values nest past [`crate::sig::MAX_DEPTH`], variants included.
    TooDeep,
    /// Bytes left over after the signature's last type.
    Trailing,
    /// An endianness flag other than `l` or `B`.
    Endian(u8),
    Version(u8),
    /// A message over 128 MiB.
    TooLarge(usize),
    /// A message type outside 1 to 4.
    Kind(u8),
    /// Serial zero.
    Serial,
    /// A header field of the wrong type, by its code.
    Field(u8),
    /// A header field the message type requires is missing, by its code.
    Missing(u8),
    /// A bus, interface, member or error name that breaks the rules.
    Name,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "output buffer full"),
            Self::Truncated => write!(f, "input ends inside a value"),
            Self::Padding => write!(f, "padding is not zero"),
            Self::Bool(v) => write!(f, "boolean {v} is not 0 or 1"),
            Self::Str => write!(f, "string is not terminated UTF-8 without NUL"),
            Self::Path => write!(f, "bad object path"),
            Self::Sig(e) => write!(f, "{e}"),
            Self::ArrayTooLong(n) => write!(f, "array of {n} bytes over the limit"),
            Self::ArrayOverrun => write!(f, "array element past the array's end"),
            Self::TooDeep => write!(f, "values nest too deep"),
            Self::Trailing => write!(f, "bytes after the last value"),
            Self::Endian(c) => write!(f, "endianness flag {c:#04x}"),
            Self::Version(v) => write!(f, "protocol version {v}"),
            Self::TooLarge(n) => write!(f, "message of {n} bytes over the limit"),
            Self::Kind(k) => write!(f, "message type {k}"),
            Self::Serial => write!(f, "serial zero"),
            Self::Field(c) => write!(f, "header field {c} has the wrong type"),
            Self::Missing(c) => write!(f, "header field {c} is required"),
            Self::Name => write!(f, "bad bus, interface, member or error name"),
        }
    }
}

impl From<SigError> for WireError {
    fn from(e: SigError) -> Self {
        Self::Sig(e)
    }
}
