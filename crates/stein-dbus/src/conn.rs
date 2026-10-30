use core::fmt;

use crate::auth::{self, AuthError, Reply};
use crate::bus;
use crate::error::WireError;
use crate::msg::{self, Fields, Kind, Message};
use crate::names::MAX_NAME;
use crate::write::Writer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbusError {
    Wire(WireError),
    Auth(AuthError),
    /// Messages cannot be queued before authentication is done.
    NotReady,
    /// The send buffer has no room. Transmit what is queued and retry.
    TxFull,
    /// A message of this many bytes does not fit the receive buffer.
    RxTooSmall(usize),
    /// The bus answered Hello with an error.
    Hello,
    /// The connection failed earlier and is dead.
    Closed,
}

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(e) => write!(f, "{e}"),
            Self::Auth(e) => write!(f, "{e}"),
            Self::NotReady => write!(f, "connection not authenticated yet"),
            Self::TxFull => write!(f, "send buffer full"),
            Self::RxTooSmall(n) => write!(f, "message of {n} bytes exceeds the receive buffer"),
            Self::Hello => write!(f, "bus refused Hello"),
            Self::Closed => write!(f, "connection closed after an error"),
        }
    }
}

impl From<WireError> for DbusError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Full => Self::TxFull,
            e => Self::Wire(e),
        }
    }
}

impl From<AuthError> for DbusError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Auth,
    Hello,
    Ready,
    Closed,
}

/// A bus name held inline, so it outlives the buffer it was read from.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; MAX_NAME],
    len: u8,
}

impl Name {
    pub const EMPTY: Self = Self {
        bytes: [0; MAX_NAME],
        len: 0,
    };

    /// `None` when `s` is longer than a name may be.
    pub fn new(s: &str) -> Option<Self> {
        let mut n = Self::EMPTY;
        let dst = n.bytes.get_mut(..s.len())?;
        dst.copy_from_slice(s.as_bytes());
        n.len = s.len() as u8;
        Some(n)
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Who to answer, copied out of a call so the answer can come later.
#[derive(Clone, Copy, Debug)]
pub struct ReplyTo {
    pub serial: u32,
    /// Empty on a peer to peer connection, which has no bus to route by.
    pub sender: Name,
    /// The caller asked for no reply.
    pub no_reply: bool,
}

impl ReplyTo {
    pub fn of(m: &Message<'_>) -> Self {
        Self {
            serial: m.serial(),
            sender: m.sender().and_then(Name::new).unwrap_or(Name::EMPTY),
            no_reply: m.head.flags & msg::NO_REPLY != 0,
        }
    }
}

/// A method call.
#[derive(Clone, Copy, Debug)]
pub struct Call<'s> {
    pub destination: Option<&'s str>,
    pub path: &'s str,
    pub interface: Option<&'s str>,
    pub member: &'s str,
    /// The body's signature, "" for none.
    pub sig: &'s str,
    pub flags: u8,
}

/// A signal.
#[derive(Clone, Copy, Debug)]
pub struct Signal<'s> {
    /// Usually `None`, a broadcast.
    pub destination: Option<&'s str>,
    pub path: &'s str,
    pub interface: &'s str,
    pub member: &'s str,
    pub sig: &'s str,
}

/// The two buffers a connection works in. The receive buffer bounds the
/// largest message that can arrive.
pub struct Storage<'a> {
    pub rx: &'a mut [u8],
    pub tx: &'a mut [u8],
}

/// What one step of the state machine produced.
enum Step {
    /// Progress was made, step again.
    Again,
    /// Nothing more until more bytes arrive.
    Idle,
    Message(usize, msg::Head),
}

/// The sending half of a connection: the send buffer, serials, and what
/// the connection's state allows. Split from the receiving half so a
/// message being read can be answered through this while it is borrowed.
pub struct Outbox<'a> {
    buf: &'a mut [u8],
    head: usize,
    tail: usize,
    serial: u32,
    state: State,
    unique: Name,
}

