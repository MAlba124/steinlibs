//! HTTP/2 frame codec (RFC 9113 section 4, section 6).

pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
pub const HEADER_LEN: usize = 9;
pub const DEFAULT_MAX_FRAME: u32 = 16_384;
pub const DEFAULT_WINDOW: i64 = 65_535;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Data,
    Headers,
    Priority,
    RstStream,
    Settings,
    PushPromise,
    Ping,
    GoAway,
    WindowUpdate,
    Continuation,
    /// Ignored on receipt, section 4.1.
    Unknown(u8),
}

impl Kind {
    pub fn from_u8(v: u8) -> Kind {
        match v {
            0 => Kind::Data,
            1 => Kind::Headers,
            2 => Kind::Priority,
            3 => Kind::RstStream,
            4 => Kind::Settings,
            5 => Kind::PushPromise,
            6 => Kind::Ping,
            7 => Kind::GoAway,
            8 => Kind::WindowUpdate,
            9 => Kind::Continuation,
            other => Kind::Unknown(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Kind::Data => 0,
            Kind::Headers => 1,
            Kind::Priority => 2,
            Kind::RstStream => 3,
            Kind::Settings => 4,
            Kind::PushPromise => 5,
            Kind::Ping => 6,
            Kind::GoAway => 7,
            Kind::WindowUpdate => 8,
            Kind::Continuation => 9,
            Kind::Unknown(v) => v,
        }
    }
}

pub mod flag {
    pub const ACK: u8 = 0x1;
    pub const END_STREAM: u8 = 0x1;
    pub const END_HEADERS: u8 = 0x4;
    pub const PADDED: u8 = 0x8;
    pub const PRIORITY: u8 = 0x20;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub len: u32,
    pub kind: Kind,
    pub flags: u8,
    pub stream: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    FrameTooLarge,
    BadPadding,
    BadLength,
    /// Nonzero stream id where zero is required, or vice versa.
    BadStreamId,
}

/// `None` if fewer than 9 bytes are available.
pub fn parse_header(src: &[u8]) -> Option<Header> {
    if src.len() < HEADER_LEN {
        return None;
    }
    let len = u32::from_be_bytes([0, src[0], src[1], src[2]]);
    let kind = Kind::from_u8(src[3]);
    let flags = src[4];
    // reserved top bit, ignored on receipt
    let stream = u32::from_be_bytes([src[5] & 0x7f, src[6], src[7], src[8]]);
    Some(Header {
        len,
        kind,
        flags,
        stream,
    })
}

pub fn write_header(dst: &mut Vec<u8>, kind: Kind, flags: u8, stream: u32, len: u32) {
    debug_assert!(len <= 0x00ff_ffff, "frame length must fit in 24 bits");
    dst.push((len >> 16) as u8);
    dst.push((len >> 8) as u8);
    dst.push(len as u8);
    dst.push(kind.to_u8());
    dst.push(flags);
    dst.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
}

/// Strip DATA or HEADERS padding, section 6.1, section 6.2.
pub fn strip_padding(payload: &[u8], flags: u8) -> Result<&[u8], Error> {
    if flags & flag::PADDED == 0 {
        return Ok(payload);
    }
    let pad = *payload.first().ok_or(Error::BadPadding)? as usize;
    let body = payload.get(1..).ok_or(Error::BadPadding)?;
    if pad > body.len() {
        return Err(Error::BadPadding);
    }
    Ok(&body[..body.len() - pad])
}

/// Strip the optional HEADERS priority block, section 6.2.
pub fn strip_priority(payload: &[u8], flags: u8) -> Result<&[u8], Error> {
    if flags & flag::PRIORITY == 0 {
        return Ok(payload);
    }
    payload.get(5..).ok_or(Error::BadLength)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Setting {
    pub id: u16,
    pub value: u32,
}

pub const SETTINGS_HEADER_TABLE_SIZE: u16 = 0x1;
pub const SETTINGS_ENABLE_PUSH: u16 = 0x2;
pub const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
pub const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;
pub const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 0x6;
/// RFC 8441 section 3.
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u16 = 0x8;

pub fn parse_settings(payload: &[u8]) -> Result<impl Iterator<Item = Setting> + '_, Error> {
    if !payload.len().is_multiple_of(6) {
        return Err(Error::BadLength);
    }
    Ok(payload.as_chunks::<6>().0.iter().map(|c| Setting {
        id: u16::from_be_bytes([c[0], c[1]]),
        value: u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
    }))
}

pub fn write_settings(dst: &mut Vec<u8>, settings: &[Setting]) {
    let len = (settings.len() * 6) as u32;
    write_header(dst, Kind::Settings, 0, 0, len);
    for s in settings {
        dst.extend_from_slice(&s.id.to_be_bytes());
        dst.extend_from_slice(&s.value.to_be_bytes());
    }
}

pub fn write_settings_ack(dst: &mut Vec<u8>) {
    write_header(dst, Kind::Settings, flag::ACK, 0, 0);
}

pub fn write_window_update(dst: &mut Vec<u8>, stream: u32, increment: u32) {
    write_header(dst, Kind::WindowUpdate, 0, stream, 4);
    dst.extend_from_slice(&(increment & 0x7fff_ffff).to_be_bytes());
}

pub fn write_rst_stream(dst: &mut Vec<u8>, stream: u32, code: u32) {
    write_header(dst, Kind::RstStream, 0, stream, 4);
    dst.extend_from_slice(&code.to_be_bytes());
}

pub fn write_ping_ack(dst: &mut Vec<u8>, payload: &[u8]) {
    write_header(dst, Kind::Ping, flag::ACK, 0, 8);
    let mut buf = [0u8; 8];
    let n = payload.len().min(8);
    buf[..n].copy_from_slice(&payload[..n]);
    dst.extend_from_slice(&buf);
}

pub fn parse_u32(payload: &[u8]) -> Result<u32, Error> {
    let b: [u8; 4] = payload
        .get(..4)
        .ok_or(Error::BadLength)?
        .try_into()
        .map_err(|_| Error::BadLength)?;
    Ok(u32::from_be_bytes(b))
}
