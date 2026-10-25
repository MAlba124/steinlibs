//! HTTP/3 client engine over quinn-proto, the third beside h1 and h2.
//!
//! Sans-IO like [`crate::Client`]: no sockets, no clock, poll functions. One
//! quinn [`q::Endpoint`] routes datagrams for every connection, and the caller
//! owns one connected UDP socket per connection. Time is [`Nanos`] from a caller
//! epoch, since quinn's pacing and RTT samples go below a millisecond.
//!
//! ```text
//! let conn = client.connect(now, "example.com", addr)?;
//! loop {
//!     while let Some(n) = client.poll_transmit(conn, now, &mut out) { socket.send(&out[..n]) }
//!     wake_at = client.poll_timeout(conn);
//!     client.handle_datagram(conn, now, &received)?;   // or handle_timeout(conn, now)
//!     while let Some(e) = client.poll_event() { ... }   // Connected: submit, Body: read_body
//! }
//! ```
//!
//! A request is a bidi stream: HEADERS, one DATA frame with the body, the end of
//! the stream. The engine reads a response's HEADERS itself, 1xx skipped, and
//! leaves DATA in quinn until [`Client::read_body`] copies it into the caller's
//! buffer, so a caller that stops reading holds the stream's flow control shut.
//! Trailers are skipped.
//!
//! Rows: request and connection tables come from a [`ClientStorage`], and each
//! request slot owns a raw section region and a packed header region. A request
//! row goes when the event that ends it is polled. [`Event::Closed`] ends every
//! request of its connection, and the connection row stays until [`Client::close`].
//!
//! Allocation: [`Client::new`] allocates the event queue, the datagram and QPACK
//! scratch, and one receive block. quinn allocates inside its connection state,
//! and a new receive block when every datagram of the last is released.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use quinn_proto as q;

use super::conn::Control;
use super::frame::{self, Out, Piece, Reader};
use super::quic::{self, MAX_UDP_PAYLOAD, Transport};
use super::{H3_REQUEST_CANCELLED, H3Error, qpack};
use crate::handle::{self, Handle, Pool};
use crate::header::{self, Header, HeaderError};

/// Nanoseconds since a caller-chosen epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Nanos(pub u64);

/// Receive block datagrams are copied into for quinn.
const RX_BLOCK: usize = 64 * 1024;
/// Room for a request head beyond the header region.
const HEAD_SLACK: usize = 4096;
/// Frame header room ahead of an encoded section.
const FRAME_HEAD: usize = 16;

/// Policy only. Capacity comes from the storage passed to [`Client::new`].
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub transport: Transport,
    /// SETTINGS_MAX_FIELD_SECTION_SIZE advertised (RFC 9114 4.2.2).
    pub max_field_section: u64,
    /// Seeded randomness and connection ids, for replayable runs.
    pub seed: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            transport: Transport::default(),
            max_field_section: 64 * 1024,
            seed: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    TooManyConnections,
    TooManyRequests,
    UnknownConnection,
    UnknownRequest,
    /// Handshake not done, or the connection is closed.
    NotConnected,
    /// The peer sent GOAWAY. Open a new connection.
    GoingAway,
    /// No stream credit. Submit again on [`Event::Available`].
    StreamsBlocked,
    /// Header in [`crate::h2::ENGINE_OWNED`], a bad field, or a bad target. Nothing sent.
    BadRequest(HeaderError),
    /// Body length differs from the one submitted.
    BodyMismatch,
    /// Protocol level refusal, such as a head over the peer's limit.
    H3(H3Error),
    BadStorage(StorageError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    Conns(handle::PoolError),
    Reqs(handle::PoolError),
    Sections(header::StoreError),
    Headers(header::StoreError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Handshaking,
    /// Control streams open, requests may go.
    Up,
    /// Lost or closed with an error. Waits for [`Client::close`].
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReqState {
    /// Waiting for the final response head.
    Head,
    Body,
    /// Terminal event queued.
    Over,
}

/// Opaque. Public only so `Handle<Conn>` can be named.
pub struct Conn {
    ch: q::ConnectionHandle,
    quic: q::Connection,
    remote: SocketAddr,
    state: ConnState,
    control: Control,
    /// The endpoint was told it is drained.
    drained: bool,
}

/// Opaque. Public only so `Handle<Req>` can be named.
pub struct Req {
    conn: ConnId,
    stream: q::StreamId,
    reader: Reader,
    state: ReqState,
    section_len: usize,
    status: u16,
    header_len: u16,
    header_count: u16,
    body_len: u64,
    body_sent: u64,
    /// This side of the stream is finished or reset.
    finished: bool,
    /// [`Event::Body`] queued since the last blocked read.
    signalled: bool,
}

pub type ConnId = Handle<Conn>;
pub type RequestId = Handle<Req>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Handshake done and this side's control streams open.
    Connected { conn: ConnId },
    /// Stream credit came back for a submit refused with [`Error::StreamsBlocked`].
    Available { conn: ConnId },
    /// Final response head. Read with [`Client::header`].
    Head { req: RequestId, status: u16 },
    /// Body bytes wait. Read with [`Client::read_body`] until it gives 0.
    Body { req: RequestId },
    /// The request body may take more of [`Client::write_body`].
    Writable { req: RequestId },
    /// The response is whole. The row goes with this event.
    Done { req: RequestId },
    /// The peer reset the stream with `code`. The row goes with this event.
    Reset { req: RequestId, code: u64 },
    /// The response broke the protocol. Stream stopped, connection kept.
    Failed { req: RequestId, error: H3Error },
    /// GOAWAY says it was not processed (RFC 9114 5.2). Safe to send again.
    Unprocessed { req: RequestId },
    /// GOAWAY with the first stream id the peer will not answer.
    GoAway { conn: ConnId, id: u64 },
    /// The connection is over, its requests with it. Call [`Client::close`].
    Closed { conn: ConnId, error: H3Error },
}

/// Request target. Scheme is https.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: &'a str,
    pub authority: &'a str,
    pub path: &'a str,
}

