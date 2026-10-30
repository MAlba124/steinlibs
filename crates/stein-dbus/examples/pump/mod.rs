//! A bus connection on stein-io: one socket, one send in flight, a
//! repeating receive feeding the connection.

use std::fmt;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use stein_dbus::{AddrError, Conn, DbusError, Message, Socket, Storage, addr};
use stein_io::op::Kind;
use stein_io::{Io, IoError, UnixAddr};

const OPS: usize = 16;
const BUFS: usize = 16;
const BUF: usize = 4096;
/// Receive and send buffers, each the largest message the pump handles.
const RX: usize = 64 * 1024;
const TX: usize = 16 * 1024;

#[derive(Debug)]
pub enum BusError {
    NoAddress,
    Addr(AddrError),
    Io(IoError),
    Kernel(rustix::io::Errno),
    Dbus(DbusError),
    /// The bus hung up.
    Closed,
    TimedOut,
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAddress => write!(f, "DBUS_SESSION_BUS_ADDRESS is not set"),
            Self::Addr(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e:?}"),
            Self::Kernel(e) => write!(f, "{e}"),
            Self::Dbus(e) => write!(f, "{e}"),
            Self::Closed => write!(f, "bus closed the connection"),
            Self::TimedOut => write!(f, "timed out"),
        }
    }
}

impl From<IoError> for BusError {
    fn from(e: IoError) -> Self {
        Self::Io(e)
    }
}

impl From<DbusError> for BusError {
    fn from(e: DbusError) -> Self {
        Self::Dbus(e)
    }
}

/// Everything the connection borrows, allocated once.
pub struct Mem {
    rx: Vec<u8>,
    tx: Vec<u8>,
}

impl Mem {
    pub fn new() -> Self {
        Self {
            rx: vec![0; RX],
            tx: vec![0; TX],
        }
    }
}

pub struct Bus<'s> {
    pub io: Io,
    pub conn: Conn<'s>,
    fd: RawFd,
    sending: bool,
}

/// The session bus address from the environment.
pub fn session() -> Result<String, BusError> {
    std::env::var("DBUS_SESSION_BUS_ADDRESS").map_err(|_| BusError::NoAddress)
}

impl<'s> Bus<'s> {
    /// Connect and authenticate. Hello is answered by the first turns.
    pub fn connect(m: &'s mut Mem, address: &str) -> Result<Self, BusError> {
        let mut path = [0u8; 108];
        let (kind, n) = addr::unix(address, &mut path).map_err(BusError::Addr)?;
        let to = match kind {
            Socket::Path => UnixAddr::Path(&path[..n]),
            Socket::Abstract => UnixAddr::Abstract(&path[..n]),
        };
        let store = stein_io::Storage::new(OPS, BUFS);
        let mut io = Io::new(store, BUF as u32)?;
        io.connect_unix(to)?;
        let fd = wait_connect(&mut io)?;
        io.recv(fd, true)?;
        let uid = rustix::process::getuid().as_raw();
        let conn = Conn::new(
            Storage {
                rx: &mut m.rx,
                tx: &mut m.tx,
            },
            uid,
        )?;
        Ok(Self {
            io,
            conn,
            fd,
            sending: false,
        })
    }

    /// Send what is queued, wait up to `wait`, and hand every message that
    /// arrived to `on`.
    pub fn turn(
        &mut self,
        wait: Duration,
        on: &mut dyn FnMut(&Message<'_>),
    ) -> Result<(), BusError> {
        self.flush()?;
        self.io.tick(Some(wait))?;
        while let Some(c) = self.io.next_completion() {
            self.completed(c, on)?;
            self.flush()?;
        }
        Ok(())
    }

    fn completed(
        &mut self,
        c: stein_io::Completion,
        on: &mut dyn FnMut(&Message<'_>),
    ) -> Result<(), BusError> {
        match c.kind {
            Kind::Send => {
                self.sending = false;
                c.result.map(|_| ()).map_err(BusError::Kernel)
            }
            Kind::Recv => self.received(c, on),
            _ => Ok(()),
        }
    }

    /// Turn until `done` says so, or `limit` passes.
    pub fn until(
        &mut self,
        limit: Duration,
        on: &mut dyn FnMut(&Message<'_>),
        done: &dyn Fn(&Self) -> bool,
    ) -> Result<(), BusError> {
        let start = Instant::now();
        while !done(self) && start.elapsed() <= limit {
            self.turn(Duration::from_millis(50), on)?;
        }
        match done(self) {
            true => Ok(()),
            false => Err(BusError::TimedOut),
        }
    }

    fn received(
        &mut self,
        c: stein_io::Completion,
        on: &mut dyn FnMut(&Message<'_>),
    ) -> Result<(), BusError> {
        let eof = c.is_eof();
        let fed = match &c.buf {
            Some(buf) if c.result.is_ok() && !eof => feed(&mut self.conn, self.io.filled(buf), on),
            _ => Ok(()),
        };
        // EOF carries an empty buffer too.
        if let Some(buf) = c.buf {
            self.io.release(buf);
        }
        c.result.map_err(BusError::Kernel)?;
        if eof {
            return Err(BusError::Closed);
        }
        fed?;
        if !c.more {
            self.io.recv(self.fd, true)?;
        }
        Ok(())
    }

    /// One send at a time, so the stream stays in order.
    fn flush(&mut self) -> Result<(), BusError> {
        if self.sending || self.conn.pending() == 0 {
            return Ok(());
        }
        let id = self.io.alloc()?;
        let n = self.conn.transmit(self.io.buf_mut(&id));
        self.io.send(self.fd, id, n as u32)?;
        self.sending = true;
        Ok(())
    }
}

/// Hand `data` to the connection, draining messages whenever it is full.
fn feed(
    conn: &mut Conn<'_>,
    mut data: &[u8],
    on: &mut dyn FnMut(&Message<'_>),
) -> Result<(), DbusError> {
    loop {
        let n = conn.received(data);
        data = &data[n..];
        while let Some(m) = conn.poll()? {
            on(&m);
        }
        if data.is_empty() {
            return Ok(());
        }
    }
}

fn wait_connect(io: &mut Io) -> Result<RawFd, BusError> {
    let start = Instant::now();
    loop {
        if let Some(c) = io.next_completion() {
            return c.result.map(|_| c.fd).map_err(BusError::Kernel);
        }
        if start.elapsed() > Duration::from_secs(5) {
            return Err(BusError::TimedOut);
        }
        io.tick(Some(Duration::from_millis(50)))?;
    }
}
