//! Chunked transfer coding encoder, RFC 9112 7.1.
//!
//! ```text
//! chunk      = chunk-size CRLF chunk-data CRLF
//! chunk-size = 1*HEXDIG
//! last-chunk = 1*("0") CRLF, then an empty trailer section and CRLF
//! ```

/// End of a chunked body with no trailers.
pub const LAST: &[u8; 5] = b"0\r\n\r\n";

/// CRLF after chunk data.
pub const TAIL: usize = 2;

/// Sixteen digits and CRLF.
pub const MAX_HEADER: usize = 18;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Hex digits of `len`, one for zero.
pub const fn digits(len: u64) -> usize {
    let bits = 64 - len.leading_zeros() as usize;
    if bits == 0 { 1 } else { bits.div_ceil(4) }
}

/// Gap to leave before data so [`frame`] fits a header for up to `max` bytes.
pub const fn gap(max: u64) -> usize {
    digits(max) + 2
}

/// Wire size of a chunk of `len` with the shortest header.
pub const fn framed_len(len: u64) -> u64 {
    digits(len) as u64 + 2 + len + TAIL as u64
}

/// Fill `out` with a zero-padded header for `len`. `None` if `out` is under
/// three bytes, `len` is zero, or `len` does not fit.
pub fn header_padded(out: &mut [u8], len: u64) -> Option<()> {
    let width = out.len().checked_sub(2)?;
    if len == 0 || width == 0 || digits(len) > width {
        return None;
    }
    let mut v = len;
    for slot in out[..width].iter_mut().rev() {
        *slot = HEX[(v & 15) as usize];
        v >>= 4;
    }
    out[width..].copy_from_slice(b"\r\n");
    Some(())
}

/// Write the shortest header for `len` and return its length. `None` without room or for zero.
pub fn header(out: &mut [u8], len: u64) -> Option<usize> {
    let n = gap(len);
    header_padded(out.get_mut(..n)?, len)?;
    Some(n)
}

/// Frame data in place. `buf` is `gap` free bytes, `len` data bytes, then room for
/// CRLF and, with `last`, [`LAST`]. Returns the framed length. Zero `len` writes
/// only [`LAST`] at the start, if `last`. `None` if the gap is too narrow, wider
/// than [`MAX_HEADER`], or `buf` is too short.
pub fn frame(buf: &mut [u8], gap: usize, len: usize, last: bool) -> Option<usize> {
    let mut end = 0;
    if len > 0 {
        if gap > MAX_HEADER {
            return None;
        }
        end = gap.checked_add(len)?.checked_add(TAIL)?;
        if end > buf.len() {
            return None;
        }
        header_padded(&mut buf[..gap], len as u64)?;
        buf[end - TAIL..end].copy_from_slice(b"\r\n");
    }
    if last {
        buf.get_mut(end..end + LAST.len())?.copy_from_slice(LAST);
        end += LAST.len();
    }
    Some(end)
}

