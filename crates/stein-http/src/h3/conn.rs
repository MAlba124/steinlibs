//! Connection-wide half of HTTP/3 over quinn-proto: this side's control stream
//! with SETTINGS and its two QPACK streams, and the peer's uni streams read as
//! RFC 9114 6.2 and RFC 9204 4.2 say. Fixed size, nothing allocated.

use quinn_proto as q;

use super::frame::{self, Out, Piece, Reader};
use super::quic::{self, PEER_UNI};
use super::{H3_STREAM_CREATION_ERROR, H3Error};

/// Largest SETTINGS or GOAWAY payload taken.
pub const CONTROL_FRAME_MAX: usize = 4096;

/// One of the peer's uni streams.
#[derive(Clone, Copy)]
struct Uni {
    id: q::StreamId,
    /// Stream type once read (RFC 9114 6.2), and its bytes so far.
    kind: Option<u64>,
    part: [u8; 8],
    have: u8,
    reader: Reader,
}

/// What reading the peer's uni streams said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Said {
    pub settings: bool,
    pub goaway: Option<u64>,
}

/// Connection-wide state.
pub struct Control {
    peer: [Option<Uni>; PEER_UNI as usize],
    /// Peer SETTINGS came.
    pub settings: bool,
    /// Peer MAX_FIELD_SECTION_SIZE, unbounded when unset (RFC 9114 4.2.2).
    pub max_field_section: u64,
    /// Last GOAWAY, the first request stream the peer will not answer.
    pub goaway: Option<u64>,
    payload: [u8; CONTROL_FRAME_MAX],
    payload_len: usize,
}

impl Control {
    pub fn new() -> Control {
        Control {
            peer: [None; PEER_UNI as usize],
            settings: false,
            max_field_section: u64::MAX,
            goaway: None,
            payload: [0; CONTROL_FRAME_MAX],
            payload_len: 0,
        }
    }

    /// Open the control stream with SETTINGS (RFC 9114 6.2.1) and the QPACK encoder
    /// and decoder streams (RFC 9204 4.2), which carry only their type.
    pub fn open(conn: &mut q::Connection, max_field_section: u64) -> Result<(), H3Error> {
        for kind in [
            frame::STREAM_CONTROL,
            frame::STREAM_ENCODER,
            frame::STREAM_DECODER,
        ] {
            let Some(id) = conn.streams().open(q::Dir::Uni) else {
                return Err(H3Error::Connect {
                    what: "the peer allows fewer than three uni streams",
                });
            };
            let mut buf = [0u8; 32];
            let mut out = Out::new(&mut buf);
            let wrote = out.varint(kind).and_then(|()| match kind {
                frame::STREAM_CONTROL => frame::put_settings(&mut out, max_field_section),
                _ => Ok(()),
            });
            let bytes = out.written();
            let sent = wrote.is_ok() && conn.send_stream(id).write(bytes) == Ok(bytes.len());
            if !sent {
                return Err(H3Error::Connect {
                    what: "the peer's window takes no SETTINGS",
                });
            }
        }
        Ok(())
    }

    /// Take the uni streams the peer opened. Past the slots, stopped.
    pub fn accept(&mut self, conn: &mut q::Connection) {
        while let Some(id) = conn.streams().accept(q::Dir::Uni) {
            let uni = Uni {
                id,
                kind: None,
                part: [0; 8],
                have: 0,
                reader: Reader::default(),
            };
            match self.peer.iter_mut().find(|u| u.is_none()) {
                Some(slot) => *slot = Some(uni),
                None => {
                    let _ = conn
                        .recv_stream(id)
                        .stop(quic::code(H3_STREAM_CREATION_ERROR));
                }
            }
        }
    }

    /// Read every peer uni stream that has bytes.
    pub fn read_all(&mut self, conn: &mut q::Connection) -> Result<Said, H3Error> {
        let mut said = Said::default();
        for i in 0..self.peer.len() {
            if self.peer[i].is_some() {
                let goaway = self.read(conn, i)?;
                said.goaway = goaway.or(said.goaway);
            }
        }
        said.settings = self.settings;
        Ok(said)
    }

    fn controls(&self) -> usize {
        self.peer
            .iter()
            .flatten()
            .filter(|u| u.kind == Some(frame::STREAM_CONTROL))
            .count()
    }

