//! Messages: a fixed header, header fields, then the body.
//!
//! ```text
//! endian type flags version | body_len serial | a(yv) fields | pad 8 | body
//! ```
//!
//! [`parse`] turns bytes into a [`Head`], offsets into those bytes and no
//! copies, and [`write()`] builds a message in place with the body filled by a
//! closure.

use crate::error::WireError;
use crate::names;
use crate::read::{self, Reader};
use crate::sig;
use crate::write::Writer;

/// The spec's cap on one message.
pub const MAX_MESSAGE: usize = 1 << 27;
/// The fixed part, up to and including the fields array's length.
pub const FIXED: usize = 16;

/// Flags.
pub const NO_REPLY: u8 = 0x1;
pub const NO_AUTO_START: u8 = 0x2;
pub const INTERACTIVE: u8 = 0x4;

/// Header field codes.
pub const PATH: u8 = 1;
pub const INTERFACE: u8 = 2;
pub const MEMBER: u8 = 3;
pub const ERROR_NAME: u8 = 4;
pub const REPLY_SERIAL: u8 = 5;
pub const DESTINATION: u8 = 6;
pub const SENDER: u8 = 7;
pub const SIGNATURE: u8 = 8;
pub const UNIX_FDS: u8 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Call = 1,
    Return = 2,
    Error = 3,
    Signal = 4,
}

/// Where a string lives inside the message bytes. Empty when absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    fn of(at: usize, s: &str) -> Self {
        Self {
            at: at as u32,
            len: s.len() as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The text of a span, from the message it was parsed out of.
    pub fn get<'a>(&self, msg: &'a [u8]) -> &'a str {
        let b = msg
            .get(self.at as usize..(self.at + self.len) as usize)
            .unwrap_or(&[]);
        core::str::from_utf8(b).unwrap_or("")
    }
}

/// A parsed header: numbers, and spans into the message for the strings.
///
/// Plain data, so it can be kept, copied into a table or compared after
/// the bytes it points into have moved on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub kind: Kind,
    pub flags: u8,
    pub big: bool,
    pub serial: u32,
    /// Zero when the message answers nothing.
    pub reply_serial: u32,
    pub unix_fds: u32,
    pub path: Span,
    pub interface: Span,
    pub member: Span,
    pub error: Span,
    pub destination: Span,
    pub sender: Span,
    pub signature: Span,
    pub body: Span,
    /// The whole message's length.
    pub len: u32,
}

/// A message and its parsed header.
#[derive(Clone, Copy, Debug)]
pub struct Message<'a> {
    pub bytes: &'a [u8],
    pub head: Head,
}

impl<'a> Message<'a> {
    pub fn kind(&self) -> Kind {
        self.head.kind
    }

    pub fn serial(&self) -> u32 {
        self.head.serial
    }

    pub fn reply_serial(&self) -> Option<u32> {
        (self.head.reply_serial != 0).then_some(self.head.reply_serial)
    }

    /// An empty span reads as absent. No name may be empty, so nothing
    /// present is lost.
    fn opt(&self, s: Span) -> Option<&'a str> {
        (!s.is_empty()).then(|| s.get(self.bytes))
    }

    pub fn path(&self) -> Option<&'a str> {
        self.opt(self.head.path)
    }

    pub fn interface(&self) -> Option<&'a str> {
        self.opt(self.head.interface)
    }

    pub fn member(&self) -> Option<&'a str> {
        self.opt(self.head.member)
    }

    pub fn error_name(&self) -> Option<&'a str> {
        self.opt(self.head.error)
    }

    pub fn destination(&self) -> Option<&'a str> {
        self.opt(self.head.destination)
    }

    pub fn sender(&self) -> Option<&'a str> {
        self.opt(self.head.sender)
    }

    /// The body's signature, empty for no body.
    pub fn signature(&self) -> &'a str {
        self.head.signature.get(self.bytes)
    }

    pub fn body_bytes(&self) -> &'a [u8] {
        let b = self.head.body;
        self.bytes
            .get(b.at as usize..(b.at + b.len) as usize)
            .unwrap_or(&[])
    }

    /// A reader over the body.
    pub fn body(&self) -> Reader<'a> {
        Reader::new(self.body_bytes(), self.head.big)
    }

    /// Walk the whole body against its signature.
    pub fn walk(&self, f: &mut dyn FnMut(read::Item<'a>)) -> Result<(), WireError> {
        read::walk(&mut self.body(), self.signature().as_bytes(), f)
    }

    /// Whether this is `interface.member`.
    pub fn is(&self, interface: &str, member: &str) -> bool {
        self.interface() == Some(interface) && self.member() == Some(member)
    }
}