impl<'a> Outbox<'a> {
    pub fn state(&self) -> State {
        self.state
    }

    /// The name the bus gave this connection, once Hello is answered.
    pub fn unique_name(&self) -> Option<&str> {
        (!self.unique.is_empty()).then(|| self.unique.as_str())
    }

    /// Bytes queued to send.
    pub fn pending(&self) -> usize {
        self.tail - self.head
    }

    /// Copy queued bytes into `out`, returning how many. They count as sent.
    pub fn transmit(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.pending());
        out[..n].copy_from_slice(&self.buf[self.head..self.head + n]);
        self.head += n;
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
        n
    }

    fn next_serial(&mut self) -> u32 {
        let s = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        s
    }

    /// Move queued bytes to the front so the tail has the room.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        self.buf.copy_within(self.head..self.tail, 0);
        self.tail -= self.head;
        self.head = 0;
    }

    fn queue_raw(&mut self, b: &[u8]) -> Result<(), DbusError> {
        self.compact();
        let dst = self
            .buf
            .get_mut(self.tail..self.tail + b.len())
            .ok_or(DbusError::TxFull)?;
        dst.copy_from_slice(b);
        self.tail += b.len();
        Ok(())
    }

    fn queue(
        &mut self,
        f: &Fields<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<(), DbusError> {
        self.compact();
        let n = msg::write(&mut self.buf[self.tail..], f, body)?;
        self.tail += n;
        Ok(())
    }

    /// Queue a message, taking a serial only if it fits.
    fn send(
        &mut self,
        mut f: Fields<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<u32, DbusError> {
        match self.state {
            State::Auth => return Err(DbusError::NotReady),
            State::Closed => return Err(DbusError::Closed),
            State::Hello | State::Ready => {}
        }
        f.serial = self.serial;
        self.queue(&f, body)?;
        Ok(self.next_serial())
    }

    /// Queue a method call. Returns its serial, which the reply names.
    pub fn call(
        &mut self,
        c: &Call<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<u32, DbusError> {
        let f = Fields {
            flags: c.flags,
            destination: c.destination,
            path: Some(c.path),
            interface: c.interface,
            member: Some(c.member),
            sig: c.sig,
            ..Fields::new(Kind::Call, 1)
        };
        self.send(f, body)
    }

    pub fn signal(
        &mut self,
        s: &Signal<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<u32, DbusError> {
        let f = Fields {
            destination: s.destination,
            path: Some(s.path),
            interface: Some(s.interface),
            member: Some(s.member),
            sig: s.sig,
            ..Fields::new(Kind::Signal, 1)
        };
        self.send(f, body)
    }

    /// Answer a call. Nothing is queued when the caller asked for no reply.
    pub fn reply(
        &mut self,
        to: &ReplyTo,
        sig: &str,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<Option<u32>, DbusError> {
        if to.no_reply {
            return Ok(None);
        }
        let f = Fields {
            reply_serial: to.serial,
            destination: (!to.sender.is_empty()).then(|| to.sender.as_str()),
            sig,
            ..Fields::new(Kind::Return, 1)
        };
        self.send(f, body).map(Some)
    }

    /// Answer a call with an error `name` and a message.
    pub fn error(
        &mut self,
        to: &ReplyTo,
        name: &str,
        text: &str,
    ) -> Result<Option<u32>, DbusError> {
        if to.no_reply {
            return Ok(None);
        }
        let f = Fields {
            reply_serial: to.serial,
            destination: (!to.sender.is_empty()).then(|| to.sender.as_str()),
            error: Some(name),
            sig: "s",
            ..Fields::new(Kind::Error, 1)
        };
        self.send(f, |w| w.str(text)).map(Some)
    }
}

pub struct Conn<'a> {
    rx: &'a mut [u8],
    rx_head: usize,
    rx_tail: usize,
    /// Bytes of the message last handed out, dropped on the next call.
    taken: usize,
    out: Outbox<'a>,
    hello: u32,
    guid: [u8; 32],
}

impl<'a> Conn<'a> {
    /// Start a client connection as `uid`, the AUTH line already queued.
    pub fn new(store: Storage<'a>, uid: u32) -> Result<Self, DbusError> {
        let n = auth::start(uid, store.tx)?;
        Ok(Self {
            rx: store.rx,
            rx_head: 0,
            rx_tail: 0,
            taken: 0,
            out: Outbox {
                buf: store.tx,
                head: 0,
                tail: n,
                serial: 1,
                state: State::Auth,
                unique: Name::EMPTY,
            },
            hello: 0,
            guid: [0; 32],
        })
    }

    /// Start over on the same buffers, for a new socket after the last one
    /// closed. What either buffer held is dropped and the AUTH line is queued.
    pub fn restart(&mut self, uid: u32) -> Result<(), DbusError> {
        let n = auth::start(uid, self.out.buf)?;
        (self.rx_head, self.rx_tail, self.taken) = (0, 0, 0);
        self.out.head = 0;
        self.out.tail = n;
        self.out.serial = 1;
        self.out.state = State::Auth;
        self.out.unique = Name::EMPTY;
        self.hello = 0;
        self.guid = [0; 32];
        Ok(())
    }

    pub fn state(&self) -> State {
        self.out.state
    }

    pub fn is_ready(&self) -> bool {
        self.out.state == State::Ready
    }

    /// The name the bus gave this connection, once Hello is answered.
    pub fn unique_name(&self) -> Option<&str> {
        self.out.unique_name()
    }

    /// The bus's guid in hex, once authenticated.
    pub fn guid(&self) -> &[u8; 32] {
        &self.guid
    }

    /// The sending half, for [`crate::bus`] and [`crate::props`].
    pub fn out(&mut self) -> &mut Outbox<'a> {
        &mut self.out
    }

    /// Bytes queued to send.
    pub fn pending(&self) -> usize {
        self.out.pending()
    }

    /// Copy queued bytes into `out`, returning how many. They count as sent.
    pub fn transmit(&mut self, out: &mut [u8]) -> usize {
        self.out.transmit(out)
    }

    /// Take received bytes, returning how many fit. Feed the rest after
    /// [`Conn::poll`] has drained what is complete.
    pub fn received(&mut self, data: &[u8]) -> usize {
        self.release();
        if self.rx_tail + data.len() > self.rx.len() && self.rx_head > 0 {
            self.rx.copy_within(self.rx_head..self.rx_tail, 0);
            self.rx_tail -= self.rx_head;
            self.rx_head = 0;
        }
        let n = data.len().min(self.rx.len() - self.rx_tail);
        self.rx[self.rx_tail..self.rx_tail + n].copy_from_slice(&data[..n]);
        self.rx_tail += n;
        n
    }

    /// The next complete message, driving auth and Hello on the way.
    /// Any error closes the connection.
    pub fn poll(&mut self) -> Result<Option<Message<'_>>, DbusError> {
        Ok(self.poll_split()?.map(|(m, _)| m))
    }

    /// [`Conn::poll`], with the sending half beside the message, so it can
    /// be answered while it is borrowed.
    pub fn poll_split(&mut self) -> Result<Option<(Message<'_>, &mut Outbox<'a>)>, DbusError> {
        self.release();
        let found = match self.advance() {
            Ok(f) => f,
            Err(e) => {
                self.out.state = State::Closed;
                return Err(e);
            }
        };
        let Some((len, head)) = found else {
            return Ok(None);
        };
        self.taken = len;
        let bytes = &self.rx[self.rx_head..self.rx_head + len];
        Ok(Some((Message { bytes, head }, &mut self.out)))
    }

    fn release(&mut self) {
        self.rx_head += self.taken;
        self.taken = 0;
        if self.rx_head == self.rx_tail {
            self.rx_head = 0;
            self.rx_tail = 0;
        }
    }

    /// Step the state machine until a message is ready or input runs out.
    fn advance(&mut self) -> Result<Option<(usize, msg::Head)>, DbusError> {
        let mut step = self.step()?;
        while let Step::Again = step {
            step = self.step()?;
        }
        Ok(match step {
            Step::Message(len, head) => Some((len, head)),
            _ => None,
        })
    }

    fn step(&mut self) -> Result<Step, DbusError> {
        match self.out.state {
            State::Closed => Err(DbusError::Closed),
            State::Auth => match self.auth_step()? {
                true => Ok(Step::Again),
                false => Ok(Step::Idle),
            },
            State::Hello | State::Ready => self.message_step(),
        }
    }

    fn message_step(&mut self) -> Result<Step, DbusError> {
        let Some((len, head)) = self.frame()? else {
            return Ok(Step::Idle);
        };
        if self.out.state == State::Hello && head.reply_serial == self.hello {
            self.hello_reply(len, &head)?;
            return Ok(Step::Again);
        }
        Ok(Step::Message(len, head))
    }

    /// The complete message at the front of the receive buffer, if any.
    fn frame(&self) -> Result<Option<(usize, msg::Head)>, DbusError> {
        let pending = &self.rx[self.rx_head..self.rx_tail];
        let Some(len) = msg::frame_len(pending)? else {
            return Ok(None);
        };
        if len > self.rx.len() {
            return Err(DbusError::RxTooSmall(len));
        }
        let Some(bytes) = pending.get(..len) else {
            return Ok(None);
        };
        Ok(Some((len, msg::parse_head(bytes)?)))
    }

    /// Consume one auth line if a whole one is here. True if one was.
    fn auth_step(&mut self) -> Result<bool, DbusError> {
        let pending = &self.rx[self.rx_head..self.rx_tail];
        let Some(n) = auth::line(pending)? else {
            return Ok(false);
        };
        let reply = auth::reply(&pending[..n]);
        self.rx_head += n;
        match reply {
            Reply::Ok(guid) => {
                self.guid = guid;
                self.out.queue_raw(auth::BEGIN)?;
                self.out.state = State::Hello;
                let serial = self.out.next_serial();
                let f = bus::fields(Kind::Call, serial, "Hello");
                self.out.queue(&f, |_| Ok(()))?;
                self.hello = serial;
                Ok(true)
            }
            Reply::Rejected => Err(AuthError::Rejected.into()),
            Reply::Error => Err(AuthError::Error.into()),
            Reply::Other => Err(AuthError::Unexpected.into()),
        }
    }

    fn hello_reply(&mut self, len: usize, head: &msg::Head) -> Result<(), DbusError> {
        if head.kind != Kind::Return {
            return Err(DbusError::Hello);
        }
        let bytes = &self.rx[self.rx_head..self.rx_head + len];
        let m = Message { bytes, head: *head };
        let name = m.body().str()?;
        self.out.unique = Name::new(name).ok_or(DbusError::Hello)?;
        self.rx_head += len;
        self.out.state = State::Ready;
        Ok(())
    }

    /// Queue a method call. Returns its serial, which the reply names.
    pub fn call(
        &mut self,
        c: &Call<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<u32, DbusError> {
        self.out.call(c, body)
    }

    pub fn signal(
        &mut self,
        s: &Signal<'_>,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<u32, DbusError> {
        self.out.signal(s, body)
    }

    /// Answer a call. Nothing is queued when the caller asked for no reply.
    pub fn reply(
        &mut self,
        to: &ReplyTo,
        sig: &str,
        body: impl FnOnce(&mut Writer<'_>) -> Result<(), WireError>,
    ) -> Result<Option<u32>, DbusError> {
        self.out.reply(to, sig, body)
    }

    /// Answer a call with an error `name` and a message.
    pub fn error(
        &mut self,
        to: &ReplyTo,
        name: &str,
        text: &str,
    ) -> Result<Option<u32>, DbusError> {
        self.out.error(to, name, text)
    }
}

#[cfg(test)]
mod tests;
