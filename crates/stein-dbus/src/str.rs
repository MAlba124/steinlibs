//! Strings held inline, so tables of them need no allocator.

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Str<const N: usize> {
    buf: [u8; N],
    len: u16,
}

impl<const N: usize> Str<N> {
    pub const EMPTY: Self = Self {
        buf: [0; N],
        len: 0,
    };

    /// `s` whole, or `None` when it does not fit.
    pub fn new(s: &str) -> Option<Self> {
        let mut out = Self::EMPTY;
        out.set(s).then_some(out)
    }

    /// Replace with `s` whole. False, and unchanged, when it does not fit.
    pub fn set(&mut self, s: &str) -> bool {
        let Some(dst) = self.buf.get_mut(..s.len()) else {
            return false;
        };
        dst.copy_from_slice(s.as_bytes());
        self.len = s.len() as u16;
        true
    }

    /// Replace with as much of `s` as fits, cut at a char boundary.
    pub fn set_cut(&mut self, s: &str) {
        let mut n = s.len().min(N);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.set(&s[..n]);
    }

    /// Add one char, if it fits.
    pub fn push(&mut self, c: char) -> bool {
        let at = usize::from(self.len);
        let Some(dst) = self.buf.get_mut(at..at + c.len_utf8()) else {
            return false;
        };
        c.encode_utf8(dst);
        self.len += c.len_utf8() as u16;
        true
    }

    /// Add `s` whole, if it fits.
    pub fn push_str(&mut self, s: &str) -> bool {
        let at = usize::from(self.len);
        let Some(dst) = self.buf.get_mut(at..at + s.len()) else {
            return false;
        };
        dst.copy_from_slice(s.as_bytes());
        self.len += s.len() as u16;
        true
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for Str<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> fmt::Debug for Str<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_or_refuses() {
        let s: Str<4> = Str::new("abcd").unwrap();
        assert_eq!(s.as_str(), "abcd");
        assert!(Str::<4>::new("abcde").is_none());
        let mut c: Str<4> = Str::EMPTY;
        c.set_cut("abcé");
        assert_eq!(c.as_str(), "abc", "é would straddle the end");
        assert!(!c.push('é'));
        assert!(c.push('x'));
        assert_eq!(c.as_str(), "abcx");
    }
}