/// The length of the message at the front of `bytes`, once its fixed
/// header has arrived. `None` means more bytes are needed to know.
pub fn frame_len(bytes: &[u8]) -> Result<Option<usize>, WireError> {
    let Some(fixed) = bytes.get(..FIXED) else {
        return Ok(None);
    };
    let big = match fixed[0] {
        b'l' => false,
        b'B' => true,
        c => return Err(WireError::Endian(c)),
    };
    let word = |at: usize| {
        let w = [fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]];
        if big {
            u32::from_be_bytes(w)
        } else {
            u32::from_le_bytes(w)
        }
    };
    let body = word(4) as usize;
    let fields = word(12) as usize;
    let len = (FIXED + fields).next_multiple_of(8) + body;
    if len > MAX_MESSAGE {
        return Err(WireError::TooLarge(len));
    }
    Ok(Some(len))
}

/// Parse the message at the front of `bytes`, `None` until all of it is
/// there.
pub fn parse(bytes: &[u8]) -> Result<Option<Message<'_>>, WireError> {
    let Some(len) = frame_len(bytes)? else {
        return Ok(None);
    };
    let Some(bytes) = bytes.get(..len) else {
        return Ok(None);
    };
    let head = parse_head(bytes)?;
    Ok(Some(Message { bytes, head }))
}

/// Parse a whole message's header. `bytes` is exactly one message.
pub fn parse_head(bytes: &[u8]) -> Result<Head, WireError> {
    let big = bytes.first() == Some(&b'B');
    let mut r = Reader::at(bytes, 1, big);
    let kind = match r.u8()? {
        1 => Kind::Call,
        2 => Kind::Return,
        3 => Kind::Error,
        4 => Kind::Signal,
        k => return Err(WireError::Kind(k)),
    };
    let flags = r.u8()?;
    let version = r.u8()?;
    if version != 1 {
        return Err(WireError::Version(version));
    }
    let body_len = r.u32()?;
    let serial = r.u32()?;
    if serial == 0 {
        return Err(WireError::Serial);
    }
    let mut head = Head {
        kind,
        flags,
        big,
        serial,
        reply_serial: 0,
        unix_fds: 0,
        path: Span::default(),
        interface: Span::default(),
        member: Span::default(),
        error: Span::default(),
        destination: Span::default(),
        sender: Span::default(),
        signature: Span::default(),
        body: Span::default(),
        len: bytes.len() as u32,
    };
    let end = r.array(b'(')?;
    while r.more(end) {
        field(&mut r, &mut head)?;
    }
    r.end(end)?;
    r.align(8)?;
    head.body = Span {
        at: r.pos() as u32,
        len: body_len,
    };
    if r.pos() + body_len as usize != bytes.len() {
        return Err(WireError::Truncated);
    }
    required(&head)?;
    Ok(head)
}

/// One `(yv)` header field into `head`. Unknown codes are skipped, as the
/// spec says.
fn field(r: &mut Reader<'_>, head: &mut Head) -> Result<(), WireError> {
    r.begin_struct()?;
    let code = r.u8()?;
    let sig = r.variant()?;
    let want: &[u8] = match code {
        PATH => b"o",
        INTERFACE | MEMBER | ERROR_NAME | DESTINATION | SENDER => b"s",
        REPLY_SERIAL | UNIX_FDS => b"u",
        SIGNATURE => b"g",
        _ => return read::skip(r, sig.as_bytes()),
    };
    if sig.as_bytes() != want {
        return Err(WireError::Field(code));
    }
    match code {
        REPLY_SERIAL => head.reply_serial = r.u32()?,
        UNIX_FDS => head.unix_fds = r.u32()?,
        SIGNATURE => {
            let (at, s) = r.sig_at()?;
            head.signature = Span::of(at, s);
        }
        _ => {
            let (at, s) = r.str_at()?;
            if code == PATH && !names::is_path(s.as_bytes()) {
                return Err(WireError::Path);
            }
            // the names write refuses are refused coming in too
            let named = match code {
                INTERFACE => names::is_interface(s.as_bytes()),
                MEMBER => names::is_member(s.as_bytes()),
                ERROR_NAME => names::is_error(s.as_bytes()),
                DESTINATION | SENDER => names::is_bus_name(s.as_bytes()),
                _ => true,
            };
            if !named {
                return Err(WireError::Name);
            }
            let span = Span::of(at, s);
            match code {
                PATH => head.path = span,
                INTERFACE => head.interface = span,
                MEMBER => head.member = span,
                ERROR_NAME => head.error = span,
                DESTINATION => head.destination = span,
                _ => head.sender = span,
            }
        }
    }
    Ok(())
}

