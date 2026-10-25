//! Character and entity reference decoding.
//! Accepts a common HTML4 entity subset since real feeds use them. A bare `&` not shaped like a reference passes through as text.

use crate::{ErrorKind, XmlError};

/// Worst-case output bytes per input byte. Every reference is at least as long as its UTF-8 output, so `out.len() >= raw.len()` suffices.
pub const MAX_EXPANSION: usize = 1;

const MAX_CODEPOINT: u32 = 0x10FFFF;

/// Max reference body length between `&` and `;`. Longer runs are a bare `&`.
const MAX_BODY: usize = 16;

/// Whether `raw` contains an `&`. Lets callers keep the borrowed slice.
#[inline]
pub fn needs_decode(raw: &[u8]) -> bool {
    find_amp(raw, 0).is_some()
}

/// Decode references in `raw` into `out`, returning bytes written.
/// `out.len() >= raw.len()` suffices. A short buffer gives [`ErrorKind::BadReference`].
/// Error offsets are relative to `raw`.
pub fn decode(raw: &[u8], out: &mut [u8]) -> Result<usize, XmlError> {
    let mut read = 0;
    let mut wrote = 0;
    // Copy the plain run before each `&`, then its one decoded char.
    while let Some(amp) = find_amp(raw, read) {
        wrote = copy_run(raw, read, amp, out, wrote)?;
        let (ch, next) = reference(raw, amp)?;
        wrote = put_char(ch, out, wrote, amp)?;
        read = next;
    }
    copy_run(raw, read, raw.len(), out, wrote)
}

/// Offset of the next `&` at or after `from`.
#[inline]
fn find_amp(raw: &[u8], from: usize) -> Option<usize> {
    let rest = raw.get(from..)?;
    rest.iter().position(|b| *b == b'&').map(|i| from + i)
}

/// Copy `raw[from..to]` to `out[wrote..]`, returning the new write position.
fn copy_run(
    raw: &[u8],
    from: usize,
    to: usize,
    out: &mut [u8],
    wrote: usize,
) -> Result<usize, XmlError> {
    let src = raw.get(from..to).unwrap_or(&[]);
    let end = wrote + src.len();
    let Some(dst) = out.get_mut(wrote..end) else {
        return Err(XmlError::new(ErrorKind::BadReference, from));
    };
    dst.copy_from_slice(src);
    Ok(end)
}

/// Write one char as UTF-8, returning the new write position.
fn put_char(ch: char, out: &mut [u8], wrote: usize, at: usize) -> Result<usize, XmlError> {
    let end = wrote + ch.len_utf8();
    let Some(dst) = out.get_mut(wrote..end) else {
        return Err(XmlError::new(ErrorKind::BadReference, at));
    };
    ch.encode_utf8(dst);
    Ok(end)
}

#[inline]
fn is_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'#'
}

/// What the `&` at `amp` stands for, and the index past it. A bare `&` consumes only itself.
fn reference(raw: &[u8], amp: usize) -> Result<(char, usize), XmlError> {
    let from = amp + 1;
    let mut end = from;
    while end < raw.len() && end - from < MAX_BODY && is_body(raw[end]) {
        end += 1;
    }
    // Input is a complete text node, so no terminator means a bare `&`.
    if end == raw.len() {
        return Ok(('&', from));
    }
    if end == from || raw.get(end) != Some(&b';') {
        return Ok(('&', from));
    }
    let body = raw.get(from..end).unwrap_or(&[]);
    Ok((resolve(body, amp)?, end + 1))
}

/// Char for a reference body without `&` or `;`.
fn resolve(body: &[u8], at: usize) -> Result<char, XmlError> {
    match body.split_first() {
        Some((b'#', rest)) => numeric(rest, at),
        _ => named(body, at),
    }
}

fn named(body: &[u8], at: usize) -> Result<char, XmlError> {
    let found = NAMED
        .binary_search_by(|(name, _)| name.as_bytes().cmp(body))
        .ok()
        .and_then(|i| NAMED.get(i));
    match found {
        Some((_, ch)) => Ok(*ch),
        None => Err(XmlError::new(ErrorKind::BadReference, at)),
    }
}

/// Numeric reference body after `#`.
fn numeric(rest: &[u8], at: usize) -> Result<char, XmlError> {
    let bad = XmlError::new(ErrorKind::BadReference, at);
    let (digits, radix) = match rest.split_first() {
        Some((b'x' | b'X', tail)) => (tail, 16),
        _ => (rest, 10),
    };
    if digits.is_empty() {
        return Err(bad);
    }
    let mut v: u32 = 0;
    for b in digits {
        let Some(d) = char::from(*b).to_digit(radix) else {
            return Err(bad);
        };
        // Saturating so long digit runs cannot wrap.
        v = v.saturating_mul(radix).saturating_add(d);
        if v > MAX_CODEPOINT {
            return Err(bad);
        }
    }
    legal(v, at)
}

