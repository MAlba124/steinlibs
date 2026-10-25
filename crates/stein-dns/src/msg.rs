//! The wire format.
//!
//! RFC 1035 section 4 for the message, RFC 6891 for EDNS0. A query is written
//! into a caller's buffer; a reply is taken apart into a caller's tables and
//! nothing is followed, retried or cached here.

use crate::{DnsError, ErrorKind};

/// Header length: id, flags, and the four section counts.
pub const HEADER: usize = 12;

/// The longest a name may be on the wire, length bytes and the root label
/// included.
pub const MAX_NAME: usize = 255;

/// The longest one label may be. A length byte reserves its top two bits for
/// the label type, so 63 is all it can express.
pub const MAX_LABEL: usize = 63;

/// How many compression jumps one name may take before the message is refused.
///
/// A real compressor never needs more than a handful: each jump lands on a
/// suffix of a name written earlier, and suffix chains are short. The name
/// reader documents why a cap is needed on top of the ordering rule.
pub const MAX_JUMPS: u32 = 16;

/// The top two bits of a length byte, both set, mean a compression pointer.
const PTR: u8 = 0xC0;

/// The remaining 14 bits of a pointer are the offset it resolves to.
const PTR_OFFSET: u16 = 0x3FFF;

/// Bytes of one OPT record carrying no options: root name, type, class, ttl
/// and a zero rdlength.
pub const OPT_LEN: usize = 11;

/// The largest query this can emit: header, longest name, qtype, qclass, OPT.
pub const QUERY_MAX: usize = HEADER + MAX_NAME + 4 + OPT_LEN;

/// What a query advertises as its receive size when the caller does not say.
///
/// 1232 is the DNS Flag Day 2020 figure: the 1280 byte IPv6 minimum MTU less
/// 48 bytes of IPv6 and UDP header, so a response of that size never has to
/// fragment. Fragmented DNS is both unreliable and a spoofing aid.
pub const DEFAULT_UDP_PAYLOAD: u16 = 1232;

// Types, classes and codes.

/// An RR type. A newtype rather than an enum because the registry is open and
/// an unknown type has to survive a round trip unharmed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Type(pub u16);

impl Type {
    pub const A: Type = Type(1);
    pub const NS: Type = Type(2);
    pub const CNAME: Type = Type(5);
    pub const SOA: Type = Type(6);
    pub const PTR: Type = Type(12);
    pub const MX: Type = Type(15);
    pub const TXT: Type = Type(16);
    pub const AAAA: Type = Type(28);
    /// EDNS0's pseudo record, which lives in the additional section and is not
    /// a record about a name.
    pub const OPT: Type = Type(41);
}

/// An RR class. Only `IN` ever appears on the public internet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class(pub u16);

impl Class {
    pub const IN: Class = Class(1);
}

/// The response code, extended to 12 bits when an OPT record supplies the
/// upper 8.
///
/// The three a driver has to tell apart: [`Rcode::NoError`] is an answer,
/// [`Rcode::NxDomain`] is a definitive "no such host" and can be cached as
/// such, [`Rcode::ServFail`] is this server failing and is worth asking
/// somebody else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rcode {
    #[default]
    NoError,
    /// The server could not parse the query.
    FormErr,
    /// The server failed. Try another one.
    ServFail,
    /// The name does not exist. Not a failure to retry.
    NxDomain,
    NotImp,
    Refused,
    /// The OPT version offered is not supported, RFC 6891.
    BadVers,
    Other(u16),
}

impl Rcode {
    #[inline]
    pub fn from_bits(bits: u16) -> Rcode {
        match bits {
            0 => Rcode::NoError,
            1 => Rcode::FormErr,
            2 => Rcode::ServFail,
            3 => Rcode::NxDomain,
            4 => Rcode::NotImp,
            5 => Rcode::Refused,
            16 => Rcode::BadVers,
            other => Rcode::Other(other),
        }
    }

    #[inline]
    pub fn bits(self) -> u16 {
        match self {
            Rcode::NoError => 0,
            Rcode::FormErr => 1,
            Rcode::ServFail => 2,
            Rcode::NxDomain => 3,
            Rcode::NotImp => 4,
            Rcode::Refused => 5,
            Rcode::BadVers => 16,
            Rcode::Other(other) => other,
        }
    }
}

