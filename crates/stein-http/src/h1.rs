//! HTTP/1.1 response state machine. Transitions are returned for the caller to commit.

pub mod chunk;
pub mod dechunk;

use crate::buf::{BlockId, BlockPool};
use crate::header::{self, Header, HeaderError};

/// Max headers per head. More is refused, not truncated.
pub const MAX_HEADERS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    AwaitingStatus,
    ReadingBody {
        remaining: u64,
    },
    /// `remaining` is bytes left in the current chunk.
    ReadingChunk {
        remaining: u64,
    },
    AwaitingChunkSize,
    /// Chunk data done, its CRLF not yet read.
    AwaitingChunkEnd,
    Complete,
    Failed(Error),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Malformed,
    HeadersTooLarge,
    TooManyHeaders,
    /// Undecodable transfer coding, or any for a caller refusing chunked. Server answers 501.
    UnsupportedTransfer,
    BodyTooLong,
    /// Final coding not chunked. Answer 400 and close (RFC 9112 6.3).
    TransferNotChunked,
    /// `Transfer-Encoding` with `Content-Length`.
    TransferWithLength,
    /// `Transfer-Encoding` on HTTP/1.0 (RFC 9112 6.1).
    TransferOnOldVersion,
}

/// What a step produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    NeedMore,
    Head {
        status: u16,
        content_length: Option<u64>,
        chunked: bool,
        /// Packed header bytes written to the capture region.
        header_len: u16,
        /// Excludes dropped `Set-Cookie`. Zero for an empty region.
        header_count: u16,
        /// Another request may follow (RFC 9112 9.3). Never for 1xx.
        keep_alive: bool,
    },
    /// Body bytes still in the block. Caller must consume `len` before stepping again.
    Body {
        offset: usize,
        len: usize,
    },
    Done,
    Failed(Error),
}

/// A transition for the caller to commit.
#[derive(Debug, PartialEq, Eq)]
pub struct Transition {
    pub from: State,
    pub to: State,
}

pub struct Decoder {
    state: State,
    /// Head bytes seen so far, bounds header size across partial reads.
    head_bytes: usize,
    max_head: usize,
}

impl Decoder {
    pub fn new(max_head: usize) -> Self {
        Decoder {
            state: State::AwaitingStatus,
            head_bytes: 0,
            max_head,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Complete | State::Failed(_))
    }

    pub fn commit(&mut self, t: Transition) {
        debug_assert_eq!(self.state, t.from, "transition applied out of order");
        self.state = t.to;
    }

    /// Caller read `n` length-framed body octets itself. `None` unless in `ReadingBody`.
    pub fn skip_body(&self, n: u64) -> Option<Transition> {
        let State::ReadingBody { remaining } = self.state else {
            return None;
        };
        let left = remaining.saturating_sub(n);
        let to = match left {
            0 => State::Complete,
            left => State::ReadingBody { remaining: left },
        };
        Some(Transition {
            from: self.state,
            to,
        })
    }

    /// Parse from `block`, consuming what it uses. Returns the step and any transition.
    /// A head is packed into `headers`. Empty keeps none, overrun is [`Error::HeadersTooLarge`].
    pub fn step(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
        headers: &mut [u8],
    ) -> (Step, Option<Transition>) {
        match self.state {
            State::AwaitingStatus => self.parse_head(pool, block, headers),
            State::ReadingBody { remaining } => self.read_body(pool, block, remaining),
            State::AwaitingChunkSize => self.read_chunk_size(pool, block),
            State::AwaitingChunkEnd => self.read_chunk_end(pool, block),
            State::ReadingChunk { remaining } => self.read_chunk(pool, block, remaining),
            State::Complete => (Step::Done, None),
            State::Failed(e) => (Step::Failed(e), None),
        }
    }

    fn fail(&self, e: Error) -> (Step, Option<Transition>) {
        (
            Step::Failed(e),
            Some(Transition {
                from: self.state,
                to: State::Failed(e),
            }),
        )
    }

