//! One framed message over TCP. The server thread echoes it back, the client on the main
//! thread checks the reply. Each thread runs its own driver.
//! Run with `cargo run -p stein-io --example echo`, pass `epoll` or `uring` to pick a backend.
//!
//! Frame, little endian, 9 byte header then the body.
//!
//! | magic u16 | kind u8 | seq u32 | len u16 | body |

use std::fmt;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use stein_io::{Backend, Completion, Io, IoError, OpId, Storage};

const MAGIC: u16 = 0x5345;
const HEADER: usize = 9;
const BUF_SIZE: u32 = 256;
const LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ping = 1,
    Echo = 2,
}

#[derive(Debug, PartialEq, Eq)]
struct Frame<'a> {
    kind: Kind,
    seq: u32,
    body: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
enum FrameError {
    Magic(u16),
    Kind(u8),
    TooLong(usize),
}

#[derive(Debug)]
enum Error {
    Io(IoError),
    Kernel(Errno),
    Frame(FrameError),
    /// Peer closed before a whole frame came.
    Closed,
    /// No completion within `LIMIT`.
    Timeout,
    /// The reply did not match the request.
    Mismatch,
    /// The server thread panicked or never sent its address.
    Server,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "driver {e}"),
            Error::Kernel(e) => write!(f, "kernel {e}"),
            Error::Frame(e) => write!(f, "frame {e:?}"),
            Error::Closed => f.write_str("peer closed mid frame"),
            Error::Timeout => f.write_str("timed out"),
            Error::Mismatch => f.write_str("reply does not match"),
            Error::Server => f.write_str("server thread failed"),
        }
    }
}

impl From<IoError> for Error {
    fn from(e: IoError) -> Error {
        Error::Io(e)
    }
}

impl From<Errno> for Error {
    fn from(e: Errno) -> Error {
        Error::Kernel(e)
    }
}

impl From<FrameError> for Error {
    fn from(e: FrameError) -> Error {
        Error::Frame(e)
    }
}

