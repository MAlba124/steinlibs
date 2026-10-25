//! Sockets against a blocking peer thread on another cpu. `ping` is a 64 byte round trip
//! at queue depth one, the send and the receive pushed together. `stream-tx` sends 64 KiB
//! at a time to a sink that acks the total, `stream-rx` receives 64 KiB at a time from a
//! source started by a header. One send or receive in flight per stream, as stein-io asks.
//!
//! Raw epoll is the least a readiness loop does. Ping sends, waits, then receives, and
//! streams try first and wait on `EAGAIN`, each on the one direction it needs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use io_uring::{IoUring, opcode, types};
use stein_io::driver::{Backend, Io};
use stein_io::op::{Kind, OpId};
use stein_io::queue::{Cqe, Fd, Op, Queue, Region, Want};

use crate::rig::{
    self, Epoll, errno, io_wait, queue_wait, raw_ring, raw_wait, recv_exact, send_all,
};
use crate::{BenchError, Ctx, count, exact};

const MSG: usize = 64;
/// Receive room of a ping, and the driver buffer size for it.
const ROOM: usize = 4096;
const CHUNK: usize = 65536;
const NOSIGNAL: i32 = libc::MSG_NOSIGNAL;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Unix,
    Tcp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Ping, the peer echoes.
    Echo,
    /// Stream tx, the peer sinks.
    Sink,
    /// Stream rx, the peer sources.
    Source,
}

pub fn run(ctx: &mut Ctx) -> Result<(), BenchError> {
    for mode in [Mode::Echo, Mode::Sink, Mode::Source] {
        for wire in [Wire::Unix, Wire::Tcp] {
            workload(ctx, mode, wire)?;
        }
    }
    Ok(())
}

fn workload(ctx: &mut Ctx, mode: Mode, wire: Wire) -> Result<(), BenchError> {
    let kind = match mode {
        Mode::Echo => "ping",
        Mode::Sink => "stream-tx",
        Mode::Source => "stream-rx",
    };
    let wire_name = if wire == Wire::Unix { "unix" } else { "tcp" };
    let w = format!("{kind}-{wire_name}");
    let s = Set {
        w: &w,
        mode,
        wire,
        ops: ctx.ops(if mode == Mode::Echo { 10_000 } else { 2048 }),
        bytes: if mode == Mode::Echo { 0 } else { CHUNK as u64 },
    };
    blocking(ctx, &s)?;
    raw_uring(ctx, &s, false)?;
    if mode == Mode::Echo {
        raw_uring(ctx, &s, true)?;
    }
    raw_epoll(ctx, &s)?;
    stein_queue(ctx, &s, Want::Uring)?;
    stein_queue(ctx, &s, Want::Epoll)?;
    for back in [Backend::Uring, Backend::Epoll] {
        stein_io(ctx, &s, back, false)?;
        if mode == Mode::Source {
            stein_io(ctx, &s, back, true)?;
        }
    }
    ctx.flush()
}

struct Set<'a> {
    w: &'a str,
    mode: Mode,
    wire: Wire,
    ops: usize,
    bytes: u64,
}

/// A connected pair. Ours is returned, the other end is served on a pinned thread until EOF.
fn pair(
    wire: Wire,
    mode: Mode,
    cpu: Option<usize>,
) -> Result<(OwnedFd, JoinHandle<()>), BenchError> {
    let (mine, peer) = match wire {
        Wire::Unix => {
            let (a, b) = UnixStream::pair()?;
            let t = std::thread::spawn(move || serve(b, mode, cpu));
            (OwnedFd::from(a), t)
        }
        Wire::Tcp => {
            let l = TcpListener::bind("127.0.0.1:0")?;
            let a = TcpStream::connect(l.local_addr()?)?;
            let (b, _) = l.accept()?;
            a.set_nodelay(true)?;
            b.set_nodelay(true)?;
            let t = std::thread::spawn(move || serve(b, mode, cpu));
            (OwnedFd::from(a), t)
        }
    };
    Ok((mine, peer))
}