/// Column storage a [`Client`] borrows. [`HeapStorage`] builds one on the heap.
pub struct ClientStorage<'a> {
    pub conn_slots: &'a mut [Option<Conn>],
    pub conn_generations: &'a mut [u32],
    pub conn_free: &'a mut [u32],
    pub req_slots: &'a mut [Option<Req>],
    pub req_generations: &'a mut [u32],
    pub req_free: &'a mut [u32],
    /// Raw HEADERS payload, one equal region per request slot.
    pub sections: &'a mut [u8],
    /// Decoded response headers, one equal region per request slot.
    pub header_bytes: &'a mut [u8],
}

/// Heap owner for a [`ClientStorage`]. Allocates once.
pub struct HeapStorage {
    conn_slots: Vec<Option<Conn>>,
    conn_generations: Vec<u32>,
    conn_free: Vec<u32>,
    req_slots: Vec<Option<Req>>,
    req_generations: Vec<u32>,
    req_free: Vec<u32>,
    sections: Vec<u8>,
    header_bytes: Vec<u8>,
}

impl HeapStorage {
    /// `header_bytes` per request for the raw section and again for the decoded one.
    pub fn new(max_connections: u32, max_requests: u32, header_bytes: usize) -> Self {
        let c = max_connections as usize;
        let r = max_requests as usize;
        HeapStorage {
            conn_slots: (0..c).map(|_| None).collect(),
            conn_generations: vec![0; c],
            conn_free: vec![0; c],
            req_slots: (0..r).map(|_| None).collect(),
            req_generations: vec![0; r],
            req_free: vec![0; r],
            sections: vec![0; header_bytes * r],
            header_bytes: vec![0; header_bytes * r],
        }
    }

    pub fn view(&mut self) -> ClientStorage<'_> {
        ClientStorage {
            conn_slots: &mut self.conn_slots,
            conn_generations: &mut self.conn_generations,
            conn_free: &mut self.conn_free,
            req_slots: &mut self.req_slots,
            req_generations: &mut self.req_generations,
            req_free: &mut self.req_free,
            sections: &mut self.sections,
            header_bytes: &mut self.header_bytes,
        }
    }
}

/// What a stream read ended on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Out of room or at a DATA payload with nowhere to put it.
    Paused,
    Blocked,
    Fin,
    Reset(u64),
    StreamFailed(H3Error),
    ConnFailed(H3Error),
}

pub struct Client<'a> {
    cfg: Config,
    ep: q::Endpoint,
    quic: q::ClientConfig,
    /// What [`Nanos`] zero is.
    base: Instant,
    conns: Pool<'a, Conn>,
    reqs: Pool<'a, Req>,
    sections: header::Store<'a>,
    headers: header::Store<'a>,
    events: VecDeque<Event>,
    /// Datagrams are copied in here, then split off for quinn.
    rx: BytesMut,
    tx: Vec<u8>,
    /// What the endpoint answers on its own, which a client never sends.
    response: Vec<u8>,
    /// One decoded QPACK line.
    line: Vec<u8>,
    /// A request's HEADERS frame and DATA frame header.
    head: Vec<u8>,
}

impl<'a> Client<'a> {
    /// `crypto` is the TLS 1.3 client config with ALPN `h3`, as
    /// `quinn_proto::crypto::rustls::QuicClientConfig` builds it.
    pub fn new(
        cfg: Config,
        crypto: Arc<dyn q::crypto::ClientConfig>,
        storage: ClientStorage<'a>,
    ) -> Result<Self, Error> {
        let requests = storage.req_slots.len();
        let conns = Pool::new(
            storage.conn_slots,
            storage.conn_generations,
            storage.conn_free,
        )
        .map_err(|e| Error::BadStorage(StorageError::Conns(e)))?;
        let reqs = Pool::new(storage.req_slots, storage.req_generations, storage.req_free)
            .map_err(|e| Error::BadStorage(StorageError::Reqs(e)))?;
        let sections = header::Store::new(storage.sections, requests)
            .map_err(|e| Error::BadStorage(StorageError::Sections(e)))?;
        let headers = header::Store::new(storage.header_bytes, requests)
            .map_err(|e| Error::BadStorage(StorageError::Headers(e)))?;
        let quic = quic::client_config(crypto, &cfg.transport, cfg.seed).map_err(Error::H3)?;
        let endpoint = quic::endpoint_config(cfg.seed).map_err(Error::H3)?;
        let ep = q::Endpoint::new(Arc::new(endpoint), None, true, None);
        let region = headers.per_request();
        Ok(Client {
            cfg,
            ep,
            quic,
            base: Instant::now(),
            conns,
            reqs,
            sections,
            headers,
            events: VecDeque::with_capacity(64),
            rx: BytesMut::with_capacity(RX_BLOCK),
            tx: Vec::with_capacity(usize::from(MAX_UDP_PAYLOAD) + 64),
            response: Vec::with_capacity(usize::from(MAX_UDP_PAYLOAD)),
            line: vec![0; region],
            head: vec![0; region + HEAD_SLACK + FRAME_HEAD],
        })
    }

