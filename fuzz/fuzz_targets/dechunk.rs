//! Chunked decoder vs a recursive descent oracle of RFC 9112 7.1, fed whole, bytewise and in random cuts.
//! Input is 4 config bytes (limits, cut seed) then the wire. Body, end and error must match. The rest is also round-tripped through `chunk` encoders.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::h1::chunk;
use stein_http::h1::dechunk::{Decoder, Error, Event, Limits, Stepped};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Short,
    /// Message ended here, the next byte is not its.
    Done(usize),
    /// Failing byte lies in `lo..hi`.
    Bad(Error, usize, usize),
}

struct Out {
    body: Vec<u8>,
    end: End,
}

fuzz_target!(|data: &[u8]| {
    let Some((cfg, wire)) = data.split_first_chunk::<4>() else {
        return;
    };
    let lim = limits(*cfg);
    let want = oracle(wire, lim);

    let whole = feed(wire, lim, || usize::MAX);
    agree(&want, &whole, "whole");
    let bytes = feed(wire, lim, || 1);
    agree(&want, &bytes, "bytewise");
    let mut rng = Rng::new(cfg[3], wire.len());
    // Zero-length pieces must be harmless.
    let cuts = feed(wire, lim, || rng.below(24));
    agree(&want, &cuts, "cuts");

    round_trip(wire, cfg[3]);
});

fn limits(c: [u8; 4]) -> Limits {
    let max_body = if c[0] >= 0x80 {
        u64::MAX
    } else {
        u64::from(c[0]) * 3
    };
    Limits {
        max_body,
        max_ext: u32::from(c[1]),
        max_trailer: u32::from(c[2]) * 2,
    }
}

fn agree(want: &Out, got: &Out, what: &str) {
    assert!(got.body == want.body, "{what} body differs");
    match (want.end, got.end) {
        (End::Bad(a, at, _), End::Bad(b, lo, hi)) => {
            assert_eq!(a, b, "{what} error kind");
            assert!(
                lo <= at && at < hi,
                "{what} failed in {lo}..{hi}, oracle at {at}"
            );
        }
        (a, b) => assert_eq!(a, b, "{what} outcome"),
    }
}

/// Feed `wire` in pieces sized by `cut`.
fn feed(wire: &[u8], lim: Limits, mut cut: impl FnMut() -> usize) -> Out {
    let mut d = Decoder::new(lim);
    let mut body = Vec::new();
    let mut at = 0;
    // Pieces may be empty, so bound the loop.
    for _ in 0..4 * wire.len() + 64 {
        let n = cut().min(wire.len() - at);
        let src = &wire[at..at + n];
        match piece(&mut d, src, &mut body, lim) {
            Ok(Some(used)) => {
                let end = at + used;
                ended(&mut d, &wire[end..], &body);
                return Out {
                    body,
                    end: End::Done(end),
                };
            }
            Ok(None) => {}
            Err(e) => {
                assert!(!d.is_done());
                assert_eq!(d.step(b"0\r\n\r\n"), Err(e), "error is not sticky");
                return Out {
                    body,
                    end: End::Bad(e, at, at + n),
                };
            }
        }
        at += n;
        if at == wire.len() {
            assert!(!d.is_done());
            assert_eq!(d.body(), body.len() as u64);
            return Out {
                body,
                end: End::Short,
            };
        }
    }
    assert!(at == wire.len(), "cuts never covered the wire");
    Out {
        body,
        end: End::Short,
    }
}

/// Step through one piece. `Some(used)` at end of body, `None` when the piece is used up.
fn piece(
    d: &mut Decoder,
    src: &[u8],
    body: &mut Vec<u8>,
    lim: Limits,
) -> Result<Option<usize>, Error> {
    let mut at = 0;
    loop {
        let s = d.step(&src[at..])?;
        let rest = src.len() - at;
        assert!(s.consumed <= rest, "consumed past the input");
        match s.event {
            Event::NeedMore => {
                assert_eq!(s.consumed, rest, "NeedMore left input behind");
                assert!(!d.is_done());
                return Ok(None);
            }
            Event::Data { at: from, len } => {
                assert!(len > 0, "empty data");
                assert_eq!(from + len, s.consumed, "data does not end the step");
                body.extend_from_slice(&src[at + from..at + from + len]);
                assert_eq!(d.body(), body.len() as u64, "body count");
                assert!(d.body() <= lim.max_body, "body over the limit");
            }
            Event::End => {
                assert!(d.is_done());
                return Ok(Some(at + s.consumed));
            }
        }
        at += s.consumed;
    }
}

/// Once done, the decoder reads nothing more.
fn ended(d: &mut Decoder, rest: &[u8], body: &[u8]) {
    let end = Ok(Stepped {
        consumed: 0,
        event: Event::End,
    });
    assert_eq!(d.step(rest), end, "read past the end");
    assert_eq!(d.step(b""), end);
    assert_eq!(d.body(), body.len() as u64);
}

/// Reference parser. Limits are checked before the byte is judged, which picks the error kind.
struct Ref<'a> {
    s: &'a [u8],
    i: usize,
    lim: Limits,
    ext: u32,
    trailer: u32,
    body: Vec<u8>,
}

