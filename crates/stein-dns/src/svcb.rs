//! HTTPS record RDATA (RFC 9460 2.2), read for whether an origin speaks HTTP/3.
//! [`crate::Data::Raw`] carries it out of [`crate::decode`].

/// HTTPS record type (RFC 9460 14.3.1).
pub const HTTPS: u16 = 65;

/// SvcParamKeys known here (RFC 9460 14.3.2).
const KEY_MANDATORY: u16 = 0;
const KEY_ALPN: u16 = 1;
const KEY_NO_DEFAULT_ALPN: u16 = 2;
const KEY_PORT: u16 = 3;
const KEY_IPV4HINT: u16 = 4;
const KEY_ECH: u16 = 5;
const KEY_IPV6HINT: u16 = 6;

/// Why RDATA does not read. Such a record is skipped (RFC 9460 2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvcbError {
    /// RDATA ended inside the field at `at`.
    Truncated { at: usize },
    /// TargetName at `at` is not an uncompressed name.
    BadTarget { at: usize },
    /// Key not above the one before it.
    KeyOrder { key: u16 },
    /// Value of `key` is not what that key holds (section 7).
    BadValue { key: u16 },
    /// `mandatory` lists a key not known here (section 8).
    Mandatory { key: u16 },
}

impl core::fmt::Display for SvcbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SvcbError::Truncated { at } => {
                write!(f, "HTTPS record ends inside the field at byte {at}")
            }
            SvcbError::BadTarget { at } => {
                write!(f, "HTTPS record TargetName at byte {at} is not a name")
            }
            SvcbError::KeyOrder { key } => {
                write!(f, "HTTPS record SvcParamKey {key} out of order")
            }
            SvcbError::BadValue { key } => {
                write!(f, "HTTPS record SvcParamKey {key} has a malformed value")
            }
            SvcbError::Mandatory { key } => {
                write!(f, "HTTPS record makes unknown SvcParamKey {key} mandatory")
            }
        }
    }
}

impl core::error::Error for SvcbError {}

/// ALPN ids a record lists, as bits.
pub const ALPN_H3: u8 = 1;
pub const ALPN_H2: u8 = 2;
pub const ALPN_H1: u8 = 4;
pub const ALPN_OTHER: u8 = 8;

/// One HTTPS record, as far as a client reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Svcb {
    /// Zero is AliasMode, the rest ServiceMode by preference.
    pub priority: u16,
    /// TargetName offset in the RDATA and wire length. One byte is the root.
    pub target: (u16, u16),
    pub alpn: u8,
    pub no_default_alpn: bool,
    pub port: Option<u16>,
    /// Address hints, counted.
    pub ipv4hints: u8,
    pub ipv6hints: u8,
    pub ech: bool,
}

impl Svcb {
    /// TargetName is the root, which in ServiceMode is the owner (RFC 9460 2.5.2).
    pub fn target_is_root(&self) -> bool {
        self.target.1 == 1
    }

    /// Wire TargetName inside `rdata`, the RDATA this was parsed from.
    pub fn target<'a>(&self, rdata: &'a [u8]) -> &'a [u8] {
        let (at, len) = (self.target.0 as usize, self.target.1 as usize);
        rdata.get(at..at + len).unwrap_or(&[])
    }
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, SvcbError> {
    match b.get(at..at + 2) {
        Some(v) => Ok(u16::from_be_bytes([v[0], v[1]])),
        None => Err(SvcbError::Truncated { at }),
    }
}

/// TargetName at 2, uncompressed (RFC 9460 2.2, RFC 3597 4). Returns its end.
fn target_end(rdata: &[u8]) -> Result<usize, SvcbError> {
    let mut at = 2;
    loop {
        let Some(&len) = rdata.get(at) else {
            return Err(SvcbError::Truncated { at });
        };
        if len & 0xC0 != 0 {
            return Err(SvcbError::BadTarget { at });
        }
        at += 1 + len as usize;
        if at - 2 > 255 {
            return Err(SvcbError::BadTarget { at: 2 });
        }
        if len == 0 {
            break;
        }
    }
    if at > rdata.len() {
        return Err(SvcbError::Truncated { at: rdata.len() });
    }
    Ok(at)
}

