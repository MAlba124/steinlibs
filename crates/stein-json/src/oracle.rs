//! The parser as it was before the lane rewrite, kept whole as the oracle
//! the differential tests and the fuzz target compare against. Byte at a
//! time, UTF-8 checked per string with `from_utf8`, escapes checked by its
//! own decoder.

#![allow(dead_code)]

use crate::{ErrorKind, JsonError, Kind, Node, Span};

pub const MAX_DEPTH: usize = 128;

/// How many nodes `src` needs, so the caller can size the slice.
pub fn count(src: &[u8]) -> Result<usize, JsonError> {
    let mut p = Parser::new(
        src,
        Sink {
            nodes: None,
            len: 0,
        },
    );
    p.run()?;
    Ok(p.sink.len)
}

/// Parse a whole document into `nodes`.
pub fn parse(src: &[u8], nodes: &mut [Node]) -> Result<usize, JsonError> {
    let mut p = Parser::new(
        src,
        Sink {
            nodes: Some(nodes),
            len: 0,
        },
    );
    p.run()?;
    Ok(p.sink.len)
}

#[inline]
fn err(kind: ErrorKind, at: usize) -> JsonError {
    JsonError {
        kind,
        at: at as u32,
    }
}

/// Where nodes go: a slice, or only a count.
struct Sink<'n> {
    nodes: Option<&'n mut [Node]>,
    len: usize,
}

impl Sink<'_> {
    fn push(&mut self, n: Node, at: usize) -> Result<u32, JsonError> {
        let i = self.len;
        if let Some(nodes) = self.nodes.as_deref_mut() {
            *nodes.get_mut(i).ok_or(err(ErrorKind::TooManyNodes, at))? = n;
        }
        self.len += 1;
        Ok(i as u32)
    }

    /// Close container `i` at byte `close`, its bracket.
    fn close(&mut self, i: u32, close: usize) {
        let end = self.len as u32;
        if let Some(n) = self
            .nodes
            .as_deref_mut()
            .and_then(|n| n.get_mut(i as usize))
        {
            n.end = end;
            n.span.len = close as u32 + 1 - n.span.at;
        }
    }
}

/// What the parser expects next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Value,
    /// After a value: a comma, a closing bracket, or the end.
    After,
    /// Just inside `{`: a key or `}`.
    FirstKey,
    /// After a comma in an object.
    Key,
    /// Just inside `[`: a value or `]`.
    FirstValue,
}

#[derive(Clone, Copy)]
struct Open {
    node: u32,
    object: bool,
}

struct Parser<'a, 'n> {
    src: &'a [u8],
    pos: usize,
    sink: Sink<'n>,
    stack: [Open; MAX_DEPTH],
    depth: usize,
}

impl<'a, 'n> Parser<'a, 'n> {
    fn new(src: &'a [u8], sink: Sink<'n>) -> Self {
        Self {
            src,
            pos: 0,
            sink,
            stack: [Open {
                node: 0,
                object: false,
            }; MAX_DEPTH],
            depth: 0,
        }
    }