    /// Stream type bytes off `data`. `Some(rest)` once known and allowed, `None` to stop it.
    fn kind<'d>(
        &mut self,
        i: usize,
        mut data: &'d [u8],
        controls: usize,
    ) -> Result<Option<&'d [u8]>, H3Error> {
        let Some(u) = self.peer[i].as_mut() else {
            return Ok(None);
        };
        while u.kind.is_none() && !data.is_empty() {
            u.part[u.have as usize] = data[0];
            u.have += 1;
            data = &data[1..];
            if u.have as usize == 1usize << (u.part[0] >> 6) {
                u.kind = frame::varint(&u.part[..u.have as usize]).map(|(v, _)| v);
            }
        }
        match u.kind {
            None => Ok(Some(data)),
            // One control stream (RFC 9114 6.2.1).
            Some(frame::STREAM_CONTROL) if controls > 0 => Err(H3Error::StreamCreation {
                kind: frame::STREAM_CONTROL,
            }),
            // Push stream with no MAX_PUSH_ID sent (4.6).
            Some(frame::STREAM_PUSH) => Err(H3Error::Id { id: u.id.into() }),
            Some(frame::STREAM_CONTROL | frame::STREAM_ENCODER | frame::STREAM_DECODER) => {
                Ok(Some(data))
            }
            // Unknown types are stopped (6.2.3).
            Some(_) => Ok(None),
        }
    }

    /// Bytes of a stream whose type is known.
    fn body(&mut self, i: usize, data: &[u8]) -> Result<Option<u64>, H3Error> {
        match self.peer[i].and_then(|u| u.kind) {
            Some(frame::STREAM_CONTROL) => self.control(i, data),
            // With capacity zero the encoder may only set capacity zero, 0x20 (RFC 9204 3.2.3).
            Some(frame::STREAM_ENCODER) => match data.iter().find(|b| **b != 0x20) {
                Some(b) => Err(H3Error::EncoderStream { byte: *b }),
                None => Ok(None),
            },
            // The decoder stream answers a table never filled. Dropped.
            _ => Ok(None),
        }
    }

    /// Read what peer uni stream `i` has. A GOAWAY id comes back.
    fn read(&mut self, conn: &mut q::Connection, i: usize) -> Result<Option<u64>, H3Error> {
        let Some(id) = self.peer[i].map(|u| u.id) else {
            return Ok(None);
        };
        let controls = self.controls();
        let mut goaway = None;
        let mut stop = false;
        let mut stream = conn.recv_stream(id);
        let Ok(mut chunks) = stream.read(true) else {
            return Ok(None);
        };
        loop {
            match chunks.next(usize::MAX) {
                Ok(Some(chunk)) => {
                    let known = self.peer[i].is_some_and(|u| u.kind.is_some());
                    let rest = match known {
                        true => Some(&chunk.bytes[..]),
                        false => self.kind(i, &chunk.bytes, controls)?,
                    };
                    let Some(rest) = rest else {
                        stop = true;
                        break;
                    };
                    goaway = self.body(i, rest)?.or(goaway);
                }
                // A critical stream may not end or reset (RFC 9114 6.2.1, RFC 9204 4.2).
                Ok(None) | Err(q::ReadError::Reset(_)) => {
                    let kind = self.peer[i].and_then(|u| u.kind).unwrap_or(u64::MAX);
                    if matches!(
                        kind,
                        frame::STREAM_CONTROL | frame::STREAM_ENCODER | frame::STREAM_DECODER
                    ) {
                        return Err(H3Error::ClosedCriticalStream { kind });
                    }
                    self.peer[i] = None;
                    break;
                }
                Err(q::ReadError::Blocked) => break,
            }
        }
        let _ = chunks.finalize();
        if stop {
            let _ = conn
                .recv_stream(id)
                .stop(quic::code(H3_STREAM_CREATION_ERROR));
            self.peer[i] = None;
        }
        Ok(goaway)
    }

    /// Control stream frames (RFC 9114 6.2.1): SETTINGS first and once, GOAWAY, the
    /// rest skipped.
    fn control(&mut self, i: usize, mut data: &[u8]) -> Result<Option<u64>, H3Error> {
        let Some(mut reader) = self.peer[i].map(|u| u.reader) else {
            return Ok(None);
        };
        let stream: u64 = self.peer[i].map_or(0, |u| u.id.into());
        let mut goaway = None;
        let mut result = Ok(());
        while !data.is_empty() && result.is_ok() {
            let (n, piece) = reader.step(data);
            data = &data[n..];
            result = match piece {
                None => Ok(()),
                Some(Piece::Frame { ty, len }) => self.frame_start(ty, len, stream, &mut goaway),
                Some(Piece::Payload { ty, bytes, end }) => {
                    self.payload_piece(ty, bytes, end, &mut goaway)
                }
            };
        }
        if let Some(u) = self.peer[i].as_mut() {
            u.reader = reader;
        }
        result.map(|()| goaway)
    }

    fn frame_start(
        &mut self,
        ty: u64,
        len: u64,
        stream: u64,
        goaway: &mut Option<u64>,
    ) -> Result<(), H3Error> {
        if !self.settings && ty != frame::SETTINGS {
            return Err(H3Error::MissingSettings { frame: ty });
        }
        match ty {
            frame::SETTINGS if self.settings => {
                return Err(H3Error::FrameUnexpected { frame: ty, stream });
            }
            frame::DATA | frame::HEADERS | frame::PUSH_PROMISE | frame::MAX_PUSH_ID => {
                return Err(H3Error::FrameUnexpected { frame: ty, stream });
            }
            frame::SETTINGS | frame::GOAWAY if len > CONTROL_FRAME_MAX as u64 => {
                return Err(H3Error::Frame { frame: ty, len });
            }
            _ => {}
        }
        self.payload_len = 0;
        if len == 0 {
            *goaway = self.whole(ty)?.or(*goaway);
        }
        Ok(())
    }

    fn payload_piece(
        &mut self,
        ty: u64,
        bytes: &[u8],
        end: bool,
        goaway: &mut Option<u64>,
    ) -> Result<(), H3Error> {
        if matches!(ty, frame::SETTINGS | frame::GOAWAY) {
            let at = self.payload_len;
            // Bounded by CONTROL_FRAME_MAX at the frame start.
            let Some(dst) = self.payload.get_mut(at..at + bytes.len()) else {
                return Err(H3Error::Frame {
                    frame: ty,
                    len: (at + bytes.len()) as u64,
                });
            };
            dst.copy_from_slice(bytes);
            self.payload_len += bytes.len();
        }
        if end {
            *goaway = self.whole(ty)?.or(*goaway);
        }
        Ok(())
    }

    /// A whole control frame of type `ty`.
    fn whole(&mut self, ty: u64) -> Result<Option<u64>, H3Error> {
        match ty {
            frame::SETTINGS => self.on_settings().map(|()| None),
            frame::GOAWAY => self.on_goaway().map(Some),
            _ => Ok(None),
        }
    }

    fn on_settings(&mut self) -> Result<(), H3Error> {
        self.settings = true;
        let p = &self.payload[..self.payload_len];
        let bad = H3Error::Frame {
            frame: frame::SETTINGS,
            len: p.len() as u64,
        };
        let mut seen = [0u64; 32];
        let (mut at, mut count) = (0, 0);
        while at < p.len() {
            let Some((id, a)) = frame::varint(&p[at..]) else {
                return Err(bad);
            };
            let Some((value, b)) = frame::varint(&p[at + a..]) else {
                return Err(bad);
            };
            at += a + b;
            // HTTP/2 settings are reserved here (7.2.4.1), and one id twice is an error.
            if matches!(id, 0x00 | 0x02..=0x05) || seen[..count].contains(&id) {
                return Err(H3Error::Settings { id });
            }
            if count < seen.len() {
                seen[count] = id;
                count += 1;
            }
            if id == frame::SETTING_MAX_FIELD_SECTION_SIZE {
                self.max_field_section = value;
            }
        }
        Ok(())
    }

    /// GOAWAY ids may only fall (5.2).
    fn on_goaway(&mut self) -> Result<u64, H3Error> {
        let p = &self.payload[..self.payload_len];
        let Some((id, _)) = frame::varint(p).filter(|(_, n)| *n == p.len()) else {
            return Err(H3Error::Frame {
                frame: frame::GOAWAY,
                len: p.len() as u64,
            });
        };
        if self.goaway.is_some_and(|before| id > before) {
            return Err(H3Error::Id { id });
        }
        self.goaway = Some(id);
        Ok(id)
    }
}