/// The second header word, kept whole so nothing is lost in translation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u16);

impl Flags {
    /// Bit 15, set on a response.
    pub const QR: u16 = 1 << 15;
    /// Bit 10, the answer comes from the zone's own server.
    pub const AA: u16 = 1 << 10;
    /// Bit 9, the reply did not fit and has to be retried over TCP.
    pub const TC: u16 = 1 << 9;
    /// Bit 8, the query asks the server to recurse.
    pub const RD: u16 = 1 << 8;
    /// Bit 7, the server is willing to recurse.
    pub const RA: u16 = 1 << 7;

    #[inline]
    pub fn is_response(self) -> bool {
        self.0 & Flags::QR != 0
    }

    /// Bits 11 to 14. Zero is a standard query, and nothing else is used here.
    #[inline]
    pub fn opcode(self) -> u8 {
        ((self.0 >> 11) & 0xF) as u8
    }

    #[inline]
    pub fn authoritative(self) -> bool {
        self.0 & Flags::AA != 0
    }

    /// Whether the reply was cut short. The records that did arrive are real,
    /// but the set is incomplete, so a caller must not read "no AAAA record"
    /// out of a truncated reply.
    #[inline]
    pub fn truncated(self) -> bool {
        self.0 & Flags::TC != 0
    }

    #[inline]
    pub fn recursion_desired(self) -> bool {
        self.0 & Flags::RD != 0
    }

    #[inline]
    pub fn recursion_available(self) -> bool {
        self.0 & Flags::RA != 0
    }

    /// Bits 0 to 3, before any EDNS0 extension.
    #[inline]
    pub fn rcode_bits(self) -> u16 {
        self.0 & 0xF
    }
}

// Spans.

/// A materialised name, as an offset and a length in the name arena.
///
/// Names cannot be spans of the message: compression means the bytes of one
/// name can be scattered across it. So a decode copies each name into the
/// arena in dotted form, without the trailing root dot, case as it arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Name {
    pub at: u16,
    pub len: u16,
}

impl Name {
    pub const EMPTY: Name = Name { at: 0, len: 0 };

    /// The bytes, or empty when the span does not fit the arena. A `Name` only
    /// ever comes from the arena it is read against, so the fallback is
    /// unreachable in practice and keeps the API total.
    #[inline]
    pub fn bytes<'n>(&self, names: &'n [u8]) -> &'n [u8] {
        let at = usize::from(self.at);
        names.get(at..at + usize::from(self.len)).unwrap_or(&[])
    }

    /// True for the root, which is what an OPT record's owner name must be.
    #[inline]
    pub fn is_root(&self) -> bool {
        self.len == 0
    }
}

/// A run of bytes in the message itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Raw {
    pub at: u16,
    pub len: u16,
}

impl Raw {
    pub const EMPTY: Raw = Raw { at: 0, len: 0 };

    #[inline]
    pub fn bytes<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        let at = usize::from(self.at);
        src.get(at..at + usize::from(self.len)).unwrap_or(&[])
    }
}

// Records.

/// The payload of one record, as far as this crate understands it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Data {
    A([u8; 4]),
    Aaaa([u8; 16]),
    /// The target of a `CNAME`, `NS` or `PTR`, materialised like any name
    /// because it may itself be compressed.
    Name(Name),
    /// Anything else, left where it lies. Nothing here needs to understand
    /// `SOA` or `TXT`, and guessing at a type would only invent errors.
    Raw(Raw),
}

impl Default for Data {
    fn default() -> Data {
        Data::Raw(Raw::EMPTY)
    }
}

/// One resource record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// The owner name, which for a `CNAME` chain is the previous link.
    pub name: Name,
    pub kind: Type,
    pub class: Class,
    pub ttl: u32,
    pub data: Data,
}

/// The question a reply echoes back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Question {
    pub name: Name,
    pub kind: Type,
    pub class: Class,
    /// The name exactly as it sat on the wire, root label included, so a
    /// driver randomising label case can compare the bytes it sent.
    pub raw: Raw,
}