    fn run(&mut self) -> Result<(), JsonError> {
        if self.src.len() > u32::MAX as usize {
            return Err(err(ErrorKind::TooLarge, 0));
        }
        let mut state = State::Value;
        loop {
            self.skip_ws();
            state = match state {
                State::Value => self.value()?,
                State::After => match self.after()? {
                    Some(s) => s,
                    None => break,
                },
                State::FirstKey if self.peek() == Some(b'}') => self.close()?,
                State::FirstKey | State::Key => self.key()?,
                State::FirstValue if self.peek() == Some(b']') => self.close()?,
                State::FirstValue => State::Value,
            };
        }
        if self.pos < self.src.len() {
            return Err(err(ErrorKind::Trailing, self.pos));
        }
        Ok(())
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    #[inline]
    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn leaf(&mut self, kind: Kind, escaped: bool, at: usize, len: usize) -> Result<(), JsonError> {
        let end = self.sink.len as u32 + 1;
        let span = Span {
            at: at as u32,
            len: len as u32,
        };
        self.sink.push(
            Node {
                kind,
                escaped,
                span,
                end,
            },
            at,
        )?;
        Ok(())
    }

    fn value(&mut self) -> Result<State, JsonError> {
        let at = self.pos;
        match self.peek().ok_or(err(ErrorKind::Truncated, at))? {
            b'{' => self.open(Kind::Object),
            b'[' => self.open(Kind::Array),
            b'"' => {
                self.string()?;
                Ok(State::After)
            }
            b't' => self.literal(b"true", Kind::True),
            b'f' => self.literal(b"false", Kind::False),
            b'n' => self.literal(b"null", Kind::Null),
            b'-' | b'0'..=b'9' => {
                let len = number(&self.src[at..]).ok_or(err(ErrorKind::BadNumber, at))?;
                self.pos += len;
                self.leaf(Kind::Number, false, at, len)?;
                Ok(State::After)
            }
            _ => Err(err(ErrorKind::BadByte, at)),
        }
    }

    fn open(&mut self, kind: Kind) -> Result<State, JsonError> {
        let at = self.pos;
        if self.depth == MAX_DEPTH {
            return Err(err(ErrorKind::TooDeep, at));
        }
        let node = self.sink.push(
            Node {
                kind,
                escaped: false,
                span: Span {
                    at: at as u32,
                    len: 1,
                },
                end: 0,
            },
            at,
        )?;
        let object = kind == Kind::Object;
        self.stack[self.depth] = Open { node, object };
        self.depth += 1;
        self.pos += 1;
        Ok(if object {
            State::FirstKey
        } else {
            State::FirstValue
        })
    }

    /// Close the innermost container on the bracket at `pos`.
    fn close(&mut self) -> Result<State, JsonError> {
        let Some(d) = self.depth.checked_sub(1) else {
            return Err(err(ErrorKind::BadByte, self.pos));
        };
        let open = self.stack[d];
        self.sink.close(open.node, self.pos);
        self.depth = d;
        self.pos += 1;
        Ok(State::After)
    }

    /// After a value: `None` when the root is done.
    fn after(&mut self) -> Result<Option<State>, JsonError> {
        let Some(top) = self.depth.checked_sub(1).map(|d| self.stack[d]) else {
            return Ok(None);
        };
        let at = self.pos;
        let (closer, next) = match top.object {
            true => (b'}', State::Key),
            false => (b']', State::Value),
        };
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                Ok(Some(next))
            }
            Some(c) if c == closer => self.close().map(Some),
            Some(_) => Err(err(ErrorKind::BadByte, at)),
            None => Err(err(ErrorKind::Truncated, at)),
        }
    }

    /// A key, its colon, and on to the value.
    fn key(&mut self) -> Result<State, JsonError> {
        match self.peek() {
            Some(b'"') => self.string()?,
            Some(_) => return Err(err(ErrorKind::BadKey, self.pos)),
            None => return Err(err(ErrorKind::Truncated, self.pos)),
        }
        self.skip_ws();
        match self.peek() {
            Some(b':') => {
                self.pos += 1;
                Ok(State::Value)
            }
            Some(_) => Err(err(ErrorKind::BadByte, self.pos)),
            None => Err(err(ErrorKind::Truncated, self.pos)),
        }
    }

    fn literal(&mut self, word: &[u8], kind: Kind) -> Result<State, JsonError> {
        let at = self.pos;
        let got = self.src.get(at..at + word.len());
        if got != Some(word) {
            let kind = match got {
                None if word.starts_with(&self.src[at..]) => ErrorKind::Truncated,
                _ => ErrorKind::BadLiteral,
            };
            return Err(err(kind, at));
        }
        self.pos += word.len();
        self.leaf(kind, false, at, word.len())?;
        Ok(State::After)
    }

    /// A string from its opening quote. Escapes are checked here so every
    /// later decode of it cannot fail.
    fn string(&mut self) -> Result<(), JsonError> {
        let start = self.pos + 1;
        let mut i = start;
        let mut escaped = false;
        loop {
            i = plain(self.src, i);
            match self.src.get(i) {
                None => return Err(err(ErrorKind::Truncated, i)),
                Some(b'"') => break,
                Some(b'\\') => {
                    escaped = true;
                    i += check_escape(&self.src[i..]).ok_or(err(ErrorKind::BadEscape, i))?;
                }
                Some(_) => return Err(err(ErrorKind::Control, i)),
            }
        }
        if core::str::from_utf8(&self.src[start..i]).is_err() {
            return Err(err(ErrorKind::Utf8, start));
        }
        self.pos = i + 1;
        self.leaf(Kind::String, escaped, start, i - start)
    }
}

/// The end of the run of plain string bytes from `i`: up to a quote, a
/// backslash, a control byte or the end.
#[inline]
fn plain(src: &[u8], i: usize) -> usize {
    let run = src[i.min(src.len())..]
        .iter()
        .position(|&b| b == b'"' || b == b'\\' || b < 0x20);
    run.map_or(src.len(), |n| i + n)
}

/// The length of the number at the front of `s`, if it is one.
fn number(s: &[u8]) -> Option<usize> {
    let digits = |from: usize| s[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut i = usize::from(s.first() == Some(&b'-'));
    match s.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => i += digits(i),
        _ => return None,
    }
    if s.get(i) == Some(&b'.') {
        let n = digits(i + 1);
        if n == 0 {
            return None;
        }
        i += 1 + n;
    }
    if let Some(b'e' | b'E') = s.get(i) {
        i += 1;
        if let Some(b'+' | b'-') = s.get(i) {
            i += 1;
        }
        let n = digits(i);
        if n == 0 {
            return None;
        }
        i += n;
    }
    Some(i)
}

// The escape checker of the time, so a change to text.rs cannot hide.
fn hex4(s: &[u8]) -> Option<u32> {
    let s = s.get(..4)?;
    s.iter().try_fold(0u32, |v, &c| {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        Some(v << 4 | u32::from(d))
    })
}

/// Decode the escape at the front of `s` (which starts with a backslash):
/// the char and how many bytes it took. `None` for anything malformed.
fn decode_escape(s: &[u8]) -> Option<(char, usize)> {
    let c = match *s.get(1)? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => return unicode(s),
        _ => return None,
    };
    Some((c, 2))
}

/// `\uXXXX`, or a surrogate pair `\uD8xx\uDCxx`.
fn unicode(s: &[u8]) -> Option<(char, usize)> {
    let hi = hex4(s.get(2..)?)?;
    match hi {
        0xd800..=0xdbff => {
            if s.get(6..8)? != b"\\u" {
                return None;
            }
            let lo = hex4(s.get(8..)?)?;
            if !(0xdc00..=0xdfff).contains(&lo) {
                return None;
            }
            let c = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
            Some((char::from_u32(c)?, 12))
        }
        // A low surrogate on its own is no character.
        0xdc00..=0xdfff => None,
        _ => Some((char::from_u32(hi)?, 6)),
    }
}

/// The length of the escape at the front of `s`, if it is well formed.
fn check_escape(s: &[u8]) -> Option<usize> {
    decode_escape(s).map(|(_, n)| n)
}