/// XML Char rule. Rejects C0 controls except tab, LF, CR, plus surrogates and values above `U+10FFFF`.
fn legal(v: u32, at: usize) -> Result<char, XmlError> {
    let forbidden = (v < 0x20 && !matches!(v, 0x09 | 0x0A | 0x0D))
        || (0xD800..=0xDFFF).contains(&v)
        || v > MAX_CODEPOINT;
    if forbidden {
        return Err(XmlError::new(ErrorKind::BadReference, at));
    }
    char::from_u32(v).ok_or_else(|| XmlError::new(ErrorKind::BadReference, at))
}

/// Named references, sorted for binary search. Case sensitive. Sortedness is tested.
pub const NAMED: &[(&str, char)] = &[
    ("AElig", '\u{00C6}'),
    ("Aacute", '\u{00C1}'),
    ("Acirc", '\u{00C2}'),
    ("Agrave", '\u{00C0}'),
    ("Alpha", '\u{0391}'),
    ("Aring", '\u{00C5}'),
    ("Atilde", '\u{00C3}'),
    ("Auml", '\u{00C4}'),
    ("Beta", '\u{0392}'),
    ("Ccedil", '\u{00C7}'),
    ("Dagger", '\u{2021}'),
    ("Delta", '\u{0394}'),
    ("ETH", '\u{00D0}'),
    ("Eacute", '\u{00C9}'),
    ("Ecirc", '\u{00CA}'),
    ("Egrave", '\u{00C8}'),
    ("Epsilon", '\u{0395}'),
    ("Euml", '\u{00CB}'),
    ("Gamma", '\u{0393}'),
    ("Iacute", '\u{00CD}'),
    ("Icirc", '\u{00CE}'),
    ("Igrave", '\u{00CC}'),
    ("Iuml", '\u{00CF}'),
    ("Lambda", '\u{039B}'),
    ("Mu", '\u{039C}'),
    ("Ntilde", '\u{00D1}'),
    ("OElig", '\u{0152}'),
    ("Oacute", '\u{00D3}'),
    ("Ocirc", '\u{00D4}'),
    ("Ograve", '\u{00D2}'),
    ("Omega", '\u{03A9}'),
    ("Oslash", '\u{00D8}'),
    ("Otilde", '\u{00D5}'),
    ("Ouml", '\u{00D6}'),
    ("Phi", '\u{03A6}'),
    ("Pi", '\u{03A0}'),
    ("Prime", '\u{2033}'),
    ("Scaron", '\u{0160}'),
    ("Sigma", '\u{03A3}'),
    ("THORN", '\u{00DE}'),
    ("Uacute", '\u{00DA}'),
    ("Ucirc", '\u{00DB}'),
    ("Ugrave", '\u{00D9}'),
    ("Uuml", '\u{00DC}'),
    ("Yacute", '\u{00DD}'),
    ("Yuml", '\u{0178}'),
    ("aacute", '\u{00E1}'),
    ("acirc", '\u{00E2}'),
    ("acute", '\u{00B4}'),
    ("aelig", '\u{00E6}'),
    ("agrave", '\u{00E0}'),
    ("alpha", '\u{03B1}'),
    ("amp", '\u{0026}'),
    ("apos", '\u{0027}'),
    ("aring", '\u{00E5}'),
    ("asymp", '\u{2248}'),
    ("atilde", '\u{00E3}'),
    ("auml", '\u{00E4}'),
    ("bdquo", '\u{201E}'),
    ("beta", '\u{03B2}'),
    ("brvbar", '\u{00A6}'),
    ("bull", '\u{2022}'),
    ("ccedil", '\u{00E7}'),
    ("cedil", '\u{00B8}'),
    ("cent", '\u{00A2}'),
    ("circ", '\u{02C6}'),
    ("clubs", '\u{2663}'),
    ("copy", '\u{00A9}'),
    ("curren", '\u{00A4}'),
    ("dagger", '\u{2020}'),
    ("darr", '\u{2193}'),
    ("deg", '\u{00B0}'),
    ("delta", '\u{03B4}'),
    ("diams", '\u{2666}'),
    ("divide", '\u{00F7}'),
    ("eacute", '\u{00E9}'),
    ("ecirc", '\u{00EA}'),
    ("egrave", '\u{00E8}'),
    ("emsp", '\u{2003}'),
    ("ensp", '\u{2002}'),
    ("epsilon", '\u{03B5}'),
    ("equiv", '\u{2261}'),
    ("eth", '\u{00F0}'),
    ("euml", '\u{00EB}'),
    ("euro", '\u{20AC}'),
    ("fnof", '\u{0192}'),
    ("frac12", '\u{00BD}'),
    ("frac14", '\u{00BC}'),
    ("frac34", '\u{00BE}'),
    ("frasl", '\u{2044}'),
    ("gamma", '\u{03B3}'),
    ("ge", '\u{2265}'),
    ("gt", '\u{003E}'),
    ("harr", '\u{2194}'),
    ("hearts", '\u{2665}'),
    ("hellip", '\u{2026}'),
    ("iacute", '\u{00ED}'),
    ("icirc", '\u{00EE}'),
    ("iexcl", '\u{00A1}'),
    ("igrave", '\u{00EC}'),
    ("infin", '\u{221E}'),
    ("iquest", '\u{00BF}'),
    ("iuml", '\u{00EF}'),
    ("lambda", '\u{03BB}'),
    ("laquo", '\u{00AB}'),
    ("larr", '\u{2190}'),
    ("ldquo", '\u{201C}'),
    ("le", '\u{2264}'),
    ("lowast", '\u{2217}'),
    ("loz", '\u{25CA}'),
    ("lrm", '\u{200E}'),
    ("lsaquo", '\u{2039}'),
    ("lsquo", '\u{2018}'),
    ("lt", '\u{003C}'),
    ("macr", '\u{00AF}'),
    ("mdash", '\u{2014}'),
    ("micro", '\u{00B5}'),
    ("middot", '\u{00B7}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{03BC}'),
    ("nbsp", '\u{00A0}'),
    ("ndash", '\u{2013}'),
    ("ne", '\u{2260}'),
    ("not", '\u{00AC}'),
    ("ntilde", '\u{00F1}'),
    ("oacute", '\u{00F3}'),
    ("ocirc", '\u{00F4}'),
    ("oelig", '\u{0153}'),
    ("ograve", '\u{00F2}'),
    ("ohm", '\u{2126}'),
    ("oline", '\u{203E}'),
    ("omega", '\u{03C9}'),
    ("ordf", '\u{00AA}'),
    ("ordm", '\u{00BA}'),
    ("oslash", '\u{00F8}'),
    ("otilde", '\u{00F5}'),
    ("ouml", '\u{00F6}'),
    ("para", '\u{00B6}'),
    ("part", '\u{2202}'),
    ("permil", '\u{2030}'),
    ("phi", '\u{03C6}'),
    ("pi", '\u{03C0}'),
    ("plusmn", '\u{00B1}'),
    ("pound", '\u{00A3}'),
    ("prime", '\u{2032}'),
    ("prod", '\u{220F}'),
    ("quot", '\u{0022}'),
    ("radic", '\u{221A}'),
    ("raquo", '\u{00BB}'),
    ("rarr", '\u{2192}'),
    ("rdquo", '\u{201D}'),
    ("reg", '\u{00AE}'),
    ("rlm", '\u{200F}'),
    ("rsaquo", '\u{203A}'),
    ("rsquo", '\u{2019}'),
    ("sbquo", '\u{201A}'),
    ("scaron", '\u{0161}'),
    ("sect", '\u{00A7}'),
    ("shy", '\u{00AD}'),
    ("sigma", '\u{03C3}'),
    ("spades", '\u{2660}'),
    ("sum", '\u{2211}'),
    ("sup1", '\u{00B9}'),
    ("sup2", '\u{00B2}'),
    ("sup3", '\u{00B3}'),
    ("szlig", '\u{00DF}'),
    ("tau", '\u{03C4}'),
    ("there4", '\u{2234}'),
    ("thinsp", '\u{2009}'),
    ("thorn", '\u{00FE}'),
    ("tilde", '\u{02DC}'),
    ("times", '\u{00D7}'),
    ("trade", '\u{2122}'),
    ("uacute", '\u{00FA}'),
    ("uarr", '\u{2191}'),
    ("ucirc", '\u{00FB}'),
    ("ugrave", '\u{00F9}'),
    ("uml", '\u{00A8}'),
    ("uuml", '\u{00FC}'),
    ("yacute", '\u{00FD}'),
    ("yen", '\u{00A5}'),
    ("yuml", '\u{00FF}'),
    ("zwj", '\u{200D}'),
    ("zwnj", '\u{200C}'),
];

#[cfg(test)]
mod tests;
