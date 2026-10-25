//! HTTP client state machine over caller-owned byte buffers.
//! No sockets, no clock. Tables and buffers come from a [`ClientStorage`].
//!
//! [`h2`] is the HTTP/2 connection. [`h3`] holds HTTP/3 framing and static QPACK,
//! and with the `h3` feature `h3::Client`, the same
//! sans-IO contract over quinn-proto's QUIC with caller-driven datagrams and
//! timers. [`altsvc`] reads `Alt-Svc` for an h3 alternative.

pub mod altsvc;
pub mod h1;
pub mod h2;
pub mod h3;
pub mod header;
pub mod hpack;

pub use stein_core::{buf, handle};

pub use header::{Header, HeaderError};

use buf::{BlockId, BlockMeta, BlockPool};
use h1::Decoder;
use handle::{Handle, Pool};

/// Milliseconds since a caller-chosen epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Millis(pub u64);

impl Millis {
    pub fn saturating_add(self, d: u64) -> Millis {
        Millis(self.0.saturating_add(d))
    }
}

/// Opaque. Public only so `Handle<Conn>` can be named.
pub struct Conn {
    rx: BlockId,
    tx: BlockId,
    /// In-flight requests, oldest first, linked through `Req::next` (RFC 9112 9.3.2).
    head: Option<RequestId>,
    tail: Option<RequestId>,
    /// Completed request. Row kept for header reads until the next completion, submit or close.
    done: Option<RequestId>,
    /// Octets at the front of `rx` named by the last [`Event::Body`], until polled past.
    body: usize,
}

pub struct Req {
    conn: ConnId,
    next: Option<RequestId>,
    decoder: Decoder,
    deadline: Millis,
    head_seen: bool,
    /// Both flags needed, a server may ignore a `Connection: close` it was sent.
    asked_keep: bool,
    keep_alive: bool,
    /// Packed header bytes and record count in this slot's store region.
    header_len: u16,
    header_count: u16,
}

pub type ConnId = Handle<Conn>;
pub type RequestId = Handle<Req>;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TooManyConnections,
    TooManyRequests,
    UnknownConnection,
    BuffersExhausted,
    ConnectionBusy,
    /// A kept connection holds bytes no request asked for. Caller should close it.
    UnsolicitedInput,
    ReceiveOverflow,
    /// Refused before queueing. CR/LF in a field, a reserved header, or a multi-line method/path/host.
    BadRequest(HeaderError),
    BadStorage(StorageError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum StorageError {
    Conns(handle::PoolError),
    Reqs(handle::PoolError),
    Blocks(buf::BufError),
    Headers(header::StoreError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Head {
        req: RequestId,
        status: u16,
        content_length: Option<u64>,
        /// Values are read with [`Client::header`].
        header_count: usize,
    },
    /// Body bytes, read with [`Client::body`] until the next [`Client::poll_event`] consumes them.
    Body {
        req: RequestId,
        len: usize,
    },
    Complete {
        req: RequestId,
    },
    Failed {
        req: RequestId,
        error: h1::Error,
    },
    TimedOut {
        req: RequestId,
    },
}

/// Policy only. Capacity comes from the storage passed to [`Client::new`].
pub struct Config {
    pub max_head: usize,
    pub request_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_head: 16 * 1024,
            request_timeout_ms: 30_000,
        }
    }
}

/// Request line and host. Extra headers go via [`Client::submit_with`], a body via [`Client::submit_body`].
pub struct Request {
    pub method: String,
    pub path: String,
    pub host: String,
}

/// Column storage a [`Client`] borrows. [`HeapStorage`] builds one on the heap.
pub struct ClientStorage<'a> {
    pub conn_slots: &'a mut [Option<Conn>],
    pub conn_generations: &'a mut [u32],
    pub conn_free: &'a mut [u32],
    pub req_slots: &'a mut [Option<Req>],
    pub req_generations: &'a mut [u32],
    pub req_free: &'a mut [u32],
    pub block_bytes: &'a mut [u8],
    pub block_meta: &'a mut [BlockMeta],
    pub block_free: &'a mut [u32],
    /// One equal region per request slot. Length must divide evenly by `req_slots.len()`.
    pub header_bytes: &'a mut [u8],
}

