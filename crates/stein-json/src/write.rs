//! Writing JSON strings into a caller's buffer.
//!
//! A string is scanned 32 bytes at a time for anything that needs an
//! escape. A block with nothing to escape is one copy. A block with
//! escapes is copied whole first, then each escape is written over its
//! byte and the bytes after it are copied again, shifted, so every copy is
//! a fixed 32 bytes rather than a run of some length.

use fearless_simd::{Level, Simd, SimdBase, dispatch, u8x32};

/// The output buffer is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// Write `s` as a quoted JSON string at `out[at..]`, returning the new end.
/// Quotes, backslashes and control bytes are escaped, everything else
/// goes as it is.
pub fn string(out: &mut [u8], at: usize, s: &str) -> Result<usize, Full> {
    let n = put(out, at, b"\"")?;
    let n = dispatch!(Level::new(), simd => body(simd, out, n, s.as_bytes()))?;
    put(out, n, b"\"")
}

/// The contents between the quotes.
#[inline(always)]
fn body<S: Simd>(simd: S, out: &mut [u8], mut n: usize, bytes: &[u8]) -> Result<usize, Full> {
    let quote = simd.splat_u8x32(b'"');
    let slash = simd.splat_u8x32(b'\\');
    let space = simd.splat_u8x32(0x20);
    let (chunks, rest) = bytes.as_chunks::<32>();
    for (k, c) in chunks.iter().enumerate() {
        let v = u8x32::load_array_ref(simd, c);
        let hit = simd.simd_eq_u8x32(v, quote)
            | simd.simd_eq_u8x32(v, slash)
            | simd.simd_lt_u8x32(v, space);
        let m = simd.to_bitmask_mask8x32(hit) as u32;
        n = block(out, n, bytes, k * 32, m)?;
    }
    let from = chunks.len() * 32;
    let mut plain = from;
    for (j, &b) in rest.iter().enumerate() {
        if b == b'"' || b == b'\\' || b < 0x20 {
            let i = from + j;
            n = put(out, n, &bytes[plain..i])?;
            n = escape(out, n, b)?;
            plain = i + 1;
        }
    }
    put(out, n, &bytes[plain..])
}

/// The block of `bytes` at `at` to `out[n..]`, `m` its escape mask.
#[inline(always)]
fn block(out: &mut [u8], mut n: usize, bytes: &[u8], at: usize, mut m: u32) -> Result<usize, Full> {
    n = copy32(out, n, bytes, at)?;
    if m == 0 {
        return Ok(n + 32);
    }
    let mut from = 0;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        n += i - from;
        n = escape(out, n, bytes[at + i])?;
        from = i + 1;
        n = copy32(out, n, bytes, at + from)?;
        m &= m - 1;
    }
    Ok(n + 32 - from)
}

/// Thirty two bytes of `bytes` from `at` to `out[n..]`, or what is left of
/// `bytes` when fewer remain. `n` unchanged, the caller knows how many of
/// them it means to keep.
#[inline(always)]
fn copy32(out: &mut [u8], n: usize, bytes: &[u8], at: usize) -> Result<usize, Full> {
    match (bytes.get(at..at + 32), out.get_mut(n..n + 32)) {
        (Some(src), Some(dst)) => dst.copy_from_slice(src),
        _ => {
            let src = bytes.get(at..).unwrap_or(&[]);
            let src = &src[..src.len().min(32)];
            out.get_mut(n..n + src.len())
                .ok_or(Full)?
                .copy_from_slice(src);
        }
    }
    Ok(n)
}

/// The escape for a byte that needs one.
#[inline]
fn escape(out: &mut [u8], at: usize, b: u8) -> Result<usize, Full> {
    let e: &[u8] = match b {
        b'"' => b"\\\"",
        b'\\' => b"\\\\",
        b'\n' => b"\\n",
        b'\r' => b"\\r",
        b'\t' => b"\\t",
        _ => return control(out, at, b),
    };
    put(out, at, e)
}

