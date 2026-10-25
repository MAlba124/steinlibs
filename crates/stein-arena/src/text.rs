//! UTF-8 text on its own reservation, validated once on commit.

use core::str::Utf8Error;

use crate::{ArenaError, Grow};

pub struct Text {
    bytes: Grow<u8>,
    /// `bytes[..valid]` is UTF-8. Raised only by [`Text::commit`], lowered only
    /// by [`Text::truncate`] to a char boundary.
    valid: usize,
}

impl Text {
    /// Room for `cap` bytes, reserved and not yet committed.
    pub fn reserve(cap: usize) -> Result<Text, ArenaError> {
        Ok(Text {
            bytes: Grow::reserve(cap)?,
            valid: 0,
        })
    }

    /// Bytes of checked text.
    pub fn len(&self) -> usize {
        self.valid
    }

    pub fn is_empty(&self) -> bool {
        self.valid == 0
    }

    /// Bytes it can ever hold, pending included.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// The checked text.
    pub fn as_str(&self) -> &str {
        let b = self.bytes.as_slice();
        let b = b.get(..self.valid).unwrap_or(b"");
        // SAFETY: `bytes[..valid]` passed `from_utf8` in `commit`. Appends land
        // past `len` and truncate lowers `valid` to a char boundary first.
        unsafe { core::str::from_utf8_unchecked(b) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// Append to the pending part, unchecked until [`Text::commit`].
    pub fn push(&mut self, b: &[u8]) -> Result<(), ArenaError> {
        self.bytes.extend_from_slice(b)
    }

    pub fn pending(&self) -> &[u8] {
        self.bytes.as_slice().get(self.valid..).unwrap_or(b"")
    }

    /// Validate the pending bytes as text. On error they stay pending.
    pub fn commit(&mut self) -> Result<(), Utf8Error> {
        core::str::from_utf8(self.pending())?;
        self.valid = self.bytes.len();
        Ok(())
    }

    /// Drop the pending bytes.
    pub fn discard(&mut self) {
        self.bytes.truncate(self.valid);
    }

    /// Keep the first `n` bytes of text, dropping pending. False if `n` is not
    /// a char boundary.
    pub fn truncate(&mut self, n: usize) -> bool {
        if n > self.valid {
            self.discard();
            return true;
        }
        if !self.as_str().is_char_boundary(n) {
            return false;
        }
        self.valid = n;
        self.bytes.truncate(n);
        true
    }
}

impl core::fmt::Debug for Text {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Text")
            .field("len", &self.valid)
            .field("pending", &self.pending().len())
            .finish()
    }
}

#[cfg(test)]
mod tests;
