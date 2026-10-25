//! Strict chunked request body decoder, RFC 9112 7.1.
//!
//! ```text
//! chunked-body = *chunk last-chunk trailer-section CRLF
//! chunk        = chunk-size [ chunk-ext ] CRLF chunk-data CRLF
//! chunk-size   = 1*HEXDIG
//! last-chunk   = 1*("0") [ chunk-ext ] CRLF
//! chunk-ext    = *( BWS ";" BWS chunk-ext-name [ BWS "=" BWS chunk-ext-val ] )
//! trailer-section = *( field-line CRLF )
//! ```
//!
//! No bare LF. At most sixteen size digits. BWS only around extension `;` and `=`
//! (RFC 9110 5.6.3). Trailers are validated and dropped.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Total body bytes. `u64::MAX` to judge via [`Decoder::body`] instead.
    pub max_body: u64,
    /// Extension bytes over the whole body.
    pub max_ext: u32,
    /// Trailer section bytes, line ends included.
    pub max_trailer: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_body: u64::MAX,
            max_ext: 256,
            max_trailer: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Size line not starting with a hex digit.
    BadSize,
    /// More than sixteen digits.
    SizeOverflow,
    BadExtension,
    /// CR without LF.
    BadLineEnd,
    /// Chunk data not followed by CRLF.
    MissingCrlf,
    BadTrailer,
    BodyTooLarge,
    ExtensionTooLarge,
    TrailerTooLarge,
}

impl Error {
    /// Status to answer with. Always close after.
    pub fn status(&self) -> u16 {
        match self {
            Error::BodyTooLarge => 413,
            Error::TrailerTooLarge => 431,
            _ => 400,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Input exhausted, all of it consumed.
    NeedMore,
    /// Body bytes `src[at..at + len]` of this step's input. Never empty.
    Data { at: usize, len: usize },
    /// `consumed` is where the message ends. Sticky.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stepped {
    pub consumed: usize,
    pub event: Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Size,
    Digits,
    /// BWS after size or value, `;` must follow.
    ExtBws,
    ExtSemi,
    ExtName,
    /// BWS after a name, `=` or `;` must follow.
    ExtNameBws,
    ExtEq,
    ExtToken,
    ExtQuoted,
    ExtEscape,
    ExtValueEnd,
    SizeLf,
    Data,
    DataCr,
    DataLf,
    /// Start of a trailer line or the final empty line.
    Trailer,
    TrailerName,
    TrailerValue,
    TrailerLf,
    EndLf,
    Done,
    Failed(Error),
}

const MAX_DIGITS: u8 = 16;

/// `Copy` state, holds no bytes.
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    state: State,
    limits: Limits,
    /// Size being read, then bytes left in the chunk.
    size: u64,
    digits: u8,
    body: u64,
    ext: u32,
    trailer: u32,
}

impl Decoder {
    pub fn new(limits: Limits) -> Decoder {
        Decoder {
            state: State::Size,
            limits,
            size: 0,
            digits: 0,
            body: 0,
            ext: 0,
            trailer: 0,
        }
    }

    pub fn body(&self) -> u64 {
        self.body
    }

    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Consume the front of `src` up to the next event.
    pub fn step(&mut self, src: &[u8]) -> Result<Stepped, Error> {
        let mut at = 0;
        loop {
            match self.state {
                State::Data => return Ok(self.data(src, at)),
                State::Done => return Ok(stepped(at, Event::End)),
                State::Failed(e) => return Err(e),
                _ => {}
            }
            let Some(&b) = src.get(at) else {
                return Ok(stepped(at, Event::NeedMore));
            };
            let taken = match self.line(&src[at..]) {
                Some(n) => Ok(n),
                None => self.byte(b).map(|()| 1),
            };
            match taken {
                Ok(n) => at += n,
                Err(e) => {
                    self.state = State::Failed(e);
                    return Err(e);
                }
            }
        }
    }

    /// Fast path for a whole data CRLF or a plain `size CRLF` line. Returns bytes taken.
    /// `None` defers to the byte path, which owns error decisions.
    fn line(&mut self, src: &[u8]) -> Option<usize> {
        if self.state == State::DataCr {
            if !src.starts_with(b"\r\n") {
                return None;
            }
            self.size = 0;
            self.digits = 0;
            self.state = State::Size;
            return Some(2);
        }
        if self.state != State::Size {
            return None;
        }
        let mut size = 0u64;
        let mut n = 0;
        while let Some(v) = src.get(n).copied().and_then(hex) {
            if n == MAX_DIGITS as usize {
                return None;
            }
            size = (size << 4) | u64::from(v);
            n += 1;
        }
        // over-limit chunks go to the byte path too
        let fits = self.body.checked_add(size)? <= self.limits.max_body;
        if n == 0 || !fits || src.get(n..n + 2) != Some(b"\r\n") {
            return None;
        }
        self.size = size;
        self.digits = n as u8;
        self.state = if size == 0 {
            State::Trailer
        } else {
            State::Data
        };
        Some(n + 2)
    }

    fn data(&mut self, src: &[u8], at: usize) -> Stepped {
        let avail = (src.len() - at) as u64;
        if avail == 0 {
            return stepped(at, Event::NeedMore);
        }
        let len = avail.min(self.size) as usize;
        self.size -= len as u64;
        self.body += len as u64;
        if self.size == 0 {
            self.state = State::DataCr;
        }
        stepped(at + len, Event::Data { at, len })
    }

