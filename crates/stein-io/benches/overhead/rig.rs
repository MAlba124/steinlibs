//! Rings, queues and drivers built as stein-io builds them, and the wait loops every variant
//! shares. Each loop takes what is ready first and waits for one more only when none is.

use std::os::fd::RawFd;

use io_uring::IoUring;
use stein_io::Buf;
use stein_io::driver::{Backend, Io, RingFlags, Storage};
use stein_io::op::Completion;
use stein_io::queue::{Cqe, Owned, Queue, Setup, Want};

use crate::{BenchError, Ctx};

/// Queue slots, so ring entries as `queue/uring.rs` sizes them for a queue this big.
pub const SLOTS: usize = 64;
/// `epoll` watch table of a bench queue.
const FDS: usize = 4096;

/// A ring with the flags `queue/uring.rs` `build` sets, deferred unless `coop`.
pub fn raw_ring(coop: bool) -> Result<IoUring, BenchError> {
    let entries = (SLOTS as u32).clamp(8, 4096).next_power_of_two();
    let cq = entries * 4;
    let mut b = IoUring::builder();
    if coop {
        b.setup_coop_taskrun()
            .setup_taskrun_flag()
            .setup_cqsize(cq)
            .setup_submit_all();
    } else {
        b.setup_single_issuer()
            .setup_defer_taskrun()
            .setup_taskrun_flag()
            .setup_cqsize(cq)
            .setup_submit_all();
    }
    Ok(b.build(entries)?)
}

pub fn queue(want: Want, coop: bool) -> Result<Queue, BenchError> {
    let setup = Setup {
        want,
        single_issuer: !coop,
        fixed_buffers: 0,
    };
    Queue::new(Owned::new(SLOTS, FDS), setup).map_err(|r| BenchError::Stein(r.error))
}

/// 64 ops over 128 buffers of `size`. Half the buffers go to the provided ring.
pub fn driver(back: Backend, coop: bool, size: u32) -> Result<Io, BenchError> {
    let store = Storage::new(64, 128);
    let mut io = if back == Backend::Uring && !coop {
        let io = Io::single_issuer(store, size)?;
        if io.ring_flags() != RingFlags::Deferred {
            return Err(BenchError::Setup("the driver ring is not deferred"));
        }
        io
    } else {
        Io::with_backend(store, size, back)?
    };
    touch(&mut io);
    Ok(io)
}

/// Write every free buffer once, as `Aligned` does, so no variant reads the zero page.
fn touch(io: &mut Io) {
    let mut held = Vec::with_capacity(128);
    while let Ok(b) = io.alloc() {
        io.buf_mut(&b).fill(0x5a);
        held.push(b);
    }
    for id in held {
        io.release(Buf { id, len: 0 });
    }
}

pub fn report_flags(ctx: &Ctx) -> Result<(), BenchError> {
    let q = queue(Want::Uring, ctx.coop)?;
    let io = driver(Backend::Uring, ctx.coop, 4096)?;
    println!(
        "         queue {:?}, driver {:?}",
        q.ring_flags(),
        io.ring_flags()
    );
    Ok(())
}