    fn at(&self, now: Nanos) -> Instant {
        self.base + Duration::from_nanos(now.0)
    }

    fn nanos(&self, t: Instant) -> Nanos {
        let d = t.saturating_duration_since(self.base).as_nanos();
        Nanos(u64::try_from(d).unwrap_or(u64::MAX))
    }

    /// Receive window of one request stream.
    pub fn stream_window(&self) -> u64 {
        self.cfg.transport.stream_window
    }

    /// Start a connection to `remote`. `server_name` is the SNI and the name the
    /// certificate must cover (RFC 9001 4). Its Initial waits in [`Client::poll_transmit`].
    pub fn connect(
        &mut self,
        now: Nanos,
        server_name: &str,
        remote: SocketAddr,
    ) -> Result<ConnId, Error> {
        if self.conns.len() >= self.conns.capacity() {
            return Err(Error::TooManyConnections);
        }
        let at = self.at(now);
        let (ch, quic) = self
            .ep
            .connect(at, self.quic.clone(), remote, server_name)
            .map_err(|e| Error::H3(quic::connect_error(&e)))?;
        let conn = Conn {
            ch,
            quic,
            remote,
            state: ConnState::Handshaking,
            control: Control::new(),
            drained: false,
        };
        match self.conns.insert(conn) {
            Some(id) => Ok(id),
            None => {
                let _ = self.ep.handle_event(ch, q::EndpointEvent::drained());
                Err(Error::TooManyConnections)
            }
        }
    }

    pub fn state(&self, conn: ConnId) -> Option<ConnState> {
        self.conns.get(conn).map(|c| c.state)
    }

    /// Up, no GOAWAY, not closing. A new request may go on it.
    pub fn usable(&self, conn: ConnId) -> bool {
        self.conns.get(conn).is_some_and(|c| {
            c.state == ConnState::Up && c.control.goaway.is_none() && !c.quic.is_closed()
        })
    }

    pub fn goaway(&self, conn: ConnId) -> Option<u64> {
        self.conns.get(conn).and_then(|c| c.control.goaway)
    }

    pub fn stats(&self, conn: ConnId) -> Option<q::ConnectionStats> {
        self.conns.get(conn).map(|c| c.quic.stats())
    }

    /// Requests on `conn`, including those whose terminal event is unpolled.
    pub fn requests_on(&self, conn: ConnId) -> usize {
        self.reqs.iter().filter(|(_, r)| r.conn == conn).count()
    }

    /// End `conn` with `code` (RFC 9000 10.2) and drop it with its requests. Returns
    /// the CONNECTION_CLOSE datagram for the caller to send, if one is owed.
    pub fn close(&mut self, conn: ConnId, now: Nanos, code: u64, out: &mut [u8]) -> Option<usize> {
        let at = self.at(now);
        let mut c = self.conns.remove(conn)?;
        drop_requests(&mut self.reqs, conn);
        if !c.quic.is_closed() {
            c.quic.close(at, quic::code(code), Bytes::new());
        }
        self.tx.clear();
        let sent = c.quic.poll_transmit(at, 1, &mut self.tx);
        if !c.drained {
            let _ = self.ep.handle_event(c.ch, q::EndpointEvent::drained());
        }
        let t = sent?;
        let n = t.size.min(self.tx.len());
        out.get_mut(..n)?.copy_from_slice(&self.tx[..n]);
        Some(n)
    }

    /// A datagram received on `conn`'s socket.
    pub fn handle_datagram(&mut self, conn: ConnId, now: Nanos, data: &[u8]) -> Result<(), Error> {
        let remote = self.conns.get(conn).ok_or(Error::UnknownConnection)?.remote;
        let at = self.at(now);
        if self.rx.capacity() < data.len() {
            // Reclaims the block when quinn released every datagram of it.
            self.rx.reserve(RX_BLOCK.max(data.len()));
        }
        self.rx.extend_from_slice(data);
        let datagram = self.rx.split();
        self.response.clear();
        let event = self
            .ep
            .handle(at, remote, None, None, datagram, &mut self.response);
        match event {
            Some(q::DatagramEvent::ConnectionEvent(ch, ev)) => {
                let owner = match self.conns.get(conn).is_some_and(|c| c.ch == ch) {
                    true => Some(conn),
                    false => self.conns.iter().find(|(_, c)| c.ch == ch).map(|(h, _)| h),
                };
                if let Some(c) = owner.and_then(|o| self.conns.get_mut(o)) {
                    c.quic.handle_event(ev);
                }
                if let Some(o) = owner.filter(|o| *o != conn) {
                    self.drive(o, now);
                }
            }
            Some(q::DatagramEvent::NewConnection(incoming)) => self.ep.ignore(incoming),
            Some(q::DatagramEvent::Response(_)) | None => {}
        }
        self.drive(conn, now);
        Ok(())
    }

