//! HTTP/2 server role (RFC 9113). Never allocates. `Conn` is `Copy` and all
//! per-connection storage is a caller-owned slab of [`mem_bytes`].
//!
//! [`Conn::step`] handles the preface or one frame per call. Body bytes are input ranges.
//! Owed control frames go into [`Ctl`]. Report sent responses via `sent_headers`,
//! `grant` and `sent_end`.
//!
//! ```text
//! [streams: max_streams * 32][hpack table][header block stash][fields]
//! slot [id: u32][state: u8][pad: 3][send window: i64][recv window: i64][tag: u64]
//! little endian, id zero when free
//! ```
//!
//! A step returns [`Event::Blocked`] before changing anything if `ctl` has under 26
//! bytes free, since HPACK decoding cannot be undone. `Ctl` needs at least 64 bytes.
//!
//! [`Error`] is fatal. Blocks over `max_header_block` and lists over `max_header_list`
//! are [`Error::Compression`], stricter than the RFC, to keep HPACK in sync.
//! Stream errors send RST_STREAM, and a stream that had a slot gives [`Event::Reset`].
//!
//! Extended CONNECT (RFC 8441) is off unless [`Conn::enable_connect_protocol`] is
//! called before SETTINGS. Read `:protocol` with [`protocol`].
//!
//! Trailers (RFC 9113 8.1). A second HEADERS must carry END_STREAM and no pseudo
//! headers. Reported as an empty [`Event::Data`] with `end_stream`, fields via
//! [`Conn::trailers`]. Send ours with [`trailers_frame`] and `sent_headers(.., true)`.

use super::frame::{self, Kind, flag};
use crate::header;
use crate::hpack::fixed::{self, Table};

pub const FRAME_HEADER: usize = 9;

/// Stream slot size and column offsets.
const SLOT: usize = 32;
const SLOT_STATE: usize = 4;
const SLOT_SEND: usize = 8;
const SLOT_RECV: usize = 16;
const SLOT_TAG: usize = 24;

/// Stream state bits. A slot is freed when both are set.
const PEER_DONE: u8 = 1;
const OURS_DONE: u8 = 2;

/// Room `ctl` must have before a frame is handled.
const CTL_RESERVE: usize = 26;

const WINDOW_MAX: i64 = i32::MAX as i64;
const MAX_FRAME_FLOOR: u32 = 16_384;
const MAX_FRAME_CEILING: u32 = 16_777_215;

// RFC 9113 section 7
const PROTOCOL_ERROR: u32 = 1;
const INTERNAL_ERROR: u32 = 2;
const FLOW_CONTROL_ERROR: u32 = 3;
const STREAM_CLOSED: u32 = 5;
const FRAME_SIZE_ERROR: u32 = 6;
const REFUSED_STREAM: u32 = 7;
const COMPRESSION_ERROR: u32 = 9;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// SETTINGS_MAX_CONCURRENT_STREAMS, also the stream table size.
    pub max_streams: u32,
    /// SETTINGS_INITIAL_WINDOW_SIZE.
    pub initial_window: u32,
    /// SETTINGS_MAX_FRAME_SIZE. At least 16384.
    pub max_frame: u32,
    /// SETTINGS_HEADER_TABLE_SIZE.
    pub header_table_size: u32,
    /// Compressed block limit across HEADERS and CONTINUATION.
    pub max_header_block: usize,
    /// Decoded list limit in packed bytes. Also SETTINGS_MAX_HEADER_LIST_SIZE.
    pub max_header_list: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_streams: 64,
            initial_window: 65_535,
            max_frame: 16_384,
            header_table_size: 4096,
            max_header_block: 8192,
            max_header_list: 8192,
        }
    }
}

/// Bytes of caller storage one connection needs for `cfg`.
pub const fn mem_bytes(cfg: &Config) -> usize {
    cfg.max_streams as usize * SLOT
        + fixed::table_bytes(cfg.header_table_size as usize)
        + cfg.max_header_block
        + cfg.max_header_list
}

/// Control frames we owe the peer. Caller sends `buf[..len]` and resets `len`.
/// At least 64 bytes.
pub struct Ctl<'a> {
    pub buf: &'a mut [u8],
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event<'m> {
    /// Not a whole frame yet. Nothing consumed.
    NeedMore,
    Nothing,
    /// `ctl` too full. Nothing consumed. Flush ctl and call again.
    Blocked,
    /// `fields` are packed `crate::header` records incl. pseudo headers, valid
    /// until `mem` is next borrowed mutably.
    Request {
        stream: u32,
        end_stream: bool,
        fields: &'m [u8],
    },
    /// `src[at..at + len]` of this step's input. Trailers arrive as an empty
    /// one with `end_stream`, read via [`Conn::trailers`].
    Data {
        stream: u32,
        end_stream: bool,
        at: usize,
        len: usize,
    },
    /// Open stream reset by the peer or by us. `tag` is the `set_tag` value.
    Reset {
        stream: u32,
        code: u32,
        tag: u64,
    },
    /// A send window grew. Stream 0 means the connection or all streams.
    Window {
        stream: u32,
    },
    GoAway {
        last_stream: u32,
        code: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Stepped<'m> {
    pub consumed: usize,
    pub event: Event<'m>,
}

