//! HPACK (RFC 7541). Full decoder, encoder uses only the static table and plain literals.
//! Huffman tables in `table.rs` are vendored from `h2` (MIT).

pub mod fixed;
mod table;

use table::DECODE_TABLE;

const MAYBE_EOS: u8 = 1;
const DECODED: u8 = 2;
const ERROR: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Input ended mid-representation.
    NeedMore,
    InvalidIndex,
    InvalidHuffman,
    IntegerOverflow,
    StringTooLong,
    TableSizeExceeded,
}

pub(crate) struct Huff {
    state: u8,
    maybe_eos: bool,
}

impl Huff {
    pub(crate) fn new() -> Self {
        Huff {
            state: 0,
            maybe_eos: false,
        }
    }

    pub(crate) fn nibble(&mut self, n: u8) -> Result<Option<u8>, Error> {
        // entry is (next state, byte, flags), tested against RFC 7541 vectors
        let (next, out, flags) = DECODE_TABLE[self.state as usize][n as usize];
        if flags & ERROR == ERROR {
            return Err(Error::InvalidHuffman);
        }
        self.state = next;
        self.maybe_eos = flags & MAYBE_EOS == MAYBE_EOS;
        if flags & DECODED == DECODED {
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn is_final(&self) -> bool {
        self.state == 0 || self.maybe_eos
    }
}

pub fn huffman_decode(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<(), Error> {
    let mut d = Huff::new();
    for b in src {
        if let Some(v) = d.nibble(b >> 4)? {
            if out.len() >= limit {
                return Err(Error::StringTooLong);
            }
            out.push(v);
        }
        if let Some(v) = d.nibble(b & 0xf)? {
            if out.len() >= limit {
                return Err(Error::StringTooLong);
            }
            out.push(v);
        }
    }
    if !d.is_final() {
        return Err(Error::InvalidHuffman);
    }
    Ok(())
}

/// RFC 7541 section 5.1 prefixed integer. Returns (value, bytes consumed).
pub fn decode_int(src: &[u8], prefix_bits: u8) -> Result<(u64, usize), Error> {
    debug_assert!((1..=8).contains(&prefix_bits));
    let first = *src.first().ok_or(Error::NeedMore)?;
    let mask = (1u16 << prefix_bits) - 1;
    let mut value = (first as u16 & mask) as u64;
    if value < mask as u64 {
        return Ok((value, 1));
    }
    let mut shift = 0u32;
    let mut i = 1usize;
    loop {
        let b = *src.get(i).ok_or(Error::NeedMore)?;
        i += 1;
        value = value
            .checked_add(
                ((b & 0x7f) as u64)
                    .checked_shl(shift)
                    .ok_or(Error::IntegerOverflow)?,
            )
            .ok_or(Error::IntegerOverflow)?;
        if b & 0x80 == 0 {
            return Ok((value, i));
        }
        shift += 7;
        if shift > 56 {
            return Err(Error::IntegerOverflow);
        }
    }
}

pub fn encode_int(mut value: u64, prefix_bits: u8, first_byte: u8, dst: &mut Vec<u8>) {
    let mask = (1u16 << prefix_bits) - 1;
    if value < mask as u64 {
        dst.push(first_byte | value as u8);
        return;
    }
    dst.push(first_byte | mask as u8);
    value -= mask as u64;
    while value >= 128 {
        dst.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    dst.push(value as u8);
}

fn decode_string(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, Error> {
    let first = *src.first().ok_or(Error::NeedMore)?;
    let huff = first & 0x80 != 0;
    let (len, n) = decode_int(src, 7)?;
    let len = len as usize;
    if len > limit {
        return Err(Error::StringTooLong);
    }
    let body = src.get(n..n + len).ok_or(Error::NeedMore)?;
    if huff {
        huffman_decode(body, out, limit)?;
    } else {
        out.extend_from_slice(body);
    }
    Ok(n + len)
}

pub fn encode_string(s: &[u8], dst: &mut Vec<u8>) {
    // never Huffman
    encode_int(s.len() as u64, 7, 0x00, dst);
    dst.extend_from_slice(s);
}

/// RFC 7541 Appendix A. HPACK indices are 1-based.
pub const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

pub fn static_index(name: &str, value: &str) -> Option<u64> {
    STATIC_TABLE
        .iter()
        .position(|(n, v)| *n == name && *v == value)
        .map(|i| i as u64 + 1)
}

pub fn static_name_index(name: &str) -> Option<u64> {
    STATIC_TABLE
        .iter()
        .position(|(n, _)| *n == name)
        .map(|i| i as u64 + 1)
}

/// Bounded FIFO of (name, value). Capacity in HPACK size units, section 4.1.
pub struct DynamicTable {
    entries: std::collections::VecDeque<(Vec<u8>, Vec<u8>)>,
    size: usize,
    max_size: usize,
}

impl DynamicTable {
    pub fn new(max_size: usize) -> Self {
        DynamicTable {
            entries: std::collections::VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn size(&self) -> usize {
        self.size
    }

    fn entry_size(n: &[u8], v: &[u8]) -> usize {
        n.len() + v.len() + 32
    }

    pub fn set_max_size(&mut self, max: usize) {
        self.max_size = max;
        self.evict();
    }

    pub fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let sz = Self::entry_size(&name, &value);
        if sz > self.max_size {
            // section 4.4, oversized entry empties the table
            self.entries.clear();
            self.size = 0;
            return;
        }
        self.entries.push_front((name, value));
        self.size += sz;
        self.evict();
    }

    fn evict(&mut self) {
        while self.size > self.max_size {
            match self.entries.pop_back() {
                Some((n, v)) => self.size -= Self::entry_size(&n, &v),
                None => break,
            }
        }
    }

    /// 1-based, static entries then dynamic.
    pub fn get(&self, index: u64) -> Option<(&[u8], &[u8])> {
        let i = index as usize;
        if i == 0 {
            return None;
        }
        if i <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[i - 1];
            return Some((n.as_bytes(), v.as_bytes()));
        }
        let d = i - STATIC_TABLE.len() - 1;
        self.entries
            .get(d)
            .map(|(n, v)| (n.as_slice(), v.as_slice()))
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Header {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Decode a complete header block. `limit` bounds any single string.
pub fn decode_block(
    src: &[u8],
    table: &mut DynamicTable,
    limit: usize,
    out: &mut Vec<Header>,
) -> Result<(), Error> {
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        if b & 0x80 != 0 {
            // section 6.1 indexed header field
            let (idx, n) = decode_int(&src[i..], 7)?;
            let (name, value) = table.get(idx).ok_or(Error::InvalidIndex)?;
            out.push(Header {
                name: name.to_vec(),
                value: value.to_vec(),
            });
            i += n;
        } else if b & 0xc0 == 0x40 {
            // section 6.2.1 literal with incremental indexing
            i += decode_literal(&src[i..], 6, table, limit, out, true)?;
        } else if b & 0xe0 == 0x20 {
            // section 6.3 dynamic table size update
            let (sz, n) = decode_int(&src[i..], 5)?;
            table.set_max_size(sz as usize);
            i += n;
        } else {
            // section 6.2.2 / section 6.2.3 literal without indexing / never indexed
            i += decode_literal(&src[i..], 4, table, limit, out, false)?;
        }
    }
    Ok(())
}

fn decode_literal(
    src: &[u8],
    prefix: u8,
    table: &mut DynamicTable,
    limit: usize,
    out: &mut Vec<Header>,
    index_it: bool,
) -> Result<usize, Error> {
    let (name_idx, mut consumed) = decode_int(src, prefix)?;
    let name = if name_idx == 0 {
        let mut n = Vec::new();
        consumed += decode_string(&src[consumed..], &mut n, limit)?;
        n
    } else {
        let (n, _) = table.get(name_idx).ok_or(Error::InvalidIndex)?;
        n.to_vec()
    };
    let mut value = Vec::new();
    consumed += decode_string(&src[consumed..], &mut value, limit)?;
    if index_it {
        table.insert(name.clone(), value.clone());
    }
    out.push(Header { name, value });
    Ok(consumed)
}

/// Static table where possible, literals otherwise. Never indexes.
pub fn encode_header(name: &str, value: &str, dst: &mut Vec<u8>) {
    if let Some(i) = static_index(name, value) {
        encode_int(i, 7, 0x80, dst);
        return;
    }
    match static_name_index(name) {
        Some(i) => encode_int(i, 4, 0x00, dst),
        None => {
            dst.push(0x00);
            encode_string(name.as_bytes(), dst);
        }
    }
    encode_string(value.as_bytes(), dst);
}

#[cfg(test)]
mod tests;