fn serve(mut s: impl Read + Write, mode: Mode, cpu: Option<usize>) {
    if let Some(c) = cpu {
        let _ = rig::pin(c);
    }
    let mut big = vec![7u8; 4 * CHUNK];
    let mut head = [0u8; 8];
    loop {
        let ok = match mode {
            Mode::Echo => s.read_exact(&mut big[..MSG]).is_ok() && s.write_all(&big[..MSG]).is_ok(),
            Mode::Sink => s.read_exact(&mut head).is_ok() && sink(&mut s, &mut big, &head),
            Mode::Source => s.read_exact(&mut head).is_ok() && source(&mut s, &big, &head),
        };
        if !ok {
            return;
        }
    }
}

/// Take the bytes `head` announces, then ack.
fn sink(s: &mut (impl Read + Write), big: &mut [u8], head: &[u8; 8]) -> bool {
    let mut left = u64::from_ne_bytes(*head) as usize;
    while left > 0 {
        let room = left.min(big.len());
        match s.read(&mut big[..room]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => left -= n,
        }
    }
    s.write_all(&[1]).is_ok()
}

/// Send the bytes `head` asks for.
fn source(s: &mut impl Write, big: &[u8], head: &[u8; 8]) -> bool {
    let chunks = u64::from_ne_bytes(*head) as usize / CHUNK;
    (0..chunks).all(|_| s.write_all(&big[..CHUNK]).is_ok())
}

/// Count a received length, failing on EOF or error.
fn more(got: &mut usize, r: i64) -> Result<(), BenchError> {
    match r {
        r if r > 0 => {
            *got += r as usize;
            Ok(())
        }
        r => Err(BenchError::Op {
            what: "recv",
            res: r,
        }),
    }
}

/// Announce `n` chunks to the peer.
fn announce(fd: RawFd, n: usize) -> Result<(), BenchError> {
    send_all(fd, &((n * CHUNK) as u64).to_ne_bytes())
}

/// The sink's ack.
fn acked(fd: RawFd) -> Result<(), BenchError> {
    let mut ack = [0u8; 1];
    recv_exact(fd, &mut ack, 1)
}

fn blocking(ctx: &mut Ctx, s: &Set) -> Result<(), BenchError> {
    if !ctx.wants(s.w, "blocking") {
        return Ok(());
    }
    let (mine, peer) = pair(s.wire, s.mode, ctx.peer_cpu)?;
    let mut buf = vec![7u8; CHUNK];
    let mode = s.mode;
    ctx.add(s.w, "blocking", s.ops, s.bytes, move |n| {
        let _peer = &peer;
        let fd = mine.as_raw_fd();
        match mode {
            Mode::Echo => {
                for _ in 0..n {
                    send_all(fd, &buf[..MSG])?;
                    recv_exact(fd, &mut buf, MSG)?;
                }
            }
            Mode::Sink => {
                announce(fd, n)?;
                for _ in 0..n {
                    send_all(fd, &buf)?;
                }
                acked(fd)?;
            }
            Mode::Source => {
                announce(fd, n)?;
                recv_exact(fd, &mut buf, n * CHUNK)?;
            }
        }
        Ok(n)
    });
    Ok(())
}

/// Raw ring state. `base` is a send chunk then a receive chunk, outliving the ring.
struct RawRing {
    ring: IoUring,
    fd: types::Fd,
    raw: RawFd,
    base: *mut u8,
    waitn: bool,
    _buf: Vec<u8>,
    _mine: OwnedFd,
    _peer: JoinHandle<()>,
}

fn push(ring: &mut IoUring, e: &io_uring::squeue::Entry) -> Result<(), BenchError> {
    // SAFETY: every entry names `RawRing::base` memory, which outlives the ring, and one
    // op at a time names each half.
    unsafe { ring.submission().push(e) }.map_err(|_| BenchError::Setup("sq full"))
}

fn ring_ping(st: &mut RawRing, n: usize) -> Result<(), BenchError> {
    let room = st.base.wrapping_add(CHUNK);
    let recv = opcode::Recv::new(st.fd, room, ROOM as u32)
        .build()
        .user_data(1);
    for _ in 0..n {
        let send = opcode::Send::new(st.fd, st.base, MSG as u32)
            .flags(NOSIGNAL)
            .build()
            .user_data(0);
        let mut sq = st.ring.submission();
        // SAFETY: as in `push`.
        unsafe { sq.push(&send) }.map_err(|_| BenchError::Setup("sq full"))?;
        // SAFETY: as in `push`.
        unsafe { sq.push(&recv) }.map_err(|_| BenchError::Setup("sq full"))?;
        drop(sq);
        let mut got = 0;
        raw_wait(&mut st.ring, 2, st.waitn, |ud, r| match ud {
            0 => exact("send", r as i64, MSG),
            _ => more(&mut got, r as i64),
        })?;
        while got < MSG {
            push(&mut st.ring, &recv)?;
            raw_wait(&mut st.ring, 1, false, |_, r| more(&mut got, r as i64))?;
        }
    }
    Ok(())
}