/// Connection errors. The caller sends GOAWAY with `code()` and closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadPreface,
    Protocol,
    FlowControl,
    FrameSize,
    /// HPACK state untrusted. Includes blocks or lists over the limits.
    Compression,
    /// `mem` is smaller than [`mem_bytes`], or `init` was not called.
    BadStorage,
}

impl Error {
    /// RFC 9113 section 7 code for GOAWAY.
    pub fn code(&self) -> u32 {
        match self {
            Error::BadPreface | Error::Protocol => PROTOCOL_ERROR,
            Error::FlowControl => FLOW_CONTROL_ERROR,
            Error::FrameSize => FRAME_SIZE_ERROR,
            Error::Compression => COMPRESSION_ERROR,
            Error::BadStorage => INTERNAL_ERROR,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// `init` has not run.
    Fresh,
    Preface,
    /// Next frame must be SETTINGS.
    Settings,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct Conn {
    cfg: Config,
    table: Table,
    phase: Phase,
    /// Region offsets in `mem`. Streams start at zero.
    table_at: usize,
    stash_at: usize,
    fields_at: usize,
    total: usize,
    conn_send: i64,
    conn_recv: i64,
    /// Peer SETTINGS_INITIAL_WINDOW_SIZE.
    peer_window: i64,
    /// New stream receive window. Ours only once the peer acked it.
    recv_window: i64,
    peer_max_frame: u32,
    last_stream: u32,
    open: u32,
    /// Stream of the block being reassembled, zero when none.
    block_stream: u32,
    block_len: u32,
    block_end_stream: bool,
    /// Stream whose trailers the last step decoded, zero when none.
    trailers_stream: u32,
    trailers_len: u32,
    settings_acked: bool,
    goaway_received: bool,
    goaway_sent: bool,
    goaway_last: u32,
    /// RFC 8441.
    connect_protocol: bool,
}

impl Conn {
    pub fn new(cfg: Config) -> Conn {
        let mut cfg = cfg;
        cfg.max_frame = cfg.max_frame.clamp(MAX_FRAME_FLOOR, MAX_FRAME_CEILING);
        cfg.initial_window = cfg.initial_window.min(i32::MAX as u32);
        let table_at = cfg.max_streams as usize * SLOT;
        let stash_at = table_at + fixed::table_bytes(cfg.header_table_size as usize);
        let fields_at = stash_at + cfg.max_header_block;
        Conn {
            cfg,
            table: Table::new(cfg.header_table_size as usize),
            phase: Phase::Fresh,
            table_at,
            stash_at,
            fields_at,
            total: fields_at + cfg.max_header_list,
            conn_send: frame::DEFAULT_WINDOW,
            conn_recv: frame::DEFAULT_WINDOW,
            peer_window: frame::DEFAULT_WINDOW,
            recv_window: frame::DEFAULT_WINDOW,
            peer_max_frame: frame::DEFAULT_MAX_FRAME,
            last_stream: 0,
            open: 0,
            block_stream: 0,
            block_len: 0,
            block_end_stream: false,
            trailers_stream: 0,
            trailers_len: 0,
            settings_acked: false,
            goaway_received: false,
            goaway_sent: false,
            goaway_last: 0,
            connect_protocol: false,
        }
    }

    /// RFC 8441. Call before [`Conn::write_settings`]. Survives `init`, irreversible.
    pub fn enable_connect_protocol(&mut self) {
        self.connect_protocol = true;
    }

    /// `mem` must be at least `mem_bytes(cfg)`. Calling again restarts the connection.
    pub fn init(&mut self, mem: &mut [u8]) -> Result<(), Error> {
        if mem.len() < self.total {
            return Err(Error::BadStorage);
        }
        let connect_protocol = self.connect_protocol;
        *self = Conn::new(self.cfg);
        self.connect_protocol = connect_protocol;
        // only ids need zeroing
        mem[..self.table_at].fill(0);
        self.phase = Phase::Preface;
        Ok(())
    }

    /// First frame the server sends. Returns bytes written, `None` if `out` is short.
    pub fn write_settings(&mut self, out: &mut [u8]) -> Option<usize> {
        let list = self.cfg.max_header_list.min(u32::MAX as usize) as u32;
        let settings = [
            (
                frame::SETTINGS_HEADER_TABLE_SIZE,
                self.cfg.header_table_size,
            ),
            (frame::SETTINGS_MAX_CONCURRENT_STREAMS, self.cfg.max_streams),
            (frame::SETTINGS_INITIAL_WINDOW_SIZE, self.cfg.initial_window),
            (frame::SETTINGS_MAX_FRAME_SIZE, self.cfg.max_frame),
            (frame::SETTINGS_MAX_HEADER_LIST_SIZE, list),
            (frame::SETTINGS_ENABLE_CONNECT_PROTOCOL, 1),
        ];
        // connect protocol only when enabled
        let count = settings.len() - usize::from(!self.connect_protocol);
        let settings = &settings[..count];
        let len = settings.len() * 6;
        let out = out.get_mut(..FRAME_HEADER + len)?;
        put_header(out, len, Kind::Settings, 0, 0);
        for (i, (id, value)) in settings.iter().enumerate() {
            let at = FRAME_HEADER + i * 6;
            out[at..at + 2].copy_from_slice(&id.to_be_bytes());
            out[at + 2..at + 6].copy_from_slice(&value.to_be_bytes());
        }
        Some(FRAME_HEADER + len)
    }

    /// Handle the preface, then one frame per call.
    pub fn step<'m>(
        &mut self,
        mem: &'m mut [u8],
        src: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Stepped<'m>, Error> {
        if self.phase == Phase::Fresh || mem.len() < self.total {
            return Err(Error::BadStorage);
        }
        if self.phase == Phase::Preface {
            return self.preface(src);
        }
        let Some(h) = frame::parse_header(src) else {
            return Ok(stepped(0, Event::NeedMore));
        };
        // refuse on the header alone, before buffering the payload
        if h.len > self.cfg.max_frame {
            return Err(Error::FrameSize);
        }
        let first = self.phase == Phase::Settings;
        if first && (h.kind != Kind::Settings || h.flags & flag::ACK != 0) {
            return Err(Error::Protocol);
        }
        // section 6.10
        let in_block = self.block_stream != 0;
        if in_block && (h.kind != Kind::Continuation || h.stream != self.block_stream) {
            return Err(Error::Protocol);
        }
        let total = FRAME_HEADER + h.len as usize;
        let Some(payload) = src.get(FRAME_HEADER..total) else {
            return Ok(stepped(0, Event::NeedMore));
        };
        if ctl.buf.len().saturating_sub(ctl.len) < CTL_RESERVE {
            return Ok(stepped(0, Event::Blocked));
        }
        // fields region gets reused
        self.trailers_stream = 0;
        let event = self.frame(mem, h, payload, ctl)?;
        Ok(stepped(total, event))
    }

    fn preface(&mut self, src: &[u8]) -> Result<Stepped<'static>, Error> {
        let n = src.len().min(frame::PREFACE.len());
        if src[..n] != frame::PREFACE[..n] {
            return Err(Error::BadPreface);
        }
        if n < frame::PREFACE.len() {
            return Ok(stepped(0, Event::NeedMore));
        }
        self.phase = Phase::Settings;
        Ok(stepped(n, Event::Nothing))
    }

    fn frame<'m>(
        &mut self,
        mem: &'m mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'m>, Error> {
        match h.kind {
            Kind::Data => self.on_data(mem, h, payload, ctl),
            Kind::Headers => self.on_headers(mem, h, payload, ctl),
            Kind::Continuation => self.on_continuation(mem, h, payload, ctl),
            Kind::Settings => self.on_settings(mem, h, payload, ctl),
            Kind::WindowUpdate => self.on_window_update(mem, h, payload, ctl),
            Kind::RstStream => self.on_rst(mem, h, payload),
            Kind::Ping => on_ping(h, payload, ctl),
            Kind::GoAway => self.on_goaway(h, payload),
            Kind::Priority => on_priority(h, payload),
            // a client may not push
            Kind::PushPromise => Err(Error::Protocol),
            Kind::Unknown(_) => Ok(Event::Nothing),
        }
    }

    fn on_settings(
        &mut self,
        mem: &mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'static>, Error> {
        if h.stream != 0 {
            return Err(Error::Protocol);
        }
        if h.flags & flag::ACK != 0 {
            if !payload.is_empty() {
                return Err(Error::FrameSize);
            }
            self.on_settings_ack(mem);
            return Ok(Event::Nothing);
        }
        let settings = frame::parse_settings(payload).map_err(|_| Error::FrameSize)?;
        let mut window = self.peer_window;
        for s in settings {
            match s.id {
                frame::SETTINGS_INITIAL_WINDOW_SIZE => window = i64::from(s.value),
                frame::SETTINGS_MAX_FRAME_SIZE => self.set_peer_max_frame(s.value)?,
                frame::SETTINGS_ENABLE_PUSH if s.value > 1 => return Err(Error::Protocol),
                // HEADER_TABLE_SIZE ignored, our encoder has no dynamic table
                _ => {}
            }
        }
        if window > WINDOW_MAX {
            return Err(Error::FlowControl);
        }
        self.phase = Phase::Open;
        put_ctl(ctl, Kind::Settings, flag::ACK, 0, &[]);
        let delta = window - self.peer_window;
        self.peer_window = window;
        if delta == 0 {
            return Ok(Event::Nothing);
        }
        // section 6.9.2, applies to open streams
        self.shift_windows(mem, SLOT_SEND, delta)?;
        if delta > 0 {
            return Ok(Event::Window { stream: 0 });
        }
        Ok(Event::Nothing)
    }

    fn set_peer_max_frame(&mut self, value: u32) -> Result<(), Error> {
        if !(MAX_FRAME_FLOOR..=MAX_FRAME_CEILING).contains(&value) {
            return Err(Error::Protocol);
        }
        self.peer_max_frame = value;
        Ok(())
    }

    /// Streams got the default window until now. Apply our delta, section 6.9.2.
    fn on_settings_ack(&mut self, mem: &mut [u8]) {
        if self.settings_acked {
            return;
        }
        self.settings_acked = true;
        let ours = i64::from(self.cfg.initial_window);
        let delta = ours - self.recv_window;
        self.recv_window = ours;
        // cannot overflow, both ends are valid windows
        let _ = self.shift_windows(mem, SLOT_RECV, delta);
    }

    fn shift_windows(&mut self, mem: &mut [u8], column: usize, delta: i64) -> Result<(), Error> {
        let mut left = self.open;
        let mut slot = 0usize;
        while left > 0 && slot < self.cfg.max_streams as usize {
            let at = slot * SLOT;
            slot += 1;
            if get_u32(mem, at) == 0 {
                continue;
            }
            left -= 1;
            let window = get_i64(mem, at + column) + delta;
            if window > WINDOW_MAX {
                return Err(Error::FlowControl);
            }
            put_i64(mem, at + column, window);
        }
        Ok(())
    }

    fn on_headers<'m>(
        &mut self,
        mem: &'m mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'m>, Error> {
        if h.stream == 0 {
            return Err(Error::Protocol);
        }
        let block = frame::strip_padding(payload, h.flags).map_err(|_| Error::Protocol)?;
        // priority is deprecated in RFC 9113
        let block = frame::strip_priority(block, h.flags).map_err(|_| Error::FrameSize)?;
        let end_stream = h.flags & flag::END_STREAM != 0;
        if self.find(mem, h.stream).is_some() {
            // trailers must end the stream
            if !end_stream {
                return Err(Error::Protocol);
            }
        } else {
            // section 5.1.1, odd and increasing
            if h.stream.is_multiple_of(2) || h.stream <= self.last_stream {
                return Err(Error::Protocol);
            }
            self.last_stream = h.stream;
        }
        if h.flags & flag::END_HEADERS != 0 {
            return self.finish_block(mem, h.stream, end_stream, Some(block), ctl);
        }
        self.block_len = 0;
        self.stash(mem, block)?;
        self.block_stream = h.stream;
        self.block_end_stream = end_stream;
        Ok(Event::Nothing)
    }

    fn on_continuation<'m>(
        &mut self,
        mem: &'m mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'m>, Error> {
        // step checked the stream id
        if self.block_stream == 0 {
            return Err(Error::Protocol);
        }
        self.stash(mem, payload)?;
        if h.flags & flag::END_HEADERS == 0 {
            return Ok(Event::Nothing);
        }
        let stream = self.block_stream;
        self.block_stream = 0;
        self.finish_block(mem, stream, self.block_end_stream, None, ctl)
    }

    fn stash(&mut self, mem: &mut [u8], fragment: &[u8]) -> Result<(), Error> {
        let len = self.block_len as usize;
        let end = len + fragment.len();
        if end > self.cfg.max_header_block {
            return Err(Error::Compression);
        }
        let at = self.stash_at;
        mem[at + len..at + end].copy_from_slice(fragment);
        self.block_len = end as u32;
        Ok(())
    }

    /// `direct` is a single-frame block, else the stash is used. Always decodes
    /// to keep the HPACK table in sync.
    fn finish_block<'m>(
        &mut self,
        mem: &'m mut [u8],
        stream: u32,
        end_stream: bool,
        direct: Option<&[u8]>,
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'m>, Error> {
        let (streams, rest) = mem.split_at_mut(self.table_at);
        let (table, rest) = rest.split_at_mut(self.stash_at - self.table_at);
        let (stash, fields) = rest.split_at_mut(self.fields_at - self.stash_at);
        let block = direct.unwrap_or(&stash[..self.block_len as usize]);
        self.block_len = 0;
        if block.len() > self.cfg.max_header_block {
            return Err(Error::Compression);
        }
        let fields = fields
            .get_mut(..self.cfg.max_header_list)
            .ok_or(Error::BadStorage)?;
        let len = self
            .table
            .decode_block(table, block, fields)
            .map_err(|_| Error::Compression)?;
        let fields: &'m [u8] = fields;
        let fields = &fields[..len];

        if let Some(slot) = self.find(streams, stream) {
            return Ok(self.on_trailers(streams, ctl, slot, stream, fields));
        }
        if self.goaway_sent {
            // section 6.8
            return Ok(Event::Nothing);
        }
        if self.open >= self.cfg.max_streams {
            put_rst(ctl, stream, REFUSED_STREAM);
            return Ok(Event::Nothing);
        }
        if !request_is_sound(fields, self.connect_protocol) {
            put_rst(ctl, stream, PROTOCOL_ERROR);
            return Ok(Event::Nothing);
        }
        self.open_stream(streams, stream, end_stream);
        Ok(Event::Request {
            stream,
            end_stream,
            fields,
        })
    }

