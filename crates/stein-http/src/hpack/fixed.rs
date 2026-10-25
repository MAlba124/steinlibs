//! HPACK over caller storage. The decoder table lives in a caller slice of [`table_bytes`].
//! The encoder is stateless, static table and raw literals only.
//!
//! ```text
//! [offset ring: ceiling / 32 slots of u32 LE][entry bytes: ceiling]
//! entry is a packed `crate::header` record [name_len: u8][value_len: u16 LE][name][value]
//! ```
//!
//! A record costs 3 bytes vs the RFC's 32, so entries always fit. Entries append and
//! evict from the front. Offsets carry a running shift so compaction needs no rewrite.

use super::{Huff, STATIC_TABLE};
use crate::header;

/// RFC 7541 section 4.1 per-entry overhead.
const ENTRY_COST: usize = 32;
/// Packed record header bytes.
const RECORD: usize = 3;
const OFFSET: usize = 4;
/// Packed format limits.
const NAME_MAX: usize = u8::MAX as usize;
const VALUE_MAX: usize = u16::MAX as usize;

/// Caller storage for a table of `max_size` HPACK size units.
pub const fn table_bytes(max_size: usize) -> usize {
    max_size + (max_size / ENTRY_COST) * OFFSET
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    InvalidIndex,
    InvalidHuffman,
    IntegerOverflow,
    /// Size update above the ceiling or after the first field.
    TableSizeExceeded,
    /// Name over 255 bytes.
    NameTooLong,
    /// Fields do not fit `out`, or a value over 65535 bytes.
    OutFull,
    /// `mem` smaller than [`table_bytes`] of the ceiling.
    BadStorage,
}

/// Table state. Storage is passed on every call.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    /// Advertised size, upper bound for size updates.
    ceiling: u32,
    /// Current max, moved by size updates.
    max: u32,
    /// HPACK size units in use.
    size: u32,
    count: u32,
    /// Ring slot of the newest entry.
    newest: u32,
    /// End of the live entry bytes.
    head: u32,
    /// Stored offsets are physical plus this.
    shift: u32,
}