/// Heap-backed owner for a [`ClientStorage`]. Allocates once.
pub struct HeapStorage {
    conn_slots: Vec<Option<Conn>>,
    conn_generations: Vec<u32>,
    conn_free: Vec<u32>,
    req_slots: Vec<Option<Req>>,
    req_generations: Vec<u32>,
    req_free: Vec<u32>,
    block_bytes: Vec<u8>,
    block_meta: Vec<BlockMeta>,
    block_free: Vec<u32>,
    header_bytes: Vec<u8>,
}

/// Default response header bytes per in-flight request.
pub const DEFAULT_HEADER_BYTES: usize = 2048;

impl HeapStorage {
    pub fn new(
        max_connections: u32,
        max_requests: u32,
        block_size: usize,
        block_count: u32,
    ) -> Self {
        HeapStorage::with_header_bytes(
            max_connections,
            max_requests,
            block_size,
            block_count,
            DEFAULT_HEADER_BYTES,
        )
    }

    /// As [`HeapStorage::new`] with `header_bytes` per in-flight request.
    pub fn with_header_bytes(
        max_connections: u32,
        max_requests: u32,
        block_size: usize,
        block_count: u32,
        header_bytes: usize,
    ) -> Self {
        let c = max_connections as usize;
        let r = max_requests as usize;
        let b = block_count as usize;
        HeapStorage {
            conn_slots: (0..c).map(|_| None).collect(),
            conn_generations: vec![0; c],
            conn_free: vec![0; c],
            req_slots: (0..r).map(|_| None).collect(),
            req_generations: vec![0; r],
            req_free: vec![0; r],
            block_bytes: vec![0; block_size * b],
            block_meta: vec![BlockMeta::EMPTY; b],
            block_free: vec![0; b],
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
            block_bytes: &mut self.block_bytes,
            block_meta: &mut self.block_meta,
            block_free: &mut self.block_free,
            header_bytes: &mut self.header_bytes,
        }
    }
}

pub struct Client<'a> {
    cfg: Config,
    conns: Pool<'a, Conn>,
    reqs: Pool<'a, Req>,
    blocks: BlockPool<'a>,
    /// Indexed by request slot. Outlives the receive block.
    headers: header::Store<'a>,
    events: Vec<Event>,
    /// Connections with unsent bytes.
    pending_tx: Vec<ConnId>,
    scratch: Vec<u8>,
    /// Count of connections with `Conn::body > 0`.
    pending_body: u32,
    /// Committed state transitions, for debugging.
    pub transitions: Vec<(RequestId, h1::State, h1::State)>,
}

impl<'a> Client<'a> {
    pub fn new(cfg: Config, storage: ClientStorage<'a>) -> Result<Self, Error> {
        let request_slots = storage.req_slots.len();
        let conns = Pool::new(
            storage.conn_slots,
            storage.conn_generations,
            storage.conn_free,
        )
        .map_err(|e| Error::BadStorage(StorageError::Conns(e)))?;
        let reqs = Pool::new(storage.req_slots, storage.req_generations, storage.req_free)
            .map_err(|e| Error::BadStorage(StorageError::Reqs(e)))?;
        let blocks = BlockPool::new(storage.block_bytes, storage.block_meta, storage.block_free)
            .map_err(|e| Error::BadStorage(StorageError::Blocks(e)))?;
        let headers = header::Store::new(storage.header_bytes, request_slots)
            .map_err(|e| Error::BadStorage(StorageError::Headers(e)))?;
        Ok(Client {
            conns,
            reqs,
            blocks,
            headers,
            events: Vec::with_capacity(64),
            pending_tx: Vec::with_capacity(64),
            scratch: Vec::with_capacity(1024),
            pending_body: 0,
            transitions: Vec::with_capacity(64),
            cfg,
        })
    }