enum Stop {
    Short,
    Bad(Error, usize),
}

type R<T> = Result<T, Stop>;

const MAX_DIGITS: usize = 16;

fn oracle(wire: &[u8], lim: Limits) -> Out {
    let mut r = Ref {
        s: wire,
        i: 0,
        lim,
        ext: 0,
        trailer: 0,
        body: Vec::new(),
    };
    let end = match r.message() {
        Ok(at) => End::Done(at),
        Err(Stop::Short) => End::Short,
        Err(Stop::Bad(e, at)) => End::Bad(e, at, at + 1),
    };
    Out { body: r.body, end }
}

fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// RFC 9110 5.6.2.
fn is_tchar(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
}

fn is_vchar_or_obs(b: u8) -> bool {
    (0x21..=0x7e).contains(&b) || b >= 0x80
}

/// RFC 9110 5.6.4 qdtext.
fn is_qdtext(b: u8) -> bool {
    is_ws(b) || b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b) || b >= 0x80
}

/// Allowed after a backslash, and in a field value.
fn is_quotable(b: u8) -> bool {
    is_ws(b) || is_vchar_or_obs(b)
}

fn hex(b: u8) -> Option<u64> {
    char::from(b).to_digit(16).map(u64::from)
}

impl Ref<'_> {
    fn peek(&self) -> R<u8> {
        self.s.get(self.i).copied().ok_or(Stop::Short)
    }

    fn bad<T>(&self, e: Error) -> R<T> {
        Err(Stop::Bad(e, self.i))
    }

    fn take(&mut self) {
        self.i += 1;
    }

    fn message(&mut self) -> R<usize> {
        loop {
            let size = self.size_line()?;
            if size == 0 {
                return self.trailers();
            }
            self.data(size)?;
        }
    }

    fn size_line(&mut self) -> R<u64> {
        let mut size = 0u64;
        let mut n = 0;
        while let Some(v) = hex(self.peek()?) {
            if n == MAX_DIGITS {
                return self.bad(Error::SizeOverflow);
            }
            size = (size << 4) | v;
            n += 1;
            self.take();
        }
        if n == 0 {
            return self.bad(Error::BadSize);
        }
        match self.peek()? {
            b'\r' => {}
            b'\n' => return self.bad(Error::BadLineEnd),
            b';' | b' ' | b'\t' => self.ext()?,
            _ => return self.bad(Error::BadSize),
        }
        self.take();
        if self.peek()? != b'\n' {
            return self.bad(Error::BadLineEnd);
        }
        let total = (self.body.len() as u64).checked_add(size);
        if size != 0 && total.is_none_or(|t| t > self.lim.max_body) {
            return self.bad(Error::BodyTooLarge);
        }
        self.take();
        Ok(size)
    }

    fn data(&mut self, size: u64) -> R<()> {
        let avail = (self.s.len() - self.i) as u64;
        let n = avail.min(size) as usize;
        self.body.extend_from_slice(&self.s[self.i..self.i + n]);
        self.i += n;
        if (n as u64) < size {
            return Err(Stop::Short);
        }
        for want in *b"\r\n" {
            if self.peek()? != want {
                return self.bad(Error::MissingCrlf);
            }
            self.take();
        }
        Ok(())
    }

    // chunk-ext = *( BWS ";" BWS name [ BWS "=" BWS ( token / quoted ) ] )

    /// The line-ending CR is not counted.
    fn ext_peek(&self) -> R<u8> {
        let b = self.peek()?;
        if b != b'\r' && self.ext >= self.lim.max_ext {
            return self.bad(Error::ExtensionTooLarge);
        }
        Ok(b)
    }

    fn ext_take(&mut self) {
        self.ext += 1;
        self.i += 1;
    }

    /// Skip BWS, return the byte after it.
    fn bws(&mut self) -> R<u8> {
        loop {
            let b = self.ext_peek()?;
            if !is_ws(b) {
                return Ok(b);
            }
            self.ext_take();
        }
    }

    fn ext_token(&mut self) -> R<u8> {
        loop {
            let b = self.ext_peek()?;
            if !is_tchar(b) {
                return Ok(b);
            }
            self.ext_take();
        }
    }

    /// From the `;` or space after the digits up to the CR, not taken.
    fn ext(&mut self) -> R<()> {
        loop {
            if self.bws()? != b';' {
                return self.bad(Error::BadExtension);
            }
            self.ext_take();
            if !is_tchar(self.bws()?) {
                return self.bad(Error::BadExtension);
            }
            let mut b = self.ext_token()?;
            if is_ws(b) {
                b = self.bws()?;
                if b != b';' && b != b'=' {
                    return self.bad(Error::BadExtension);
                }
            }
            match b {
                b'\r' => return Ok(()),
                b';' => continue,
                b'=' => self.ext_take(),
                _ => return self.bad(Error::BadExtension),
            }
            self.ext_value()?;
            match self.ext_peek()? {
                b'\r' => return Ok(()),
                b';' => {}
                b if is_ws(b) => {}
                _ => return self.bad(Error::BadExtension),
            }
        }
    }

    fn ext_value(&mut self) -> R<()> {
        let v = self.bws()?;
        if is_tchar(v) {
            self.ext_token()?;
            return Ok(());
        }
        if v != b'"' {
            return self.bad(Error::BadExtension);
        }
        self.ext_take();
        loop {
            let b = self.ext_peek()?;
            if b == b'"' {
                self.ext_take();
                return Ok(());
            }
            if b == b'\\' {
                self.ext_take();
                if !is_quotable(self.ext_peek()?) {
                    return self.bad(Error::BadExtension);
                }
            } else if !is_qdtext(b) {
                return self.bad(Error::BadExtension);
            }
            self.ext_take();
        }
    }

    // trailer-section = *( field-name ":" field-value CRLF ) CRLF

    fn tr_peek(&self) -> R<u8> {
        let b = self.peek()?;
        if self.trailer >= self.lim.max_trailer {
            return self.bad(Error::TrailerTooLarge);
        }
        Ok(b)
    }

    fn tr_take(&mut self) {
        self.trailer += 1;
        self.i += 1;
    }

    fn trailers(&mut self) -> R<usize> {
        loop {
            // The final empty line is not counted.
            if self.peek()? == b'\r' {
                self.take();
                if self.peek()? != b'\n' {
                    return self.bad(Error::BadLineEnd);
                }
                self.take();
                return Ok(self.i);
            }
            self.field_line()?;
        }
    }

    fn field_line(&mut self) -> R<()> {
        if !is_tchar(self.tr_peek()?) {
            return self.bad(Error::BadTrailer);
        }
        while is_tchar(self.tr_peek()?) {
            self.tr_take();
        }
        if self.tr_peek()? != b':' {
            return self.bad(Error::BadTrailer);
        }
        self.tr_take();
        loop {
            let b = self.tr_peek()?;
            if b == b'\r' {
                break;
            }
            if !is_quotable(b) {
                return self.bad(Error::BadTrailer);
            }
            self.tr_take();
        }
        self.tr_take();
        if self.tr_peek()? != b'\n' {
            return self.bad(Error::BadLineEnd);
        }
        self.tr_take();
        Ok(())
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u8, len: usize) -> Rng {
        Rng(0x9e37_79b9_7f4a_7c15 ^ (u64::from(seed) << 32) ^ len as u64)
    }

    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Frame `payload` with mixed encoders, then append a following message.