fn ring_tx(st: &mut RawRing, n: usize) -> Result<(), BenchError> {
    announce(st.raw, n)?;
    for _ in 0..n {
        let mut sent = 0;
        while sent < CHUNK {
            let p = st.base.wrapping_add(sent);
            let e = opcode::Send::new(st.fd, p, (CHUNK - sent) as u32)
                .flags(NOSIGNAL)
                .build();
            push(&mut st.ring, &e)?;
            raw_wait(&mut st.ring, 1, false, |_, r| {
                sent += count("send", r as i64)?;
                Ok(())
            })?;
        }
    }
    acked(st.raw)
}

fn ring_rx(st: &mut RawRing, n: usize) -> Result<(), BenchError> {
    announce(st.raw, n)?;
    let room = st.base.wrapping_add(CHUNK);
    let e = opcode::Recv::new(st.fd, room, CHUNK as u32).build();
    let mut got = 0;
    while got < n * CHUNK {
        push(&mut st.ring, &e)?;
        raw_wait(&mut st.ring, 1, false, |_, r| more(&mut got, r as i64))?;
    }
    Ok(())
}

fn raw_uring(ctx: &mut Ctx, s: &Set, waitn: bool) -> Result<(), BenchError> {
    let name = if waitn {
        "raw-uring-waitn"
    } else {
        "raw-uring"
    };
    if !ctx.wants(s.w, name) {
        return Ok(());
    }
    let (mine, peer) = pair(s.wire, s.mode, ctx.peer_cpu)?;
    let mut buf = vec![7u8; 2 * CHUNK];
    let raw = mine.as_raw_fd();
    // The ring is the first field, so it drops before the bytes it names.
    let mut st = RawRing {
        ring: raw_ring(ctx.coop)?,
        fd: types::Fd(raw),
        raw,
        base: buf.as_mut_ptr(),
        waitn,
        _buf: buf,
        _mine: mine,
        _peer: peer,
    };
    let mode = s.mode;
    ctx.add(s.w, name, s.ops, s.bytes, move |n| {
        match mode {
            Mode::Echo => ring_ping(&mut st, n)?,
            Mode::Sink => ring_tx(&mut st, n)?,
            Mode::Source => ring_rx(&mut st, n)?,
        }
        Ok(n)
    });
    Ok(())
}

fn recv_now(fd: RawFd, buf: &mut [u8]) -> isize {
    // SAFETY: `buf` is valid for its length during the call.
    unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) }
}

fn send_now(fd: RawFd, buf: &[u8]) -> isize {
    let flags = NOSIGNAL | libc::MSG_DONTWAIT;
    // SAFETY: `buf` is valid for its length during the call.
    unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) }
}

/// Bytes moved, `None` on `EAGAIN`, else the error.
fn moved(what: &'static str, r: isize) -> Result<Option<usize>, BenchError> {
    match r {
        r if r > 0 => Ok(Some(r as usize)),
        r if r < 0 && errno() == libc::EAGAIN => Ok(None),
        r => Err(BenchError::Op {
            what,
            res: r as i64,
        }),
    }
}

struct RawPoll {
    ep: Epoll,
    fd: RawFd,
    buf: Vec<u8>,
    _mine: OwnedFd,
    _peer: JoinHandle<()>,
}

fn poll_ping(st: &mut RawPoll, n: usize) -> Result<(), BenchError> {
    for _ in 0..n {
        exact("send", send_now(st.fd, &st.buf[..MSG]) as i64, MSG)?;
        let mut got = 0;
        while got < MSG {
            st.ep.wait()?;
            while got < MSG {
                match moved("recv", recv_now(st.fd, &mut st.buf[CHUNK..CHUNK + ROOM]))? {
                    Some(k) => got += k,
                    None => break,
                }
            }
        }
    }
    Ok(())
}