impl Table {
    /// `ceiling` is our advertised SETTINGS_HEADER_TABLE_SIZE.
    pub fn new(ceiling: usize) -> Table {
        let ceiling = ceiling.min(u32::MAX as usize) as u32;
        Table {
            ceiling,
            max: ceiling,
            size: 0,
            count: 0,
            newest: 0,
            head: 0,
            shift: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// HPACK size units in use.
    pub fn size(&self) -> usize {
        self.size as usize
    }

    /// Decode a whole block into packed `crate::header` records. Returns bytes written.
    /// On error the table is out of sync and the connection must end.
    pub fn decode_block(
        &mut self,
        mem: &mut [u8],
        src: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Error> {
        let need = table_bytes(self.ceiling as usize);
        let mem = mem.get_mut(..need).ok_or(Error::BadStorage)?;
        let mut i = 0usize;
        let mut at = 0usize;
        while i < src.len() {
            let b = src[i];
            let rest = &src[i..];
            if b & 0x80 != 0 {
                // section 6.1 indexed field
                let (index, n) = int(rest, 7)?;
                let (name, value) = self.get(mem, index).ok_or(Error::InvalidIndex)?;
                at = header::push(out, at, name, value).ok_or(Error::OutFull)?;
                i += n;
            } else if b & 0xc0 == 0x40 {
                // section 6.2.1 literal with incremental indexing
                let (n, end) = self.literal(mem, rest, 6, out, at)?;
                self.insert(mem, &out[at..end]);
                at = end;
                i += n;
            } else if b & 0xe0 == 0x20 {
                // section 4.2, only before the first field
                if at != 0 {
                    return Err(Error::TableSizeExceeded);
                }
                let (max, n) = int(rest, 5)?;
                if max > u64::from(self.ceiling) {
                    return Err(Error::TableSizeExceeded);
                }
                self.set_max(mem, max as u32);
                i += n;
            } else {
                // section 6.2.2 and section 6.2.3, without indexing and never indexed
                let (n, end) = self.literal(mem, rest, 4, out, at)?;
                at = end;
                i += n;
            }
        }
        Ok(at)
    }

    /// 1-based, static then dynamic.
    pub fn get<'a>(&self, mem: &'a [u8], index: u64) -> Option<(&'a [u8], &'a [u8])> {
        let statics = STATIC_TABLE.len() as u64;
        if index == 0 {
            return None;
        }
        if index <= statics {
            let (n, v) = STATIC_TABLE[index as usize - 1];
            return Some((n.as_bytes(), v.as_bytes()));
        }
        let d = index - statics - 1;
        if d >= u64::from(self.count) || mem.len() < table_bytes(self.ceiling as usize) {
            return None;
        }
        let at = self.data() + self.offset(mem, d as usize);
        let names = at + RECORD;
        let values = names + mem[at] as usize;
        let end = values + u16::from_le_bytes([mem[at + 1], mem[at + 2]]) as usize;
        Some((&mem[names..values], &mem[values..end]))
    }

    /// Write a literal as a packed record at `at`. Returns (src used, record end).
    /// The name is copied to `out` first so it survives eviction by the insert.
    fn literal(
        &self,
        mem: &[u8],
        src: &[u8],
        prefix: u8,
        out: &mut [u8],
        at: usize,
    ) -> Result<(usize, usize), Error> {
        let (index, mut used) = int(src, prefix)?;
        let names = at
            .checked_add(RECORD)
            .filter(|n| *n <= out.len())
            .ok_or(Error::OutFull)?;
        let name_len = if index == 0 {
            let (n, len) = string(
                &src[used..],
                &mut out[names..],
                NAME_MAX,
                Error::NameTooLong,
            )?;
            used += n;
            len
        } else {
            let (name, _) = self.get(mem, index).ok_or(Error::InvalidIndex)?;
            out.get_mut(names..names + name.len())
                .ok_or(Error::OutFull)?
                .copy_from_slice(name);
            name.len()
        };
        let values = names + name_len;
        let (n, value_len) = string(&src[used..], &mut out[values..], VALUE_MAX, Error::OutFull)?;
        used += n;
        out[at] = name_len as u8;
        out[at + 1..names].copy_from_slice(&(value_len as u16).to_le_bytes());
        Ok((used, values + value_len))
    }

    fn slots(&self) -> usize {
        self.ceiling as usize / ENTRY_COST
    }

    fn data(&self) -> usize {
        self.slots() * OFFSET
    }

    /// Entry byte offset of the `i`th newest entry.
    fn offset(&self, mem: &[u8], i: usize) -> usize {
        let newest = self.newest as usize;
        let slot = if newest >= i {
            newest - i
        } else {
            newest + self.slots() - i
        };
        let p = slot * OFFSET;
        let stored = u32::from_le_bytes([mem[p], mem[p + 1], mem[p + 2], mem[p + 3]]);
        stored.wrapping_sub(self.shift) as usize
    }

    fn insert(&mut self, mem: &mut [u8], record: &[u8]) {
        let cost = record.len() - RECORD + ENTRY_COST;
        if cost > self.max as usize {
            // section 4.4, oversized entry empties the table
            self.count = 0;
            self.size = 0;
            return;
        }
        while self.size as usize + cost > self.max as usize {
            self.evict(mem);
        }
        let data = self.data();
        if self.count == 0 {
            self.head = 0;
            self.shift = 0;
            self.newest = 0;
        } else {
            if self.head as usize + record.len() > self.ceiling as usize {
                self.compact(mem);
            }
            self.newest = (self.newest + 1) % self.slots() as u32;
        }
        let at = data + self.head as usize;
        mem[at..at + record.len()].copy_from_slice(record);
        let p = self.newest as usize * OFFSET;
        mem[p..p + OFFSET].copy_from_slice(&self.head.wrapping_add(self.shift).to_le_bytes());
        self.head += record.len() as u32;
        self.count += 1;
        self.size += cost as u32;
    }

    /// Live records total `size` minus 29 per entry, so the next record fits after the move.
    fn compact(&mut self, mem: &mut [u8]) {
        let data = self.data();
        let tail = self.offset(mem, self.count as usize - 1);
        mem.copy_within(data + tail..data + self.head as usize, data);
        self.shift = self.shift.wrapping_add(tail as u32);
        self.head -= tail as u32;
    }

    /// Caller ensures an entry exists.
    fn evict(&mut self, mem: &[u8]) {
        let at = self.data() + self.offset(mem, self.count as usize - 1);
        let name_len = mem[at] as usize;
        let value_len = u16::from_le_bytes([mem[at + 1], mem[at + 2]]) as usize;
        self.size -= (name_len + value_len + ENTRY_COST) as u32;
        self.count -= 1;
    }

    fn set_max(&mut self, mem: &[u8], max: u32) {
        self.max = max;
        while self.size > self.max {
            self.evict(mem);
        }
    }
}

fn int(src: &[u8], prefix: u8) -> Result<(u64, usize), Error> {
    super::decode_int(src, prefix).map_err(|e| match e {
        super::Error::IntegerOverflow => Error::IntegerOverflow,
        _ => Error::Truncated,
    })
}

/// Returns (src used, bytes written). `over` is the error for exceeding `limit`
/// when `dst` itself had room.
fn string(src: &[u8], dst: &mut [u8], limit: usize, over: Error) -> Result<(usize, usize), Error> {
    let first = *src.first().ok_or(Error::Truncated)?;
    let (len, n) = int(src, 7)?;
    let len = usize::try_from(len).map_err(|_| Error::Truncated)?;
    let end = n.checked_add(len).ok_or(Error::Truncated)?;
    let body = src.get(n..end).ok_or(Error::Truncated)?;
    let full = if dst.len() > limit {
        over
    } else {
        Error::OutFull
    };
    let room = dst.len().min(limit);
    let dst = &mut dst[..room];
    if first & 0x80 != 0 {
        return Ok((end, huffman(body, dst, full)?));
    }
    dst.get_mut(..len).ok_or(full)?.copy_from_slice(body);
    Ok((end, len))
}

/// `full` is the error when `dst` runs out.
fn huffman(src: &[u8], dst: &mut [u8], full: Error) -> Result<usize, Error> {
    let mut d = Huff::new();
    let mut at = 0usize;
    for b in src {
        for nibble in [b >> 4, b & 0xf] {
            let decoded = d.nibble(nibble).map_err(|_| Error::InvalidHuffman)?;
            if let Some(v) = decoded {
                *dst.get_mut(at).ok_or(full)? = v;
                at += 1;
            }
        }
    }
    if !d.is_final() {
        return Err(Error::InvalidHuffman);
    }
    Ok(at)
}

/// Static index, static name with literal value, or full literal. Never indexes,
/// no Huffman. Returns the new `at`, `None` if it does not fit.
pub fn encode_field(out: &mut [u8], at: usize, name: &[u8], value: &[u8]) -> Option<usize> {
    let at = match static_lookup(name, value) {
        Static::Field(i) => return put_int(out, at, 7, 0x80, i),
        Static::Name(i) => put_int(out, at, 4, 0x00, i)?,
        Static::Miss => {
            let at = put_int(out, at, 4, 0x00, 0)?;
            put_string(out, at, name)?
        }
    };
    put_string(out, at, value)
}

pub fn encode_status(out: &mut [u8], at: usize, status: u16) -> Option<usize> {
    if !(100..=999).contains(&status) {
        return None;
    }
    let digits = [
        b'0' + (status / 100) as u8,
        b'0' + (status / 10 % 10) as u8,
        b'0' + (status % 10) as u8,
    ];
    encode_field(out, at, b":status", &digits)
}

enum Static {
    /// 1-based index matching name and value.
    Field(usize),
    /// First 1-based index matching only the name.
    Name(usize),
    Miss,
}

/// Same-name entries are contiguous, so the scan stops after the run.
fn static_lookup(name: &[u8], value: &[u8]) -> Static {
    let mut first = 0usize;
    for (i, (n, v)) in STATIC_TABLE.iter().enumerate() {
        if n.as_bytes() != name {
            if first != 0 {
                break;
            }
            continue;
        }
        if v.as_bytes() == value {
            return Static::Field(i + 1);
        }
        if first == 0 {
            first = i + 1;
        }
    }
    if first == 0 {
        Static::Miss
    } else {
        Static::Name(first)
    }
}

/// RFC 7541 section 5.1.
fn put_int(out: &mut [u8], at: usize, prefix: u8, first: u8, value: usize) -> Option<usize> {
    let mask = (1usize << prefix) - 1;
    if value < mask {
        *out.get_mut(at)? = first | value as u8;
        return Some(at + 1);
    }
    *out.get_mut(at)? = first | mask as u8;
    let mut at = at + 1;
    let mut rest = value - mask;
    while rest >= 128 {
        *out.get_mut(at)? = (rest & 0x7f) as u8 | 0x80;
        at += 1;
        rest >>= 7;
    }
    *out.get_mut(at)? = rest as u8;
    Some(at + 1)
}

/// Raw string literal, no Huffman.
fn put_string(out: &mut [u8], at: usize, s: &[u8]) -> Option<usize> {
    let at = put_int(out, at, 7, 0x00, s.len())?;
    let end = at.checked_add(s.len())?;
    out.get_mut(at..end)?.copy_from_slice(s);
    Some(end)
}

#[cfg(test)]
mod tests;
