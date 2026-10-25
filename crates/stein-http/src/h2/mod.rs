//! HTTP/2 client connection and stream state.
//! Stream transitions are buffered per batch and committed in one place.

pub mod frame;
pub mod server;

use crate::handle::{Handle, Pool};
use crate::header::{self, Header, HeaderError};
use crate::hpack::{self, DynamicTable};
use frame::{Kind, flag};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamState {
    /// HEADERS sent, nothing received.
    Open,
    /// Response head received, body in progress.
    ReadingBody,
    HalfClosed,
    Closed,
    Reset(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Frame(frame::Error),
    Hpack(hpack::Error),
    /// Peer violated the protocol in a way that kills the connection.
    Protocol,
    FlowControl,
    TooManyStreams,
    HeaderListTooLarge,
    UnknownStream,
    BadStorage(crate::handle::PoolError),
    /// Header in [`ENGINE_OWNED`] or not a valid field. Nothing written.
    BadHeader(HeaderError),
    /// Body length differs from the one the stream was opened with.
    BodyMismatch,
}

/// Fields a caller may not set, case insensitive. Refused, not dropped.
/// Connection-specific ones per RFC 9113 8.2.2. Pseudo fields are refused too.
pub const ENGINE_OWNED: [&str; 8] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
];

impl From<frame::Error> for Error {
    fn from(e: frame::Error) -> Self {
        Error::Frame(e)
    }
}
impl From<hpack::Error> for Error {
    fn from(e: hpack::Error) -> Self {
        Error::Hpack(e)
    }
}

pub struct Stream {
    pub id: u32,
    pub state: StreamState,
    /// Signed, SETTINGS can legally make it negative (section 6.9.2).
    pub recv_window: i64,
    /// Spent by the peer, not yet granted back.
    pub recv_spent: i64,
    pub send_window: i64,
    pub head_seen: bool,
    /// Request body octets and how many went out. Equal once END_STREAM sent.
    pub body_len: u64,
    pub body_sent: u64,
}

pub type StreamId = Handle<Stream>;

/// RFC 9113 7.
const CANCEL: u32 = 0x8;

/// Emitted by `Connection::step`. Body bytes are an uncopied span of the step input.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Head {
        stream: StreamId,
        status: u16,
        headers: Vec<hpack::Header>,
    },
    /// `src[at..at + len]` of that `step` call's input.
    Body {
        stream: StreamId,
        at: usize,
        len: usize,
    },
    Complete {
        stream: StreamId,
    },
    Reset {
        stream: StreamId,
        code: u32,
    },
    GoAway {
        last_stream: u32,
        code: u32,
    },
    SettingsChanged,
}

/// Committed after the whole frame batch is parsed.
#[derive(Debug, PartialEq, Eq)]
pub struct Transition {
    pub stream: StreamId,
    pub from: StreamState,
    pub to: StreamState,
}

/// Policy only. Stream capacity comes from the storage passed to [`Connection::new`].
pub struct Config {
    pub max_frame: u32,
    pub max_header_list: usize,
    /// SETTINGS_INITIAL_WINDOW_SIZE we advertise.
    pub initial_window: i64,
    /// Connection receive window. The part above 65535 is granted after the preface.
    pub conn_window: i64,
    pub header_table_size: usize,
    /// Bounds a flood of empty frames from holding the tick.
    pub max_frames_per_step: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_frame: frame::DEFAULT_MAX_FRAME,
            max_header_list: 16 * 1024,
            initial_window: frame::DEFAULT_WINDOW,
            conn_window: frame::DEFAULT_WINDOW,
            header_table_size: 4096,
            max_frames_per_step: 256,
        }
    }
}

/// Stream table borrowed from the application.
pub struct StreamStorage<'a> {
    pub slots: &'a mut [Option<Stream>],
    pub generations: &'a mut [u32],
    pub free: &'a mut [u32],
}

/// Heap-backed owner for a [`StreamStorage`].
pub struct HeapStreamStorage {
    slots: Vec<Option<Stream>>,
    generations: Vec<u32>,
    free: Vec<u32>,
}