    /// Earliest time `conn` wants [`Client::handle_timeout`].
    pub fn poll_timeout(&mut self, conn: ConnId) -> Option<Nanos> {
        let t = self.conns.get_mut(conn)?.quic.poll_timeout()?;
        Some(self.nanos(t))
    }

    pub fn handle_timeout(&mut self, conn: ConnId, now: Nanos) {
        let at = self.at(now);
        if let Some(c) = self.conns.get_mut(conn) {
            c.quic.handle_timeout(at);
        }
        self.drive(conn, now);
    }

    /// One datagram owed to `conn`'s peer, written to `out`. `None` when nothing is
    /// owed or `out` is under [`MAX_UDP_PAYLOAD`].
    pub fn poll_transmit(&mut self, conn: ConnId, now: Nanos, out: &mut [u8]) -> Option<usize> {
        if out.len() < usize::from(MAX_UDP_PAYLOAD) {
            return None;
        }
        let at = self.at(now);
        let c = self.conns.get_mut(conn)?;
        self.tx.clear();
        let t = c.quic.poll_transmit(at, 1, &mut self.tx)?;
        let n = t.size.min(self.tx.len());
        out[..n].copy_from_slice(&self.tx[..n]);
        Some(n)
    }

    /// Requests whose terminal event is polled lose their row then.
    pub fn poll_event(&mut self) -> Option<Event> {
        let e = self.events.pop_front()?;
        match e {
            Event::Done { req }
            | Event::Reset { req, .. }
            | Event::Failed { req, .. }
            | Event::Unprocessed { req } => {
                self.reqs.remove(req);
            }
            Event::Closed { conn, .. } => drop_requests(&mut self.reqs, conn),
            _ => {}
        }
        Some(e)
    }

    /// Open a request stream on `conn` and send its HEADERS (RFC 9114 4.1). With
    /// `body_len` the body follows in one DATA frame through [`Client::write_body`].
    pub fn submit(
        &mut self,
        conn: ConnId,
        target: &Request<'_>,
        headers: &[Header<'_>],
        body_len: Option<u64>,
    ) -> Result<RequestId, Error> {
        check_request(target, headers)?;
        let c = self.conns.get(conn).ok_or(Error::UnknownConnection)?;
        if c.state != ConnState::Up || c.quic.is_closed() {
            return Err(Error::NotConnected);
        }
        if c.control.goaway.is_some() {
            return Err(Error::GoingAway);
        }
        if self.reqs.len() >= self.reqs.capacity() {
            return Err(Error::TooManyRequests);
        }
        let peer_max = c.control.max_field_section;
        let size = section_size(target, headers, body_len);
        if size > peer_max {
            return Err(Error::H3(H3Error::ExcessiveLoad {
                len: size,
                max: peer_max,
            }));
        }
        let (from, to) = encode_head(&mut self.head, target, headers, body_len)?;
        let c = self.conns.get_mut(conn).ok_or(Error::UnknownConnection)?;
        let stream = c
            .quic
            .streams()
            .open(q::Dir::Bi)
            .ok_or(Error::StreamsBlocked)?;
        let bytes = &self.head[from..to];
        let mut send = c.quic.send_stream(stream);
        // The head goes whole. A stream's first window is far above it.
        if send.write(bytes) != Ok(bytes.len()) {
            let _ = send.reset(quic::code(H3_REQUEST_CANCELLED));
            return Err(Error::H3(H3Error::Connect {
                what: "the stream's window takes no request head",
            }));
        }
        let body_len = body_len.unwrap_or(0);
        let finished = body_len == 0;
        if finished {
            let _ = send.finish();
        }
        let row = Req {
            conn,
            stream,
            reader: Reader::default(),
            state: ReqState::Head,
            section_len: 0,
            status: 0,
            header_len: 0,
            header_count: 0,
            body_len,
            body_sent: 0,
            finished,
            signalled: false,
        };
        match self.reqs.insert(row) {
            Some(id) => Ok(id),
            None => {
                let _ = c
                    .quic
                    .send_stream(stream)
                    .reset(quic::code(H3_REQUEST_CANCELLED));
                Err(Error::TooManyRequests)
            }
        }
    }

    /// Hand quinn what its send window takes of the body, from where it stands.
    /// `body` must be the whole body submitted. Returns octets taken this call.
    pub fn write_body(&mut self, req: RequestId, body: &[u8]) -> Result<usize, Error> {
        let r = self.reqs.get_mut(req).ok_or(Error::UnknownRequest)?;
        if body.len() as u64 != r.body_len {
            return Err(Error::BodyMismatch);
        }
        if r.finished {
            return Ok(0);
        }
        let c = self.conns.get_mut(r.conn).ok_or(Error::NotConnected)?;
        let mut send = c.quic.send_stream(r.stream);
        let start = r.body_sent;
        while r.body_sent < r.body_len {
            match send.write(&body[r.body_sent as usize..]) {
                Ok(0) => break,
                Ok(n) => r.body_sent += n as u64,
                // Stopped by a peer that answered without the rest.
                Err(q::WriteError::Stopped(_) | q::WriteError::ClosedStream) => {
                    r.finished = true;
                    return Ok((r.body_sent - start) as usize);
                }
                Err(q::WriteError::Blocked) => break,
            }
        }
        if r.body_sent == r.body_len {
            r.finished = true;
            let _ = send.finish();
        }
        Ok((r.body_sent - start) as usize)
    }

    /// Unsent request body octets. Zero for a gone request.
    pub fn body_owed(&self, req: RequestId) -> u64 {
        self.reqs
            .get(req)
            .filter(|r| !r.finished)
            .map_or(0, |r| r.body_len - r.body_sent)
    }

    /// Give up `req`: STOP_SENDING and RESET_STREAM with H3_REQUEST_CANCELLED, the
    /// connection kept. The row goes now, events already queued for it are stale.
    pub fn cancel(&mut self, req: RequestId) {
        if let Some(r) = self.reqs.remove(req)
            && let Some(c) = self.conns.get_mut(r.conn)
        {
            stop_stream(&mut c.quic, r.stream, r.finished);
        }
    }

    /// Copy response body bytes into `out`. 0 once nothing waits, the next
    /// [`Event::Body`] says when to come back.
    pub fn read_body(
        &mut self,
        req: RequestId,
        now: Nanos,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        if !self.reqs.contains(req) {
            return Err(Error::UnknownRequest);
        }
        Ok(self.pump(req, now, Some(out)))
    }

    /// Status of the final head. `None` before it.
    pub fn status(&self, req: RequestId) -> Option<u16> {
        self.reqs
            .get(req)
            .filter(|r| r.state != ReqState::Head)
            .map(|r| r.status)
    }

    /// Response header by case-insensitive name.
    pub fn header(&self, req: RequestId, name: &str) -> Option<&[u8]> {
        header::find(self.packed(req), name)
    }

    pub fn header_str(&self, req: RequestId, name: &str) -> Option<&str> {
        core::str::from_utf8(self.header(req, name)?).ok()
    }

    /// Captured headers in wire order, duplicates included.
    pub fn headers(&self, req: RequestId) -> header::Iter<'_> {
        header::iter(self.packed(req))
    }