/// The fields each message type must carry.
fn required(h: &Head) -> Result<(), WireError> {
    let need = |s: Span, code| {
        if s.is_empty() {
            Err(WireError::Missing(code))
        } else {
            Ok(())
        }
    };
    if h.signature.is_empty() && h.body.len > 0 {
        return Err(WireError::Missing(SIGNATURE));
    }
    match h.kind {
        Kind::Call => {
            need(h.path, PATH)?;
            need(h.member, MEMBER)
        }
        Kind::Signal => {
            need(h.path, PATH)?;
            need(h.interface, INTERFACE)?;
            need(h.member, MEMBER)
        }
        Kind::Error => {
            need(h.error, ERROR_NAME)?;
            if h.reply_serial == 0 {
                return Err(WireError::Missing(REPLY_SERIAL));
            }
            Ok(())
        }
        Kind::Return if h.reply_serial == 0 => Err(WireError::Missing(REPLY_SERIAL)),
        Kind::Return => Ok(()),
    }
}

/// What goes in a header, borrowed.
#[derive(Clone, Copy, Debug)]
pub struct Fields<'s> {
    pub kind: Kind,
    pub flags: u8,
    pub serial: u32,
    pub reply_serial: u32,
    pub path: Option<&'s str>,
    pub interface: Option<&'s str>,
    pub member: Option<&'s str>,
    pub error: Option<&'s str>,
    pub destination: Option<&'s str>,
    /// The body's signature, "" for none.
    pub sig: &'s str,
}

impl<'s> Fields<'s> {
    pub fn new(kind: Kind, serial: u32) -> Self {
        Self {
            kind,
            flags: 0,
            serial,
            reply_serial: 0,
            path: None,
            interface: None,
            member: None,
            error: None,
            destination: None,
            sig: "",
        }
    }

    /// Check names before any of them goes on the wire.
    fn check(&self) -> Result<(), WireError> {
        let ok = |v: Option<&str>, f: fn(&[u8]) -> bool| v.is_none_or(|s| f(s.as_bytes()));
        if !ok(self.path, names::is_path) {
            return Err(WireError::Path);
        }
        let names_ok = ok(self.interface, names::is_interface)
            && ok(self.member, names::is_member)
            && ok(self.error, names::is_error)
            && ok(self.destination, names::is_bus_name);
        if !names_ok {
            return Err(WireError::Name);
        }
        sig::check(self.sig.as_bytes())?;
        Ok(())
    }
}

/// Build a message into `out`, the body written by `body` to match
/// `fields.sig`, which is checked. Returns the message's length.
pub fn write(
    out: &mut [u8],
    f: &Fields<'_>,
    body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
) -> Result<usize, WireError> {
    f.check()?;
    if f.serial == 0 {
        return Err(WireError::Serial);
    }
    let mut w = Writer::new(out);
    w.u8(b'l')?;
    w.u8(f.kind as u8)?;
    w.u8(f.flags)?;
    w.u8(1)?;
    w.u32(0)?;
    w.u32(f.serial)?;
    let a = w.begin_array(b'(')?;
    let strs = [
        (PATH, "o", f.path),
        (INTERFACE, "s", f.interface),
        (MEMBER, "s", f.member),
        (ERROR_NAME, "s", f.error),
        (DESTINATION, "s", f.destination),
    ];
    for (code, sig, v) in strs {
        if let Some(v) = v {
            header_str(&mut w, code, sig, v)?;
        }
    }
    if f.reply_serial != 0 {
        w.begin_struct()?;
        w.u8(REPLY_SERIAL)?;
        w.variant("u")?;
        w.u32(f.reply_serial)?;
    }
    if !f.sig.is_empty() {
        w.begin_struct()?;
        w.u8(SIGNATURE)?;
        w.variant("g")?;
        w.sig(f.sig)?;
    }
    w.end_array(a)?;
    w.pad(8)?;
    let start = w.len();
    body(&mut w)?;
    let body_len = w.len() - start;
    // A body that does not match its signature gets the connection dropped,
    // so it is walked here where the mistake can still be named.
    let mut r = Reader::new(&w.written()[start..], false);
    read::walk(&mut r, f.sig.as_bytes(), &mut |_| {})?;
    w.patch_u32(4, body_len as u32);
    Ok(w.len())
}

fn header_str(w: &mut Writer<'_>, code: u8, sig: &str, v: &str) -> Result<(), WireError> {
    w.begin_struct()?;
    w.u8(code)?;
    w.variant(sig)?;
    if code == PATH { w.path(v) } else { w.str(v) }
}

#[cfg(test)]
mod tests;
