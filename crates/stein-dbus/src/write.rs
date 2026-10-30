//! Marshalling into a caller's buffer, little endian.
//!
//! Offsets are relative to the start of the buffer, which is the start of
//! the message, since that is what alignment is measured from.

use crate::error::WireError;
use crate::names;
use crate::sig;

/// The spec's cap on one array's bytes.
pub const MAX_ARRAY: usize = 1 << 26;

pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

/// An array being written, closed with [`Writer::end_array`].
#[must_use]
#[derive(Clone, Copy, Debug)]
pub struct Array {
    len_at: usize,
    start: usize,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    /// Everything written so far.
    pub fn written(&self) -> &[u8] {
        &self.buf[..self.pos]
    }

    pub fn pad(&mut self, align: usize) -> Result<(), WireError> {
        let to = self.pos.next_multiple_of(align);
        let dst = self.buf.get_mut(self.pos..to).ok_or(WireError::Full)?;
        dst.fill(0);
        self.pos = to;
        Ok(())
    }

    fn put(&mut self, b: &[u8]) -> Result<(), WireError> {
        let end = self.pos + b.len();
        let dst = self.buf.get_mut(self.pos..end).ok_or(WireError::Full)?;
        dst.copy_from_slice(b);
        self.pos = end;
        Ok(())
    }

    fn aligned(&mut self, b: &[u8]) -> Result<(), WireError> {
        self.pad(b.len())?;
        self.put(b)
    }

    pub fn u8(&mut self, v: u8) -> Result<(), WireError> {
        self.put(&[v])
    }

    pub fn bool(&mut self, v: bool) -> Result<(), WireError> {
        self.u32(u32::from(v))
    }

    pub fn i16(&mut self, v: i16) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn u16(&mut self, v: u16) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn i32(&mut self, v: i32) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn u32(&mut self, v: u32) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn i64(&mut self, v: i64) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn u64(&mut self, v: u64) -> Result<(), WireError> {
        self.aligned(&v.to_le_bytes())
    }

    pub fn f64(&mut self, v: f64) -> Result<(), WireError> {
        self.aligned(&v.to_bits().to_le_bytes())
    }

    /// A unix fd, as its index in the message's descriptor list.
    pub fn fd(&mut self, index: u32) -> Result<(), WireError> {
        self.u32(index)
    }

    pub fn str(&mut self, s: &str) -> Result<(), WireError> {
        if s.as_bytes().contains(&0) {
            return Err(WireError::Str);
        }
        let len = u32::try_from(s.len()).map_err(|_| WireError::Str)?;
        self.u32(len)?;
        self.put(s.as_bytes())?;
        self.u8(0)
    }

    pub fn path(&mut self, p: &str) -> Result<(), WireError> {
        if !names::is_path(p.as_bytes()) {
            return Err(WireError::Path);
        }
        self.str(p)
    }

    /// A signature of any number of complete types.
    pub fn sig(&mut self, s: &str) -> Result<(), WireError> {
        sig::check(s.as_bytes())?;
        self.u8(s.len() as u8)?;
        self.put(s.as_bytes())?;
        self.u8(0)
    }

    /// Open an array of `elem`, its element type's first code.
    ///
    /// The padding to the element's alignment is written even when no
    /// element follows, which the spec requires.
    pub fn begin_array(&mut self, elem: u8) -> Result<Array, WireError> {
        self.pad(4)?;
        let len_at = self.pos;
        self.put(&[0; 4])?;
        self.pad(sig::align(elem))?;
        Ok(Array {
            len_at,
            start: self.pos,
        })
    }

    /// Close an array, filling in its length.
    pub fn end_array(&mut self, a: Array) -> Result<(), WireError> {
        let len = self.pos - a.start;
        if len > MAX_ARRAY {
            return Err(WireError::ArrayTooLong(len as u32));
        }
        self.buf[a.len_at..a.len_at + 4].copy_from_slice(&(len as u32).to_le_bytes());
        Ok(())
    }

    /// Start a struct or dict entry. Nothing closes one, the fields follow.
    pub fn begin_struct(&mut self) -> Result<(), WireError> {
        self.pad(8)
    }

    /// Start a variant holding one value of `sig`, which follows.
    pub fn variant(&mut self, s: &str) -> Result<(), WireError> {
        sig::check_single(s.as_bytes())?;
        self.sig(s)
    }

    /// One `{sv}` dict entry: `key`, then a variant of `sig` that `value`
    /// writes.
    pub fn sv(
        &mut self,
        key: &str,
        sig: &str,
        value: impl FnOnce(&mut Self) -> Result<(), WireError>,
    ) -> Result<(), WireError> {
        self.begin_struct()?;
        self.str(key)?;
        self.variant(sig)?;
        value(self)
    }

    /// Overwrite a u32 already written, for lengths known only later.
    pub(crate) fn patch_u32(&mut self, at: usize, v: u32) {
        self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}