impl HeapStreamStorage {
    pub fn new(max_streams: u32) -> Self {
        let n = max_streams as usize;
        HeapStreamStorage {
            slots: (0..n).map(|_| None).collect(),
            generations: vec![0; n],
            free: vec![0; n],
        }
    }

    pub fn view(&mut self) -> StreamStorage<'_> {
        StreamStorage {
            slots: &mut self.slots,
            generations: &mut self.generations,
            free: &mut self.free,
        }
    }
}

pub struct Connection<'a> {
    cfg: Config,
    streams: Pool<'a, Stream>,
    /// Wire stream id to handle. Entries removed on close.
    by_id: Vec<(u32, StreamId)>,
    next_id: u32,
    decoder_table: DynamicTable,
    conn_recv_window: i64,
    /// Spent by the peer, not yet granted back.
    conn_spent: i64,
    conn_send_window: i64,
    peer_max_frame: u32,
    peer_initial_window: i64,
    preface_sent: bool,
    goaway_received: bool,
    /// Partial HEADERS block awaiting CONTINUATION frames.
    header_fragment: Vec<u8>,
    header_stream: Option<u32>,
    header_end_stream: bool,
    /// Every committed transition, for debugging.
    pub transitions: Vec<Transition>,
}

impl<'a> Connection<'a> {
    pub fn new(cfg: Config, storage: StreamStorage<'a>) -> Result<Self, Error> {
        let table = DynamicTable::new(cfg.header_table_size);
        let streams = Pool::new(storage.slots, storage.generations, storage.free)
            .map_err(Error::BadStorage)?;
        let max_streams = streams.capacity();
        Ok(Connection {
            streams,
            by_id: Vec::with_capacity(max_streams),
            next_id: 1, // client-initiated streams are odd
            decoder_table: table,
            conn_recv_window: frame::DEFAULT_WINDOW,
            conn_spent: 0,
            conn_send_window: frame::DEFAULT_WINDOW,
            peer_max_frame: frame::DEFAULT_MAX_FRAME,
            peer_initial_window: frame::DEFAULT_WINDOW,
            preface_sent: false,
            goaway_received: false,
            header_fragment: Vec::new(),
            header_stream: None,
            header_end_stream: false,
            transitions: Vec::new(),
            cfg,
        })
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }
    pub fn get(&self, s: StreamId) -> Option<&Stream> {
        self.streams.get(s)
    }
    pub fn conn_send_window(&self) -> i64 {
        self.conn_send_window
    }
    pub fn goaway_received(&self) -> bool {
        self.goaway_received
    }

    /// Client preface, SETTINGS and the connection window grant. Must be first on the wire.
    pub fn write_preface(&mut self, out: &mut Vec<u8>) {
        if self.preface_sent {
            return;
        }
        out.extend_from_slice(frame::PREFACE);
        frame::write_settings(
            out,
            &[
                frame::Setting {
                    id: frame::SETTINGS_ENABLE_PUSH,
                    value: 0,
                },
                frame::Setting {
                    id: frame::SETTINGS_INITIAL_WINDOW_SIZE,
                    value: self.cfg.initial_window as u32,
                },
                frame::Setting {
                    id: frame::SETTINGS_MAX_HEADER_LIST_SIZE,
                    value: self.cfg.max_header_list as u32,
                },
            ],
        );
        let grant = (self.cfg.conn_window - frame::DEFAULT_WINDOW).clamp(0, i32::MAX as i64);
        if grant > 0 {
            frame::write_window_update(out, 0, grant as u32);
            self.conn_recv_window += grant;
        }
        self.preface_sent = true;
    }