/// Drain the completion queue, then wait for one more, until `want` arrived. `waitn` waits
/// for all that are left at once, which the queue cannot ask for.
pub fn raw_wait(
    ring: &mut IoUring,
    want: usize,
    waitn: bool,
    mut each: impl FnMut(u64, i32) -> Result<(), BenchError>,
) -> Result<(), BenchError> {
    let mut got = 0;
    loop {
        for e in ring.completion() {
            each(e.user_data(), e.result())?;
            got += 1;
        }
        if got >= want {
            return Ok(());
        }
        let n = if waitn { want - got } else { 1 };
        match ring.submit_and_wait(n) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// [`raw_wait`] on the stein queue.
pub fn queue_wait(
    q: &mut Queue,
    out: &mut [Cqe],
    want: usize,
    mut each: impl FnMut(&Cqe) -> Result<(), BenchError>,
) -> Result<(), BenchError> {
    let mut got = 0;
    loop {
        let n = q.reap(out);
        for c in &out[..n] {
            each(c)?;
        }
        got += n;
        if got >= want {
            return Ok(());
        }
        if n == 0 {
            q.tick(None)?;
        }
    }
}

/// [`raw_wait`] on the driver. Buffers a completion carries are released after `each`.
pub fn io_wait(
    io: &mut Io,
    want: usize,
    mut each: impl FnMut(&mut Io, &Completion) -> Result<(), BenchError>,
) -> Result<(), BenchError> {
    let mut got = 0;
    loop {
        while let Some(c) = io.next_completion() {
            if let Err(e) = c.result {
                return Err(BenchError::Op {
                    what: "driver op",
                    res: -(e.raw_os_error() as i64),
                });
            }
            each(io, &c)?;
            if let Some(b) = c.buf {
                io.release(b);
            }
            got += 1;
        }
        if got >= want {
            return Ok(());
        }
        io.tick(None)?;
    }
}

/// Pin the calling thread to `cpu`.
pub fn pin(cpu: usize) -> Result<(), BenchError> {
    if cpu >= 8 * size_of::<libc::cpu_set_t>() {
        return Err(BenchError::Setup("cpu out of range"));
    }
    // SAFETY: a plain bit set, all zeros is empty.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: `cpu` fits the set, checked above.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: the set outlives the call, zero means this thread.
    let r = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
    if r != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Bytes with a 4096 aligned run of `len` inside, for `O_DIRECT`.
pub struct Aligned {
    bytes: Vec<u8>,
    at: usize,
    len: usize,
}

impl Aligned {
    pub fn new(len: usize) -> Aligned {
        // Written, so every page is its own and not the shared zero page.
        let bytes = vec![0x5au8; len + 4096];
        let at = bytes.as_ptr().align_offset(4096);
        Aligned { bytes, at, len }
    }

    pub fn slice(&mut self) -> &mut [u8] {
        &mut self.bytes[self.at..self.at + self.len]
    }
}

/// Blocking `send` of all of `buf`.
pub fn send_all(fd: RawFd, buf: &[u8]) -> Result<(), BenchError> {
    let mut at = 0;
    while at < buf.len() {
        let rest = &buf[at..];
        // SAFETY: `rest` is valid for its length during the call.
        let r = unsafe { libc::send(fd, rest.as_ptr().cast(), rest.len(), libc::MSG_NOSIGNAL) };
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        at += crate::count("send", r as i64)?;
    }
    Ok(())
}

/// Blocking `recv` until `len` bytes arrived into `buf`, reused from the start each call.
pub fn recv_exact(fd: RawFd, buf: &mut [u8], len: usize) -> Result<(), BenchError> {
    let mut got = 0;
    while got < len {
        let room = buf.len().min(len - got);
        // SAFETY: `buf` is valid for `room` bytes during the call.
        let r = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), room, 0) };
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        match crate::count("recv", r as i64)? {
            0 => return Err(BenchError::Setup("peer closed")),
            n => got += n,
        }
    }
    Ok(())
}

pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// An epoll fd watching `fd` edge triggered.
pub struct Epoll {
    ep: std::os::fd::OwnedFd,
    events: [libc::epoll_event; 8],
}

impl Epoll {
    pub fn new(fd: RawFd, flags: u32) -> Result<Epoll, BenchError> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        // SAFETY: no pointers.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fresh from the kernel, owned by nothing else.
        let ep = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut ev = libc::epoll_event {
            events: flags | libc::EPOLLET as u32,
            u64: fd as u64,
        };
        // SAFETY: `ev` outlives the call.
        let r = unsafe { libc::epoll_ctl(ep.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if r != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let events = [libc::epoll_event { events: 0, u64: 0 }; 8];
        Ok(Epoll { ep, events })
    }

    /// Block until an event.
    pub fn wait(&mut self) -> Result<(), BenchError> {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: the event array outlives the call and holds 8.
            let r =
                unsafe { libc::epoll_wait(self.ep.as_raw_fd(), self.events.as_mut_ptr(), 8, -1) };
            if r >= 0 {
                return Ok(());
            }
            if errno() != libc::EINTR {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
}