fn round_trip(payload: &[u8], seed: u8) {
    let mut rng = Rng::new(seed, payload.len());
    let mut wire = Vec::new();
    let mut at = 0;
    let mut closed = false;
    while at < payload.len() {
        let n = (1 + rng.below(300)).min(payload.len() - at);
        closed = at + n == payload.len() && rng.below(2) == 0;
        frame_one(&mut wire, &payload[at..at + n], closed, &mut rng);
        at += n;
    }
    if !closed {
        wire.extend_from_slice(chunk::LAST);
    }
    let end = wire.len();
    wire.extend_from_slice(b"GET / HTTP/1.1\r\n");

    let lim = Limits {
        max_body: u64::MAX,
        ..Limits::default()
    };
    let want = Out {
        body: payload.to_vec(),
        end: End::Done(end),
    };
    agree(&want, &oracle(&wire, lim), "round trip oracle");
    agree(&want, &feed(&wire, lim, || usize::MAX), "round trip whole");
    let cuts = feed(&wire, lim, || 1 + rng.below(40));
    agree(&want, &cuts, "round trip cuts");
}

fn frame_one(wire: &mut Vec<u8>, data: &[u8], last: bool, rng: &mut Rng) {
    let len = data.len() as u64;
    let start = wire.len();
    match rng.below(3) {
        0 => {
            wire.resize(start + chunk::framed_len(len) as usize, 0);
            let n = chunk::encode(&mut wire[start..], data);
            assert_eq!(n, Some(wire.len() - start), "encode length");
            if last {
                wire.extend_from_slice(chunk::LAST);
            }
        }
        1 => {
            // Any header width from minimal up to the decoder max.
            let lo = chunk::gap(len);
            let gap = lo + rng.below(chunk::MAX_HEADER - lo + 1);
            let tail = chunk::TAIL + if last { chunk::LAST.len() } else { 0 };
            wire.resize(start + gap + data.len() + tail, 0);
            wire[start + gap..start + gap + data.len()].copy_from_slice(data);
            let n = chunk::frame(&mut wire[start..], gap, data.len(), last);
            assert_eq!(n, Some(wire.len() - start), "frame length");
        }
        _ => {
            wire.resize(start + chunk::MAX_HEADER, 0);
            let h = chunk::header(&mut wire[start..], len);
            assert_eq!(h, Some(chunk::gap(len)), "header length");
            wire.truncate(start + chunk::gap(len));
            wire.extend_from_slice(data);
            wire.extend_from_slice(b"\r\n");
            if last {
                wire.extend_from_slice(chunk::LAST);
            }
        }
    }
}