/// Bytes written to `out`.
fn encode(f: &Frame, out: &mut [u8]) -> Result<usize, FrameError> {
    let n = HEADER + f.body.len();
    if f.body.len() > u16::MAX as usize || n > out.len() {
        return Err(FrameError::TooLong(n));
    }
    out[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    out[2] = f.kind as u8;
    out[3..7].copy_from_slice(&f.seq.to_le_bytes());
    out[7..9].copy_from_slice(&(f.body.len() as u16).to_le_bytes());
    out[HEADER..n].copy_from_slice(f.body);
    Ok(n)
}

/// The frame and its length, `None` until `b` holds a whole one.
fn decode(b: &[u8]) -> Result<Option<(Frame<'_>, usize)>, FrameError> {
    if b.len() < HEADER {
        return Ok(None);
    }
    let magic = u16::from_le_bytes([b[0], b[1]]);
    if magic != MAGIC {
        return Err(FrameError::Magic(magic));
    }
    let kind = match b[2] {
        1 => Kind::Ping,
        2 => Kind::Echo,
        k => return Err(FrameError::Kind(k)),
    };
    let seq = u32::from_le_bytes([b[3], b[4], b[5], b[6]]);
    let n = HEADER + u16::from_le_bytes([b[7], b[8]]) as usize;
    if b.len() < n {
        return Ok(None);
    }
    let body = &b[HEADER..n];
    Ok(Some((Frame { kind, seq, body }, n)))
}

/// Tick until `id` completes. Only one op is in flight per thread here.
fn wait(io: &mut Io, id: OpId) -> Result<Completion, Error> {
    let start = Instant::now();
    loop {
        while let Some(mut c) = io.next_completion() {
            if c.op == id {
                return Ok(c);
            }
            if let Some(b) = c.buf.take() {
                io.release(b);
            }
        }
        if start.elapsed() > LIMIT {
            return Err(Error::Timeout);
        }
        io.tick(Some(Duration::from_millis(100)))?;
    }
}

/// Receive into `acc` until it holds a whole frame. Returns the frame length.
fn recv_frame(io: &mut Io, fd: RawFd, acc: &mut Vec<u8>) -> Result<usize, Error> {
    loop {
        if let Some((_, n)) = decode(acc)? {
            return Ok(n);
        }
        let id = io.recv(fd, false)?;
        let mut c = wait(io, id)?;
        // End of stream may carry an empty buffer too.
        if let Some(b) = c.buf.take() {
            acc.extend_from_slice(io.filled(&b));
            io.release(b);
        }
        if c.result? == 0 {
            return Err(Error::Closed);
        }
    }
}

fn send_frame(io: &mut Io, fd: RawFd, f: &Frame) -> Result<(), Error> {
    let b = io.alloc()?;
    let n = match encode(f, io.buf_mut(&b)) {
        Ok(n) => n,
        Err(e) => {
            io.release(b);
            return Err(e.into());
        }
    };
    let id = io.send(fd, b, n as u32)?;
    let sent = wait(io, id)?.result?;
    println!("{:?} sent {sent} bytes", f.kind);
    Ok(())
}

/// The named backend, else io_uring with an epoll fallback.
fn driver(back: Option<Backend>) -> Result<Io, IoError> {
    let store = Storage::new(8, 8);
    match back {
        Some(b) => Io::with_backend(store, BUF_SIZE, b),
        None => Io::new(store, BUF_SIZE),
    }
}

/// Accept one connection, echo one frame, close.
fn serve(back: Option<Backend>, ready: mpsc::Sender<SocketAddr>) -> Result<(), Error> {
    let mut io = driver(back)?;
    let listener = io.listen(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let addr = io.local_addr(listener).ok_or(Error::Server)?;
    ready.send(addr).map_err(|_| Error::Server)?;

    let id = io.accept(listener, false)?;
    let c = wait(&mut io, id)?;
    let fd = c.result? as RawFd;
    println!("server accepted {:?}", c.addr);

    let mut acc = Vec::with_capacity(BUF_SIZE as usize);
    let n = recv_frame(&mut io, fd, &mut acc)?;
    let Some((ping, _)) = decode(&acc[..n])? else {
        return Err(Error::Closed);
    };
    if ping.kind != Kind::Ping {
        return Err(Error::Mismatch);
    }
    let echo = Frame {
        kind: Kind::Echo,
        ..ping
    };
    send_frame(&mut io, fd, &echo)?;
    io.close(fd);
    io.close(listener);
    Ok(())
}

fn main() -> Result<(), Error> {
    let back = match std::env::args().nth(1).as_deref() {
        Some("epoll") => Some(Backend::Epoll),
        Some("uring") => Some(Backend::Uring),
        _ => None,
    };
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || serve(back, tx));
    let addr = rx.recv_timeout(LIMIT).map_err(|_| Error::Server)?;

    let mut io = driver(back)?;
    println!("client on {:?}, server at {addr}", io.backend());
    let id = io.connect(addr)?;
    let c = wait(&mut io, id)?;
    c.result?;
    let fd = c.fd;

    let ping = Frame {
        kind: Kind::Ping,
        seq: 7,
        body: b"hello over stein-io",
    };
    send_frame(&mut io, fd, &ping)?;

    let mut acc = Vec::with_capacity(BUF_SIZE as usize);
    let n = recv_frame(&mut io, fd, &mut acc)?;
    let Some((echo, _)) = decode(&acc[..n])? else {
        return Err(Error::Closed);
    };
    if echo.kind != Kind::Echo || echo.seq != ping.seq || echo.body != ping.body {
        return Err(Error::Mismatch);
    }
    println!(
        "client got seq {} {:?}",
        echo.seq,
        String::from_utf8_lossy(echo.body)
    );
    io.close(fd);
    server.join().map_err(|_| Error::Server)?
}

#[cfg(test)]
mod tests {
    use super::*;

    const PING: Frame = Frame {
        kind: Kind::Ping,
        seq: 0x0102_0304,
        body: b"abc",
    };

    #[test]
    fn a_frame_round_trips() {
        let mut out = [0u8; 32];
        assert_eq!(encode(&PING, &mut out), Ok(HEADER + 3));
        assert_eq!(decode(&out[..HEADER + 3]), Ok(Some((PING, HEADER + 3))));
    }

    #[test]
    fn a_partial_frame_waits_for_more() {
        let mut out = [0u8; 32];
        assert_eq!(encode(&PING, &mut out), Ok(HEADER + 3));
        for cut in 0..HEADER + 3 {
            assert_eq!(decode(&out[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn trailing_bytes_are_left_for_the_next_frame() {
        let mut out = [0u8; 32];
        assert_eq!(encode(&PING, &mut out), Ok(HEADER + 3));
        assert_eq!(decode(&out[..HEADER + 5]), Ok(Some((PING, HEADER + 3))));
    }

    #[test]
    fn bad_headers_are_refused() {
        let mut out = [0u8; 32];
        assert_eq!(encode(&PING, &mut out), Ok(HEADER + 3));
        let mut bad = out;
        bad[0] = 0;
        assert_eq!(decode(&bad), Err(FrameError::Magic(0x5300)));
        let mut bad = out;
        bad[2] = 9;
        assert_eq!(decode(&bad), Err(FrameError::Kind(9)));
    }

    #[test]
    fn a_frame_too_long_for_the_buffer_is_refused() {
        let mut out = [0u8; HEADER + 2];
        assert_eq!(
            encode(&PING, &mut out),
            Err(FrameError::TooLong(HEADER + 3))
        );
    }
}