/// What an OPT record carried. Its class and ttl fields are reused by EDNS0
/// for things that have nothing to do with classes or time to live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opt {
    /// The sender's largest reassembled UDP payload, from the class field.
    pub udp_payload: u16,
    /// The upper 8 bits of the 12 bit rcode.
    pub ext_rcode: u8,
    /// EDNS version. Only 0 is defined.
    pub version: u8,
    /// Bit 15 is DO, the DNSSEC OK bit. The rest are reserved.
    pub flags: u16,
    /// The option list, unparsed but length checked.
    pub options: Raw,
}

/// The 12 byte header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub id: u16,
    pub flags: Flags,
    /// Question, answer, authority and additional counts, as claimed.
    pub counts: [u16; 4],
}

/// A decoded reply. Records live in the caller's table, names in the caller's
/// arena, and this says how they are laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// The id the responder echoed. The driver must check it against the id it
    /// sent, because this crate never saw the query.
    pub id: u16,
    pub flags: Flags,
    /// The full 12 bit code, OPT's extension folded in.
    pub rcode: Rcode,
    /// The echoed question, absent only when the count was zero, which happens
    /// on a `FormErr` from a server that could not parse the query.
    pub question: Option<Question>,
    /// Present when the responder sent an OPT record.
    pub edns: Option<Opt>,
    /// Records stored, per section, laid out in the table in this order. These
    /// are counts of what was stored, which for the additional section is the
    /// claimed count less any OPT record, since OPT is reported as [`edns`].
    ///
    /// [`edns`]: Reply::edns
    pub answers: u16,
    pub authorities: u16,
    pub additionals: u16,
    /// Bytes of the name arena in use.
    pub names_len: u16,
}

impl Reply {
    /// The reply did not fit in a datagram and has to be asked again over TCP.
    /// The records present are genuine, but the set is not complete.
    #[inline]
    pub fn is_truncated(&self) -> bool {
        self.flags.truncated()
    }

    /// The answer section. A `CNAME` chain arrives here whole and unfollowed.
    #[inline]
    pub fn answers<'r>(&self, records: &'r [Record]) -> &'r [Record] {
        records.get(..usize::from(self.answers)).unwrap_or(&[])
    }

    #[inline]
    pub fn authorities<'r>(&self, records: &'r [Record]) -> &'r [Record] {
        let at = usize::from(self.answers);
        records
            .get(at..at + usize::from(self.authorities))
            .unwrap_or(&[])
    }

    #[inline]
    pub fn additionals<'r>(&self, records: &'r [Record]) -> &'r [Record] {
        let at = usize::from(self.answers) + usize::from(self.authorities);
        records
            .get(at..at + usize::from(self.additionals))
            .unwrap_or(&[])
    }

    /// The question's name, or empty when there was no question.
    #[inline]
    pub fn question_name<'n>(&self, names: &'n [u8]) -> &'n [u8] {
        match self.question {
            Some(q) => q.name.bytes(names),
            None => &[],
        }
    }
}

/// The tables a decode writes into. Capacity is a ceiling and never grows, so
/// a response cannot make this allocate or spin.
///
/// The arena needs room for every name the reply mentions, each up to 253
/// bytes. Two records naming the same host store it twice; deduplicating would
/// cost a scan per name to save bytes nobody is short of.
pub struct Storage<'s> {
    pub records: &'s mut [Record],
    pub names: &'s mut [u8],
}

impl<'s> Storage<'s> {
    #[inline]
    pub fn new(records: &'s mut [Record], names: &'s mut [u8]) -> Storage<'s> {
        Storage { records, names }
    }
}

// Encoding.

