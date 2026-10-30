//! Unmarshalling in place. Strings come back as borrows of the input.
//!
//! Every read checks bounds, padding and encoding, so a reader over bytes
//! from anywhere fails with a [`WireError`] rather than reading past them.

use crate::error::WireError;
use crate::names;
use crate::sig::{self, MAX_DEPTH};
use crate::write::MAX_ARRAY;

#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    big: bool,
}

/// Where an array ends, from [`Reader::array`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayEnd(usize);

impl<'a> Reader<'a> {
    /// `buf` starts at an 8 aligned offset of its message, as a body does.
    pub fn new(buf: &'a [u8], big: bool) -> Self {
        Self { buf, pos: 0, big }
    }

    pub(crate) fn at(buf: &'a [u8], pos: usize, big: bool) -> Self {
        Self { buf, pos, big }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn align(&mut self, a: usize) -> Result<(), WireError> {
        let to = self.pos.next_multiple_of(a);
        let pad = self.buf.get(self.pos..to).ok_or(WireError::Truncated)?;
        if pad.iter().any(|&b| b != 0) {
            return Err(WireError::Padding);
        }
        self.pos = to;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Truncated)?;
        let b = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        Ok(b)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        self.align(N)?;
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        if self.big {
            out.reverse();
        }
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, WireError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(WireError::Bool(v)),
        }
    }

    pub fn i16(&mut self) -> Result<i16, WireError> {
        self.fixed().map(i16::from_le_bytes)
    }

    pub fn u16(&mut self) -> Result<u16, WireError> {
        self.fixed().map(u16::from_le_bytes)
    }

    pub fn i32(&mut self) -> Result<i32, WireError> {
        self.fixed().map(i32::from_le_bytes)
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        self.fixed().map(u32::from_le_bytes)
    }

    pub fn i64(&mut self) -> Result<i64, WireError> {
        self.fixed().map(i64::from_le_bytes)
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        self.fixed().map(u64::from_le_bytes)
    }

    pub fn f64(&mut self) -> Result<f64, WireError> {
        self.u64().map(f64::from_bits)
    }

    pub fn fd(&mut self) -> Result<u32, WireError> {
        self.u32()
    }

    /// A string and the offset its bytes start at.
    pub(crate) fn str_at(&mut self) -> Result<(usize, &'a str), WireError> {
        let len = self.u32()? as usize;
        let at = self.pos;
        let s = self.take(len)?;
        let s = terminated(self.take(1)?, s)?;
        Ok((at, s))
    }

    pub fn str(&mut self) -> Result<&'a str, WireError> {
        self.str_at().map(|(_, s)| s)
    }

    pub fn path(&mut self) -> Result<&'a str, WireError> {
        let p = self.str()?;
        if !names::is_path(p.as_bytes()) {
            return Err(WireError::Path);
        }
        Ok(p)
    }

    pub(crate) fn sig_at(&mut self) -> Result<(usize, &'a str), WireError> {
        let len = self.u8()? as usize;
        let at = self.pos;
        let s = self.take(len)?;
        let s = terminated(self.take(1)?, s)?;
        sig::check(s.as_bytes())?;
        Ok((at, s))
    }

    pub fn sig(&mut self) -> Result<&'a str, WireError> {
        self.sig_at().map(|(_, s)| s)
    }

    /// Open an array of `elem`, its element type's first code. Read
    /// elements while [`Reader::more`], then [`Reader::end`] checks the
    /// last one stopped on the boundary.
    pub fn array(&mut self, elem: u8) -> Result<ArrayEnd, WireError> {
        let len = self.u32()?;
        if len as usize > MAX_ARRAY {
            return Err(WireError::ArrayTooLong(len));
        }
        self.align(sig::align(elem))?;
        let end = self.pos + len as usize;
        if end > self.buf.len() {
            return Err(WireError::Truncated);
        }
        Ok(ArrayEnd(end))
    }

    /// An `ay`, borrowed whole.
    pub fn byte_array(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.u32()?;
        if len as usize > MAX_ARRAY {
            return Err(WireError::ArrayTooLong(len));
        }
        self.take(len as usize)
    }

    pub fn more(&self, end: ArrayEnd) -> bool {
        self.pos < end.0
    }

    pub fn end(&self, end: ArrayEnd) -> Result<(), WireError> {
        if self.pos == end.0 {
            Ok(())
        } else {
            Err(WireError::ArrayOverrun)
        }
    }

    /// Step over the rest of an array.
    pub fn skip_to(&mut self, end: ArrayEnd) {
        self.pos = end.0;
    }

    /// Start a struct or dict entry.
    pub fn begin_struct(&mut self) -> Result<(), WireError> {
        self.align(8)
    }

    /// A variant's signature, one complete type whose value follows.
    pub fn variant(&mut self) -> Result<&'a str, WireError> {
        let s = self.sig()?;
        sig::check_single(s.as_bytes())?;
        Ok(s)
    }
}