    pub fn header_count(&self, req: RequestId) -> usize {
        self.status(req)
            .and_then(|_| self.reqs.get(req))
            .map_or(0, |r| r.header_count as usize)
    }

    fn packed(&self, req: RequestId) -> &[u8] {
        let Some(r) = self.reqs.get(req).filter(|r| r.state != ReqState::Head) else {
            return &[];
        };
        let region = self.headers.region(req.slot());
        &region[..(r.header_len as usize).min(region.len())]
    }

    /// Run quinn's events for `conn` and act on them.
    fn drive(&mut self, conn: ConnId, now: Nanos) {
        let mut connected = false;
        let mut lost = None;
        {
            let Client {
                conns,
                reqs,
                ep,
                events,
                ..
            } = self;
            let Some(c) = conns.get_mut(conn) else {
                return;
            };
            loop {
                let mut any = false;
                while let Some(ev) = c.quic.poll_endpoint_events() {
                    any = true;
                    c.drained |= ev.is_drained();
                    if let Some(back) = ep.handle_event(c.ch, ev) {
                        c.quic.handle_event(back);
                    }
                }
                while let Some(ev) = c.quic.poll() {
                    any = true;
                    match ev {
                        q::Event::Connected => connected = true,
                        q::Event::ConnectionLost { reason } => lost = Some(quic::loss(&reason)),
                        q::Event::Stream(s) => on_stream_event(reqs, events, conn, s),
                        _ => {}
                    }
                }
                if !any {
                    break;
                }
            }
        }
        if let Some(loss) = lost {
            return self.end(conn, now, H3Error::Lost { loss }, false);
        }
        if connected
            && self.state(conn) == Some(ConnState::Handshaking)
            && let Err(e) = self.open_control(conn)
        {
            return self.end(conn, now, e, true);
        }
        if self.state(conn) != Some(ConnState::Up) {
            return;
        }
        if let Err(e) = self.read_control(conn) {
            return self.end(conn, now, e, true);
        }
        self.read_heads(conn, now);
    }

    fn open_control(&mut self, conn: ConnId) -> Result<(), H3Error> {
        let max = self.cfg.max_field_section;
        let Some(c) = self.conns.get_mut(conn) else {
            return Ok(());
        };
        Control::open(&mut c.quic, max)?;
        c.state = ConnState::Up;
        self.events.push_back(Event::Connected { conn });
        Ok(())
    }