    /// Open a bodyless stream and write HEADERS with END_STREAM. Names are lowercased
    /// (RFC 9113 8.2). Bad headers give [`Error::BadHeader`] with nothing written.
    pub fn send_request(
        &mut self,
        out: &mut Vec<u8>,
        method: &str,
        path: &str,
        authority: &str,
        scheme: &str,
        headers: &[Header<'_>],
    ) -> Result<StreamId, Error> {
        let target = Target {
            method,
            path,
            authority,
            scheme,
        };
        self.open_stream(out, &target, headers, None)
    }

    /// As [`Connection::send_request`] with a body, sent as far as the windows allow.
    /// Caller keeps `body` and passes it to [`Connection::send_body`] for the rest.
    #[allow(clippy::too_many_arguments)]
    pub fn send_request_body(
        &mut self,
        out: &mut Vec<u8>,
        method: &str,
        path: &str,
        authority: &str,
        scheme: &str,
        headers: &[Header<'_>],
        body: &[u8],
    ) -> Result<StreamId, Error> {
        let target = Target {
            method,
            path,
            authority,
            scheme,
        };
        let h = self.open_stream(out, &target, headers, Some(body.len() as u64))?;
        self.send_body(out, h, body)?;
        Ok(h)
    }

    /// Send what the windows allow of the remaining body. `body` must be the one
    /// the stream was opened with. Returns octets written.
    pub fn send_body(
        &mut self,
        out: &mut Vec<u8>,
        stream: StreamId,
        body: &[u8],
    ) -> Result<usize, Error> {
        let max_frame = self.peer_max_frame.max(1) as u64;
        let conn_window = self.conn_send_window;
        let s = self.streams.get_mut(stream).ok_or(Error::UnknownStream)?;
        if body.len() as u64 != s.body_len {
            return Err(Error::BodyMismatch);
        }
        let room = s.send_window.min(conn_window).max(0) as u64;
        let take = (s.body_len - s.body_sent).min(room);
        let mut at = s.body_sent as usize;
        let end = at + take as usize;
        while at < end {
            let n = (end - at).min(max_frame as usize);
            write_data(out, s.id, &body[at..at + n], at + n == body.len());
            at += n;
        }
        s.body_sent += take;
        s.send_window -= take as i64;
        self.conn_send_window -= take as i64;
        Ok(take as usize)
    }

    /// Unsent request body octets. Zero for a gone stream.
    pub fn body_owed(&self, stream: StreamId) -> u64 {
        self.streams
            .get(stream)
            .map_or(0, |s| s.body_len - s.body_sent)
    }

    fn open_stream(
        &mut self,
        out: &mut Vec<u8>,
        t: &Target<'_>,
        headers: &[Header<'_>],
        body_len: Option<u64>,
    ) -> Result<StreamId, Error> {
        check_headers(headers)?;
        let mut block = Vec::with_capacity(64);
        hpack::encode_header(":method", t.method, &mut block);
        hpack::encode_header(":scheme", t.scheme, &mut block);
        hpack::encode_header(":path", t.path, &mut block);
        hpack::encode_header(":authority", t.authority, &mut block);
        for h in headers {
            encode_lowercase(h, &mut block);
        }
        if let Some(n) = body_len {
            let mut digits = [0u8; 20];
            let len = decimal(n, &mut digits);
            hpack::encode_header("content-length", len, &mut block);
        }
        // CONTINUATION is not sent, so oversized blocks are refused.
        if block.len() > self.peer_max_frame as usize {
            return Err(Error::HeaderListTooLarge);
        }
        let id = self.next_id;
        let stream = Stream {
            id,
            state: StreamState::Open,
            recv_window: self.cfg.initial_window,
            recv_spent: 0,
            send_window: self.peer_initial_window,
            head_seen: false,
            body_len: body_len.unwrap_or(0),
            body_sent: 0,
        };
        let h = self.streams.insert(stream).ok_or(Error::TooManyStreams)?;
        self.next_id = self.next_id.wrapping_add(2);
        self.by_id.push((id, h));
        let ends = body_len.is_none_or(|n| n == 0);
        let flags = flag::END_HEADERS | if ends { flag::END_STREAM } else { 0 };
        frame::write_header(out, Kind::Headers, flags, id, block.len() as u32);
        out.extend_from_slice(&block);
        Ok(h)
    }

    /// State including transitions queued this batch. Handlers must use this, not
    /// `stream.state`, or a second transition gets a stale `from`.
    fn effective_state(&self, sh: StreamId, pending: &[Transition]) -> Option<StreamState> {
        pending
            .iter()
            .rev()
            .find(|t| t.stream == sh)
            .map(|t| t.to)
            .or_else(|| self.streams.get(sh).map(|s| s.state))
    }

    fn lookup(&self, id: u32) -> Option<StreamId> {
        self.by_id
            .iter()
            .find(|(wire, _)| *wire == id)
            .map(|(_, h)| *h)
    }

    /// Send RST_STREAM and drop the stream. Late frames for it are ignored.
    pub fn reset_stream(&mut self, out: &mut Vec<u8>, sh: StreamId, code: u32) {
        let Some(s) = self.streams.get_mut(sh) else {
            return;
        };
        frame::write_rst_stream(out, s.id, code);
        let from = s.state;
        s.state = StreamState::Reset(code);
        self.transitions.push(Transition {
            stream: sh,
            from,
            to: StreamState::Reset(code),
        });
        self.reap_closed();
    }

    /// Parse all whole frames in `src`. Returns bytes consumed.
    pub fn step(
        &mut self,
        src: &[u8],
        out: &mut Vec<u8>,
        events: &mut Vec<Event>,
    ) -> Result<usize, Error> {
        let mut pos = 0usize;
        let mut pending: Vec<Transition> = Vec::new();
        let mut frames = 0usize;

        while frames < self.cfg.max_frames_per_step {
            let Some(h) = frame::parse_header(&src[pos..]) else {
                break;
            };
            if h.len > self.cfg.max_frame {
                return Err(Error::Frame(frame::Error::FrameTooLarge));
            }
            let total = frame::HEADER_LEN + h.len as usize;
            if src.len() - pos < total {
                break; // incomplete frame
            }
            let payload = &src[pos + frame::HEADER_LEN..pos + total];
            self.handle_frame(
                h,
                payload,
                pos + frame::HEADER_LEN,
                out,
                events,
                &mut pending,
            )?;
            pos += total;
            frames += 1;
        }

        for t in pending {
            if let Some(s) = self.streams.get_mut(t.stream) {
                debug_assert_eq!(s.state, t.from, "transition committed out of order");
                s.state = t.to;
            }
            self.transitions.push(t);
        }
        self.cancel_owed(out);
        self.reap_closed();
        Ok(pos)
    }

    /// Response ended before the request body was sent. RST_STREAM CANCEL (RFC 9113 8.1).
    fn cancel_owed(&mut self, out: &mut Vec<u8>) {
        let mut slot = 0;
        while let Some(h) = self.streams.next_live(slot) {
            slot = h.slot() + 1;
            let Some(s) = self.streams.get_mut(h) else {
                continue;
            };
            if s.state == StreamState::Closed && s.body_sent < s.body_len {
                frame::write_rst_stream(out, s.id, CANCEL);
                s.body_sent = s.body_len;
            }
        }
    }

    /// `at` is where `payload` starts in the step's input.
    fn handle_frame(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        at: usize,
        out: &mut Vec<u8>,
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        match h.kind {
            Kind::Settings => self.on_settings(h, payload, out),
            Kind::Ping => self.on_ping(h, payload, out),
            Kind::GoAway => self.on_goaway(payload, events),
            Kind::WindowUpdate => self.on_window_update(h, payload),
            Kind::Headers => self.on_headers(h, payload, events, pending),
            Kind::Continuation => self.on_continuation(h, payload, events, pending),
            Kind::Data => self.on_data(h, payload, at, out, events, pending),
            Kind::RstStream => self.on_rst(h, payload, events, pending),
            // PRIORITY advisory, push disabled, unknown types ignored per section 4.1
            Kind::Priority | Kind::PushPromise | Kind::Unknown(_) => Ok(()),
        }
    }

    fn on_settings(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if h.flags & flag::ACK != 0 {
            return Ok(());
        }
        let mut new_initial: Option<i64> = None;
        for s in frame::parse_settings(payload)? {
            match s.id {
                frame::SETTINGS_MAX_FRAME_SIZE => self.peer_max_frame = s.value,
                frame::SETTINGS_INITIAL_WINDOW_SIZE => new_initial = Some(s.value as i64),
                frame::SETTINGS_HEADER_TABLE_SIZE => {
                    self.decoder_table.set_max_size(s.value as usize)
                }
                _ => {}
            }
        }
        // section 6.9.2. INITIAL_WINDOW_SIZE delta applies to every open stream.
        if let Some(v) = new_initial {
            let delta = v - self.peer_initial_window;
            self.peer_initial_window = v;
            let mut slot = 0;
            while let Some(h) = self.streams.next_live(slot) {
                slot = h.slot() + 1;
                if let Some(s) = self.streams.get_mut(h) {
                    s.send_window += delta;
                }
            }
        }
        frame::write_settings_ack(out);
        Ok(())
    }

    fn on_ping(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if h.flags & flag::ACK == 0 {
            frame::write_ping_ack(out, payload);
        }
        Ok(())
    }

    fn on_goaway(&mut self, payload: &[u8], events: &mut Vec<Event>) -> Result<(), Error> {
        let last = frame::parse_u32(payload)? & 0x7fff_ffff;
        let code = frame::parse_u32(payload.get(4..).unwrap_or(&[]))?;
        self.goaway_received = true;
        events.push(Event::GoAway {
            last_stream: last,
            code,
        });
        Ok(())
    }

    fn on_window_update(&mut self, h: frame::Header, payload: &[u8]) -> Result<(), Error> {
        let inc = (frame::parse_u32(payload)? & 0x7fff_ffff) as i64;
        if inc == 0 {
            return Err(Error::Protocol);
        }
        if h.stream == 0 {
            self.conn_send_window += inc;
            if self.conn_send_window > i32::MAX as i64 {
                return Err(Error::FlowControl);
            }
        } else if let Some(sh) = self.lookup(h.stream)
            && let Some(s) = self.streams.get_mut(sh)
        {
            s.send_window += inc;
            if s.send_window > i32::MAX as i64 {
                return Err(Error::FlowControl);
            }
        }
        Ok(())
    }

    fn on_headers(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        let body = frame::strip_padding(payload, h.flags)?;
        let body = frame::strip_priority(body, h.flags)?;
        if self.header_fragment.len() + body.len() > self.cfg.max_header_list {
            return Err(Error::HeaderListTooLarge);
        }
        self.header_fragment.extend_from_slice(body);
        self.header_stream = Some(h.stream);
        self.header_end_stream = h.flags & flag::END_STREAM != 0;
        if h.flags & flag::END_HEADERS != 0 {
            self.finish_header_block(events, pending)?;
        }
        Ok(())
    }

    fn on_continuation(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        // section 6.10. Only legal directly after HEADERS on the same stream.
        if self.header_stream != Some(h.stream) {
            return Err(Error::Protocol);
        }
        if self.header_fragment.len() + payload.len() > self.cfg.max_header_list {
            return Err(Error::HeaderListTooLarge);
        }
        self.header_fragment.extend_from_slice(payload);
        if h.flags & flag::END_HEADERS != 0 {
            self.finish_header_block(events, pending)?;
        }
        Ok(())
    }

    fn finish_header_block(
        &mut self,
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        let wire = self.header_stream.take().ok_or(Error::Protocol)?;
        let block = core::mem::take(&mut self.header_fragment);
        let mut headers = Vec::new();
        // Decode even for untracked streams or the HPACK tables desync.
        let decoded = hpack::decode_block(
            &block,
            &mut self.decoder_table,
            self.cfg.max_header_list,
            &mut headers,
        );
        self.header_fragment = block;
        self.header_fragment.clear();
        decoded?;

        let Some(sh) = self.lookup(wire) else {
            return Ok(());
        };
        let status = headers
            .iter()
            .find(|h| h.name == b":status")
            .and_then(|h| core::str::from_utf8(&h.value).ok())
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or(Error::Protocol)?;

        let end = self.header_end_stream;
        let Some(from) = self.effective_state(sh, pending) else {
            return Ok(());
        };
        let Some(s) = self.streams.get_mut(sh) else {
            return Ok(());
        };
        s.head_seen = true;
        events.push(Event::Head {
            stream: sh,
            status,
            headers,
        });
        let to = if end {
            StreamState::Closed
        } else {
            StreamState::ReadingBody
        };
        pending.push(Transition {
            stream: sh,
            from,
            to,
        });
        if end {
            events.push(Event::Complete { stream: sh });
        }
        Ok(())
    }

    fn on_data(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        at: usize,
        out: &mut Vec<u8>,
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        let body = frame::strip_padding(payload, h.flags)?;
        // skip the pad length octet
        let at = at + usize::from(h.flags & flag::PADDED != 0);
        // padding counts toward flow control, section 6.9.1
        let counted = payload.len() as i64;
        self.conn_recv_window -= counted;
        if self.conn_recv_window < 0 {
            return Err(Error::FlowControl);
        }
        // regrant at half spent, not per frame
        self.conn_spent += counted;
        if self.conn_spent * 2 >= self.cfg.conn_window.max(frame::DEFAULT_WINDOW) {
            frame::write_window_update(out, 0, self.conn_spent as u32);
            self.conn_recv_window += self.conn_spent;
            self.conn_spent = 0;
        }

        let Some(sh) = self.lookup(h.stream) else {
            return Ok(());
        };
        let end = h.flags & flag::END_STREAM != 0;
        let Some(from) = self.effective_state(sh, pending) else {
            return Ok(());
        };
        let initial = self.cfg.initial_window;
        match self.streams.get_mut(sh) {
            None => return Ok(()),
            Some(s) => {
                s.recv_window -= counted;
                if s.recv_window < 0 {
                    return Err(Error::FlowControl);
                }
                s.recv_spent += counted;
                if s.recv_spent * 2 >= initial && !end {
                    frame::write_window_update(out, h.stream, s.recv_spent as u32);
                    s.recv_window += s.recv_spent;
                    s.recv_spent = 0;
                }
            }
        }
        if !body.is_empty() {
            events.push(Event::Body {
                stream: sh,
                at,
                len: body.len(),
            });
        }
        if end {
            pending.push(Transition {
                stream: sh,
                from,
                to: StreamState::Closed,
            });
            events.push(Event::Complete { stream: sh });
        }
        Ok(())
    }

    fn on_rst(
        &mut self,
        h: frame::Header,
        payload: &[u8],
        events: &mut Vec<Event>,
        pending: &mut Vec<Transition>,
    ) -> Result<(), Error> {
        let code = frame::parse_u32(payload)?;
        let Some(sh) = self.lookup(h.stream) else {
            return Ok(());
        };
        let Some(from) = self.effective_state(sh, pending) else {
            return Ok(());
        };
        pending.push(Transition {
            stream: sh,
            from,
            to: StreamState::Reset(code),
        });
        events.push(Event::Reset { stream: sh, code });
        Ok(())
    }

    /// Remove finished streams after the batch so handles stay valid during it.
    fn reap_closed(&mut self) {
        let done: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(_, s)| matches!(s.state, StreamState::Closed | StreamState::Reset(_)))
            .map(|(h, _)| h)
            .collect();
        for h in done {
            if let Some(s) = self.streams.remove(h) {
                self.by_id.retain(|(wire, _)| *wire != s.id);
            }
        }
    }
}

/// Request pseudo fields.
struct Target<'a> {
    method: &'a str,
    path: &'a str,
    authority: &'a str,
    scheme: &'a str,
}

fn write_data(out: &mut Vec<u8>, stream: u32, piece: &[u8], ends: bool) {
    let flags = if ends { flag::END_STREAM } else { 0 };
    frame::write_header(out, Kind::Data, flags, stream, piece.len() as u32);
    out.extend_from_slice(piece);
}

fn check_headers(headers: &[Header<'_>]) -> Result<(), Error> {
    for h in headers {
        header::check_field(h.name.as_bytes(), h.value.as_bytes()).map_err(Error::BadHeader)?;
        if ENGINE_OWNED.iter().any(|r| h.name.eq_ignore_ascii_case(r)) {
            return Err(Error::BadHeader(HeaderError::Reserved));
        }
    }
    Ok(())
}

/// Literal with lowercased name. Tokens are ASCII so the length is unchanged.
fn encode_lowercase(h: &Header<'_>, dst: &mut Vec<u8>) {
    if !h.name.bytes().any(|b| b.is_ascii_uppercase()) {
        return hpack::encode_header(h.name, h.value, dst);
    }
    dst.push(0x00);
    hpack::encode_int(h.name.len() as u64, 7, 0x00, dst);
    dst.extend(h.name.bytes().map(|b| b.to_ascii_lowercase()));
    hpack::encode_string(h.value.as_bytes(), dst);
}

/// `n` in decimal, written at the end of `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[at..]).unwrap_or("0")
}

#[cfg(test)]
mod tests;