/// A query to write. Everything a stub resolver varies, and nothing else.
#[derive(Clone, Copy, Debug)]
pub struct Query<'q> {
    /// A dotted name. A trailing dot is allowed and means the same thing.
    /// Labels must already be punycode: this does not do IDNA.
    pub name: &'q str,
    pub kind: Type,
    pub class: Class,
    /// Echoed in the reply, and the only thing tying the two together. It must
    /// be unpredictable, which is the caller's problem since this crate has no
    /// entropy.
    pub id: u16,
    /// The RD bit. A stub resolver talking to a recursive server wants it set.
    pub recursion_desired: bool,
    /// Advertise an EDNS0 receive size, or `None` to send no OPT record.
    ///
    /// Without EDNS0 a responder may not send more than 512 bytes, so anything
    /// larger comes back truncated and costs a TCP round trip. Sending OPT is
    /// the default for that reason.
    pub udp_payload: Option<u16>,
}

impl<'q> Query<'q> {
    /// A recursive query with EDNS0 at the default size.
    #[inline]
    pub fn new(name: &'q str, kind: Type, id: u16) -> Query<'q> {
        Query {
            name,
            kind,
            class: Class::IN,
            id,
            recursion_desired: true,
            udp_payload: Some(DEFAULT_UDP_PAYLOAD),
        }
    }
}

/// Write `q` into `out` and report how many bytes it took.
pub fn encode(q: &Query<'_>, out: &mut [u8]) -> Result<usize, DnsError> {
    let flags = if q.recursion_desired { Flags::RD } else { 0 };
    let additional = u16::from(q.udp_payload.is_some());
    let mut at = put_u16(out, 0, q.id)?;
    at = put_u16(out, at, flags)?;
    at = put_u16(out, at, 1)?;
    at = put_u16(out, at, 0)?;
    at = put_u16(out, at, 0)?;
    at = put_u16(out, at, additional)?;
    at = put_name(out, at, q.name)?;
    at = put_u16(out, at, q.kind.0)?;
    at = put_u16(out, at, q.class.0)?;
    match q.udp_payload {
        Some(size) => put_opt(out, at, size),
        None => Ok(at),
    }
}

/// Write a recursive `IN` query with EDNS0 at the default receive size.
#[inline]
pub fn encode_query(name: &str, kind: Type, id: u16, out: &mut [u8]) -> Result<usize, DnsError> {
    encode(&Query::new(name, kind, id), out)
}

/// An OPT record with no options. Its class field carries the receive size and
/// its ttl field carries the extended rcode, version and flags, all zero for a
/// plain EDNS0 query with no DNSSEC.
fn put_opt(out: &mut [u8], at: usize, size: u16) -> Result<usize, DnsError> {
    let end = at + OPT_LEN;
    let dst = out
        .get_mut(at..end)
        .ok_or_else(|| DnsError::new(ErrorKind::OutputFull, at))?;
    // Root name, then type and class. The ttl and rdlength stay zero: no
    // extended rcode, version 0, DO clear, no options.
    dst.fill(0);
    let i = put_u16(dst, 1, Type::OPT.0)?;
    put_u16(dst, i, size)?;
    Ok(end)
}

fn put_u16(out: &mut [u8], at: usize, v: u16) -> Result<usize, DnsError> {
    let end = at + 2;
    let dst = out
        .get_mut(at..end)
        .ok_or_else(|| DnsError::new(ErrorKind::OutputFull, at))?;
    dst.copy_from_slice(&v.to_be_bytes());
    Ok(end)
}

/// Write a dotted name as length prefixed labels plus a root byte.
fn put_name(out: &mut [u8], at: usize, name: &str) -> Result<usize, DnsError> {
    let bytes = trim_root(name.as_bytes());
    // The root is the one name with no labels, and splitting it would yield an
    // empty label instead of nothing.
    if bytes.is_empty() {
        return put_root(out, at);
    }
    let mut cur = at;
    // Starts at 1 for the root byte, which every name pays for.
    let mut wire = 1usize;
    for label in bytes.split(|&b| b == b'.') {
        wire += 1 + label.len();
        if wire > MAX_NAME {
            return Err(DnsError::new(ErrorKind::BadName, cur));
        }
        cur = put_label(out, cur, label)?;
    }
    put_root(out, cur)
}

fn put_root(out: &mut [u8], at: usize) -> Result<usize, DnsError> {
    let root = out
        .get_mut(at)
        .ok_or_else(|| DnsError::new(ErrorKind::OutputFull, at))?;
    *root = 0;
    Ok(at + 1)
}

fn put_label(out: &mut [u8], at: usize, label: &[u8]) -> Result<usize, DnsError> {
    // An empty label is only legal as the root, which is written separately.
    if label.is_empty() || label.len() > MAX_LABEL {
        return Err(DnsError::new(ErrorKind::BadName, at));
    }
    // Refuses spaces, control bytes and anything above ASCII. A label may hold
    // arbitrary bytes in principle, but a hostname that needs them is either a
    // mistake or an injection, and IDNA is the caller's job.
    if !label.iter().all(u8::is_ascii_graphic) {
        return Err(DnsError::new(ErrorKind::BadName, at));
    }
    let end = at + 1 + label.len();
    let dst = out
        .get_mut(at..end)
        .ok_or_else(|| DnsError::new(ErrorKind::OutputFull, at))?;
    let Some((head, tail)) = dst.split_first_mut() else {
        return Err(DnsError::new(ErrorKind::OutputFull, at));
    };
    *head = label.len() as u8;
    tail.copy_from_slice(label);
    Ok(end)
}

/// Drop one trailing root dot. `example.com.` and `example.com` are the same
/// name, and the empty string is the root itself.
#[inline]
fn trim_root(name: &[u8]) -> &[u8] {
    match name.split_last() {
        Some((b'.', head)) => head,
        _ => name,
    }
}

// Decoding.

fn be16(src: &[u8], at: usize) -> Result<u16, DnsError> {
    let Some(v) = src.get(at..).and_then(|s| s.first_chunk::<2>()) else {
        return Err(DnsError::new(ErrorKind::Truncated, at));
    };
    Ok(u16::from_be_bytes(*v))
}

fn be32(src: &[u8], at: usize) -> Result<u32, DnsError> {
    let Some(v) = src.get(at..).and_then(|s| s.first_chunk::<4>()) else {
        return Err(DnsError::new(ErrorKind::Truncated, at));
    };
    Ok(u32::from_be_bytes(*v))
}

/// Read the header alone, so a driver can check the id and the QR bit before
/// deciding whether a datagram is worth decoding.
pub fn decode_header(src: &[u8]) -> Result<Header, DnsError> {
    let Some(head) = src.first_chunk::<HEADER>() else {
        return Err(DnsError::new(ErrorKind::Truncated, src.len()));
    };
    Ok(Header {
        id: u16::from_be_bytes([head[0], head[1]]),
        flags: Flags(u16::from_be_bytes([head[2], head[3]])),
        counts: [
            u16::from_be_bytes([head[4], head[5]]),
            u16::from_be_bytes([head[6], head[7]]),
            u16::from_be_bytes([head[8], head[9]]),
            u16::from_be_bytes([head[10], head[11]]),
        ],
    })
}

/// The name arena, filled front to back.
struct Arena<'n> {
    buf: &'n mut [u8],
    used: usize,
}