/// Copy `data` into `out` as one chunk and return its length. Empty data writes
/// nothing. `None` without room.
pub fn encode(out: &mut [u8], data: &[u8]) -> Option<usize> {
    if data.is_empty() {
        return Some(0);
    }
    let end = framed_len(data.len() as u64) as usize;
    let out = out.get_mut(..end)?;
    let at = header(out, data.len() as u64)?;
    out[at..at + data.len()].copy_from_slice(data);
    out[end - TAIL..].copy_from_slice(b"\r\n");
    Some(end)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Strict reader, no extensions or trailers. Returns body and bytes consumed.
    fn decode(mut src: &[u8]) -> Option<(Vec<u8>, usize)> {
        let whole = src.len();
        let mut body = Vec::new();
        loop {
            let line = src.windows(2).position(|w| w == b"\r\n")?;
            let size = std::str::from_utf8(&src[..line]).ok()?;
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let n = usize::from_str_radix(size, 16).ok()?;
            src = &src[line + 2..];
            if n == 0 {
                src = src.strip_prefix(b"\r\n")?;
                return Some((body, whole - src.len()));
            }
            body.extend_from_slice(src.get(..n)?);
            src = src.get(n..)?.strip_prefix(b"\r\n")?;
        }
    }

    #[test]
    fn digits_count_hex_places() {
        let cases = [
            (0, 1),
            (1, 1),
            (15, 1),
            (16, 2),
            (255, 2),
            (256, 3),
            (4095, 3),
            (4096, 4),
            (65535, 4),
            (65536, 5),
            (u64::MAX, 16),
        ];
        for (len, want) in cases {
            assert_eq!(digits(len), want, "{len}");
        }
        assert_eq!(gap(4096), 6);
        assert_eq!(gap(u64::MAX), MAX_HEADER);
        assert_eq!(framed_len(5), 1 + 2 + 5 + 2);
        assert_eq!(framed_len(0x1234), 4 + 2 + 0x1234 + 2);
    }

    #[test]
    fn the_shortest_header_is_lowercase_hex_and_crlf() {
        let mut out = [0xaau8; 24];
        assert_eq!(header(&mut out, 1), Some(3));
        assert_eq!(&out[..3], b"1\r\n");
        assert_eq!(header(&mut out, 0xbeef), Some(6));
        assert_eq!(&out[..6], b"beef\r\n");
        assert_eq!(header(&mut out, u64::MAX), Some(18));
        assert_eq!(&out[..18], b"ffffffffffffffff\r\n");
        assert_eq!(out[18], 0xaa, "wrote past its length");
        assert_eq!(header(&mut out, 0), None);
        assert_eq!(header(&mut out[..5], 0xbeef), None);
        assert_eq!(header(&mut [], 1), None);
    }

    #[test]
    fn a_padded_header_fills_its_width() {
        let mut out = [0u8; 6];
        assert_eq!(header_padded(&mut out, 0x1a), Some(()));
        assert_eq!(&out, b"001a\r\n");
        assert_eq!(header_padded(&mut out, 0xffff), Some(()));
        assert_eq!(&out, b"ffff\r\n");
        assert_eq!(header_padded(&mut out, 0x10000), None, "five digits");
        assert_eq!(header_padded(&mut out, 0), None);
        assert_eq!(header_padded(&mut out[..2], 1), None);
        assert_eq!(header_padded(&mut out[..1], 1), None);
        assert_eq!(header_padded(&mut [], 1), None);
    }

    #[test]
    fn data_is_framed_where_it_lies() {
        let mut buf = [b'.'; 32];
        buf[6..11].copy_from_slice(b"hello");
        assert_eq!(frame(&mut buf, 6, 5, false), Some(13));
        assert_eq!(&buf[..13], b"0005\r\nhello\r\n");
        assert_eq!(buf[13], b'.');
        assert_eq!(frame(&mut buf, 6, 5, true), Some(18));
        assert_eq!(&buf[..18], b"0005\r\nhello\r\n0\r\n\r\n");
        assert_eq!(decode(&buf[..18]), Some((b"hello".to_vec(), 18)));
        assert_eq!(frame(&mut buf, 6, 0, false), Some(0));
        assert_eq!(frame(&mut buf, 6, 0, true), Some(5));
        assert_eq!(&buf[..5], LAST);
    }

    #[test]
    fn framing_refuses_what_does_not_fit() {
        let mut buf = [0u8; 16];
        // Exact fit.
        assert_eq!(frame(&mut buf, 3, 11, false), Some(16));
        assert_eq!(&buf[..3], b"b\r\n");
        assert_eq!(frame(&mut buf, 3, 12, false), None, "no room for CRLF");
        assert_eq!(frame(&mut buf, 3, 11, true), None, "no room for the last");
        assert_eq!(frame(&mut buf, 3, 6, true), Some(16));
        assert_eq!(frame(&mut buf[..4], 3, 0, true), None);
        let mut buf = [0u8; 64];
        assert_eq!(frame(&mut buf, 3, 16, false), None);
        assert_eq!(frame(&mut buf, 2, 1, false), None, "no digit at all");
        assert_eq!(frame(&mut buf, usize::MAX, 1, false), None);
    }

    #[test]
    fn encode_copies_one_chunk() {
        let mut out = [0u8; 64];
        assert_eq!(encode(&mut out, b""), Some(0));
        assert_eq!(encode(&mut out, b"abc"), Some(8));
        assert_eq!(&out[..8], b"3\r\nabc\r\n");
        let data = [7u8; 17];
        let n = encode(&mut out, &data).unwrap();
        assert_eq!(n as u64, framed_len(17));
        assert_eq!(&out[..4], b"11\r\n");
        assert_eq!(encode(&mut out[..n - 1], &data), None);
    }

    /// Lengths around digit boundaries, both framing paths.
    #[test]
    fn bodies_of_many_chunks_read_back() {
        let sizes = [
            1usize, 2, 15, 16, 17, 255, 256, 257, 4095, 4096, 4097, 70_000,
        ];
        let mut wire = Vec::new();
        let mut want = Vec::new();
        for (i, n) in sizes.iter().enumerate() {
            let data: Vec<u8> = (0..*n).map(|j| (i * 31 + j * 7) as u8).collect();
            want.extend_from_slice(&data);
            if i % 2 == 0 {
                let mut out = vec![0u8; framed_len(*n as u64) as usize];
                let len = encode(&mut out, &data).unwrap();
                assert_eq!(len, out.len());
                wire.extend_from_slice(&out);
            } else {
                let g = gap(100_000);
                let mut buf = vec![0u8; g + n + TAIL];
                buf[g..g + n].copy_from_slice(&data);
                let len = frame(&mut buf, g, *n, false).unwrap();
                assert_eq!(len, buf.len());
                wire.extend_from_slice(&buf);
            }
        }
        wire.extend_from_slice(LAST);
        let (body, used) = decode(&wire).unwrap();
        assert_eq!(used, wire.len());
        assert!(body == want, "body differs");
    }

    #[test]
    fn a_padded_chunk_and_the_last_read_back() {
        let mut buf = [0u8; 64];
        let g = gap(4096);
        buf[g..g + 4].copy_from_slice(b"data");
        let n = frame(&mut buf, g, 4, true).unwrap();
        assert_eq!(&buf[..n], b"0004\r\ndata\r\n0\r\n\r\n");
        let (body, used) = decode(&buf[..n]).unwrap();
        assert_eq!((body.as_slice(), used), (&b"data"[..], n));
    }

    #[test]
    fn a_gap_wider_than_any_header_is_refused() {
        let mut buf = [b'x'; 64];
        assert_eq!(frame(&mut buf, MAX_HEADER + 1, 4, true), None);
        let n = frame(&mut buf, MAX_HEADER, 4, true).unwrap();
        let (body, used) = decode(&buf[..n]).unwrap();
        assert_eq!((body.as_slice(), used), (&b"xxxx"[..], n));
    }
}