    /// Fixed buffer byte ceiling.
    pub fn buffer_ceiling(&self) -> usize {
        self.blocks.bytes()
    }

    /// Fixed response header byte ceiling.
    pub fn header_ceiling(&self) -> usize {
        self.headers.bytes()
    }

    pub fn open(&mut self) -> Result<ConnId, Error> {
        let rx = self.blocks.acquire().map_err(|_| Error::BuffersExhausted)?;
        let tx = match self.blocks.acquire() {
            Ok(t) => t,
            Err(_) => {
                self.blocks.release(rx);
                return Err(Error::BuffersExhausted);
            }
        };
        match self.conns.insert(Conn {
            rx,
            tx,
            head: None,
            tail: None,
            done: None,
            body: 0,
        }) {
            Some(c) => Ok(c),
            None => {
                self.blocks.release(rx);
                self.blocks.release(tx);
                Err(Error::TooManyConnections)
            }
        }
    }

    /// Drop a connection and all its request rows.
    pub fn close(&mut self, conn: ConnId) {
        let Some(c) = self.conns.remove(conn) else {
            self.pending_tx.retain(|p| *p != conn);
            return;
        };
        if c.body > 0 {
            self.pending_body -= 1;
        }
        self.blocks.release(c.rx);
        self.blocks.release(c.tx);
        let mut next = c.head;
        while let Some(req) = next {
            next = self.reqs.remove(req).and_then(|r| r.next);
        }
        if let Some(req) = c.done {
            self.reqs.remove(req);
        }
        self.pending_tx.retain(|p| *p != conn);
    }

    pub fn submit(&mut self, conn: ConnId, now: Millis, req: Request) -> Result<RequestId, Error> {
        self.submit_with(conn, now, req, &[])
    }

