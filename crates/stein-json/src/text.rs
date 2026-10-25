//! String contents: escapes checked at parse time, decoded on demand.

/// A string value as written between its quotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text<'a> {
    raw: &'a str,
    escaped: bool,
}

/// The unescaped text does not fit the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong;

impl Text<'static> {
    pub const EMPTY: Text<'static> = Text {
        raw: "",
        escaped: false,
    };
}

impl<'a> Text<'a> {
    pub(crate) fn new(raw: &'a str, escaped: bool) -> Self {
        Self { raw, escaped }
    }

    /// The text itself when it has no escapes, which is most of it.
    pub fn as_str(&self) -> Option<&'a str> {
        (!self.escaped).then_some(self.raw)
    }

    /// As written, escapes and all.
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    pub fn is_escaped(&self) -> bool {
        self.escaped
    }

    /// The decoded text, borrowed when there is nothing to decode and
    /// written into `buf` otherwise.
    pub fn get<'b>(&self, buf: &'b mut [u8]) -> Result<&'b str, TooLong>
    where
        'a: 'b,
    {
        if !self.escaped {
            return Ok(self.raw);
        }
        let mut n = 0;
        for c in self.chars() {
            let dst = buf.get_mut(n..n + c.len_utf8()).ok_or(TooLong)?;
            c.encode_utf8(dst);
            n += c.len_utf8();
        }
        // Whole chars were written, so this is UTF-8.
        core::str::from_utf8(&buf[..n]).map_err(|_| TooLong)
    }

    /// Compare the decoded text with `s`, without a buffer.
    pub fn eq_str(&self, s: &str) -> bool {
        if !self.escaped {
            return self.raw == s;
        }
        self.chars().eq(s.chars())
    }

    /// The decoded characters.
    pub fn chars(&self) -> Chars<'a> {
        Chars {
            rest: self.raw.as_bytes(),
        }
    }
}

/// Decoded characters of a checked string.
pub struct Chars<'a> {
    rest: &'a [u8],
}

impl Iterator for Chars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        let (c, n) = match self.rest.first()? {
            b'\\' => decode_escape(self.rest)?,
            _ => {
                // Checked UTF-8, so the first char of the rest is whole.
                let s = core::str::from_utf8(&self.rest[..utf8_len(self.rest[0])]).ok()?;
                let c = s.chars().next()?;
                (c, c.len_utf8())
            }
        };
        self.rest = &self.rest[n..];
        Some(c)
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[inline]
fn hex4(s: &[u8]) -> Option<u32> {
    let s = s.get(..4)?;
    s.iter().try_fold(0u32, |v, &c| {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        Some(v << 4 | u32::from(d))
    })
}

/// Decode the escape at the front of `s` (which starts with a backslash):
/// the char and how many bytes it took. `None` for anything malformed.
#[inline]
fn decode_escape(s: &[u8]) -> Option<(char, usize)> {
    let c = match *s.get(1)? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => return unicode(s),
        _ => return None,
    };
    Some((c, 2))
}

/// `\uXXXX`, or a surrogate pair `\uD8xx\uDCxx`.
fn unicode(s: &[u8]) -> Option<(char, usize)> {
    let hi = hex4(s.get(2..)?)?;
    match hi {
        0xd800..=0xdbff => {
            if s.get(6..8)? != b"\\u" {
                return None;
            }
            let lo = hex4(s.get(8..)?)?;
            if !(0xdc00..=0xdfff).contains(&lo) {
                return None;
            }
            let c = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
            Some((char::from_u32(c)?, 12))
        }
        // A low surrogate on its own is no character.
        0xdc00..=0xdfff => None,
        _ => Some((char::from_u32(hi)?, 6)),
    }
}

/// The length of the escape at the front of `s`, if it is well formed.
#[inline]
pub(crate) fn check_escape(s: &[u8]) -> Option<usize> {
    decode_escape(s).map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(raw: &str) -> Text<'_> {
        Text::new(raw, raw.contains('\\'))
    }

    #[test]
    fn plain_text_borrows() {
        let x = t("DP-3");
        assert_eq!(x.as_str(), Some("DP-3"));
        let mut buf = [0u8; 0];
        assert_eq!(x.get(&mut buf), Ok("DP-3"));
    }

    #[test]
    fn escapes_decode() {
        let x = t(r#"a\/b \"q\" \\ \n\t \u00e9 \ud83d\ude00"#);
        let mut buf = [0u8; 64];
        assert_eq!(x.get(&mut buf), Ok("a/b \"q\" \\ \n\t é 😀"));
        assert!(x.eq_str("a/b \"q\" \\ \n\t é 😀"));
        assert!(!x.eq_str("a/b"));
        assert_eq!(x.as_str(), None);
    }

    #[test]
    fn a_small_buffer_is_refused() {
        let mut buf = [0u8; 3];
        assert_eq!(t(r"a\/bcd").get(&mut buf), Err(TooLong));
    }

    #[test]
    fn bad_escapes_are_caught() {
        assert_eq!(check_escape(br"\x"), None);
        assert_eq!(check_escape(br"\u12"), None);
        assert_eq!(check_escape(br"\u12g4"), None);
        assert_eq!(check_escape(br"\ud800"), None, "high surrogate alone");
        assert_eq!(check_escape(br"\ud800\u0041"), None, "high then not low");
        assert_eq!(check_escape(br"\udc00"), None, "low surrogate alone");
        assert_eq!(check_escape(br"\ud83d\ude00"), Some(12));
        assert_eq!(check_escape(br"\u0041"), Some(6));
        assert_eq!(check_escape(br"\n"), Some(2));
    }
}