    /// Fields stay in place for [`Conn::trailers`].
    fn on_trailers(
        &mut self,
        mem: &mut [u8],
        ctl: &mut Ctl<'_>,
        slot: usize,
        stream: u32,
        fields: &[u8],
    ) -> Event<'static> {
        if mem[slot * SLOT + SLOT_STATE] & PEER_DONE != 0 {
            return self.kill(mem, ctl, slot, stream, STREAM_CLOSED);
        }
        // section 8.1, malformed
        if !trailers_are_sound(fields) {
            return self.kill(mem, ctl, slot, stream, PROTOCOL_ERROR);
        }
        self.trailers_stream = stream;
        self.trailers_len = fields.len() as u32;
        self.close_side(mem, slot, PEER_DONE);
        Event::Data {
            stream,
            end_stream: true,
            at: 0,
            len: 0,
        }
    }

    fn on_data(
        &mut self,
        mem: &mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'static>, Error> {
        if h.stream == 0 {
            return Err(Error::Protocol);
        }
        let body = frame::strip_padding(payload, h.flags).map_err(|_| Error::Protocol)?;
        let slot = self.find(mem, h.stream);
        if slot.is_none() && self.is_idle(h.stream) {
            return Err(Error::Protocol);
        }
        // section 6.9.1, padding counts. Regranted at once, so the window stays put.
        let counted = payload.len() as i64;
        if counted > self.conn_recv {
            return Err(Error::FlowControl);
        }
        if counted > 0 {
            put_window(ctl, 0, counted as u32);
        }
        // gone streams still spend the connection window
        let Some(slot) = slot else {
            return Ok(Event::Nothing);
        };
        let at = slot * SLOT;
        if mem[at + SLOT_STATE] & PEER_DONE != 0 {
            return Ok(self.kill(mem, ctl, slot, h.stream, STREAM_CLOSED));
        }
        if counted > get_i64(mem, at + SLOT_RECV) {
            return Ok(self.kill(mem, ctl, slot, h.stream, FLOW_CONTROL_ERROR));
        }
        let end_stream = h.flags & flag::END_STREAM != 0;
        if end_stream {
            self.close_side(mem, slot, PEER_DONE);
        } else if counted > 0 {
            put_window(ctl, h.stream, counted as u32);
        }
        if body.is_empty() && !end_stream {
            return Ok(Event::Nothing);
        }
        let pad_byte = usize::from(h.flags & flag::PADDED != 0);
        Ok(Event::Data {
            stream: h.stream,
            end_stream,
            at: FRAME_HEADER + pad_byte,
            len: body.len(),
        })
    }

    fn on_window_update(
        &mut self,
        mem: &mut [u8],
        h: frame::Header,
        payload: &[u8],
        ctl: &mut Ctl<'_>,
    ) -> Result<Event<'static>, Error> {
        if payload.len() != 4 {
            return Err(Error::FrameSize);
        }
        let raw = frame::parse_u32(payload).map_err(|_| Error::FrameSize)?;
        let increment = i64::from(raw & 0x7fff_ffff);
        if h.stream == 0 {
            if increment == 0 {
                return Err(Error::Protocol);
            }
            if self.conn_send + increment > WINDOW_MAX {
                return Err(Error::FlowControl);
            }
            self.conn_send += increment;
            return Ok(Event::Window { stream: 0 });
        }
        let Some(slot) = self.find(mem, h.stream) else {
            if self.is_idle(h.stream) {
                return Err(Error::Protocol);
            }
            // crossed our last frame
            return Ok(Event::Nothing);
        };
        if increment == 0 {
            return Ok(self.kill(mem, ctl, slot, h.stream, PROTOCOL_ERROR));
        }
        let at = slot * SLOT;
        let window = get_i64(mem, at + SLOT_SEND) + increment;
        if window > WINDOW_MAX {
            return Ok(self.kill(mem, ctl, slot, h.stream, FLOW_CONTROL_ERROR));
        }
        put_i64(mem, at + SLOT_SEND, window);
        if mem[at + SLOT_STATE] & OURS_DONE != 0 {
            return Ok(Event::Nothing);
        }
        Ok(Event::Window { stream: h.stream })
    }

    fn on_rst(
        &mut self,
        mem: &mut [u8],
        h: frame::Header,
        payload: &[u8],
    ) -> Result<Event<'static>, Error> {
        if h.stream == 0 {
            return Err(Error::Protocol);
        }
        if payload.len() != 4 {
            return Err(Error::FrameSize);
        }
        let code = frame::parse_u32(payload).map_err(|_| Error::FrameSize)?;
        let Some(slot) = self.find(mem, h.stream) else {
            if self.is_idle(h.stream) {
                return Err(Error::Protocol);
            }
            return Ok(Event::Nothing);
        };
        let tag = get_i64(mem, slot * SLOT + SLOT_TAG) as u64;
        self.free(mem, slot);
        Ok(Event::Reset {
            stream: h.stream,
            code,
            tag,
        })
    }

    fn on_goaway(&mut self, h: frame::Header, payload: &[u8]) -> Result<Event<'static>, Error> {
        if h.stream != 0 {
            return Err(Error::Protocol);
        }
        let last = frame::parse_u32(payload).map_err(|_| Error::FrameSize)?;
        let code = frame::parse_u32(payload.get(4..).unwrap_or(&[]));
        let code = code.map_err(|_| Error::FrameSize)?;
        self.goaway_received = true;
        Ok(Event::GoAway {
            last_stream: last & 0x7fff_ffff,
            code,
        })
    }

    /// Never opened. We never push, so every even id is idle.
    fn is_idle(&self, stream: u32) -> bool {
        stream.is_multiple_of(2) || stream > self.last_stream
    }

    fn find(&self, mem: &[u8], stream: u32) -> Option<usize> {
        if stream == 0 || mem.len() < self.table_at {
            return None;
        }
        let mut left = self.open;
        let mut slot = 0usize;
        while left > 0 && slot < self.cfg.max_streams as usize {
            let id = get_u32(mem, slot * SLOT);
            if id == stream {
                return Some(slot);
            }
            left -= u32::from(id != 0);
            slot += 1;
        }
        None
    }

    /// Caller checked that a free slot exists.
    fn open_stream(&mut self, mem: &mut [u8], stream: u32, end_stream: bool) {
        let mut slot = 0usize;
        while slot < self.cfg.max_streams as usize && get_u32(mem, slot * SLOT) != 0 {
            slot += 1;
        }
        let at = slot * SLOT;
        let Some(row) = mem.get_mut(at..at + SLOT) else {
            return;
        };
        row.fill(0);
        row[..4].copy_from_slice(&stream.to_le_bytes());
        row[SLOT_STATE] = if end_stream { PEER_DONE } else { 0 };
        put_i64(row, SLOT_SEND, self.peer_window);
        put_i64(row, SLOT_RECV, self.recv_window);
        self.open += 1;
    }

    fn free(&mut self, mem: &mut [u8], slot: usize) {
        put_u32(mem, slot * SLOT, 0);
        self.open -= 1;
    }

    /// Frees the slot when both sides are closed.
    fn close_side(&mut self, mem: &mut [u8], slot: usize, side: u8) {
        let at = slot * SLOT + SLOT_STATE;
        mem[at] |= side;
        if mem[at] == PEER_DONE | OURS_DONE {
            self.free(mem, slot);
        }
    }

    /// Stream error. RST_STREAM, free slot, report [`Event::Reset`].
    fn kill(
        &mut self,
        mem: &mut [u8],
        ctl: &mut Ctl<'_>,
        slot: usize,
        stream: u32,
        code: u32,
    ) -> Event<'static> {
        put_rst(ctl, stream, code);
        let tag = get_i64(mem, slot * SLOT + SLOT_TAG) as u64;
        self.free(mem, slot);
        Event::Reset { stream, code, tag }
    }

    /// A HEADERS frame was queued. False if not open for sending, then drop the
    /// response. `end_stream` closes our side.
    pub fn sent_headers(&mut self, mem: &mut [u8], stream: u32, end_stream: bool) -> bool {
        let Some(slot) = self.find(mem, stream) else {
            return false;
        };
        if mem[slot * SLOT + SLOT_STATE] & OURS_DONE != 0 {
            return false;
        }
        if end_stream {
            self.close_side(mem, slot, OURS_DONE);
        }
        true
    }

    /// DATA bytes sendable now, min of `want`, both send windows and peer max frame.
    /// Deducts the result from both windows.
    pub fn grant(&mut self, mem: &mut [u8], stream: u32, want: usize) -> usize {
        let Some(slot) = self.find(mem, stream) else {
            return 0;
        };
        let at = slot * SLOT;
        if mem[at + SLOT_STATE] & OURS_DONE != 0 {
            return 0;
        }
        // SETTINGS can make it negative
        let window = get_i64(mem, at + SLOT_SEND);
        let most = window.min(self.conn_send).max(0) as usize;
        let n = want.min(most).min(self.peer_max_frame as usize);
        put_i64(mem, at + SLOT_SEND, window - n as i64);
        self.conn_send -= n as i64;
        n
    }

    /// Return `n` granted but unsent bytes to both windows, else the connection
    /// window shrinks for good. Clamped to 2^31-1.
    pub fn ungrant(&mut self, mem: &mut [u8], stream: u32, n: usize) {
        let n = n as i64;
        self.conn_send = (self.conn_send + n).min(WINDOW_MAX);
        if let Some(slot) = self.find(mem, stream) {
            let at = slot * SLOT + SLOT_SEND;
            put_i64(mem, at, (get_i64(mem, at) + n).min(WINDOW_MAX));
        }
    }

    /// A DATA frame with END_STREAM was queued.
    pub fn sent_end(&mut self, mem: &mut [u8], stream: u32) {
        if let Some(slot) = self.find(mem, stream) {
            self.close_side(mem, slot, OURS_DONE);
        }
    }

    /// RST_STREAM into ctl and free the slot. False if ctl is full. True for gone streams.
    pub fn reset(&mut self, mem: &mut [u8], ctl: &mut Ctl<'_>, stream: u32, code: u32) -> bool {
        let Some(slot) = self.find(mem, stream) else {
            return true;
        };
        if !put_rst(ctl, stream, code) {
            return false;
        }
        self.free(mem, slot);
        true
    }

    /// False if ctl is full. Later requests are dropped. Repeat calls keep the
    /// first last-stream id.
    pub fn goaway(&mut self, ctl: &mut Ctl<'_>, code: u32) -> bool {
        let last = if self.goaway_sent {
            self.goaway_last
        } else {
            self.last_stream
        };
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&last.to_be_bytes());
        payload[4..].copy_from_slice(&code.to_be_bytes());
        if !put_ctl(ctl, Kind::GoAway, 0, 0, &payload) {
            return false;
        }
        self.goaway_sent = true;
        self.goaway_last = last;
        true
    }

    /// Packed trailer fields. `Some` only until the next frame step after the
    /// empty end-stream [`Event::Data`]. `None` if the body ended in DATA.
    pub fn trailers<'m>(&self, mem: &'m [u8], stream: u32) -> Option<&'m [u8]> {
        if stream == 0 || self.trailers_stream != stream {
            return None;
        }
        mem.get(self.fields_at..self.fields_at + self.trailers_len as usize)
    }

    /// Opaque caller word tied to a stream.
    pub fn set_tag(&mut self, mem: &mut [u8], stream: u32, tag: u64) -> bool {
        let Some(slot) = self.find(mem, stream) else {
            return false;
        };
        let at = slot * SLOT + SLOT_TAG;
        mem[at..at + 8].copy_from_slice(&tag.to_le_bytes());
        true
    }

    pub fn tag(&self, mem: &[u8], stream: u32) -> Option<u64> {
        let slot = self.find(mem, stream)?;
        Some(get_i64(mem, slot * SLOT + SLOT_TAG) as u64)
    }

    /// (stream id, tag) of slot `index` in 0..max_streams, for walking streams.
    pub fn stream_at(&self, mem: &[u8], index: u32) -> Option<(u32, u64)> {
        if index >= self.cfg.max_streams || mem.len() < self.table_at {
            return None;
        }
        let at = index as usize * SLOT;
        let id = get_u32(mem, at);
        if id == 0 {
            return None;
        }
        Some((id, get_i64(mem, at + SLOT_TAG) as u64))
    }

    pub fn is_open(&self, mem: &[u8], stream: u32) -> bool {
        self.find(mem, stream)
            .is_some_and(|slot| mem[slot * SLOT + SLOT_STATE] & OURS_DONE == 0)
    }

    /// Streams with either side still open.
    pub fn open_streams(&self) -> u32 {
        self.open
    }

    pub fn conn_send_window(&self) -> i64 {
        self.conn_send
    }

    /// Zero for a stream that is gone.
    pub fn stream_send_window(&self, mem: &[u8], stream: u32) -> i64 {
        self.find(mem, stream)
            .map_or(0, |slot| get_i64(mem, slot * SLOT + SLOT_SEND))
    }

    pub fn peer_max_frame(&self) -> u32 {
        self.peer_max_frame
    }

    pub fn last_stream(&self) -> u32 {
        self.last_stream
    }

    pub fn goaway_received(&self) -> bool {
        self.goaway_received
    }
}