    /// The peer's uni streams. A GOAWAY ends the requests it did not take.
    fn read_control(&mut self, conn: ConnId) -> Result<(), H3Error> {
        let Some(c) = self.conns.get_mut(conn) else {
            return Ok(());
        };
        c.control.accept(&mut c.quic);
        let said = c.control.read_all(&mut c.quic)?;
        let Some(last) = said.goaway else {
            return Ok(());
        };
        self.events.push_back(Event::GoAway { conn, id: last });
        let mut slot = 0;
        while let Some(id) = self.reqs.next_live(slot) {
            slot = id.slot() + 1;
            let Some(r) = self.reqs.get_mut(id) else {
                continue;
            };
            let past = r.conn == conn && u64::from(r.stream) >= last;
            if !past || r.state == ReqState::Over {
                continue;
            }
            r.state = ReqState::Over;
            stop_stream(&mut c.quic, r.stream, r.finished);
            r.finished = true;
            self.events.push_back(Event::Unprocessed { req: id });
        }
        Ok(())
    }

    /// Read the heads of requests on `conn` still waiting for one.
    fn read_heads(&mut self, conn: ConnId, now: Nanos) {
        let mut slot = 0;
        while let Some(id) = self.reqs.next_live(slot) {
            slot = id.slot() + 1;
            let waits = self
                .reqs
                .get(id)
                .is_some_and(|r| r.conn == conn && r.state == ReqState::Head);
            if waits {
                self.pump(id, now, None);
            }
            if self.state(conn) != Some(ConnState::Up) {
                return;
            }
        }
    }

    /// The connection is over with `error`. With `close` this side closes it with
    /// the error's code (RFC 9114 8).
    fn end(&mut self, conn: ConnId, now: Nanos, error: H3Error, close: bool) {
        let at = self.at(now);
        let Some(c) = self.conns.get_mut(conn) else {
            return;
        };
        if close && !c.quic.is_closed() {
            c.quic.close(at, quic::code(error.code()), Bytes::new());
        }
        if c.state == ConnState::Closed {
            return;
        }
        c.state = ConnState::Closed;
        let mut slot = 0;
        while let Some(id) = self.reqs.next_live(slot) {
            slot = id.slot() + 1;
            if let Some(r) = self.reqs.get_mut(id).filter(|r| r.conn == conn) {
                r.state = ReqState::Over;
            }
        }
        self.events.push_back(Event::Closed { conn, error });
    }

    /// Read `req`'s stream: its head into the section region, its DATA into `out`
    /// when given. Returns body octets copied.
    fn pump(&mut self, req: RequestId, now: Nanos, out: Option<&mut [u8]>) -> usize {
        let Some(r) = self.reqs.get(req) else {
            return 0;
        };
        if r.state == ReqState::Over {
            return 0;
        }
        let conn = r.conn;
        let (copied, outcome) = {
            let Client {
                conns,
                reqs,
                sections,
                headers,
                line,
                events,
                ..
            } = self;
            let Some(c) = conns.get_mut(conn).filter(|c| c.state == ConnState::Up) else {
                return 0;
            };
            let Some(r) = reqs.get_mut(req) else {
                return 0;
            };
            let mut rd = Read {
                req,
                r,
                section: sections.region_mut(req.slot()),
                headers: headers.region_mut(req.slot()),
                line,
                events,
                out,
                copied: 0,
            };
            let outcome = rd.run(&mut c.quic);
            (rd.copied, outcome)
        };
        self.settle(req, conn, now, outcome);
        copied
    }

    /// Act on how a read of `req` ended.
    fn settle(&mut self, req: RequestId, conn: ConnId, now: Nanos, outcome: Outcome) {
        let Some(r) = self.reqs.get_mut(req) else {
            return;
        };
        match outcome {
            Outcome::Blocked => r.signalled = false,
            Outcome::Paused => {
                if r.state == ReqState::Body && !r.signalled {
                    r.signalled = true;
                    self.events.push_back(Event::Body { req });
                }
            }
            Outcome::Fin if r.state == ReqState::Body && r.reader.at_boundary() => {
                r.state = ReqState::Over;
                self.events.push_back(Event::Done { req });
            }
            Outcome::Fin => {
                let what = match r.state {
                    ReqState::Body => "the stream ended inside a frame",
                    _ => "the stream ended before a response",
                };
                self.fail(req, H3Error::Message { what });
            }
            Outcome::Reset(code) => {
                r.state = ReqState::Over;
                self.events.push_back(Event::Reset { req, code });
            }
            Outcome::StreamFailed(error) => self.fail(req, error),
            Outcome::ConnFailed(error) => self.end(conn, now, error, true),
        }
    }

    /// `req`'s response is refused with `error`. Its stream stops, the connection stays.
    fn fail(&mut self, req: RequestId, error: H3Error) {
        let Some(r) = self.reqs.get_mut(req) else {
            return;
        };
        if let Some(c) = self.conns.get_mut(r.conn) {
            stop_stream(&mut c.quic, r.stream, r.finished);
        }
        r.finished = true;
        r.state = ReqState::Over;
        self.events.push_back(Event::Failed { req, error });
    }
}