fn poll_tx(st: &mut RawPoll, n: usize) -> Result<(), BenchError> {
    announce(st.fd, n)?;
    for _ in 0..n {
        let mut sent = 0;
        while sent < CHUNK {
            match moved("send", send_now(st.fd, &st.buf[sent..CHUNK]))? {
                Some(k) => sent += k,
                None => st.ep.wait()?,
            }
        }
    }
    acked(st.fd)
}

fn poll_rx(st: &mut RawPoll, n: usize) -> Result<(), BenchError> {
    announce(st.fd, n)?;
    let mut got = 0;
    while got < n * CHUNK {
        match moved("recv", recv_now(st.fd, &mut st.buf[CHUNK..]))? {
            Some(k) => got += k,
            None => st.ep.wait()?,
        }
    }
    Ok(())
}

fn raw_epoll(ctx: &mut Ctx, s: &Set) -> Result<(), BenchError> {
    if !ctx.wants(s.w, "raw-epoll") {
        return Ok(());
    }
    let (mine, peer) = pair(s.wire, s.mode, ctx.peer_cpu)?;
    let fd = mine.as_raw_fd();
    let flags = match s.mode {
        Mode::Sink => libc::EPOLLOUT,
        _ => libc::EPOLLIN | libc::EPOLLRDHUP,
    };
    let mut st = RawPoll {
        ep: Epoll::new(fd, flags as u32)?,
        fd,
        buf: vec![7u8; 2 * CHUNK],
        _mine: mine,
        _peer: peer,
    };
    let mode = s.mode;
    ctx.add(s.w, "raw-epoll", s.ops, s.bytes, move |n| {
        match mode {
            Mode::Echo => poll_ping(&mut st, n)?,
            Mode::Sink => poll_tx(&mut st, n)?,
            Mode::Source => poll_rx(&mut st, n)?,
        }
        Ok(n)
    });
    Ok(())
}

/// Queue state. The region is a send chunk then a receive chunk.
struct Qs {
    q: Queue,
    out: [Cqe; 64],
    region: Region,
    fd: Fd,
    raw: RawFd,
    _buf: Vec<u8>,
    _mine: OwnedFd,
    _peer: JoinHandle<()>,
}

fn queue_ping(st: &mut Qs, n: usize) -> Result<(), BenchError> {
    for _ in 0..n {
        let send = st.q.mem(st.region, 0, MSG)?;
        let room = st.q.mem(st.region, CHUNK, ROOM)?;
        st.q.push(
            Op::Send {
                fd: st.fd,
                mem: send,
            },
            0,
        )?;
        st.q.push(
            Op::Recv {
                fd: st.fd,
                mem: room,
            },
            1,
        )?;
        let mut got = 0;
        queue_wait(&mut st.q, &mut st.out, 2, |c| match c.user {
            0 => exact("send", c.res as i64, MSG),
            _ => more(&mut got, c.res as i64),
        })?;
        while got < MSG {
            st.q.push(
                Op::Recv {
                    fd: st.fd,
                    mem: room,
                },
                1,
            )?;
            queue_wait(&mut st.q, &mut st.out, 1, |c| more(&mut got, c.res as i64))?;
        }
    }
    Ok(())
}

fn queue_tx(st: &mut Qs, n: usize) -> Result<(), BenchError> {
    announce(st.raw, n)?;
    for _ in 0..n {
        let mut sent = 0;
        while sent < CHUNK {
            let mem = st.q.mem(st.region, sent, CHUNK - sent)?;
            st.q.push(Op::Send { fd: st.fd, mem }, 0)?;
            queue_wait(&mut st.q, &mut st.out, 1, |c| {
                sent += count("send", c.res as i64)?;
                Ok(())
            })?;
        }
    }
    acked(st.raw)
}

fn queue_rx(st: &mut Qs, n: usize) -> Result<(), BenchError> {
    announce(st.raw, n)?;
    let mut got = 0;
    while got < n * CHUNK {
        let mem = st.q.mem(st.region, CHUNK, CHUNK)?;
        st.q.push(Op::Recv { fd: st.fd, mem }, 1)?;
        queue_wait(&mut st.q, &mut st.out, 1, |c| more(&mut got, c.res as i64))?;
    }
    Ok(())
}