fn on_ping(h: frame::Header, payload: &[u8], ctl: &mut Ctl<'_>) -> Result<Event<'static>, Error> {
    if h.stream != 0 {
        return Err(Error::Protocol);
    }
    if payload.len() != 8 {
        return Err(Error::FrameSize);
    }
    if h.flags & flag::ACK == 0 {
        put_ctl(ctl, Kind::Ping, flag::ACK, 0, payload);
    }
    Ok(Event::Nothing)
}

/// Priorities unimplemented, frame only validated.
fn on_priority(h: frame::Header, payload: &[u8]) -> Result<Event<'static>, Error> {
    if h.stream == 0 {
        return Err(Error::Protocol);
    }
    if payload.len() != 5 {
        return Err(Error::FrameSize);
    }
    Ok(Event::Nothing)
}

/// Minimal RFC 9113 section 8.3 checks. With `extended`, also `:protocol` per RFC 8441 section 4.
fn request_is_sound(fields: &[u8], extended: bool) -> bool {
    let mut regular = false;
    let mut method = 0u32;
    let mut path = 0u32;
    let mut scheme = 0u32;
    let mut protocol = 0u32;
    let mut connect = false;
    for (name, value) in header::iter(fields) {
        if name.first() != Some(&b':') {
            regular = true;
            let upper = name.iter().any(u8::is_ascii_uppercase);
            if name.is_empty() || upper || name == b"connection" {
                return false;
            }
            continue;
        }
        if regular {
            return false;
        }
        match name {
            b":method" => {
                method += 1;
                connect = value == b"CONNECT";
            }
            b":path" => path += 1,
            b":scheme" => scheme += 1,
            b":authority" => {}
            b":protocol" if extended => protocol += 1,
            _ => return false,
        }
    }
    if protocol > 0 && (protocol > 1 || !connect || scheme != 1) {
        return false;
    }
    method == 1 && path == 1
}

