//! Type signatures: what a body holds, one code per type.
//!
//! Checked once before anything is read or written against them, so the
//! walkers can trust a signature's shape and only the bytes are untrusted.

use core::fmt;

/// Longest signature the wire can carry, its length is one byte.
pub const MAX_SIG: usize = 255;
/// Arrays inside arrays.
pub const MAX_ARRAYS: u32 = 32;
/// Structs and dict entries inside each other.
pub const MAX_STRUCTS: u32 = 32;
/// Containers of any kind, variants included once values are read.
pub const MAX_DEPTH: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigError {
    TooLong(usize),
    /// A code that names no type.
    Unknown(u8),
    /// Ended inside a container, or an array with no element type.
    Truncated,
    /// `()`.
    EmptyStruct,
    /// A dict entry without a basic key and exactly one value.
    BadDictEntry,
    /// A dict entry that is not an array's element.
    LooseDictEntry,
    TooDeep,
    /// A variant's signature holding other than one complete type.
    NotSingle,
}

impl fmt::Display for SigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(n) => write!(f, "signature of {n} bytes, the limit is {MAX_SIG}"),
            Self::Unknown(c) => write!(f, "signature code {c:#04x} names no type"),
            Self::Truncated => write!(f, "signature ends inside a type"),
            Self::EmptyStruct => write!(f, "signature has an empty struct"),
            Self::BadDictEntry => write!(f, "dict entry needs a basic key and one value"),
            Self::LooseDictEntry => write!(f, "dict entry outside an array"),
            Self::TooDeep => write!(f, "signature nests too deep"),
            Self::NotSingle => write!(f, "variant signature is not one complete type"),
        }
    }
}

/// Fixed size and string types, the ones a dict key may be.
pub const fn is_basic(c: u8) -> bool {
    matches!(
        c,
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b'h' | b's' | b'o' | b'g'
    )
}

/// Where a value of type `c` starts, relative to the message.
pub const fn align(c: u8) -> usize {
    match c {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// Check a whole signature, any number of complete types.
pub fn check(sig: &[u8]) -> Result<(), SigError> {
    if sig.len() > MAX_SIG {
        return Err(SigError::TooLong(sig.len()));
    }
    let mut at = 0;
    while at < sig.len() {
        at = complete(sig, at, 0, 0, false)?;
    }
    Ok(())
}

/// Check a variant's signature, exactly one complete type.
pub fn check_single(sig: &[u8]) -> Result<(), SigError> {
    if sig.len() > MAX_SIG {
        return Err(SigError::TooLong(sig.len()));
    }
    if sig.is_empty() {
        return Err(SigError::NotSingle);
    }
    match complete(sig, 0, 0, 0, false)? {
        end if end == sig.len() => Ok(()),
        _ => Err(SigError::NotSingle),
    }
}

/// Length of the complete type at the front of `sig`.
pub fn first_len(sig: &[u8]) -> Result<usize, SigError> {
    complete(sig, 0, 0, 0, false)
}

/// Length of an array's element type at the front of `sig`, which may be
/// a dict entry.
pub fn element_len(sig: &[u8]) -> Result<usize, SigError> {
    complete(sig, 0, 0, 0, true)
}

/// The end of the complete type starting at `at`.
fn complete(
    sig: &[u8],
    at: usize,
    arrays: u32,
    structs: u32,
    element: bool,
) -> Result<usize, SigError> {
    let c = *sig.get(at).ok_or(SigError::Truncated)?;
    if is_basic(c) || c == b'v' {
        return Ok(at + 1);
    }
    let deep = |a: u32, s: u32| a > MAX_ARRAYS || s > MAX_STRUCTS || a + s > MAX_DEPTH;
    match c {
        b'a' => {
            if deep(arrays + 1, structs) {
                return Err(SigError::TooDeep);
            }
            complete(sig, at + 1, arrays + 1, structs, true)
        }
        b'(' => {
            if deep(arrays, structs + 1) {
                return Err(SigError::TooDeep);
            }
            let mut p = at + 1;
            if sig.get(p) == Some(&b')') {
                return Err(SigError::EmptyStruct);
            }
            while sig.get(p) != Some(&b')') {
                p = complete(sig, p, arrays, structs + 1, false)?;
            }
            Ok(p + 1)
        }
        b'{' => dict_entry(sig, at, arrays, structs, element),
        other => Err(SigError::Unknown(other)),
    }
}

fn dict_entry(
    sig: &[u8],
    at: usize,
    arrays: u32,
    structs: u32,
    element: bool,
) -> Result<usize, SigError> {
    if !element {
        return Err(SigError::LooseDictEntry);
    }
    if arrays > MAX_ARRAYS || structs + 1 > MAX_STRUCTS || arrays + structs + 1 > MAX_DEPTH {
        return Err(SigError::TooDeep);
    }
    let key = *sig.get(at + 1).ok_or(SigError::Truncated)?;
    if !is_basic(key) || sig.get(at + 2) == Some(&b'}') {
        return Err(SigError::BadDictEntry);
    }
    let end = complete(sig, at + 2, arrays, structs + 1, false)?;
    match sig.get(end) {
        Some(b'}') => Ok(end + 1),
        Some(_) => Err(SigError::BadDictEntry),
        None => Err(SigError::Truncated),
    }
}

/// The complete types of a checked signature, one slice each.
pub struct Types<'a> {
    sig: &'a [u8],
}

impl<'a> Types<'a> {
    pub fn new(sig: &'a [u8]) -> Self {
        Self { sig }
    }
}

impl<'a> Iterator for Types<'a> {
    type Item = Result<&'a [u8], SigError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.sig.is_empty() {
            return None;
        }
        match first_len(self.sig) {
            Ok(n) => {
                let (t, rest) = self.sig.split_at(n);
                self.sig = rest;
                Some(Ok(t))
            }
            Err(e) => {
                self.sig = &[];
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests;