/// One read of a request stream, the borrows it needs split out of the client.
struct Read<'r, 'o> {
    req: RequestId,
    r: &'r mut Req,
    section: &'r mut [u8],
    headers: &'r mut [u8],
    line: &'r mut [u8],
    events: &'r mut VecDeque<Event>,
    out: Option<&'o mut [u8]>,
    copied: usize,
}

impl Read<'_, '_> {
    fn room(&self) -> Option<u64> {
        self.out.as_ref().map(|o| (o.len() - self.copied) as u64)
    }

    /// Bytes to ask quinn for next. `None` to pause.
    fn want(&self) -> Option<u64> {
        let want = match self.r.reader.in_payload() {
            Some((frame::DATA, left)) if self.r.state == ReqState::Body => left.min(self.room()?),
            _ => self.r.reader.want(),
        };
        (want > 0).then_some(want)
    }

    fn run(&mut self, quic: &mut q::Connection) -> Outcome {
        let stream = self.r.stream;
        let mut recv = quic.recv_stream(stream);
        let Ok(mut chunks) = recv.read(true) else {
            return Outcome::Blocked;
        };
        let outcome = loop {
            let Some(want) = self.want() else {
                break Outcome::Paused;
            };
            match chunks.next(usize::try_from(want).unwrap_or(usize::MAX)) {
                Ok(Some(chunk)) => {
                    if let Err(o) = self.bytes(&chunk.bytes) {
                        break o;
                    }
                }
                Ok(None) => break Outcome::Fin,
                Err(q::ReadError::Reset(code)) => break Outcome::Reset(code.into_inner()),
                Err(q::ReadError::Blocked) => break Outcome::Blocked,
            }
        };
        let _ = chunks.finalize();
        outcome
    }

    /// Stream bytes through the frame reader.
    fn bytes(&mut self, mut data: &[u8]) -> Result<(), Outcome> {
        let stream = u64::from(self.r.stream);
        while !data.is_empty() {
            let (n, piece) = self.r.reader.step(data);
            data = &data[n..];
            match piece {
                None => {}
                Some(Piece::Frame { ty, len }) => self.frame(ty, len, stream)?,
                Some(Piece::Payload { ty, bytes, end }) => self.payload(ty, bytes, end)?,
            }
        }
        Ok(())
    }

    fn frame(&mut self, ty: u64, len: u64, stream: u64) -> Result<(), Outcome> {
        let head = self.r.state == ReqState::Head;
        match ty {
            frame::HEADERS if head => {
                let max = self.section.len() as u64;
                if len == 0 || len > max {
                    return Err(Outcome::StreamFailed(H3Error::ExcessiveLoad { len, max }));
                }
                self.r.section_len = 0;
                Ok(())
            }
            // DATA before the response HEADERS (RFC 9114 4.1).
            frame::DATA if head => Err(Outcome::ConnFailed(H3Error::FrameUnexpected {
                frame: ty,
                stream,
            })),
            // No push without MAX_PUSH_ID (7.2.5).
            frame::PUSH_PROMISE => Err(Outcome::ConnFailed(H3Error::Id { id: stream })),
            // Control frames on a request stream (7.2).
            frame::SETTINGS | frame::GOAWAY | frame::MAX_PUSH_ID | frame::CANCEL_PUSH => {
                Err(Outcome::ConnFailed(H3Error::FrameUnexpected {
                    frame: ty,
                    stream,
                }))
            }
            // DATA framing, trailers, unknown and reserved types (9).
            _ => Ok(()),
        }
    }