fn stein_queue(ctx: &mut Ctx, s: &Set, want: Want) -> Result<(), BenchError> {
    let name = match want {
        Want::Epoll => "queue-epoll",
        _ => "queue-uring",
    };
    if !ctx.wants(s.w, name) {
        return Ok(());
    }
    let (mine, peer) = pair(s.wire, s.mode, ctx.peer_cpu)?;
    let raw = mine.as_raw_fd();
    let mut buf = vec![7u8; 2 * CHUNK];
    let mut q = rig::queue(want, ctx.coop)?;
    // SAFETY: `buf` moves into the state below after the queue, so it outlives it, and
    // nothing else touches it while the queue lives. Moving the `Vec` keeps its heap bytes.
    let region = unsafe { q.register(buf.as_mut_ptr(), buf.len(), false) }?;
    let mut st = Qs {
        q,
        out: [Cqe::EMPTY; 64],
        region,
        fd: Fd::Raw(raw),
        raw,
        _buf: buf,
        _mine: mine,
        _peer: peer,
    };
    let mode = s.mode;
    ctx.add(s.w, name, s.ops, s.bytes, move |n| {
        match mode {
            Mode::Echo => queue_ping(&mut st, n)?,
            Mode::Sink => queue_tx(&mut st, n)?,
            Mode::Source => queue_rx(&mut st, n)?,
        }
        Ok(n)
    });
    Ok(())
}

struct Ds {
    io: Io,
    fd: RawFd,
    msg: [u8; MSG],
    _peer: JoinHandle<()>,
}

fn len_of(c: &stein_io::op::Completion) -> i64 {
    c.result.map_or(-1, i64::from)
}

fn io_ping(st: &mut Ds, n: usize) -> Result<(), BenchError> {
    for _ in 0..n {
        let b = st.io.alloc()?;
        st.io.buf_mut(&b)[..MSG].copy_from_slice(&st.msg);
        st.io.send(st.fd, b, MSG as u32)?;
        st.io.recv(st.fd, false)?;
        let mut got = 0;
        io_wait(&mut st.io, 2, |_, c| match c.kind {
            Kind::Recv => more(&mut got, len_of(c)),
            _ => exact("send", len_of(c), MSG),
        })?;
        while got < MSG {
            st.io.recv(st.fd, false)?;
            io_wait(&mut st.io, 1, |_, c| more(&mut got, len_of(c)))?;
        }
    }
    Ok(())
}

fn io_tx(st: &mut Ds, n: usize) -> Result<(), BenchError> {
    announce(st.fd, n)?;
    for _ in 0..n {
        let b = st.io.alloc()?;
        st.io.send(st.fd, b, CHUNK as u32)?;
        io_wait(&mut st.io, 1, |_, c| exact("send", len_of(c), CHUNK))?;
    }
    acked(st.fd)
}

/// One receive per chunk, or the multishot armed at setup when `multi`.
fn io_rx(st: &mut Ds, n: usize, multi: bool) -> Result<(), BenchError> {
    announce(st.fd, n)?;
    let mut got = 0;
    while got < n * CHUNK {
        if !multi {
            st.io.recv(st.fd, false)?;
        }
        io_wait(&mut st.io, 1, |_, c| more(&mut got, len_of(c)))?;
    }
    Ok(())
}

fn stein_io(ctx: &mut Ctx, s: &Set, back: Backend, multi: bool) -> Result<(), BenchError> {
    let name = match (back, multi) {
        (Backend::Uring, false) => "io-uring",
        (Backend::Uring, true) => "io-uring-multi",
        (Backend::Epoll, false) => "io-epoll",
        (Backend::Epoll, true) => "io-epoll-multi",
    };
    if !ctx.wants(s.w, name) {
        return Ok(());
    }
    let (mine, peer) = pair(s.wire, s.mode, ctx.peer_cpu)?;
    let size = if s.mode == Mode::Echo { ROOM } else { CHUNK };
    let mut io = rig::driver(back, ctx.coop, size as u32)?;
    let fd = io.own(mine);
    let armed: Option<OpId> = if multi {
        Some(io.recv(fd, true)?)
    } else {
        None
    };
    let mut st = Ds {
        io,
        fd,
        msg: [7u8; MSG],
        _peer: peer,
    };
    let mode = s.mode;
    ctx.add(s.w, name, s.ops, s.bytes, move |n| {
        match mode {
            Mode::Echo => io_ping(&mut st, n)?,
            Mode::Sink => io_tx(&mut st, n)?,
            Mode::Source => io_rx(&mut st, n, armed.is_some())?,
        }
        Ok(n)
    });
    Ok(())
}