/// The spec's string rules: a NUL after, none inside, UTF-8.
fn terminated<'a>(nul: &[u8], s: &'a [u8]) -> Result<&'a str, WireError> {
    if nul != [0] || s.contains(&0) {
        return Err(WireError::Str);
    }
    core::str::from_utf8(s).map_err(|_| WireError::Str)
}

/// One step of [`walk`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item<'a> {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(&'a str),
    Path(&'a str),
    Sig(&'a str),
    Fd(u32),
    /// An array of `elem`, its element type.
    Array(&'a [u8]),
    ArrayEnd,
    Struct,
    StructEnd,
    Dict,
    DictEnd,
    /// A variant holding a value of this signature.
    Variant(&'a str),
    VariantEnd,
}

/// Read every value of `sig` in order, handing each to `f`. Checks the
/// values end exactly where the input does.
pub fn walk<'a>(
    r: &mut Reader<'a>,
    sig: &'a [u8],
    f: &mut dyn FnMut(Item<'a>),
) -> Result<(), WireError> {
    sig::check(sig)?;
    let mut at = 0;
    while at < sig.len() {
        at = value(r, sig, at, 0, f)?;
    }
    if !r.is_empty() {
        return Err(WireError::Trailing);
    }
    Ok(())
}

/// Step over one value of the complete type `sig`.
pub fn skip<'a>(r: &mut Reader<'a>, sig: &'a [u8]) -> Result<(), WireError> {
    sig::check_single(sig)?;
    value(r, sig, 0, 0, &mut |_| {}).map(|_| ())
}

/// Read one value of the complete type at `sig[at..]`, return where the
/// next type starts. The signature is already checked.
fn value<'a>(
    r: &mut Reader<'a>,
    sig: &'a [u8],
    at: usize,
    depth: u32,
    f: &mut dyn FnMut(Item<'a>),
) -> Result<usize, WireError> {
    let c = *sig
        .get(at)
        .ok_or(WireError::Sig(sig::SigError::Truncated))?;
    if sig::is_basic(c) {
        f(basic(r, c)?);
        return Ok(at + 1);
    }
    if depth >= MAX_DEPTH {
        return Err(WireError::TooDeep);
    }
    match c {
        b'a' => array(r, sig, at, depth, f),
        b'(' | b'{' => {
            r.begin_struct()?;
            let (open, close, end_code) = match c {
                b'(' => (Item::Struct, Item::StructEnd, b')'),
                _ => (Item::Dict, Item::DictEnd, b'}'),
            };
            f(open);
            let mut p = at + 1;
            while sig.get(p) != Some(&end_code) {
                p = value(r, sig, p, depth + 1, f)?;
            }
            f(close);
            Ok(p + 1)
        }
        b'v' => {
            let inner = r.variant()?;
            f(Item::Variant(inner));
            value(r, inner.as_bytes(), 0, depth + 1, f)?;
            f(Item::VariantEnd);
            Ok(at + 1)
        }
        other => Err(WireError::Sig(sig::SigError::Unknown(other))),
    }
}

fn array<'a>(
    r: &mut Reader<'a>,
    sig: &'a [u8],
    at: usize,
    depth: u32,
    f: &mut dyn FnMut(Item<'a>),
) -> Result<usize, WireError> {
    let elem_len = sig::element_len(&sig[at + 1..])?;
    let elem = &sig[at + 1..at + 1 + elem_len];
    let end = r.array(elem[0])?;
    f(Item::Array(elem));
    while r.more(end) {
        value(r, elem, 0, depth + 1, f)?;
    }
    r.end(end)?;
    f(Item::ArrayEnd);
    Ok(at + 1 + elem_len)
}

fn basic<'a>(r: &mut Reader<'a>, c: u8) -> Result<Item<'a>, WireError> {
    Ok(match c {
        b'y' => Item::Byte(r.u8()?),
        b'b' => Item::Bool(r.bool()?),
        b'n' => Item::I16(r.i16()?),
        b'q' => Item::U16(r.u16()?),
        b'i' => Item::I32(r.i32()?),
        b'u' => Item::U32(r.u32()?),
        b'x' => Item::I64(r.i64()?),
        b't' => Item::U64(r.u64()?),
        b'd' => Item::F64(r.f64()?),
        b'h' => Item::Fd(r.fd()?),
        b's' => Item::Str(r.str()?),
        b'o' => Item::Path(r.path()?),
        _ => Item::Sig(r.sig()?),
    })
}

#[cfg(test)]
mod tests;
