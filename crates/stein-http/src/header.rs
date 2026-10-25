//! Outbound header validation and the packed response header store.
//!
//! One region per request slot, records back to back, scanned linearly.
//!
//! ```text
//! [name_len: u8][value_len: u16 LE][name][value]
//! ```

/// Record header bytes.
const OVERHEAD: usize = 3;

/// A header to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

impl<'a> Header<'a> {
    pub const fn new(name: &'a str, value: &'a str) -> Self {
        Header { name, value }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// CR or LF in a name or value.
    Injection,
    /// Name is not an RFC 9110 token.
    BadName,
    BadValue,
    /// See [`RESERVED`].
    Reserved,
    /// Empty method, path or host, or one with whitespace or control bytes.
    BadTarget,
}

/// Headers the crate writes itself. Caller values are refused, never merged.
pub const RESERVED: [&str; 4] = ["host", "content-length", "transfer-encoding", "connection"];

/// Validate outbound headers, failing on the first bad one.
pub fn check(headers: &[Header<'_>]) -> Result<(), HeaderError> {
    for h in headers {
        check_one(h)?;
    }
    Ok(())
}

fn check_one(h: &Header<'_>) -> Result<(), HeaderError> {
    check_field(h.name.as_bytes(), h.value.as_bytes())?;
    if RESERVED.iter().any(|r| h.name.eq_ignore_ascii_case(r)) {
        return Err(HeaderError::Reserved);
    }
    Ok(())
}

/// Validate one field. Does not check [`RESERVED`].
pub fn check_field(name: &[u8], value: &[u8]) -> Result<(), HeaderError> {
    if splits_line(name) || splits_line(value) {
        return Err(HeaderError::Injection);
    }
    if name.is_empty() || !name.iter().all(|b| is_token(*b)) {
        return Err(HeaderError::BadName);
    }
    if !value.iter().all(|b| is_field_value(*b)) {
        return Err(HeaderError::BadValue);
    }
    Ok(())
}

fn splits_line(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| *b == b'\r' || *b == b'\n')
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Visible ASCII, space, tab and obs-text.
fn is_field_value(b: u8) -> bool {
    b == b'\t' || (0x20..=0x7e).contains(&b) || b >= 0x80
}

/// Refuse a method, path or host that is empty or holds whitespace or control bytes.
pub fn check_target(method: &str, path: &str, host: &str) -> Result<(), HeaderError> {
    if bad_target(method) || bad_target(path) || bad_target(host) {
        return Err(HeaderError::BadTarget);
    }
    Ok(())
}

fn bad_target(s: &str) -> bool {
    s.is_empty() || s.bytes().any(|b| b < 0x21 || b == 0x7f)
}

/// Append one record at `at` and return its end. `None` if it or a length field overflows.
pub fn push(out: &mut [u8], at: usize, name: &[u8], value: &[u8]) -> Option<usize> {
    if name.len() > u8::MAX as usize || value.len() > u16::MAX as usize {
        return None;
    }
    let names = at.checked_add(OVERHEAD)?;
    let values = names.checked_add(name.len())?;
    let end = values.checked_add(value.len())?;
    if end > out.len() {
        return None;
    }
    out[at] = name.len() as u8;
    let vl = (value.len() as u16).to_le_bytes();
    out[at + 1] = vl[0];
    out[at + 2] = vl[1];
    out[names..values].copy_from_slice(name);
    out[values..end].copy_from_slice(value);
    Some(end)
}

/// `(name, value)` borrowed from the region.
pub type Pair<'a> = (&'a [u8], &'a [u8]);

/// Record at `at` and the next offset. `None` at the end or on a truncated tail.
fn read(packed: &[u8], at: usize) -> Option<(Pair<'_>, usize)> {
    if at + OVERHEAD > packed.len() {
        return None;
    }
    let nl = packed[at] as usize;
    let vl = u16::from_le_bytes([packed[at + 1], packed[at + 2]]) as usize;
    let names = at + OVERHEAD;
    let values = names + nl;
    let end = values + vl;
    if end > packed.len() {
        return None;
    }
    Some(((&packed[names..values], &packed[values..end]), end))
}

/// Yields records in wire order, duplicates included.
pub struct Iter<'a> {
    packed: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Pair<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let (record, next) = read(self.packed, self.at)?;
        self.at = next;
        Some(record)
    }
}

pub fn iter(packed: &[u8]) -> Iter<'_> {
    Iter { packed, at: 0 }
}

/// First value for `name`, case insensitive. No comma joining, use [`iter`] for all.
pub fn find<'a>(packed: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let want = name.as_bytes();
    iter(packed)
        .find(|(n, _)| eq_ignore_case(n, want))
        .map(|(_, v)| v)
}

pub fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// `header_bytes` must divide evenly across request slots.
    Ragged { bytes: usize, requests: usize },
    /// Region exceeds `u16::MAX`.
    RegionTooLarge(usize),
}

/// One packed header region per request slot. Not cleared on reuse, rows track their own length.
pub struct Store<'a> {
    bytes: &'a mut [u8],
    per_request: usize,
}

impl<'a> Store<'a> {
    pub fn new(bytes: &'a mut [u8], requests: usize) -> Result<Self, StoreError> {
        let ragged = StoreError::Ragged {
            bytes: bytes.len(),
            requests,
        };
        if requests == 0 {
            return empty_store(bytes, ragged);
        }
        if !bytes.len().is_multiple_of(requests) {
            return Err(ragged);
        }
        let per_request = bytes.len() / requests;
        if per_request > u16::MAX as usize {
            return Err(StoreError::RegionTooLarge(per_request));
        }
        Ok(Store { bytes, per_request })
    }

    pub fn per_request(&self) -> usize {
        self.per_request
    }

    /// Total bytes held.
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn region(&self, slot: u32) -> &[u8] {
        let (at, end) = self.range(slot);
        &self.bytes[at..end]
    }

    pub fn region_mut(&mut self, slot: u32) -> &mut [u8] {
        let (at, end) = self.range(slot);
        &mut self.bytes[at..end]
    }

    /// Empty for an out-of-range slot.
    fn range(&self, slot: u32) -> (usize, usize) {
        let at = (slot as usize).saturating_mul(self.per_request);
        let end = at.saturating_add(self.per_request);
        if end > self.bytes.len() {
            return (0, 0);
        }
        (at, end)
    }
}

/// Zero slots requires empty storage.
fn empty_store(bytes: &mut [u8], ragged: StoreError) -> Result<Store<'_>, StoreError> {
    if bytes.is_empty() {
        return Ok(Store {
            bytes,
            per_request: 0,
        });
    }
    Err(ragged)
}