impl<'n> Arena<'n> {
    fn new(buf: &'n mut [u8]) -> Arena<'n> {
        Arena { buf, used: 0 }
    }

    fn push(&mut self, b: u8, at: usize) -> Result<(), DnsError> {
        self.extend(&[b], at)
    }

    fn extend(&mut self, bytes: &[u8], at: usize) -> Result<(), DnsError> {
        let end = self.used + bytes.len();
        // Offsets are u16 because a message is, and a name span has to fit one.
        if end > usize::from(u16::MAX) {
            return Err(DnsError::new(ErrorKind::NamesFull, at));
        }
        let dst = self
            .buf
            .get_mut(self.used..end)
            .ok_or_else(|| DnsError::new(ErrorKind::NamesFull, at))?;
        dst.copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }
}

/// Materialise the name starting at `at` into `arena`, and report where it
/// ended in the message.
///
/// The end is one past the terminating root byte, or one past the first
/// compression pointer, because a pointer is where the name stops occupying
/// bytes of its own.
///
/// # Compression, and why three bounds
///
/// A length byte with both top bits set is a pointer: the low 14 bits are an
/// offset from the start of the message where the name continues. A response
/// is attacker controlled, so a pointer may point at itself, at a pointer that
/// points back, or at a ladder of thousands of pointers. Three separate bounds
/// hold here, and each one alone is not enough.
///
/// 1. Every jump must land strictly below `bound`, which starts at the name's
///    own offset and drops to each target taken. So the targets of one name
///    strictly decrease and no offset can be visited twice. Note that the
///    weaker and more common rule, "the target is below the pointer's own
///    offset", does NOT terminate: a pointer at 120 to 110 is backwards, and
///    if the labels at 110 run up to 120 again the name loops forever. The
///    bound has to be the lowest point reached, not the current one.
/// 2. At most [`MAX_JUMPS`] jumps. Rule 1 alone bounds a name to as many jumps
///    as the message has bytes, which is 16384 for a 64 KiB TCP reply, and a
///    reply full of such names costs quadratic time. A chain of pointers that
///    each point at another pointer writes no label bytes at all, so the
///    length limits below never fire on it. This is the only bound that makes
///    the work per name constant.
/// 3. The wire length limits, 63 per label and 255 per name, bound the bytes
///    written into the arena regardless of how the name was assembled.
///
/// Rule 1 also rejects a forward pointer outright rather than resolving it. A
/// real compressor only ever points at a name it has already emitted, and
/// along a suffix chain those targets decrease, so nothing legitimate is
/// refused. A pointer into the header is refused for the same reason: the
/// first 12 bytes are not a name.
fn read_name(src: &[u8], at: usize, arena: &mut Arena<'_>) -> Result<(Name, usize), DnsError> {
    let start = arena.used;
    let mut cur = at;
    let mut bound = at;
    let mut jumps = 0u32;
    let mut wire = 1usize;
    // Set by whichever comes first, the root byte or the first pointer.
    let mut end = None;
    let stop = loop {
        let Some(&len) = src.get(cur) else {
            return Err(DnsError::new(ErrorKind::Truncated, cur));
        };
        if len & PTR == PTR {
            end.get_or_insert(cur + 2);
            cur = take_jump(src, cur, &mut bound, &mut jumps)?;
            continue;
        }
        if len & PTR != 0 {
            // RFC 1035 section 4.1.4 reserves the 01 and 10 label types, so
            // there is no way to know how many bytes one of them would span.
            return Err(DnsError::new(ErrorKind::BadName, cur));
        }
        if len == 0 {
            break end.unwrap_or(cur + 1);
        }
        cur = read_label(src, cur, usize::from(len), arena, start, &mut wire)?;
    };
    let name = Name {
        at: start as u16,
        len: (arena.used - start) as u16,
    };
    Ok((name, stop))
}

/// Follow one compression pointer, holding both bounds. `bound` drops to each
/// target so the targets of one name strictly decrease.
fn take_jump(
    src: &[u8],
    cur: usize,
    bound: &mut usize,
    jumps: &mut u32,
) -> Result<usize, DnsError> {
    let target = usize::from(be16(src, cur)? & PTR_OFFSET);
    if target >= *bound || target < HEADER {
        return Err(DnsError::new(ErrorKind::BadPointer, cur));
    }
    *jumps += 1;
    if *jumps > MAX_JUMPS {
        return Err(DnsError::new(ErrorKind::BadPointer, cur));
    }
    *bound = target;
    Ok(target)
}

/// Copy one label into the arena, dot separated, and report where it ended.
fn read_label(
    src: &[u8],
    at: usize,
    len: usize,
    arena: &mut Arena<'_>,
    start: usize,
    wire: &mut usize,
) -> Result<usize, DnsError> {
    *wire += 1 + len;
    if *wire > MAX_NAME {
        return Err(DnsError::new(ErrorKind::BadName, at));
    }
    let end = at + 1 + len;
    let Some(label) = src.get(at + 1..end) else {
        return Err(DnsError::new(ErrorKind::Truncated, at + 1));
    };
    if arena.used > start {
        arena.push(b'.', at)?;
    }
    arena.extend(label, at)?;
    Ok(end)
}

/// Decode a reply into `into`.
///
/// Trailing bytes after the last counted record are ignored, not refused: a
/// signed or padded reply carries them, and nothing here reads them.
pub fn decode(src: &[u8], into: &mut Storage<'_>) -> Result<Reply, DnsError> {
    if src.len() > usize::from(u16::MAX) {
        return Err(DnsError::new(ErrorKind::TooLong, u16::MAX as usize));
    }
    let head = decode_header(src)?;
    if !head.flags.is_response() {
        return Err(DnsError::new(ErrorKind::NotAResponse, 2));
    }
    // A reply to a single question echoes exactly that one. Accepting several
    // would mean one of them could match while another asked for something
    // else, and the driver would only ever check the first.
    if head.counts[0] > 1 {
        return Err(DnsError::new(ErrorKind::BadQuestion, 4));
    }
    let mut d = Decode {
        src,
        at: HEADER,
        arena: Arena::new(&mut *into.names),
        records: &mut *into.records,
        filled: 0,
        opt: None,
    };
    let question = d.read_question(head.counts[0])?;
    let answers = d.read_section(head.counts[1], false)?;
    let authorities = d.read_section(head.counts[2], false)?;
    let additionals = d.read_section(head.counts[3], true)?;
    let ext = d.opt.map_or(0, |o| u16::from(o.ext_rcode) << 4);
    Ok(Reply {
        id: head.id,
        flags: head.flags,
        rcode: Rcode::from_bits(head.flags.rcode_bits() | ext),
        question,
        edns: d.opt,
        answers,
        authorities,
        additionals,
        names_len: d.arena.used as u16,
    })
}

/// State threaded through one decode.
struct Decode<'a, 's> {
    src: &'a [u8],
    at: usize,
    arena: Arena<'s>,
    records: &'s mut [Record],
    filled: usize,
    opt: Option<Opt>,
}

impl Decode<'_, '_> {
    fn read_question(&mut self, count: u16) -> Result<Option<Question>, DnsError> {
        if count == 0 {
            return Ok(None);
        }
        let start = self.at;
        let (name, end) = read_name(self.src, start, &mut self.arena)?;
        let kind = Type(be16(self.src, end)?);
        let class = Class(be16(self.src, end + 2)?);
        self.at = end + 4;
        Ok(Some(Question {
            name,
            kind,
            class,
            raw: Raw {
                at: start as u16,
                len: (end - start) as u16,
            },
        }))
    }