    fn parse_head(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
        out: &mut [u8],
    ) -> (Step, Option<Transition>) {
        let data = pool.data(block);
        if data.len() > self.max_head {
            return self.fail(Error::HeadersTooLarge);
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut headers);
        let parsed = match resp.parse(data) {
            Ok(p) => p,
            Err(httparse::Error::TooManyHeaders) => return self.fail(Error::TooManyHeaders),
            Err(_) => return self.fail(Error::Malformed),
        };
        let consumed = match parsed {
            httparse::Status::Partial => {
                self.head_bytes = data.len();
                if self.head_bytes > self.max_head {
                    return self.fail(Error::HeadersTooLarge);
                }
                return (Step::NeedMore, None);
            }
            httparse::Status::Complete(n) => n,
        };

        let status = resp.code.unwrap_or(0);
        let version = resp.version;
        // Capture before consume, the records borrow the block.
        let scan = match scan_head(resp.headers, out) {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        pool.consume(block, consumed);

        (
            Step::Head {
                status,
                content_length: scan.content_length,
                chunked: scan.chunked,
                header_len: scan.header_len,
                header_count: scan.header_count,
                keep_alive: keeps_alive(version, status, &scan),
            },
            Some(Transition {
                from: self.state,
                to: after_head(status, scan.chunked, scan.content_length),
            }),
        )
    }

    fn read_body(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
        remaining: u64,
    ) -> (Step, Option<Transition>) {
        let avail = pool.len(block) as u64;
        if avail == 0 {
            return (Step::NeedMore, None);
        }
        let take = avail.min(remaining);
        let step = Step::Body {
            offset: 0,
            len: take as usize,
        };
        let left = remaining - take;
        let to = if left == 0 {
            State::Complete
        } else {
            State::ReadingBody { remaining: left }
        };
        (
            step,
            Some(Transition {
                from: self.state,
                to,
            }),
        )
    }

    fn read_chunk_size(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
    ) -> (Step, Option<Transition>) {
        let data = pool.data(block);
        let Some(eol) = find_crlf(data) else {
            if data.len() > self.max_head {
                return self.fail(Error::Malformed);
            }
            return (Step::NeedMore, None);
        };
        let line = &data[..eol];
        // Drop chunk extensions after ';'.
        let size_part = line.split(|b| *b == b';').next().unwrap_or(line);
        let text = match core::str::from_utf8(size_part) {
            Ok(t) => t.trim(),
            Err(_) => return self.fail(Error::Malformed),
        };
        let Ok(size) = u64::from_str_radix(text, 16) else {
            return self.fail(Error::Malformed);
        };
        pool.consume(block, eol + 2);
        let to = if size == 0 {
            State::Complete
        } else {
            State::ReadingChunk { remaining: size }
        };
        (
            Step::NeedMore,
            Some(Transition {
                from: self.state,
                to,
            }),
        )
    }

    fn read_chunk_end(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
    ) -> (Step, Option<Transition>) {
        let data = pool.data(block);
        if data.len() < 2 {
            return (Step::NeedMore, None);
        }
        if &data[..2] != b"\r\n" {
            return self.fail(Error::Malformed);
        }
        pool.consume(block, 2);
        (
            Step::NeedMore,
            Some(Transition {
                from: self.state,
                to: State::AwaitingChunkSize,
            }),
        )
    }

    fn read_chunk(
        &mut self,
        pool: &mut BlockPool,
        block: BlockId,
        remaining: u64,
    ) -> (Step, Option<Transition>) {
        let avail = pool.len(block) as u64;
        if avail == 0 {
            return (Step::NeedMore, None);
        }
        let take = avail.min(remaining);
        let step = Step::Body {
            offset: 0,
            len: take as usize,
        };
        let left = remaining - take;
        if left > 0 {
            return (
                step,
                Some(Transition {
                    from: self.state,
                    to: State::ReadingChunk { remaining: left },
                }),
            );
        }
        // Own state for the CRLF so a split between data and terminator works.
        (
            step,
            Some(Transition {
                from: self.state,
                to: State::AwaitingChunkEnd,
            }),
        )
    }
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    if data.len() < 2 {
        return None;
    }
    (0..data.len() - 1).find(|&i| data[i] == b'\r' && data[i + 1] == b'\n')
}

struct HeadScan {
    content_length: Option<u64>,
    chunked: bool,
    header_len: u16,
    header_count: u16,
    /// `Connection` tokens seen.
    close: bool,
    keep_alive: bool,
}

/// Whether another request may follow this response (RFC 9112 9.3).
/// Requires a framed body. Never for 1xx since the final response is still to come.
fn keeps_alive(version: Option<u8>, status: u16, scan: &HeadScan) -> bool {
    if (100..200).contains(&status) {
        return false;
    }
    let framed = scan.chunked || scan.content_length.is_some() || has_no_body(status);
    let persists = match version {
        Some(1) => !scan.close,
        Some(0) => scan.keep_alive && !scan.close,
        _ => false,
    };
    framed && persists
}

fn has_no_body(status: u16) -> bool {
    status == 204 || status == 304 || (100..200).contains(&status)
}

fn note_connection(value: &[u8], scan: &mut HeadScan) {
    let (close, keep_alive) = connection_options(value);
    scan.close |= close;
    scan.keep_alive |= keep_alive;
}

/// `(close, keep_alive)` tokens in a `Connection` value (RFC 9110 7.6.1).
fn connection_options(value: &[u8]) -> (bool, bool) {
    let (mut close, mut keep_alive) = (false, false);
    for token in value.split(|b| *b == b',') {
        let token = token.trim_ascii();
        close |= header::eq_ignore_case(token, b"close");
        keep_alive |= header::eq_ignore_case(token, b"keep-alive");
    }
    (close, keep_alive)
}

enum Framing {
    Plain,
    Length(u64),
    Chunked,
    /// `Set-Cookie`, never stored.
    Drop,
    Bad,
}

fn framing(h: &httparse::Header<'_>) -> Framing {
    if h.name.eq_ignore_ascii_case("content-length") {
        return match parse_u64(h.value) {
            Some(v) => Framing::Length(v),
            None => Framing::Bad,
        };
    }
    if h.name.eq_ignore_ascii_case("transfer-encoding") {
        return match contains_chunked(h.value) {
            true => Framing::Chunked,
            false => Framing::Bad,
        };
    }
    if h.name.eq_ignore_ascii_case("set-cookie") {
        return Framing::Drop;
    }
    Framing::Plain
}

/// Read framing off the head and pack every other header into `out`.
fn scan_head(headers: &[httparse::Header<'_>], out: &mut [u8]) -> Result<HeadScan, Error> {
    let mut scan = HeadScan {
        content_length: None,
        chunked: false,
        header_len: 0,
        header_count: 0,
        close: false,
        keep_alive: false,
    };
    for h in headers {
        if h.name.eq_ignore_ascii_case("connection") {
            note_connection(h.value, &mut scan);
        }
        match framing(h) {
            Framing::Bad => return Err(Error::Malformed),
            Framing::Drop => continue,
            Framing::Length(v) => scan.content_length = Some(v),
            Framing::Chunked => scan.chunked = true,
            Framing::Plain => {}
        }
        capture(h, out, &mut scan)?;
    }
    Ok(scan)
}

/// Append one header. Empty region keeps none, full region is an error.
fn capture(h: &httparse::Header<'_>, out: &mut [u8], scan: &mut HeadScan) -> Result<(), Error> {
    if out.is_empty() {
        return Ok(());
    }
    let at = scan.header_len as usize;
    let Some(end) = header::push(out, at, h.name.as_bytes(), h.value) else {
        return Err(Error::HeadersTooLarge);
    };
    if end > u16::MAX as usize {
        return Err(Error::HeadersTooLarge);
    }
    scan.header_len = end as u16;
    scan.header_count += 1;
    Ok(())
}

/// 1xx, 204 and 304 carry no body. A 304 `Content-Length` describes the cached entity.
fn after_head(status: u16, chunked: bool, content_length: Option<u64>) -> State {
    if has_no_body(status) {
        return State::Complete;
    }
    if chunked {
        return State::AwaitingChunkSize;
    }
    match content_length {
        Some(0) | None => State::Complete,
        Some(n) => State::ReadingBody { remaining: n },
    }
}

fn parse_u64(value: &[u8]) -> Option<u64> {
    core::str::from_utf8(value).ok()?.trim().parse::<u64>().ok()
}

fn contains_chunked(value: &[u8]) -> bool {
    let needle = b"chunked";
    if value.len() < needle.len() {
        return false;
    }
    (0..=value.len() - needle.len())
        .any(|i| header::eq_ignore_case(&value[i..i + needle.len()], needle))
}

/// Encode a request head into `out`, which is cleared first.
pub fn encode_request(
    out: &mut Vec<u8>,
    method: &str,
    path: &str,
    host: &str,
) -> Result<(), HeaderError> {
    encode_request_with(out, method, path, host, &[])
}

/// As [`encode_request`] plus caller headers, with `Connection: close`.
/// [`header::RESERVED`] names are refused. Nothing is written on error.
pub fn encode_request_with(
    out: &mut Vec<u8>,
    method: &str,
    path: &str,
    host: &str,
    headers: &[Header<'_>],
) -> Result<(), HeaderError> {
    encode_request_opts(out, method, path, host, headers, false)
}

/// As [`encode_request_with`]. `keep_alive` omits `Connection` (RFC 9112 9.3).
pub fn encode_request_opts(
    out: &mut Vec<u8>,
    method: &str,
    path: &str,
    host: &str,
    headers: &[Header<'_>],
    keep_alive: bool,
) -> Result<(), HeaderError> {
    encode_request_head(out, method, path, host, headers, keep_alive, None)
}

/// As [`encode_request_opts`] with `Content-Length: body_len`. Caller appends the body.
pub fn encode_request_with_body(
    out: &mut Vec<u8>,
    method: &str,
    path: &str,
    host: &str,
    headers: &[Header<'_>],
    keep_alive: bool,
    body_len: u64,
) -> Result<(), HeaderError> {
    encode_request_head(out, method, path, host, headers, keep_alive, Some(body_len))
}

fn encode_request_head(
    out: &mut Vec<u8>,
    method: &str,
    path: &str,
    host: &str,
    headers: &[Header<'_>],
    keep_alive: bool,
    body_len: Option<u64>,
) -> Result<(), HeaderError> {
    header::check_target(method, path, host)?;
    header::check(headers)?;
    out.clear();
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    out.extend_from_slice(host.as_bytes());
    for h in headers {
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(h.value.as_bytes());
    }
    if let Some(n) = body_len {
        let mut digits = [0u8; 20];
        out.extend_from_slice(b"\r\nContent-Length: ");
        out.extend_from_slice(decimal(n, &mut digits));
    }
    match keep_alive {
        true => out.extend_from_slice(b"\r\n\r\n"),
        false => out.extend_from_slice(b"\r\nConnection: close\r\n\r\n"),
    }
    Ok(())
}

/// Decimal ASCII, written at the end of `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[at..];
        }
    }
}

/// Server side. Request parsing and response writing.
pub mod server {
    use super::{Error, MAX_HEADERS, connection_options, find_crlf};
    use crate::header::eq_ignore_case;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Transfer {
        /// Any `Transfer-Encoding` is [`Error::UnsupportedTransfer`].
        Refuse,
        /// Accept a sole `chunked` coding, decoded via [`super::dechunk`] (RFC 9112 6.3).
        Chunked,
    }

    /// A parsed request head, borrowing the buffer it was read from.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Request<'a> {
        pub method: &'a str,
        pub path: &'a str,
        pub content_length: Option<u64>,
        /// Only set with [`Transfer::Chunked`].
        pub chunked: bool,
        pub keep_alive: bool,
        /// Offset where the body starts.
        pub head_len: usize,
    }

    /// Parse a request head. `Ok(None)` means incomplete.
    pub fn parse_request(buf: &[u8]) -> Result<Option<Request<'_>>, Error> {
        parse_request_with(buf, Transfer::Refuse)
    }

    pub fn parse_request_with(
        buf: &[u8],
        transfer: Transfer,
    ) -> Result<Option<Request<'_>>, Error> {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let Some(head_len) = head(&mut req, buf)? else {
            return Ok(None);
        };