    fn payload(&mut self, ty: u64, bytes: &[u8], end: bool) -> Result<(), Outcome> {
        match (ty, self.r.state) {
            (frame::HEADERS, ReqState::Head) => {
                let at = self.r.section_len;
                // Bounded by the frame length check.
                let Some(dst) = self.section.get_mut(at..at + bytes.len()) else {
                    return Err(Outcome::StreamFailed(H3Error::ExcessiveLoad {
                        len: (at + bytes.len()) as u64,
                        max: self.section.len() as u64,
                    }));
                };
                dst.copy_from_slice(bytes);
                self.r.section_len += bytes.len();
                if end {
                    self.head()?;
                }
                Ok(())
            }
            (frame::DATA, ReqState::Body) => {
                let Some(out) = self.out.as_deref_mut() else {
                    return Ok(());
                };
                let at = self.copied;
                // `want` never asks past the room.
                if let Some(dst) = out.get_mut(at..at + bytes.len()) {
                    dst.copy_from_slice(bytes);
                    self.copied += bytes.len();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A whole HEADERS frame before the final head. 1xx are skipped.
    fn head(&mut self) -> Result<(), Outcome> {
        let section = &self.section[..self.r.section_len];
        let max = self.headers.len();
        let decoded = qpack::decode_response(section, max, self.line, self.headers);
        let head = decoded.map_err(Outcome::StreamFailed)?;
        if (100..200).contains(&head.status) {
            return Ok(());
        }
        let r = &mut *self.r;
        (r.status, r.state) = (head.status, ReqState::Body);
        // Bounded by the u16 region.
        r.header_len = head.len.min(u16::MAX as usize) as u16;
        r.header_count = head.count.min(u16::MAX as usize) as u16;
        self.events.push_back(Event::Head {
            req: self.req,
            status: head.status,
        });
        Ok(())
    }
}

/// A quinn stream event for a request of `conn`.
fn on_stream_event(
    reqs: &mut Pool<'_, Req>,
    events: &mut VecDeque<Event>,
    conn: ConnId,
    ev: q::StreamEvent,
) {
    let find = |reqs: &Pool<'_, Req>, id: q::StreamId| {
        reqs.iter()
            .find(|(_, r)| r.conn == conn && r.stream == id && r.state != ReqState::Over)
            .map(|(h, _)| h)
    };
    match ev {
        q::StreamEvent::Readable { id } => {
            let Some(h) = find(reqs, id) else { return };
            if let Some(r) = reqs
                .get_mut(h)
                .filter(|r| r.state == ReqState::Body && !r.signalled)
            {
                r.signalled = true;
                events.push_back(Event::Body { req: h });
            }
        }
        q::StreamEvent::Writable { id } => {
            let Some(h) = find(reqs, id) else { return };
            if reqs.get(h).is_some_and(|r| !r.finished) {
                events.push_back(Event::Writable { req: h });
            }
        }
        q::StreamEvent::Stopped { id, .. } => {
            let Some(h) = find(reqs, id) else { return };
            if let Some(r) = reqs.get_mut(h) {
                r.finished = true;
            }
        }
        q::StreamEvent::Available { dir: q::Dir::Bi }
            if !events.contains(&Event::Available { conn }) =>
        {
            events.push_back(Event::Available { conn });
        }
        _ => {}
    }
}

/// STOP_SENDING, and RESET_STREAM unless this side finished.
fn stop_stream(quic: &mut q::Connection, stream: q::StreamId, finished: bool) {
    let code = quic::code(H3_REQUEST_CANCELLED);
    let _ = quic.recv_stream(stream).stop(code);
    if !finished {
        let _ = quic.send_stream(stream).reset(code);
    }
}

fn drop_requests(reqs: &mut Pool<'_, Req>, conn: ConnId) {
    let mut slot = 0;
    while let Some(id) = reqs.next_live(slot) {
        slot = id.slot() + 1;
        if reqs.get(id).is_some_and(|r| r.conn == conn) {
            reqs.remove(id);
        }
    }
}

/// RFC 9114 4.2: connection-specific fields are malformed, as in HTTP/2.
fn check_request(target: &Request<'_>, headers: &[Header<'_>]) -> Result<(), Error> {
    header::check_target(target.method, target.path, target.authority)
        .map_err(Error::BadRequest)?;
    for h in headers {
        header::check_field(h.name.as_bytes(), h.value.as_bytes()).map_err(Error::BadRequest)?;
        let owned = crate::h2::ENGINE_OWNED
            .iter()
            .any(|r| h.name.eq_ignore_ascii_case(r));
        if owned {
            return Err(Error::BadRequest(HeaderError::Reserved));
        }
    }
    Ok(())
}

/// Field section size as RFC 9114 4.2.2 counts it.
fn section_size(target: &Request<'_>, headers: &[Header<'_>], body_len: Option<u64>) -> u64 {
    let pseudo = [
        (":method", target.method),
        (":scheme", "https"),
        (":authority", target.authority),
        (":path", target.path),
    ];
    let fields = headers.iter().map(|h| (h.name, h.value));
    let all: usize = pseudo
        .into_iter()
        .chain(fields)
        .map(|(n, v)| qpack::field_size(n.as_bytes(), v.as_bytes()))
        .sum();
    let length = body_len.map_or(0, |_| "content-length".len() + 20 + 32);
    (all + length) as u64
}

/// HEADERS frame of the request, then the DATA frame header when there is a body,
/// into `buf`. Returns the span written.
fn encode_head(
    buf: &mut [u8],
    target: &Request<'_>,
    headers: &[Header<'_>],
    body_len: Option<u64>,
) -> Result<(usize, usize), Error> {
    let too_big = Error::H3(H3Error::ExcessiveLoad {
        len: buf.len() as u64,
        max: buf.len().saturating_sub(FRAME_HEAD) as u64,
    });
    let (front, rest) = buf.split_at_mut(FRAME_HEAD);
    let mut digits = [0u8; 20];
    let length = body_len.map(|n| ("content-length", decimal(n, &mut digits)));
    let mut out = Out::new(rest);
    let fields = headers.iter().map(|h| (h.name, h.value)).chain(length);
    qpack::put_request(
        &mut out,
        target.method,
        target.authority,
        target.path,
        fields,
    )
    .map_err(|_| too_big)?;
    let section = out.len();
    if let Some(n) = body_len.filter(|n| *n > 0) {
        frame::put_header(&mut out, frame::DATA, n).map_err(|_| too_big)?;
    }
    let end = FRAME_HEAD + out.len();
    let mut tag = [0u8; FRAME_HEAD];
    let mut t = Out::new(&mut tag);
    frame::put_header(&mut t, frame::HEADERS, section as u64).map_err(|_| too_big)?;
    let from = FRAME_HEAD - t.len();
    front[from..].copy_from_slice(t.written());
    Ok((from, end))
}

/// `n` in decimal, at the end of `buf`.
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