    /// As [`Client::submit`] with extra headers. Any invalid header refuses the whole request.
    pub fn submit_with(
        &mut self,
        conn: ConnId,
        now: Millis,
        req: Request,
        headers: &[Header<'_>],
    ) -> Result<RequestId, Error> {
        self.submit_opts(conn, now, req, headers, false, false)
    }

    /// As [`Client::submit_with`], asking for keep-alive. Check [`Client::keep_alive`]
    /// after the head, submit the next request after [`Event::Complete`].
    pub fn submit_keep_alive(
        &mut self,
        conn: ConnId,
        now: Millis,
        req: Request,
        headers: &[Header<'_>],
    ) -> Result<RequestId, Error> {
        self.submit_opts(conn, now, req, headers, true, false)
    }

    /// As [`Client::submit_keep_alive`], pipelined behind in-flight requests (RFC 9112 9.3.2).
    /// The one ahead must have asked for keep-alive, else [`Error::ConnectionBusy`].
    /// Pipeline idempotent requests only.
    pub fn submit_pipelined(
        &mut self,
        conn: ConnId,
        now: Millis,
        req: Request,
        headers: &[Header<'_>],
    ) -> Result<RequestId, Error> {
        self.submit_opts(conn, now, req, headers, true, true)
    }

    /// Request with a `Content-Length` body. Head and body must fit the transmit
    /// block together, else [`Error::BuffersExhausted`]. Never pipelined.
    /// Caller values for [`header::RESERVED`] names refuse the request.
    pub fn submit_body(
        &mut self,
        conn: ConnId,
        now: Millis,
        req: Request,
        headers: &[Header<'_>],
        body: &[u8],
        keep_alive: bool,
    ) -> Result<RequestId, Error> {
        let tx = self.ready_tx(conn, false)?;
        let mut scratch = core::mem::take(&mut self.scratch);
        let encoded = h1::encode_request_with_body(
            &mut scratch,
            &req.method,
            &req.path,
            &req.host,
            headers,
            keep_alive,
            body.len() as u64,
        );
        let outcome = match encoded {
            Err(e) => Err(Error::BadRequest(e)),
            Ok(()) => self.queue(conn, now, tx, &scratch, body, keep_alive),
        };
        self.scratch = scratch;
        outcome
    }

    fn submit_opts(
        &mut self,
        conn: ConnId,
        now: Millis,
        req: Request,
        headers: &[Header<'_>],
        keep_alive: bool,
        behind: bool,
    ) -> Result<RequestId, Error> {
        let tx = self.ready_tx(conn, behind)?;
        // Taken out so encode and queue can both borrow.
        let mut scratch = core::mem::take(&mut self.scratch);
        let encoded = h1::encode_request_opts(
            &mut scratch,
            &req.method,
            &req.path,
            &req.host,
            headers,
            keep_alive,
        );
        let outcome = match encoded {
            Err(e) => Err(Error::BadRequest(e)),
            Ok(()) => self.queue(conn, now, tx, &scratch, &[], keep_alive),
        };
        self.scratch = scratch;
        outcome
    }

    /// Whether `req`'s connection may carry another request (RFC 9112 9.3).
    /// False before the head, for close-delimited bodies, and once the request is gone.
    pub fn keep_alive(&self, req: RequestId) -> bool {
        self.reqs
            .get(req)
            .map(|r| r.head_seen && r.asked_keep && r.keep_alive)
            .unwrap_or(false)
    }

    /// Transmit block of an idle connection, or with `behind` one whose tail asked for keep-alive.
    fn ready_tx(&self, conn: ConnId, behind: bool) -> Result<BlockId, Error> {
        let c = self.conns.get(conn).ok_or(Error::UnknownConnection)?;
        if let Some(tail) = c.tail {
            let kept = self.reqs.get(tail).is_some_and(|r| r.asked_keep);
            if !(behind && kept) {
                return Err(Error::ConnectionBusy);
            }
            return Ok(c.tx);
        }
        // Nothing in flight, so stray bytes would parse as the next response.
        if !self.blocks.is_empty(c.rx) {
            return Err(Error::UnsolicitedInput);
        }
        Ok(c.tx)
    }

    /// Open the request row and queue head and body, both or neither.
    fn queue(
        &mut self,
        conn: ConnId,
        now: Millis,
        tx: BlockId,
        bytes: &[u8],
        body: &[u8],
        asked_keep: bool,
    ) -> Result<RequestId, Error> {
        if self.blocks.writable(tx) < bytes.len().saturating_add(body.len()) {
            return Err(Error::BuffersExhausted);
        }
        // Free the previous completed row. With one slot this is the room needed.
        if let Some(done) = self.conns.get_mut(conn).and_then(|c| c.done.take()) {
            self.reqs.remove(done);
        }
        let id = self
            .reqs
            .insert(Req {
                conn,
                next: None,
                decoder: Decoder::new(self.cfg.max_head),
                deadline: now.saturating_add(self.cfg.request_timeout_ms),
                head_seen: false,
                asked_keep,
                keep_alive: false,
                header_len: 0,
                header_count: 0,
            })
            .ok_or(Error::TooManyRequests)?;
        if self
            .blocks
            .append(tx, bytes)
            .and_then(|()| self.blocks.append(tx, body))
            .is_err()
        {
            self.reqs.remove(id);
            return Err(Error::BuffersExhausted);
        }
        let tail = self.conns.get(conn).and_then(|c| c.tail);
        match tail.and_then(|t| self.reqs.get_mut(t)) {
            Some(ahead) => ahead.next = Some(id),
            None => {
                if let Some(c) = self.conns.get_mut(conn) {
                    c.head = Some(id);
                }
            }
        }
        if let Some(c) = self.conns.get_mut(conn) {
            c.tail = Some(id);
        }
        if !self.pending_tx.contains(&conn) {
            self.pending_tx.push(conn);
        }
        Ok(id)
    }

    /// Response header by case-insensitive name. `None` before the head and after the request is gone.
    pub fn header(&self, req: RequestId, name: &str) -> Option<&[u8]> {
        header::find(self.packed_headers(req), name)
    }

    /// As [`Client::header`], for the many header values that are ASCII.
    pub fn header_str(&self, req: RequestId, name: &str) -> Option<&str> {
        core::str::from_utf8(self.header(req, name)?).ok()
    }

    /// All captured headers in wire order, duplicates included.
    pub fn headers(&self, req: RequestId) -> header::Iter<'_> {
        header::iter(self.packed_headers(req))
    }

    /// Zero before the head arrives.
    pub fn header_count(&self, req: RequestId) -> usize {
        self.reqs
            .get(req)
            .map(|r| r.header_count as usize)
            .unwrap_or(0)
    }

    fn packed_headers(&self, req: RequestId) -> &[u8] {
        let Some(r) = self.reqs.get(req) else {
            return &[];
        };
        if !r.head_seen {
            return &[];
        }
        let region = self.headers.region(req.slot());
        &region[..(r.header_len as usize).min(region.len())]
    }

    /// Feed received bytes and drive the parser. Waits behind unpolled [`Event::Body`] bytes.
    pub fn handle_input(&mut self, conn: ConnId, _now: Millis, bytes: &[u8]) -> Result<(), Error> {
        let rx = self.conns.get(conn).ok_or(Error::UnknownConnection)?.rx;
        self.blocks
            .append(rx, bytes)
            .map_err(|_| Error::ReceiveOverflow)?;
        self.drive(conn);
        Ok(())
    }

    /// Bytes of the last [`Event::Body`] of `req`, until the next [`Client::poll_event`].
    pub fn body(&self, req: RequestId) -> &[u8] {
        let conn = self.reqs.get(req).map(|r| r.conn);
        let Some(c) = conn
            .and_then(|c| self.conns.get(c))
            .filter(|c| c.head == Some(req))
        else {
            return &[];
        };
        self.blocks.data(c.rx).get(..c.body).unwrap_or(&[])
    }

    /// Consume read body bytes on every connection and parse on.
    fn settle(&mut self) {
        let mut slot = 0;
        while self.pending_body > 0 {
            let Some(conn) = self.conns.next_live(slot) else {
                return;
            };
            slot = conn.slot() + 1;
            let Some(c) = self.conns.get_mut(conn).filter(|c| c.body > 0) else {
                continue;
            };
            let (rx, len) = (c.rx, core::mem::take(&mut c.body));
            self.pending_body -= 1;
            self.blocks.consume(rx, len);
            self.drive(conn);
        }
    }

    fn holds_body(&self, conn: ConnId) -> bool {
        self.conns.get(conn).is_some_and(|c| c.body > 0)
    }

    /// `Content-Length` body octets the caller may read off the wire itself, when the
    /// receive block is empty. Report them with [`Client::took_body`].
    pub fn direct_body(&self, conn: ConnId) -> Option<(RequestId, u64)> {
        let c = self.conns.get(conn)?;
        let req = c.head?;
        if !self.blocks.is_empty(c.rx) || c.body > 0 {
            return None;
        }
        match self.reqs.get(req)?.decoder.state() {
            h1::State::ReadingBody { remaining } => Some((req, remaining)),
            _ => None,
        }
    }

    /// The caller took `n` octets offered by [`Client::direct_body`]. Too many is [`Error::ReceiveOverflow`].
    pub fn took_body(&mut self, conn: ConnId, n: u64) -> Result<(), Error> {
        let Some((req, owed)) = self.direct_body(conn) else {
            return Err(Error::ConnectionBusy);
        };
        if n > owed {
            return Err(Error::ReceiveOverflow);
        }
        let Some(r) = self.reqs.get_mut(req) else {
            return Err(Error::UnknownConnection);
        };
        let before = r.decoder.state();
        if let Some(t) = r.decoder.skip_body(n) {
            r.decoder.commit(t);
            self.transitions.push((req, before, r.decoder.state()));
        }
        if is_complete(&self.reqs, req) {
            self.events.push(Event::Complete { req });
            release(&mut self.conns, &mut self.reqs, conn, req);
        }
        Ok(())
    }

    /// Run the decoder until it needs input or emits body bytes. Commits and logs transitions.
    fn drive(&mut self, conn: ConnId) {
        if self.holds_body(conn) {
            return;
        }
        let mut guard = 0usize;
        loop {
            let Some(c) = self.conns.get(conn) else {
                return;
            };
            let (rx, Some(req)) = (c.rx, c.head) else {
                return;
            };
            guard += 1;
            // Bounded so a stream of tiny chunks cannot spin.
            assert!(guard <= 4096, "decoder made no progress");

            let Some(r) = self.reqs.get_mut(req) else {
                return;
            };
            if r.decoder.state() == h1::State::Complete {
                self.events.push(Event::Complete { req });
                release(&mut self.conns, &mut self.reqs, conn, req);
                continue;
            }
            if r.decoder.is_done() {
                break;
            }
            let before = r.decoder.state();
            let (step, transition) =
                r.decoder
                    .step(&mut self.blocks, rx, self.headers.region_mut(req.slot()));
            if let Some(t) = transition {
                r.decoder.commit(t);
                self.transitions.push((req, before, r.decoder.state()));
            }
            let stalled = matches!(step, h1::Step::NeedMore)
                && transition_was_none(&self.transitions, req, before);
            // Body bytes stay in the block until polled past.
            if let (h1::Step::Body { len, .. }, Some(c)) = (&step, self.conns.get_mut(conn)) {
                c.body = *len;
                self.pending_body += u32::from(*len > 0);
            }
            if !emit(&mut self.reqs, &mut self.events, req, step) || stalled {
                break;
            }
            if is_complete(&self.reqs, req) {
                self.events.push(Event::Complete { req });
                release(&mut self.conns, &mut self.reqs, conn, req);
                // A pipelined response may already be buffered.
                continue;
            }
        }
    }

    pub fn poll_transmit(&mut self, out: &mut [u8]) -> Option<usize> {
        while let Some(conn) = self.pending_tx.first().copied() {
            let Some(c) = self.conns.get(conn) else {
                self.pending_tx.remove(0);
                continue;
            };
            let tx = c.tx;
            let data = self.blocks.data(tx);
            if data.is_empty() {
                self.pending_tx.remove(0);
                continue;
            }
            let n = data.len().min(out.len());
            out[..n].copy_from_slice(&data[..n]);
            self.blocks.consume(tx, n);
            if self.blocks.is_empty(tx) {
                self.pending_tx.remove(0);
            }
            return Some(n);
        }
        None
    }

    /// Take bytes owed to one connection. For drivers with many sockets.
    pub fn poll_transmit_on(&mut self, conn: ConnId, out: &mut [u8]) -> Option<usize> {
        let tx = self.conns.get(conn)?.tx;
        let data = self.blocks.data(tx);
        if data.is_empty() || out.is_empty() {
            return None;
        }
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        self.blocks.consume(tx, n);
        if self.blocks.is_empty(tx) {
            self.pending_tx.retain(|p| *p != conn);
        }
        Some(n)
    }

    /// Polling past an [`Event::Body`] consumes its bytes.
    pub fn poll_event(&mut self) -> Option<Event> {
        if self.events.is_empty() {
            self.settle();
        }
        if self.events.is_empty() {
            return None;
        }
        Some(self.events.remove(0))
    }

    /// Earliest request deadline. Completed keep-alive requests have none.
    pub fn poll_timeout(&self) -> Option<Millis> {
        self.reqs
            .iter()
            .filter(|(_, r)| !idles_kept(r))
            .map(|(_, r)| r.deadline)
            .min()
    }

    /// Time out expired requests, together with those pipelined behind them.
    pub fn handle_timeout(&mut self, now: Millis) {
        let mut slot = 0;
        while let Some(id) = self.reqs.next_live(slot) {
            slot = id.slot() + 1;
            let expired = self
                .reqs
                .get(id)
                .map(|r| r.deadline <= now && !idles_kept(r))
                .unwrap_or(false);
            if !expired {
                continue;
            }
            let conn = self.reqs.get(id).map(|r| r.conn);
            let Some(conn) = conn else { continue };
            if let Some(c) = self.conns.get_mut(conn).filter(|c| c.body > 0) {
                c.body = 0;
                self.pending_body -= 1;
            }
            let mut next = Some(id);
            while let Some(req) = next {
                self.events.push(Event::TimedOut { req });
                next = self.reqs.remove(req).and_then(|r| r.next);
            }
            // Rows ahead of it stay in flight.
            if let Some(c) = self.conns.get_mut(conn) {
                if c.head == Some(id) {
                    c.head = None;
                }
                c.tail = last_of(&self.reqs, c.head);
                if let Some(r) = c.tail.and_then(|t| self.reqs.get_mut(t)) {
                    r.next = None;
                }
                c.done = None;
            }
        }
    }
}

fn last_of(reqs: &Pool<'_, Req>, from: Option<RequestId>) -> Option<RequestId> {
    let mut at = from?;
    while let Some(next) = reqs
        .get(at)
        .and_then(|r| r.next)
        .filter(|n| reqs.contains(*n))
    {
        at = next;
    }
    Some(at)
}

/// Complete on a kept connection. No deadline applies.
fn idles_kept(r: &Req) -> bool {
    r.asked_keep && r.decoder.state() == h1::State::Complete
}

fn transition_was_none(
    log: &[(RequestId, h1::State, h1::State)],
    req: RequestId,
    before: h1::State,
) -> bool {
    match log.last() {
        Some((r, from, _)) => !(*r == req && *from == before),
        None => true,
    }
}

/// Turn one decoder step into events. Returns whether to loop again.
fn emit(reqs: &mut Pool<'_, Req>, events: &mut Vec<Event>, req: RequestId, step: h1::Step) -> bool {
    match step {
        h1::Step::NeedMore => true,
        h1::Step::Head {
            status,
            content_length,
            header_len,
            header_count,
            keep_alive,
            ..
        } => {
            let count = record_head(reqs, req, header_len, header_count, keep_alive);
            events.push(Event::Head {
                req,
                status,
                content_length,
                header_count: count,
            });
            true
        }
        h1::Step::Body { len, .. } => {
            events.push(Event::Body { req, len });
            false
        }
        h1::Step::Done => false,
        h1::Step::Failed(e) => {
            events.push(Event::Failed { req, error: e });
            false
        }
    }
}

/// Record the captured head's length. Returns the recorded count.
fn record_head(
    reqs: &mut Pool<'_, Req>,
    req: RequestId,
    len: u16,
    count: u16,
    keep_alive: bool,
) -> usize {
    let Some(r) = reqs.get_mut(req) else {
        return 0;
    };
    r.head_seen = true;
    r.keep_alive = keep_alive;
    r.header_len = len;
    r.header_count = count;
    count as usize
}

/// Advance a connection past completed `req`. Its row becomes `Conn::done`.
fn release(conns: &mut Pool<'_, Conn>, reqs: &mut Pool<'_, Req>, conn: ConnId, req: RequestId) {
    let Some(c) = conns.get_mut(conn) else {
        return;
    };
    let next = reqs.get_mut(req).and_then(|r| r.next.take());
    c.head = next;
    if next.is_none() {
        c.tail = None;
    }
    if let Some(done) = c.done.replace(req).filter(|d| *d != req) {
        reqs.remove(done);
    }
}

fn is_complete(reqs: &Pool<'_, Req>, req: RequestId) -> bool {
    reqs.get(req)
        .map(|r| r.decoder.state() == h1::State::Complete)
        .unwrap_or(false)
}
