//! QPACK (RFC 9204) with a dynamic table of capacity zero, both ways.
//!
//! The client announces capacity zero and no blocked streams, so sections it gets
//! use the static table and literals alone (RFC 9204 3.2.3), and sections it sends
//! never reference a dynamic entry. Encoding never uses Huffman. Decoding takes
//! every static and literal line form, Huffman or not, with HPACK's code.

use super::H3Error;
use super::frame::{Full, Out};
use crate::header;
use crate::hpack::{self, Huff};

/// Static table (RFC 9204 Appendix A).
pub const STATIC: [(&str, &str); 99] = [
    (":authority", ""),
    (":path", "/"),
    ("age", "0"),
    ("content-disposition", ""),
    ("content-length", "0"),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("referer", ""),
    ("set-cookie", ""),
    (":method", "CONNECT"),
    (":method", "DELETE"),
    (":method", "GET"),
    (":method", "HEAD"),
    (":method", "OPTIONS"),
    (":method", "POST"),
    (":method", "PUT"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "103"),
    (":status", "200"),
    (":status", "304"),
    (":status", "404"),
    (":status", "503"),
    ("accept", "*/*"),
    ("accept", "application/dns-message"),
    ("accept-encoding", "gzip, deflate, br"),
    ("accept-ranges", "bytes"),
    ("access-control-allow-headers", "cache-control"),
    ("access-control-allow-headers", "content-type"),
    ("access-control-allow-origin", "*"),
    ("cache-control", "max-age=0"),
    ("cache-control", "max-age=2592000"),
    ("cache-control", "max-age=604800"),
    ("cache-control", "no-cache"),
    ("cache-control", "no-store"),
    ("cache-control", "public, max-age=31536000"),
    ("content-encoding", "br"),
    ("content-encoding", "gzip"),
    ("content-type", "application/dns-message"),
    ("content-type", "application/javascript"),
    ("content-type", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("content-type", "image/gif"),
    ("content-type", "image/jpeg"),
    ("content-type", "image/png"),
    ("content-type", "text/css"),
    ("content-type", "text/html; charset=utf-8"),
    ("content-type", "text/plain"),
    ("content-type", "text/plain;charset=utf-8"),
    ("range", "bytes=0-"),
    ("strict-transport-security", "max-age=31536000"),
    (
        "strict-transport-security",
        "max-age=31536000; includesubdomains",
    ),
    (
        "strict-transport-security",
        "max-age=31536000; includesubdomains; preload",
    ),
    ("vary", "accept-encoding"),
    ("vary", "origin"),
    ("x-content-type-options", "nosniff"),
    ("x-xss-protection", "1; mode=block"),
    (":status", "100"),
    (":status", "204"),
    (":status", "206"),
    (":status", "302"),
    (":status", "400"),
    (":status", "403"),
    (":status", "421"),
    (":status", "425"),
    (":status", "500"),
    ("accept-language", ""),
    ("access-control-allow-credentials", "FALSE"),
    ("access-control-allow-credentials", "TRUE"),
    ("access-control-allow-headers", "*"),
    ("access-control-allow-methods", "get"),
    ("access-control-allow-methods", "get, post, options"),
    ("access-control-allow-methods", "options"),
    ("access-control-expose-headers", "content-length"),
    ("access-control-request-headers", "content-type"),
    ("access-control-request-method", "get"),
    ("access-control-request-method", "post"),
    ("alt-svc", "clear"),
    ("authorization", ""),
    (
        "content-security-policy",
        "script-src 'none'; object-src 'none'; base-uri 'none'",
    ),
    ("early-data", "1"),
    ("expect-ct", ""),
    ("forwarded", ""),
    ("if-range", ""),
    ("origin", ""),
    ("purpose", "prefetch"),
    ("server", ""),
    ("timing-allow-origin", "*"),
    ("upgrade-insecure-requests", "1"),
    ("user-agent", ""),
    ("x-forwarded-for", ""),
    ("x-frame-options", "deny"),
    ("x-frame-options", "sameorigin"),
];

/// Static indices the request encoder names directly.
const AUTHORITY: u64 = 0;
const PATH: u64 = 1;
const METHOD: u64 = 15;
const SCHEME_HTTPS: u64 = 23;

/// Size a field counts against a section limit (RFC 9114 4.2.2).
pub fn field_size(name: &[u8], value: &[u8]) -> usize {
    name.len() + value.len() + 32
}

/// Prefixed integer (RFC 7541 5.1 via RFC 9204 4.1.1), `flags` in the high bits.
pub fn put_int(out: &mut Out<'_>, flags: u8, prefix: u8, mut value: u64) -> Result<(), Full> {
    let mask = (1u16 << prefix) as u64 - 1;
    if value < mask {
        return out.byte(flags | value as u8);
    }
    out.byte(flags | mask as u8)?;
    value -= mask;
    while value >= 0x80 {
        out.byte(value as u8 | 0x80)?;
        value >>= 7;
    }
    out.byte(value as u8)
}

/// String literal, not Huffman coded, its length behind `prefix` bits (RFC 9204 4.1.2).
pub fn put_str(out: &mut Out<'_>, flags: u8, prefix: u8, s: &[u8]) -> Result<(), Full> {
    put_int(out, flags, prefix, s.len() as u64)?;
    out.bytes(s)
}

/// Static entry that is `name: value`, and the first whose name is `name`.
pub fn find(name: &[u8], value: &[u8]) -> (Option<u64>, Option<u64>) {
    let mut by_name = None;
    for (i, (n, v)) in STATIC.iter().enumerate() {
        if n.as_bytes() == name {
            if v.as_bytes() == value {
                return (Some(i as u64), Some(i as u64));
            }
            by_name = by_name.or(Some(i as u64));
        }
    }
    (None, by_name)
}

/// One field line by the static table where it can (RFC 9204 4.5). `name` must be lowercase.
pub fn put_field(out: &mut Out<'_>, name: &[u8], value: &[u8]) -> Result<(), Full> {
    match find(name, value) {
        // Indexed, `1 T index(6)`, T set for static.
        (Some(i), _) => put_int(out, 0xc0, 6, i),
        // Name reference, `01 N T index(4)`.
        (None, Some(i)) => {
            put_int(out, 0x50, 4, i)?;
            put_str(out, 0x00, 7, value)
        }
        // Literal name, `001 N H len(3)`.
        (None, None) => {
            put_str(out, 0x20, 3, name)?;
            put_str(out, 0x00, 7, value)
        }
    }
}

/// Request field section (RFC 9114 4.3.1): prefix with Required Insert Count 0 and
/// Base 0, pseudo fields with scheme https, then `fields` with names lowercased.
pub fn put_request<'f>(
    out: &mut Out<'_>,
    method: &str,
    authority: &str,
    path: &str,
    fields: impl Iterator<Item = (&'f str, &'f str)>,
) -> Result<(), Full> {
    out.bytes(&[0, 0])?;
    match find(b":method", method.as_bytes()) {
        (Some(i), _) => put_int(out, 0xc0, 6, i)?,
        _ => {
            put_int(out, 0x50, 4, METHOD)?;
            put_str(out, 0x00, 7, method.as_bytes())?;
        }
    }
    put_int(out, 0xc0, 6, SCHEME_HTTPS)?;
    put_int(out, 0x50, 4, AUTHORITY)?;
    put_str(out, 0x00, 7, authority.as_bytes())?;
    if path == "/" {
        put_int(out, 0xc0, 6, PATH)?;
    } else {
        put_int(out, 0x50, 4, PATH)?;
        put_str(out, 0x00, 7, path.as_bytes())?;
    }
    let mut lower = [0u8; 64];
    for (n, v) in fields {
        let n = n.as_bytes();
        match lower.get_mut(..n.len()) {
            Some(l) => {
                for (d, s) in l.iter_mut().zip(n) {
                    *d = s.to_ascii_lowercase();
                }
                put_field(out, l, v.as_bytes())?;
            }
            // Past 64 bytes, written as a literal and lowercased in place.
            None => {
                put_str(out, 0x20, 3, n)?;
                out.tail_mut(n.len()).make_ascii_lowercase();
                put_str(out, 0x00, 7, v.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn int(b: &[u8], at: usize, prefix: u8) -> Result<(u64, usize), H3Error> {
    let src = b.get(at..).unwrap_or(&[]);
    hpack::decode_int(src, prefix).map_err(|e| H3Error::Decompression {
        at,
        what: match e {
            hpack::Error::IntegerOverflow => "an integer too large",
            _ => "the section ends inside an integer",
        },
    })
}

/// Huffman `src` into `out`. Bytes written, `None` when `out` is too small.
fn huffman(src: &[u8], out: &mut [u8]) -> Result<Option<usize>, hpack::Error> {
    let mut d = Huff::new();
    let mut n = 0;
    for b in src {
        for nibble in [b >> 4, b & 0xf] {
            if let Some(v) = d.nibble(nibble)? {
                let Some(slot) = out.get_mut(n) else {
                    return Ok(None);
                };
                *slot = v;
                n += 1;
            }
        }
    }
    if !d.is_final() {
        return Err(hpack::Error::InvalidHuffman);
    }
    Ok(Some(n))
}

/// String literal at `at` with H bit `h` and a `prefix` bit length, decoded into
/// `line[len..]`. Returns (wire bytes taken, new line length).
fn string(
    b: &[u8],
    at: usize,
    h: u8,
    prefix: u8,
    line: &mut [u8],
    len: usize,
) -> Result<(usize, usize), H3Error> {
    let huffman_coded = b.get(at).is_some_and(|first| first & h != 0);
    let (n_len, n) = int(b, at, prefix)?;
    let from = at + n;
    let body = usize::try_from(n_len)
        .ok()
        .and_then(|l| b.get(from..from.checked_add(l)?));
    let Some(body) = body else {
        return Err(H3Error::Decompression {
            at,
            what: "a string past the end of the section",
        });
    };
    let room = line.len().saturating_sub(len);
    let over = H3Error::ExcessiveLoad {
        len: (len + body.len()) as u64,
        max: line.len() as u64,
    };
    let dst = line.get_mut(len..).unwrap_or(&mut []);
    let wrote = if huffman_coded {
        // Huffman of RFC 7541 5.2, RFC 9204 4.1.2.
        match huffman(body, dst) {
            Ok(Some(w)) => w,
            Ok(None) => return Err(over),
            Err(_) => {
                return Err(H3Error::Decompression {
                    at: from,
                    what: "a Huffman string that does not decode",
                });
            }
        }
    } else {
        if body.len() > room {
            return Err(over);
        }
        dst[..body.len()].copy_from_slice(body);
        body.len()
    };
    Ok((n + body.len(), len + wrote))
}

fn entry(index: u64, at: usize) -> Result<(&'static str, &'static str), H3Error> {
    usize::try_from(index)
        .ok()
        .and_then(|i| STATIC.get(i))
        .copied()
        .ok_or(H3Error::Decompression {
            at,
            what: "a static index past the table",
        })
}

fn dynamic(at: usize) -> H3Error {
    H3Error::Decompression {
        at,
        what: "a reference to the dynamic table",
    }
}

/// `s` into `line` at `at`. Returns the end.
fn copy_at(line: &mut [u8], at: usize, s: &str) -> Result<usize, H3Error> {
    let max = line.len() as u64;
    let end = at + s.len();
    let Some(dst) = line.get_mut(at..end) else {
        return Err(H3Error::ExcessiveLoad {
            len: end as u64,
            max,
        });
    };
    dst.copy_from_slice(s.as_bytes());
    Ok(end)
}

/// One line at `at` decoded into `line`. Returns (wire bytes, name length, line length).
fn line_at(block: &[u8], at: usize, line: &mut [u8]) -> Result<(usize, usize, usize), H3Error> {
    let b = block[at];
    if b & 0x80 != 0 {
        // Indexed (4.5.2), T is 0x40.
        if b & 0x40 == 0 {
            return Err(dynamic(at));
        }
        let (index, n) = int(block, at, 6)?;
        let (name, value) = entry(index, at)?;
        let k = copy_at(line, 0, name)?;
        return Ok((n, k, copy_at(line, k, value)?));
    }
    if b & 0xc0 == 0x40 {
        // Name reference (4.5.4), T is 0x10.
        if b & 0x10 == 0 {
            return Err(dynamic(at));
        }
        let (index, n) = int(block, at, 4)?;
        let (name, _) = entry(index, at)?;
        let k = copy_at(line, 0, name)?;
        let (v, end) = string(block, at + n, 0x80, 7, line, k)?;
        return Ok((n + v, k, end));
    }
    if b & 0xe0 == 0x20 {
        // Literal name (4.5.6), H is 0x08.
        let (k, split) = string(block, at, 0x08, 3, line, 0)?;
        let (v, end) = string(block, at + k, 0x80, 7, line, split)?;
        return Ok((k + v, split, end));
    }
    // Post-base index (4.5.3) or post-base name reference (4.5.5), both dynamic.
    Err(dynamic(at))
}

/// Decode `block` (RFC 9204 4.5), calling `field` per line in order. `line` holds one
/// decoded line. Over `max` bytes as RFC 9114 4.2.2 counts is refused, as is any
/// dynamic table reference.
pub fn decode(
    block: &[u8],
    max: usize,
    line: &mut [u8],
    mut field: impl FnMut(&[u8], &[u8]) -> Result<(), H3Error>,
) -> Result<(), H3Error> {
    // Prefix: Required Insert Count, then S and Delta Base (4.5.1). Nonzero count
    // needs entries a zero capacity table cannot have.
    let (ric, n) = int(block, 0, 8)?;
    if ric != 0 {
        return Err(dynamic(0));
    }
    let (_, m) = int(block, n, 7)?;
    let mut at = n + m;
    let mut size = 0usize;
    while at < block.len() {
        let (took, split, len) = line_at(block, at, line)?;
        let (name, value) = line[..len].split_at(split);
        size += field_size(name, value);
        if size > max {
            return Err(H3Error::ExcessiveLoad {
                len: size as u64,
                max: max as u64,
            });
        }
        field(name, value)?;
        at += took;
    }
    Ok(())
}

/// A decoded response head. Records are in the caller's `out`, as [`header`] packs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    /// Packed bytes and records written.
    pub len: usize,
    pub count: usize,
}

fn check_line(name: &[u8], value: &[u8]) -> Result<(), H3Error> {
    if value.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(H3Error::Message {
            what: "a field value with CR, LF or NUL",
        });
    }
    let pseudo = usize::from(name.starts_with(b":"));
    let odd = |b: &u8| b.is_ascii_uppercase() || matches!(b, b'\r' | b'\n' | 0 | b':' | b' ');
    if name.len() == pseudo || name[pseudo..].iter().any(odd) {
        return Err(H3Error::Message {
            what: "a field name that is not lowercase",
        });
    }
    Ok(())
}

fn status_of(value: &[u8]) -> Result<u16, H3Error> {
    let code = core::str::from_utf8(value)
        .ok()
        .filter(|v| v.len() == 3)
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|c| (100..600).contains(c));
    code.ok_or(H3Error::Message {
        what: "a :status that is not three digits",
    })
}

/// Decode a response section into packed header records in `out`, `:status` left
/// out. RFC 9114 4.3.2: `:status` first and the only pseudo field, names lowercase,
/// no value with CR, LF or NUL (4.2).
pub fn decode_response(
    section: &[u8],
    max: usize,
    line: &mut [u8],
    out: &mut [u8],
) -> Result<Head, H3Error> {
    let mut status = None;
    let (mut len, mut count) = (0usize, 0usize);
    let room = out.len() as u64;
    decode(section, max, line, |name, value| {
        check_line(name, value)?;
        match (name, status) {
            (b":status", None) => {
                status = Some(status_of(value)?);
                Ok(())
            }
            (n, _) if n.starts_with(b":") => Err(H3Error::Message {
                what: "a pseudo-header field other than one :status first",
            }),
            (_, None) => Err(H3Error::Message {
                what: "a field before :status",
            }),
            _ => {
                len = header::push(out, len, name, value).ok_or(H3Error::ExcessiveLoad {
                    len: (len + name.len() + value.len()) as u64,
                    max: room,
                })?;
                count += 1;
                Ok(())
            }
        }
    })?;
    let status = status.ok_or(H3Error::Message { what: "no :status" })?;
    Ok(Head { status, len, count })
}

#[cfg(test)]
mod tests;