/// RFC 9113 section 8.1, no pseudo headers, plus the section 8.2 field rules.
fn trailers_are_sound(fields: &[u8]) -> bool {
    header::iter(fields).all(|(name, _)| {
        let upper = name.iter().any(u8::is_ascii_uppercase);
        !(name.is_empty() || name[0] == b':' || upper || name == b"connection")
    })
}

/// Extended CONNECT `:protocol`, e.g. `websocket`. `fields` from [`Event::Request`].
pub fn protocol(fields: &[u8]) -> Option<&[u8]> {
    header::find(fields, ":protocol")
}

fn stepped(consumed: usize, event: Event<'_>) -> Stepped<'_> {
    Stepped { consumed, event }
}

fn get_u32(mem: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([mem[at], mem[at + 1], mem[at + 2], mem[at + 3]])
}

fn put_u32(mem: &mut [u8], at: usize, v: u32) {
    mem[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn get_i64(mem: &[u8], at: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&mem[at..at + 8]);
    i64::from_le_bytes(b)
}

fn put_i64(mem: &mut [u8], at: usize, v: i64) {
    mem[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_header(out: &mut [u8], len: usize, kind: Kind, flags: u8, stream: u32) {
    out[0] = (len >> 16) as u8;
    out[1] = (len >> 8) as u8;
    out[2] = len as u8;
    out[3] = kind.to_u8();
    out[4] = flags;
    out[5..FRAME_HEADER].copy_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
}

/// False with nothing written if there is no room.
fn put_ctl(ctl: &mut Ctl<'_>, kind: Kind, flags: u8, stream: u32, payload: &[u8]) -> bool {
    let end = ctl.len + FRAME_HEADER + payload.len();
    let Some(out) = ctl.buf.get_mut(ctl.len..end) else {
        return false;
    };
    put_header(out, payload.len(), kind, flags, stream);
    out[FRAME_HEADER..].copy_from_slice(payload);
    ctl.len = end;
    true
}

fn put_rst(ctl: &mut Ctl<'_>, stream: u32, code: u32) -> bool {
    put_ctl(ctl, Kind::RstStream, 0, stream, &code.to_be_bytes())
}

fn put_window(ctl: &mut Ctl<'_>, stream: u32, increment: u32) -> bool {
    put_ctl(ctl, Kind::WindowUpdate, 0, stream, &increment.to_be_bytes())
}

/// Minimum peer max frame. Blocks are never split over CONTINUATION.
const HEADERS_MAX: usize = MAX_FRAME_FLOOR as usize;

/// HEADERS with END_HEADERS, `:status` then `fields`. Returns bytes written.
/// `None` if it does not fit `out` or exceeds 16384 bytes.
pub fn headers_frame(
    out: &mut [u8],
    stream: u32,
    status: u16,
    fields: &[(&[u8], &[u8])],
    end_stream: bool,
) -> Option<usize> {
    headers_frame_from(out, stream, status, fields.iter().copied(), end_stream)
}

/// As [`headers_frame`] with fields from any iterator.
pub fn headers_frame_from<'a>(
    out: &mut [u8],
    stream: u32,
    status: u16,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    end_stream: bool,
) -> Option<usize> {
    // truncation enforces the limit
    let room = out.len().min(FRAME_HEADER + HEADERS_MAX);
    let out = &mut out[..room];
    let at = fixed::encode_status(out, FRAME_HEADER, status)?;
    let at = encode_fields(out, at, fields)?;
    let end = if end_stream { flag::END_STREAM } else { 0 };
    let flags = flag::END_HEADERS | end;
    put_header(out, at - FRAME_HEADER, Kind::Headers, flags, stream);
    Some(at)
}

/// Response trailers, RFC 9113 section 8.1. No pseudo headers. `None` as [`headers_frame`].
/// Report with `sent_headers(.., true)`. DATA frames then carry no END_STREAM.
pub fn trailers_frame(out: &mut [u8], stream: u32, fields: &[(&[u8], &[u8])]) -> Option<usize> {
    trailers_frame_from(out, stream, fields.iter().copied())
}

/// As [`trailers_frame`] with fields from any iterator.
pub fn trailers_frame_from<'a>(
    out: &mut [u8],
    stream: u32,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Option<usize> {
    let room = out.len().min(FRAME_HEADER + HEADERS_MAX);
    let out = &mut out[..room];
    if out.len() < FRAME_HEADER {
        return None;
    }
    let at = encode_fields(out, FRAME_HEADER, fields)?;
    let flags = flag::END_HEADERS | flag::END_STREAM;
    put_header(out, at - FRAME_HEADER, Kind::Headers, flags, stream);
    Some(at)
}

fn encode_fields<'a>(
    out: &mut [u8],
    mut at: usize,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Option<usize> {
    for (name, value) in fields {
        at = fixed::encode_field(out, at, name, value)?;
    }
    Some(at)
}

/// Returns 9. `None` if `out` is short or `len` exceeds 24 bits.
pub fn data_header(out: &mut [u8], stream: u32, len: usize, end_stream: bool) -> Option<usize> {
    if len > MAX_FRAME_CEILING as usize {
        return None;
    }
    let out = out.get_mut(..FRAME_HEADER)?;
    let flags = if end_stream { flag::END_STREAM } else { 0 };
    put_header(out, len, Kind::Data, flags, stream);
    Some(FRAME_HEADER)
}

/// Set END_STREAM on an already written HEADERS or DATA frame. False if too short.
pub fn end_stream(frame: &mut [u8]) -> bool {
    if frame.len() < FRAME_HEADER {
        return false;
    }
    frame[4] |= flag::END_STREAM;
    true
}

#[cfg(test)]
mod tests;
