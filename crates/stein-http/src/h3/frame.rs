//! HTTP/3 frames (RFC 9114 7) and QUIC varints (RFC 9000 16), read off a
//! stream's ordered bytes in any pieces and written into a caller buffer.

/// Frame types (RFC 9114 7.2).
pub const DATA: u64 = 0x00;
pub const HEADERS: u64 = 0x01;
pub const CANCEL_PUSH: u64 = 0x03;
pub const SETTINGS: u64 = 0x04;
pub const PUSH_PROMISE: u64 = 0x05;
pub const GOAWAY: u64 = 0x07;
pub const MAX_PUSH_ID: u64 = 0x0d;

/// Unidirectional stream types (RFC 9114 6.2, RFC 9204 4.2).
pub const STREAM_CONTROL: u64 = 0x00;
pub const STREAM_PUSH: u64 = 0x01;
pub const STREAM_ENCODER: u64 = 0x02;
pub const STREAM_DECODER: u64 = 0x03;

/// Settings (RFC 9114 7.2.4.1, RFC 9204 5).
pub const SETTING_QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
pub const SETTING_MAX_FIELD_SECTION_SIZE: u64 = 0x06;
pub const SETTING_QPACK_BLOCKED_STREAMS: u64 = 0x07;

/// 2^62 - 1.
pub const VARINT_MAX: u64 = (1 << 62) - 1;

/// Varint off the front of `b` and its length. `None` while `b` is short of it.
pub fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let len = 1usize << (first >> 6);
    let bytes = b.get(1..len)?;
    let mut v = u64::from(first & 0x3f);
    for byte in bytes {
        v = v << 8 | u64::from(*byte);
    }
    Some((v, len))
}

/// Bytes of the shortest encoding of `v`.
pub fn varint_len(v: u64) -> usize {
    match v {
        0..=63 => 1,
        64..=16383 => 2,
        16384..=1_073_741_823 => 4,
        _ => 8,
    }
}

/// Buffer had no room. Nothing past the room was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// Bounded writer over a caller buffer.
pub struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Out<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Out { buf, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn written(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn bytes(&mut self, b: &[u8]) -> Result<(), Full> {
        let end = self.len.checked_add(b.len()).ok_or(Full)?;
        self.buf
            .get_mut(self.len..end)
            .ok_or(Full)?
            .copy_from_slice(b);
        self.len = end;
        Ok(())
    }

    pub fn byte(&mut self, b: u8) -> Result<(), Full> {
        *self.buf.get_mut(self.len).ok_or(Full)? = b;
        self.len += 1;
        Ok(())
    }

    /// `v` in its shortest encoding, clamped to [`VARINT_MAX`].
    pub fn varint(&mut self, v: u64) -> Result<(), Full> {
        let v = v.min(VARINT_MAX);
        match varint_len(v) {
            1 => self.byte(v as u8),
            2 => self.bytes(&(v as u16 | 0x4000).to_be_bytes()),
            4 => self.bytes(&(v as u32 | 0x8000_0000).to_be_bytes()),
            _ => self.bytes(&(v | 0xc000_0000_0000_0000).to_be_bytes()),
        }
    }

    /// The last `n` written bytes.
    pub fn tail_mut(&mut self, n: usize) -> &mut [u8] {
        let from = self.len.saturating_sub(n);
        &mut self.buf[from..self.len]
    }
}

/// Frame header, type and length (RFC 9114 7.1).
pub fn put_header(out: &mut Out<'_>, ty: u64, len: u64) -> Result<(), Full> {
    out.varint(ty)?;
    out.varint(len)
}

/// This client's SETTINGS: QPACK table capacity zero and no blocked streams, so the
/// peer encodes with the static table and literals (RFC 9204 3.2.3). No MAX_PUSH_ID
/// is ever sent, so no push.
pub fn put_settings(out: &mut Out<'_>, max_field_section: u64) -> Result<(), Full> {
    let settings = [
        (SETTING_QPACK_MAX_TABLE_CAPACITY, 0),
        (SETTING_MAX_FIELD_SECTION_SIZE, max_field_section),
        (SETTING_QPACK_BLOCKED_STREAMS, 0),
    ];
    let len: usize = settings
        .iter()
        .map(|(id, v)| varint_len(*id) + varint_len(*v))
        .sum();
    put_header(out, SETTINGS, len as u64)?;
    for (id, v) in settings {
        out.varint(id)?;
        out.varint(v)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Type,
    Len,
    Payload,
}

/// What a [`Reader::step`] made of its input.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Type and payload length read. A zero length frame is whole with this.
    Frame { ty: u64, len: u64 },
    /// Payload bytes of a frame of type `ty`, `end` with its last.
    Payload { ty: u64, bytes: &'a [u8], end: bool },
}

/// Frames off one stream (RFC 9114 7.1), varints split across pieces included.
#[derive(Clone, Copy, Debug)]
pub struct Reader {
    state: State,
    ty: u64,
    left: u64,
    /// Varint bytes seen so far.
    part: [u8; 8],
    have: u8,
}

impl Default for Reader {
    fn default() -> Reader {
        Reader {
            state: State::Type,
            ty: 0,
            left: 0,
            part: [0; 8],
            have: 0,
        }
    }
}

impl Reader {
    /// Between frames.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Type && self.have == 0
    }

    /// Type of the frame whose payload is next, and the octets left of it.
    pub fn in_payload(&self) -> Option<(u64, u64)> {
        (self.state == State::Payload).then_some((self.ty, self.left))
    }

    /// Most bytes the next step takes without crossing into the following field.
    /// A caller reading exactly this many keeps payload apart from framing.
    pub fn want(&self) -> u64 {
        match self.state {
            State::Payload => self.left,
            _ if self.have == 0 => 1,
            _ => ((1usize << (self.part[0] >> 6)) - self.have as usize) as u64,
        }
    }

    /// Varint from earlier bytes and `data`. Bytes taken, and the value once whole.
    fn varint_from(&mut self, data: &[u8]) -> (usize, Option<u64>) {
        let first = match (self.have, data.first()) {
            (0, Some(b)) => *b,
            (0, None) => return (0, None),
            _ => self.part[0],
        };
        let len = 1usize << (first >> 6);
        let have = self.have as usize;
        let take = (len - have).min(data.len());
        self.part[have..have + take].copy_from_slice(&data[..take]);
        self.have += take as u8;
        if (self.have as usize) < len {
            return (take, None);
        }
        self.have = 0;
        (take, varint(&self.part[..len]).map(|(v, _)| v))
    }

    /// Read what `data` holds. Bytes used and what they made, `None` while a varint is partial.
    pub fn step<'a>(&mut self, data: &'a [u8]) -> (usize, Option<Piece<'a>>) {
        match self.state {
            State::Type => {
                let (n, v) = self.varint_from(data);
                if let Some(ty) = v {
                    (self.ty, self.state) = (ty, State::Len);
                }
                (n, None)
            }
            State::Len => {
                let (n, v) = self.varint_from(data);
                let Some(len) = v else {
                    return (n, None);
                };
                self.left = len;
                self.state = if len == 0 {
                    State::Type
                } else {
                    State::Payload
                };
                (n, Some(Piece::Frame { ty: self.ty, len }))
            }
            State::Payload => {
                let n = (data.len() as u64).min(self.left) as usize;
                self.left -= n as u64;
                let end = self.left == 0;
                if end {
                    self.state = State::Type;
                }
                let piece = Piece::Payload {
                    ty: self.ty,
                    bytes: &data[..n],
                    end,
                };
                (n, Some(piece))
            }
        }
    }
}

#[cfg(test)]
mod tests;
