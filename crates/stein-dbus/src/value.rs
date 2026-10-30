//! Variants and `a{sv}` dictionaries, the shape every property comes in.
//!
//! A [`Value`] is a variant's signature and a reader parked at its value,
//! so a caller asks for the type it expects and gets `None` for any other.
//! [`Dict`] walks an `a{sv}` handing out key and value pairs, stepping
//! over each value whether or not it was read.

use crate::error::WireError;
use crate::read::{self, ArrayEnd, Reader};

/// One variant's value, not yet read.
#[derive(Clone, Copy, Debug)]
pub struct Value<'a> {
    pub sig: &'a str,
    pub r: Reader<'a>,
}

impl<'a> Value<'a> {
    /// Read a variant at `r`, leaving `r` past it.
    pub fn read(r: &mut Reader<'a>) -> Result<Value<'a>, WireError> {
        let sig = r.variant()?;
        let at = *r;
        read::skip(r, sig.as_bytes())?;
        Ok(Value { sig, r: at })
    }

    fn is(&self, s: &str) -> Option<Reader<'a>> {
        (self.sig == s).then_some(self.r)
    }

    pub fn str(&self) -> Option<&'a str> {
        self.is("s")?.str().ok()
    }

    pub fn path(&self) -> Option<&'a str> {
        self.is("o")?.path().ok()
    }

    pub fn bool(&self) -> Option<bool> {
        self.is("b")?.bool().ok()
    }

    pub fn i32(&self) -> Option<i32> {
        self.is("i")?.i32().ok()
    }

    pub fn u32(&self) -> Option<u32> {
        self.is("u")?.u32().ok()
    }

    pub fn i64(&self) -> Option<i64> {
        self.is("x")?.i64().ok()
    }

    pub fn f64(&self) -> Option<f64> {
        self.is("d")?.f64().ok()
    }

    /// A variant inside this one, as `v` wraps anything.
    pub fn inner(&self) -> Option<Value<'a>> {
        let mut r = self.is("v")?;
        Value::read(&mut r).ok()
    }

    /// The strings of an `as`.
    pub fn strs(&self) -> Option<Strs<'a>> {
        let mut r = self.is("as")?;
        let end = r.array(b's').ok()?;
        Some(Strs { r, end })
    }

    /// The entries of an `a{sv}`.
    pub fn dict(&self) -> Option<Dict<'a>> {
        let mut r = self.is("a{sv}")?;
        Dict::read(&mut r).ok()
    }
}

/// The strings of an `as`, in order.
#[derive(Clone, Copy, Debug)]
pub struct Strs<'a> {
    r: Reader<'a>,
    end: ArrayEnd,
}

impl<'a> Strs<'a> {
    /// Open the `as` at `r`, leaving `r` past all of it.
    pub fn read(r: &mut Reader<'a>) -> Result<Strs<'a>, WireError> {
        let end = r.array(b's')?;
        let at = *r;
        r.skip_to(end);
        Ok(Strs { r: at, end })
    }
}

impl<'a> Iterator for Strs<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if !self.r.more(self.end) {
            return None;
        }
        self.r.str().ok()
    }
}

/// The entries of an `a{sv}`, in order. Stops at the first malformed one.
#[derive(Clone, Copy, Debug)]
pub struct Dict<'a> {
    r: Reader<'a>,
    end: ArrayEnd,
}

impl<'a> Dict<'a> {
    /// Open the `a{sv}` at `r`, leaving `r` past all of it.
    pub fn read(r: &mut Reader<'a>) -> Result<Dict<'a>, WireError> {
        let end = r.array(b'{')?;
        let at = *r;
        r.skip_to(end);
        Ok(Dict { r: at, end })
    }

    /// The value under `key`.
    pub fn get(self, key: &str) -> Option<Value<'a>> {
        self.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

impl<'a> Iterator for Dict<'a> {
    type Item = (&'a str, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if !self.r.more(self.end) {
            return None;
        }
        let entry = (|| {
            self.r.begin_struct()?;
            let k = self.r.str()?;
            let v = Value::read(&mut self.r)?;
            Ok::<_, WireError>((k, v))
        })();
        match entry {
            Ok(e) => Some(e),
            Err(_) => {
                self.r.skip_to(self.end);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