/// `\u00XX` for a control byte.
fn control(out: &mut [u8], at: usize, b: u8) -> Result<usize, Full> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let e = [
        b'\\',
        b'u',
        b'0',
        b'0',
        HEX[(b >> 4) as usize],
        HEX[(b & 0xf) as usize],
    ];
    put(out, at, &e)
}

/// Copy raw bytes, punctuation or a number, at `out[at..]`.
#[inline]
pub fn put(out: &mut [u8], at: usize, b: &[u8]) -> Result<usize, Full> {
    let dst = out.get_mut(at..at + b.len()).ok_or(Full)?;
    dst.copy_from_slice(b);
    Ok(at + b.len())
}

/// An array of strings, `["a","b"]`.
pub fn string_array(out: &mut [u8], at: usize, items: &[&str]) -> Result<usize, Full> {
    let mut n = put(out, at, b"[")?;
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            n = put(out, n, b",")?;
        }
        n = string(out, n, s)?;
    }
    put(out, n, b"]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec;

    fn w(s: &str) -> String {
        let mut out = [0u8; 64];
        let n = string(&mut out, 0, s).unwrap();
        String::from_utf8(out[..n].to_vec()).unwrap()
    }

    #[test]
    fn strings_escape_what_they_must() {
        assert_eq!(w("plain é"), "\"plain é\"");
        assert_eq!(w("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        assert_eq!(w("\u{1}"), r#""\u0001""#);
    }

    #[test]
    fn written_strings_parse_back() {
        let s = "q\"u\\o\tte\u{7}";
        let mut out = [0u8; 64];
        let n = string(&mut out, 0, s).unwrap();
        let mut nodes = [crate::Node::EMPTY; 1];
        let doc = crate::parse(&out[..n], &mut nodes).unwrap();
        assert!(doc.text(0).unwrap().eq_str(s));
    }

    /// Escapes on every lane offset of long strings, against a byte loop,
    /// into a buffer exactly as large as needed and one byte short.
    #[test]
    fn long_strings_match_the_byte_form() {
        for len in [0usize, 1, 31, 32, 33, 63, 64, 65, 100, 1000] {
            for at in (0..len.min(70)).chain(len.saturating_sub(3)..len) {
                for bad in ['"', '\\', '\n', '\u{1}', '\u{1f}', 'é'] {
                    let mut s: String = "x".repeat(len);
                    s.replace_range(at..at + 1, &String::from(bad));
                    let want = byte_form(&s);
                    let mut out = vec![0u8; want.len()];
                    let n = string(&mut out, 0, &s).unwrap();
                    assert_eq!(&out[..n], want.as_bytes(), "{len} {at} {bad:?}");
                    assert_eq!(string(&mut out[..n - 1], 0, &s), Err(Full));
                }
            }
        }
    }

    /// Escapes packed as densely as they come, at every offset.
    #[test]
    fn dense_escapes_match_the_byte_form() {
        for start in 0..40 {
            let s: String = (0..90)
                .map(|i| if (i + start) % 3 == 0 { '"' } else { 'a' })
                .collect();
            let want = byte_form(&s);
            let mut out = vec![0u8; want.len() + 40];
            let n = string(&mut out, 7, &s).unwrap();
            assert_eq!(&out[7..n], want.as_bytes(), "{start}");
        }
    }

    fn byte_form(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    o.push_str(&std::format!("\\u{:04x}", c as u32));
                }
                c => o.push(c),
            }
        }
        o.push('"');
        o
    }

    #[test]
    fn arrays_and_full_buffers() {
        let mut out = [0u8; 32];
        let n = string_array(&mut out, 0, &["workspace", "output"]).unwrap();
        assert_eq!(&out[..n], br#"["workspace","output"]"#);
        assert_eq!(string_array(&mut out[..5], 0, &["long"]), Err(Full));
    }
}