    /// Read `count` records, storing all but an OPT record, which is reported
    /// as EDNS0 instead. Returns how many were stored.
    fn read_section(&mut self, count: u16, additional: bool) -> Result<u16, DnsError> {
        let mut stored = 0u16;
        for _ in 0..count {
            stored += u16::from(self.read_one(additional)?);
        }
        Ok(stored)
    }

    /// Read one record and report whether it was stored. An OPT record is not:
    /// it becomes [`Reply::edns`].
    fn read_one(&mut self, additional: bool) -> Result<bool, DnsError> {
        let (rec, rdata) = self.read_record()?;
        if rec.kind == Type::OPT {
            self.take_opt(&rec, rdata, additional)?;
            return Ok(false);
        }
        let Some(slot) = self.records.get_mut(self.filled) else {
            return Err(DnsError::new(ErrorKind::TooManyRecords, self.at));
        };
        *slot = rec;
        self.filled += 1;
        Ok(true)
    }

    fn read_record(&mut self) -> Result<(Record, Raw), DnsError> {
        let (name, end) = read_name(self.src, self.at, &mut self.arena)?;
        let kind = Type(be16(self.src, end)?);
        let class = Class(be16(self.src, end + 2)?);
        let ttl = be32(self.src, end + 4)?;
        let len = usize::from(be16(self.src, end + 8)?);
        let at = end + 10;
        // RDLENGTH is the attacker's lever on every field after it, so it is
        // checked against the message before a byte of rdata is read.
        if at + len > self.src.len() {
            return Err(DnsError::new(ErrorKind::Truncated, at));
        }
        let rdata = Raw {
            at: at as u16,
            len: len as u16,
        };
        let data = read_data(self.src, kind, rdata, &mut self.arena)?;
        self.at = at + len;
        Ok((
            Record {
                name,
                kind,
                class,
                ttl,
                data,
            },
            rdata,
        ))
    }