    /// One non-data byte.
    fn byte(&mut self, b: u8) -> Result<(), Error> {
        self.state = match self.state {
            State::Size | State::Digits => self.size_byte(b)?,
            State::SizeLf => self.size_end(b)?,
            State::DataCr if b == b'\r' => State::DataLf,
            State::DataLf if b == b'\n' => {
                self.size = 0;
                self.digits = 0;
                State::Size
            }
            State::DataCr | State::DataLf => return Err(Error::MissingCrlf),
            State::Trailer | State::TrailerName | State::TrailerValue | State::TrailerLf => {
                self.trailer_byte(b)?
            }
            State::EndLf if b == b'\n' => State::Done,
            State::EndLf => return Err(Error::BadLineEnd),
            // handled in step
            State::Data | State::Done => self.state,
            State::Failed(e) => return Err(e),
            _ => self.ext_byte(b)?,
        };
        Ok(())
    }

    fn size_byte(&mut self, b: u8) -> Result<State, Error> {
        if let Some(v) = hex(b) {
            if self.digits == MAX_DIGITS {
                return Err(Error::SizeOverflow);
            }
            self.digits += 1;
            self.size = (self.size << 4) | u64::from(v);
            return Ok(State::Digits);
        }
        if self.state == State::Size {
            return Err(Error::BadSize);
        }
        match b {
            b'\r' => Ok(State::SizeLf),
            b'\n' => Err(Error::BadLineEnd),
            b';' | b' ' | b'\t' => self.ext_byte(b),
            _ => Err(Error::BadSize),
        }
    }

    /// Size line LF. Checks the body limit.
    fn size_end(&mut self, b: u8) -> Result<State, Error> {
        if b != b'\n' {
            return Err(Error::BadLineEnd);
        }
        if self.size == 0 {
            return Ok(State::Trailer);
        }
        let total = self.body.checked_add(self.size);
        if total.is_none_or(|t| t > self.limits.max_body) {
            return Err(Error::BodyTooLarge);
        }
        Ok(State::Data)
    }

    /// One extension byte, including `;` and preceding BWS.
    fn ext_byte(&mut self, b: u8) -> Result<State, Error> {
        // CR is not counted
        if b != b'\r' {
            if self.ext >= self.limits.max_ext {
                return Err(Error::ExtensionTooLarge);
            }
            self.ext += 1;
        }
        let ws = b == b' ' || b == b'\t';
        let next = match (self.state, b) {
            (State::ExtQuoted, b'"') => State::ExtValueEnd,
            (State::ExtQuoted, b'\\') => State::ExtEscape,
            (State::ExtQuoted, _) if is_qdtext(b) => State::ExtQuoted,
            (State::ExtEscape, _) if is_escapable(b) => State::ExtQuoted,
            (State::ExtQuoted | State::ExtEscape, _) => return Err(Error::BadExtension),
            (State::ExtSemi | State::ExtEq | State::ExtBws | State::ExtNameBws, _) if ws => {
                self.state
            }
            (State::Digits | State::ExtToken | State::ExtValueEnd, _) if ws => State::ExtBws,
            (State::ExtName, _) if ws => State::ExtNameBws,
            (
                State::Digits
                | State::ExtBws
                | State::ExtName
                | State::ExtNameBws
                | State::ExtToken
                | State::ExtValueEnd,
                b';',
            ) => State::ExtSemi,
            (State::ExtName | State::ExtNameBws, b'=') => State::ExtEq,
            (State::ExtName | State::ExtToken | State::ExtValueEnd, b'\r') => State::SizeLf,
            (State::ExtSemi | State::ExtName, _) if is_token(b) => State::ExtName,
            (State::ExtEq | State::ExtToken, _) if is_token(b) => State::ExtToken,
            (State::ExtEq, b'"') => State::ExtQuoted,
            _ => return Err(Error::BadExtension),
        };
        Ok(next)
    }

    fn trailer_byte(&mut self, b: u8) -> Result<State, Error> {
        if self.state == State::Trailer && b == b'\r' {
            return Ok(State::EndLf);
        }
        if self.trailer >= self.limits.max_trailer {
            return Err(Error::TrailerTooLarge);
        }
        self.trailer += 1;
        let next = match (self.state, b) {
            (State::Trailer | State::TrailerName, _) if is_token(b) => State::TrailerName,
            (State::TrailerName, b':') => State::TrailerValue,
            (State::TrailerValue, b'\r') => State::TrailerLf,
            (State::TrailerValue, _) if is_field_byte(b) => State::TrailerValue,
            (State::TrailerLf, b'\n') => State::Trailer,
            (State::TrailerLf, _) => return Err(Error::BadLineEnd),
            _ => return Err(Error::BadTrailer),
        };
        Ok(next)
    }
}

fn stepped(consumed: usize, event: Event) -> Stepped {
    Stepped { consumed, event }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// RFC 9110 5.6.2 tchar.
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// RFC 9110 5.6.4 qdtext.
fn is_qdtext(b: u8) -> bool {
    matches!(b, b'\t' | b' ' | 0x21 | 0x23..=0x5b | 0x5d..=0x7e | 0x80..=0xff)
}

/// RFC 9110 5.6.4 quoted-pair octet.
fn is_escapable(b: u8) -> bool {
    matches!(b, b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff)
}

/// Field value byte, OWS included.
fn is_field_byte(b: u8) -> bool {
    is_escapable(b)
}

#[cfg(test)]
mod tests;