/// Length prefixed ALPN ids (section 7.1.1) as bits.
fn alpn_bits(value: &[u8], key: u16) -> Result<u8, SvcbError> {
    let bad = SvcbError::BadValue { key };
    if value.is_empty() {
        return Err(bad);
    }
    let (mut bits, mut i) = (0, 0);
    while i < value.len() {
        let n = value[i] as usize;
        let Some(id) = value.get(i + 1..i + 1 + n).filter(|_| n > 0) else {
            return Err(bad);
        };
        bits |= match id {
            b"h3" => ALPN_H3,
            b"h2" => ALPN_H2,
            b"http/1.1" => ALPN_H1,
            _ => ALPN_OTHER,
        };
        i += 1 + n;
    }
    Ok(bits)
}

/// One SvcParam into `svcb`. Returns the `mandatory` value span when it is that key.
fn param(
    svcb: &mut Svcb,
    key: u16,
    from: usize,
    value: &[u8],
) -> Result<Option<(usize, usize)>, SvcbError> {
    let bad = SvcbError::BadValue { key };
    let len = value.len();
    match key {
        // Keys two bytes each (section 8).
        KEY_MANDATORY if len == 0 || !len.is_multiple_of(2) => return Err(bad),
        KEY_MANDATORY => return Ok(Some((from, len))),
        KEY_ALPN => svcb.alpn |= alpn_bits(value, key)?,
        KEY_NO_DEFAULT_ALPN if len == 0 => svcb.no_default_alpn = true,
        KEY_PORT if len == 2 => svcb.port = Some(u16::from_be_bytes([value[0], value[1]])),
        KEY_IPV4HINT if len > 0 && len.is_multiple_of(4) => {
            svcb.ipv4hints = (len / 4).min(255) as u8
        }
        KEY_IPV6HINT if len > 0 && len.is_multiple_of(16) => {
            svcb.ipv6hints = (len / 16).min(255) as u8
        }
        // ECHConfigList, a flag only.
        KEY_ECH => svcb.ech = len > 0,
        KEY_NO_DEFAULT_ALPN | KEY_PORT | KEY_IPV4HINT | KEY_IPV6HINT => return Err(bad),
        _ => {}
    }
    Ok(None)
}

/// Read the RDATA of one HTTPS record (RFC 9460 2.2).
pub fn parse(rdata: &[u8]) -> Result<Svcb, SvcbError> {
    let priority = u16_at(rdata, 0)?;
    let mut at = target_end(rdata)?;
    let mut svcb = Svcb {
        priority,
        target: (2, (at - 2) as u16),
        alpn: 0,
        no_default_alpn: false,
        port: None,
        ipv4hints: 0,
        ipv6hints: 0,
        ech: false,
    };
    let mut last: Option<u16> = None;
    let mut mandatory = (0, 0);
    while at < rdata.len() {
        let key = u16_at(rdata, at)?;
        let len = u16_at(rdata, at + 2)? as usize;
        let from = at + 4;
        let Some(value) = rdata.get(from..from + len) else {
            return Err(SvcbError::Truncated { at: from });
        };
        // Strictly increasing keys (section 2.2).
        if last.is_some_and(|l| key <= l) {
            return Err(SvcbError::KeyOrder { key });
        }
        last = Some(key);
        if let Some(span) = param(&mut svcb, key, from, value)? {
            mandatory = span;
        }
        at = from + len;
    }
    // Every key `mandatory` names is known and not itself (section 8).
    let (from, len) = mandatory;
    for i in (from..from + len).step_by(2) {
        let key = u16_at(rdata, i)?;
        if key == KEY_MANDATORY {
            return Err(SvcbError::BadValue { key: KEY_MANDATORY });
        }
        if key > KEY_IPV6HINT {
            return Err(SvcbError::Mandatory { key });
        }
    }
    // `no-default-alpn` needs `alpn` (section 7.1.1).
    if svcb.no_default_alpn && svcb.alpn == 0 {
        return Err(SvcbError::BadValue {
            key: KEY_NO_DEFAULT_ALPN,
        });
    }
    Ok(svcb)
}

/// Wire name as dotted text into `out`. `None` for the root or a name that does not fit.
pub fn dotted(wire: &[u8], out: &mut [u8; 253]) -> Option<u8> {
    let (mut at, mut n) = (0, 0);
    while let Some(&len) = wire.get(at) {
        if len == 0 {
            break;
        }
        let label = wire.get(at + 1..at + 1 + len as usize)?;
        if n > 0 {
            *out.get_mut(n)? = b'.';
            n += 1;
        }
        out.get_mut(n..n + label.len())?.copy_from_slice(label);
        n += label.len();
        at += 1 + len as usize;
    }
    (n > 0).then_some(n as u8)
}

#[cfg(test)]
mod tests;