    /// EDNS0 reuses an OPT record's class and ttl fields, so it is pulled out
    /// of the record stream rather than stored as if it described a name.
    fn take_opt(&mut self, rec: &Record, rdata: Raw, additional: bool) -> Result<(), DnsError> {
        // Anywhere but the additional section, or twice, or on a name other
        // than the root, and the responder is not speaking EDNS0.
        if !additional || self.opt.is_some() || !rec.name.is_root() {
            return Err(DnsError::new(ErrorKind::BadOpt, usize::from(rdata.at)));
        }
        check_options(self.src, rdata)?;
        self.opt = Some(Opt {
            udp_payload: rec.class.0,
            ext_rcode: (rec.ttl >> 24) as u8,
            version: (rec.ttl >> 16) as u8,
            flags: rec.ttl as u16,
            options: rdata,
        });
        Ok(())
    }
}

/// Walk the option list so a length that runs past the rdata is caught here
/// rather than by whoever reads an option later.
fn check_options(src: &[u8], rdata: Raw) -> Result<(), DnsError> {
    let end = usize::from(rdata.at) + usize::from(rdata.len);
    let mut at = usize::from(rdata.at);
    while at < end {
        if at + 4 > end {
            return Err(DnsError::new(ErrorKind::BadOpt, at));
        }
        at += 4 + usize::from(be16(src, at + 2)?);
        if at > end {
            return Err(DnsError::new(ErrorKind::BadOpt, at));
        }
    }
    Ok(())
}

/// Read rdata according to the record type. A length that disagrees with the
/// type is a malformed message: an `A` record is four bytes and a five byte
/// one is not an address with something extra, it is a lie.
fn read_data(src: &[u8], kind: Type, rdata: Raw, arena: &mut Arena<'_>) -> Result<Data, DnsError> {
    let at = usize::from(rdata.at);
    let len = usize::from(rdata.len);
    match kind {
        Type::A => Ok(Data::A(fixed::<4>(src, at, len)?)),
        Type::AAAA => Ok(Data::Aaaa(fixed::<16>(src, at, len)?)),
        Type::CNAME | Type::NS | Type::PTR => {
            let (name, end) = read_name(src, at, arena)?;
            // The name has to fill the rdata exactly. Short means trailing
            // bytes nobody accounts for, long means it read past its record.
            if end != at + len {
                return Err(DnsError::new(ErrorKind::BadRdLength, at));
            }
            Ok(Data::Name(name))
        }
        _ => Ok(Data::Raw(rdata)),
    }
}

fn fixed<const N: usize>(src: &[u8], at: usize, len: usize) -> Result<[u8; N], DnsError> {
    if len != N {
        return Err(DnsError::new(ErrorKind::BadRdLength, at));
    }
    let Some(v) = src.get(at..).and_then(|s| s.first_chunk::<N>()) else {
        return Err(DnsError::new(ErrorKind::Truncated, at));
    };
    Ok(*v)
}

// Matching a reply to its query.

/// Compare two dotted names the way DNS does, ASCII case insensitively.
pub fn name_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Whether `reply` answers the query that was sent.
///
/// An off path attacker who guesses the id and the source port can answer
/// first, and a stub that accepts whatever arrives will cache it. Checking the
/// echoed question is the cheap half of not doing that: the reply must carry
/// the same id and repeat the same name, type and class. A reply with no
/// question at all cannot be tied to anything and is refused here, even though
/// a genuine `FormErr` may look like that. The driver still owns the rest of
/// the policy, source address and port included, which this crate cannot see.
pub fn matches_query(reply: &Reply, names: &[u8], q: &Query<'_>) -> bool {
    if reply.id != q.id || !reply.flags.is_response() {
        return false;
    }
    let Some(question) = reply.question else {
        return false;
    };
    question.kind == q.kind
        && question.class == q.class
        && name_eq(question.name.bytes(names), trim_root(q.name.as_bytes()))
}

#[cfg(test)]
mod tests;