        let method = req.method.ok_or(Error::Malformed)?;
        let path = req.path.ok_or(Error::Malformed)?;
        let f = framing(req.version, req.headers, transfer)?;

        Ok(Some(Request {
            method,
            path,
            content_length: f.content_length,
            chunked: f.chunked,
            keep_alive: f.keep_alive,
            head_len,
        }))
    }

    /// Length of a complete head. Bare LF is refused (RFC 9112 2.2), [`pack_headers`] needs CRLF.
    fn head<'b>(
        req: &mut httparse::Request<'_, 'b>,
        buf: &'b [u8],
    ) -> Result<Option<usize>, Error> {
        let n = match req.parse(buf) {
            Ok(httparse::Status::Complete(n)) => n,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(httparse::Error::TooManyHeaders) => return Err(Error::TooManyHeaders),
            Err(_) => return Err(Error::Malformed),
        };
        let head = buf.get(..n).ok_or(Error::Malformed)?;
        let bare = head
            .iter()
            .enumerate()
            .any(|(i, b)| *b == b'\n' && (i == 0 || head[i - 1] != b'\r'));
        if bare {
            return Err(Error::Malformed);
        }
        Ok(Some(n))
    }

    struct Framing {
        content_length: Option<u64>,
        chunked: bool,
        keep_alive: bool,
    }

    /// Digits only, optional spaces and tabs around.
    fn length(value: &[u8]) -> Result<u64, Error> {
        let ows = |b: &u8| *b == b' ' || *b == b'\t';
        let start = value.iter().position(|b| !ows(b)).ok_or(Error::Malformed)?;
        let end = value.iter().rposition(|b| !ows(b)).map_or(start, |i| i + 1);
        let mut n: u64 = 0;
        for b in &value[start..end] {
            if !b.is_ascii_digit() {
                return Err(Error::Malformed);
            }
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(b - b'0')))
                .ok_or(Error::Malformed)?;
        }
        Ok(n)
    }

    fn framing(
        version: Option<u8>,
        headers: &[httparse::Header<'_>],
        transfer: Transfer,
    ) -> Result<Framing, Error> {
        let mut content_length = None;
        let (mut close, mut keep) = (false, false);
        let mut codings = Codings::default();
        for h in headers {
            if h.name.eq_ignore_ascii_case("content-length") {
                let n = length(h.value)?;
                // Disagreeing lengths enable smuggling.
                if content_length.is_some_and(|prev| prev != n) {
                    return Err(Error::Malformed);
                }
                content_length = Some(n);
            } else if h.name.eq_ignore_ascii_case("connection") {
                let (c, k) = connection_options(h.value);
                close |= c;
                keep |= k;
            } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
                // Refuse early so the error stays the same.
                if transfer == Transfer::Refuse {
                    return Err(Error::UnsupportedTransfer);
                }
                codings.add(h.value)?;
            }
        }
        // 1.0 defaults to close, 1.1 to keep. `close` wins.
        let keep_alive = !close && (version == Some(1) || keep);
        let chunked = codings.seen;
        if chunked {
            if version != Some(1) {
                return Err(Error::TransferOnOldVersion);
            }
            codings.judge()?;
            if content_length.is_some() {
                return Err(Error::TransferWithLength);
            }
        }
        Ok(Framing {
            content_length,
            chunked,
            keep_alive,
        })
    }

    /// Codings across all `Transfer-Encoding` lines.
    #[derive(Default)]
    struct Codings {
        seen: bool,
        chunked: u32,
        other: u32,
        last_chunked: bool,
    }

    impl Codings {
        /// One field line. Bare tokens only, no parameters, quotes or empty elements.
        fn add(&mut self, value: &[u8]) -> Result<(), Error> {
            self.seen = true;
            for element in value.split(|b| *b == b',') {
                let name = trim(element);
                if name.is_empty() || !name.iter().all(|b| is_token(*b)) {
                    return Err(Error::Malformed);
                }
                self.last_chunked = eq_ignore_case(name, b"chunked");
                if self.last_chunked {
                    self.chunked += 1;
                } else {
                    self.other += 1;
                }
            }
            Ok(())
        }

        /// RFC 9112 sections 6.1 and 6.3.
        fn judge(&self) -> Result<(), Error> {
            if !self.last_chunked {
                return Err(Error::TransferNotChunked);
            }
            if self.chunked > 1 {
                return Err(Error::Malformed);
            }
            if self.other > 0 {
                return Err(Error::UnsupportedTransfer);
            }
            Ok(())
        }
    }

    /// Strip OWS.
    fn trim(mut v: &[u8]) -> &[u8] {
        while let [b' ' | b'\t', rest @ ..] = v {
            v = rest;
        }
        while let [rest @ .., b' ' | b'\t'] = v {
            v = rest;
        }
        v
    }

    fn is_token(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
    }

    /// A request head as offsets into its buffer, so the buffer stays writable.
    pub struct Located {
        pub method: (usize, usize),
        pub path: (usize, usize),
        pub content_length: Option<u64>,
        /// See [`Request::chunked`].
        pub chunked: bool,
        pub keep_alive: bool,
        pub head_len: usize,
        /// HTTP minor version.
        pub minor: u8,
        /// `[name_off, name_len, value_off, value_len]` per header.
        lines: [[u32; 4]; MAX_HEADERS],
        count: usize,
    }

    /// As [`parse_request`], as offsets.
    pub fn locate_request(buf: &[u8]) -> Result<Option<Located>, Error> {
        locate_request_with(buf, Transfer::Refuse)
    }

    pub fn locate_request_with(buf: &[u8], transfer: Transfer) -> Result<Option<Located>, Error> {
        if buf.len() > u32::MAX as usize {
            return Err(Error::HeadersTooLarge);
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let Some(head_len) = head(&mut req, buf)? else {
            return Ok(None);
        };
        let method = req.method.ok_or(Error::Malformed)?;
        let path = req.path.ok_or(Error::Malformed)?;
        let f = framing(req.version, req.headers, transfer)?;
        let mut lines = [[0u32; 4]; MAX_HEADERS];
        for (line, h) in lines.iter_mut().zip(req.headers.iter()) {
            let (na, nl) = offset(buf, h.name.as_bytes());
            let (va, vl) = offset(buf, h.value);
            // Packed record field widths.
            if nl > u8::MAX as usize || vl > u16::MAX as usize {
                return Err(Error::HeadersTooLarge);
            }
            *line = [na as u32, nl as u32, va as u32, vl as u32];
        }
        Ok(Some(Located {
            method: offset(buf, method.as_bytes()),
            path: offset(buf, path.as_bytes()),
            content_length: f.content_length,
            chunked: f.chunked,
            keep_alive: f.keep_alive,
            head_len,
            minor: req.version.unwrap_or(1),
            lines,
            count: req.headers.len(),
        }))
    }

    /// Pack header lines in place into [`crate::header`] records. Returns `(offset, len)`.
    /// Records are never longer than their lines, so nothing after the head moves.
    /// `buf` must be the located buffer.
    pub fn pack_headers(buf: &mut [u8], l: &Located) -> (usize, usize) {
        let Some(first) = l.lines.get(..l.count).and_then(|s| s.first()) else {
            return (l.head_len, 0);
        };
        let at = first[0] as usize;
        let mut w = at;
        for line in &l.lines[..l.count] {
            let [na, nl, va, vl] = line.map(|x| x as usize);
            let (name_to, value_to) = (w + 3, w + 3 + nl);
            // Copy whatever moves right first.
            if name_to > na {
                buf.copy_within(va..va + vl, value_to);
                buf.copy_within(na..na + nl, name_to);
            } else {
                buf.copy_within(na..na + nl, name_to);
                buf.copy_within(va..va + vl, value_to);
            }
            buf[w] = nl as u8;
            buf[w + 1..w + 3].copy_from_slice(&(vl as u16).to_le_bytes());
            w = value_to + vl;
        }
        (at, w - at)
    }

    /// `part` as `(offset, len)` in `whole`. `(0, 0)` if outside.
    fn offset(whole: &[u8], part: &[u8]) -> (usize, usize) {
        let base = whole.as_ptr() as usize;
        let at = part.as_ptr() as usize;
        if at < base || at + part.len() > base + whole.len() {
            return (0, 0);
        }
        (at - base, part.len())
    }

    /// Total length of a complete request, head plus body, if all of it is in `buf`.
    pub fn request_len(buf: &[u8]) -> Result<Option<usize>, Error> {
        let Some(r) = parse_request(buf)? else {
            return Ok(None);
        };
        let body = r.content_length.unwrap_or(0) as usize;
        let total = r.head_len.saturating_add(body);
        if buf.len() < total {
            return Ok(None);
        }
        Ok(Some(total))
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            202 => "Accepted",
            204 => "No Content",
            206 => "Partial Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            416 => "Range Not Satisfiable",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "Unknown",
        }
    }

    /// Encode a response into `out`, cleared first. Always sends `Content-Length`.
    pub fn encode_response(out: &mut Vec<u8>, status: u16, content_type: &str, body: &[u8]) {
        encode_response_with(out, status, content_type, &[], body)
    }

    /// As [`encode_response`] plus extra headers, written verbatim and unvalidated.
    pub fn encode_response_with(
        out: &mut Vec<u8>,
        status: u16,
        content_type: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) {
        encode_head(
            out,
            status,
            content_type,
            extra,
            Body::Length(body.len() as u64),
        );
        out.extend_from_slice(body);
    }

    /// How the body after a response head is framed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Body {
        /// `Content-Length`, the body this many bytes.
        Length(u64),
        /// `Transfer-Encoding: chunked`, the body in [`super::chunk`]'s framing.
        Chunked,
    }

    /// A response head alone into `out`, cleared first, for a body the caller sends from
    /// where it lies or as it comes. Extra headers are written verbatim and unvalidated.
    pub fn encode_head(
        out: &mut Vec<u8>,
        status: u16,
        content_type: &str,
        extra: &[(&str, &str)],
        body: Body,
    ) {
        out.clear();
        out.extend_from_slice(b"HTTP/1.1 ");
        let mut num = [0u8; 8];
        out.extend_from_slice(write_u16(&mut num, status));
        out.push(b' ');
        out.extend_from_slice(reason(status).as_bytes());
        out.extend_from_slice(b"\r\nContent-Type: ");
        out.extend_from_slice(content_type.as_bytes());
        match body {
            Body::Length(n) => {
                out.extend_from_slice(b"\r\nContent-Length: ");
                let mut len = [0u8; 20];
                out.extend_from_slice(write_u64(&mut len, n));
            }
            Body::Chunked => out.extend_from_slice(b"\r\nTransfer-Encoding: chunked"),
        }
        for (k, v) in extra {
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
        }
        out.extend_from_slice(b"\r\n\r\n");
    }

    fn write_u16(buf: &mut [u8; 8], mut v: u16) -> &[u8] {
        let mut n = 0;
        loop {
            buf[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
            if v == 0 {
                break;
            }
        }
        buf[..n].reverse();
        &buf[..n]
    }

    fn write_u64(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
        let mut n = 0;
        loop {
            buf[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
            if v == 0 {
                break;
            }
        }
        buf[..n].reverse();
        &buf[..n]
    }

    /// Keeps `find_crlf` used in a server build.
    #[allow(dead_code)]
    fn _uses_find_crlf(b: &[u8]) -> Option<usize> {
        find_crlf(b)
    }
}
