//! Positional reads and writes in batches of the queue depth. Buffered ones hit a file in
//! the page cache, writes never sync. `direct` reads use `O_DIRECT` at random 4 KiB offsets.
//!
//! The epoll backend runs file ops as blocking calls at push, so its raw baseline is the
//! blocking call and its queue depth buys nothing. There is no raw epoll for files.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use io_uring::{IoUring, opcode, types};
use stein_io::driver::Backend;
use stein_io::queue::{Cqe, Fd, Op, Queue, Region, Want};

use crate::rig::{Aligned, driver, io_wait, queue, queue_wait, raw_ring, raw_wait};
use crate::{BenchError, Ctx, exact};

const CACHED: u64 = 64 << 20;
const DIRECT: u64 = 256 << 20;

#[derive(Clone, Copy)]
struct Shape {
    size: usize,
    qd: usize,
    write: bool,
    direct: bool,
}

impl Shape {
    fn workload(&self) -> String {
        let kind = match (self.direct, self.write) {
            (true, _) => "direct-read",
            (false, true) => "disk-write",
            (false, false) => "disk-read",
        };
        format!("{kind}-{}k/qd{}", self.size >> 10, self.qd)
    }

    fn file_len(&self) -> u64 {
        if self.direct { DIRECT } else { CACHED }
    }

    fn ops(&self, ctx: &Ctx) -> usize {
        let base = match (self.direct, self.size, self.qd) {
            (true, _, 1) => 3_000,
            (true, _, _) => 20_000,
            (false, 4096, 1) => 100_000,
            (false, 4096, _) => 200_000,
            (false, _, 1) => 20_000,
            (false, _, _) => 40_000,
        };
        ctx.ops(base)
    }
}

/// Offsets cycling through the file, or random blocks for `direct`.
struct Offsets {
    k: u64,
    size: u64,
    blocks: u64,
    random: bool,
}

impl Offsets {
    fn new(s: Shape) -> Offsets {
        Offsets {
            k: 0,
            size: s.size as u64,
            blocks: s.file_len() / s.size as u64,
            random: s.direct,
        }
    }

    fn next(&mut self) -> u64 {
        self.k += 1;
        let b = match self.random {
            true => mix(self.k) % self.blocks,
            false => self.k % self.blocks,
        };
        b * self.size
    }
}

fn mix(k: u64) -> u64 {
    let mut z = k.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^ (z >> 31)
}

struct Files {
    read: PathBuf,
    write: PathBuf,
    direct: PathBuf,
}

/// Fill `path` to `len` unless it already is that long, and read it once into the cache.
fn prepare(path: &Path, len: u64, warm: bool) -> Result<(), BenchError> {
    let have = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if have != len {
        let mut f = File::create(path)?;
        let chunk: Vec<u8> = (0..1 << 20).map(|i| (i * 7) as u8).collect();
        for _ in 0..len >> 20 {
            f.write_all(&chunk)?;
        }
        f.sync_all()?;
    }
    if warm {
        std::fs::read(path)?;
    }
    Ok(())
}

pub fn run(ctx: &mut Ctx) -> Result<(), BenchError> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let files = Files {
        read: dir.join("overhead-read"),
        write: dir.join("overhead-write"),
        direct: dir.join("overhead-direct"),
    };
    // Files only for filters that can match a disk workload.
    let other = |f: &str, own: &str| {
        ["nop", "ping", "stream", own]
            .iter()
            .any(|p| f.starts_with(p))
    };
    let f = ctx.filter.as_deref().unwrap_or("");
    let (cached, direct) = (!other(f, "direct"), !other(f, "disk"));
    if cached {
        prepare(&files.read, CACHED, !ctx.once)?;
        prepare(&files.write, CACHED, !ctx.once)?;
    }
    if direct {
        prepare(&files.direct, DIRECT, false)?;
    }
    for write in [false, true] {
        for size in [4096, 65536] {
            for qd in [1, 32] {
                let s = Shape {
                    size,
                    qd,
                    write,
                    direct: false,
                };
                let path = if write { &files.write } else { &files.read };
                shape(ctx, s, path)?;
            }
        }
    }
    for qd in [1, 32] {
        let s = Shape {
            size: 4096,
            qd,
            write: false,
            direct: true,
        };
        shape(ctx, s, &files.direct)?;
    }
    Ok(())
}

fn shape(ctx: &mut Ctx, s: Shape, path: &Path) -> Result<(), BenchError> {
    let w = s.workload();
    blocking(ctx, &w, s, path)?;
    raw(ctx, &w, s, path, false)?;
    if s.qd > 1 {
        raw(ctx, &w, s, path, true)?;
    }
    for want in [Want::Uring, Want::Epoll] {
        stein_queue(ctx, &w, s, path, want)?;
    }
    for back in [Backend::Uring, Backend::Epoll] {
        if !s.direct {
            stein_io(ctx, &w, s, path, back, false)?;
        }
        stein_io(ctx, &w, s, path, back, true)?;
    }
    ctx.flush()
}

fn open(path: &Path, s: Shape) -> Result<File, BenchError> {
    let flags = if s.direct { libc::O_DIRECT } else { 0 };
    Ok(OpenOptions::new()
        .read(true)
        .write(s.write)
        .custom_flags(flags)
        .open(path)?)
}

fn blocking(ctx: &mut Ctx, w: &str, s: Shape, path: &Path) -> Result<(), BenchError> {
    if !ctx.wants(w, "blocking") {
        return Ok(());
    }
    let ops = s.ops(ctx);
    let f = open(path, s)?;
    let mut buf = Aligned::new(s.size * s.qd);
    let mut off = Offsets::new(s);
    ctx.add(w, "blocking", ops, s.size as u64, move |n| {
        let (fd, base) = (f.as_raw_fd(), buf.slice().as_mut_ptr());
        for k in 0..n {
            let p = base.wrapping_add((k % s.qd) * s.size);
            let at = off.next() as i64;
            // SAFETY: `p` is `size` bytes inside `buf`, which outlives the call.
            let r = unsafe {
                match s.write {
                    true => libc::pwrite(fd, p.cast(), s.size, at),
                    false => libc::pread(fd, p.cast(), s.size, at),
                }
            };
            exact("pread or pwrite", r as i64, s.size)?;
        }
        Ok(n)
    });
    Ok(())
}

/// A raw ring and the memory and file its ops name, the ring first so it drops first.
struct RawRig {
    ring: IoUring,
    buf: Aligned,
    file: File,
}

fn raw(ctx: &mut Ctx, w: &str, s: Shape, path: &Path, waitn: bool) -> Result<(), BenchError> {
    let name = if waitn {
        "raw-uring-waitn"
    } else {
        "raw-uring"
    };
    if !ctx.wants(w, name) {
        return Ok(());
    }
    let ops = s.ops(ctx);
    let mut rig = RawRig {
        ring: raw_ring(ctx.coop)?,
        buf: Aligned::new(s.size * s.qd),
        file: open(path, s)?,
    };
    let mut off = Offsets::new(s);
    ctx.add(w, name, ops, s.size as u64, move |n| {
        let rig = &mut rig;
        let fd = types::Fd(rig.file.as_raw_fd());
        let base = rig.buf.slice().as_mut_ptr();
        let batches = n.div_ceil(s.qd);
        for _ in 0..batches {
            let mut sq = rig.ring.submission();
            for i in 0..s.qd {
                let p = base.wrapping_add(i * s.size);
                let (at, len) = (off.next(), s.size as u32);
                let e = match s.write {
                    true => opcode::Write::new(fd, p, len).offset(at).build(),
                    false => opcode::Read::new(fd, p, len).offset(at).build(),
                };
                // SAFETY: `p` is `size` bytes of `buf`, which outlives the ring, and no
                // other op of the batch names it.
                unsafe { sq.push(&e.user_data(i as u64)) }
                    .map_err(|_| BenchError::Setup("sq full"))?;
            }
            drop(sq);
            raw_wait(&mut rig.ring, s.qd, waitn, |_, r| {
                exact("ring read or write", r as i64, s.size)
            })?;
        }
        Ok(batches * s.qd)
    });
    Ok(())
}

/// A queue and the memory and file its ops name, the queue first so it drops first.
struct Lent {
    q: Queue,
    out: [Cqe; 64],
    region: Region,
    _buf: Aligned,
    file: File,
}

fn stein_queue(
    ctx: &mut Ctx,
    w: &str,
    s: Shape,
    path: &Path,
    want: Want,
) -> Result<(), BenchError> {
    let name = match want {
        Want::Epoll => "queue-epoll",
        _ => "queue-uring",
    };
    if !ctx.wants(w, name) {
        return Ok(());
    }
    let ops = s.ops(ctx);
    let mut buf = Aligned::new(s.size * s.qd);
    let mut q = queue(want, ctx.coop)?;
    let bytes = buf.slice();
    // SAFETY: `buf` moves into the rig below, after the queue, so it outlives it, and
    // nothing else touches it while the queue lives. Moving the `Vec` keeps its heap bytes.
    let region = unsafe { q.register(bytes.as_mut_ptr(), bytes.len(), false) }?;
    let mut rig = Lent {
        q,
        out: [Cqe::EMPTY; 64],
        region,
        _buf: buf,
        file: open(path, s)?,
    };
    let mut off = Offsets::new(s);
    ctx.add(w, name, ops, s.size as u64, move |n| {
        // Whole, as a closure captures only the fields it names and `_buf` must live.
        let rig = &mut rig;
        let fd = Fd::Raw(rig.file.as_raw_fd());
        let batches = n.div_ceil(s.qd);
        for _ in 0..batches {
            for i in 0..s.qd {
                let mem = rig.q.mem(rig.region, i * s.size, s.size)?;
                let at = off.next();
                let op = match s.write {
                    true => Op::Write { fd, mem, at },
                    false => Op::Read { fd, mem, at },
                };
                rig.q.push(op, i as u64)?;
            }
            queue_wait(&mut rig.q, &mut rig.out, s.qd, |c| {
                exact("queue read or write", c.res as i64, s.size)
            })?;
        }
        Ok(batches * s.qd)
    });
    Ok(())
}

fn stein_io(
    ctx: &mut Ctx,
    w: &str,
    s: Shape,
    path: &Path,
    back: Backend,
    region: bool,
) -> Result<(), BenchError> {
    let name = match (back, region) {
        (Backend::Uring, false) => "io-uring",
        (Backend::Uring, true) => "io-uring-region",
        (Backend::Epoll, false) => "io-epoll",
        (Backend::Epoll, true) => "io-epoll-region",
    };
    if !ctx.wants(w, name) {
        return Ok(());
    }
    let ops = s.ops(ctx);
    let mut io = driver(back, ctx.coop, s.size as u32)?;
    let fd: RawFd = io.own(OwnedFd::from(open(path, s)?));
    let r = io.add_region(vec![0x5au8; s.size * s.qd + 4096].into_boxed_slice())?;
    let pad = io.region(r).as_ptr().align_offset(4096);
    let mut off = Offsets::new(s);
    let len = s.size as u32;
    ctx.add(w, name, ops, s.size as u64, move |n| {
        let batches = n.div_ceil(s.qd);
        for _ in 0..batches {
            for i in 0..s.qd {
                let at = off.next();
                let at_r = pad + i * s.size;
                match (region, s.write) {
                    (true, true) => io.write_mem(fd, at, r, at_r, len)?,
                    (true, false) => io.read_mem(fd, at, r, at_r, len)?,
                    (false, true) => {
                        let b = io.alloc()?;
                        io.write(fd, b, len, at)?
                    }
                    (false, false) => io.read_at(fd, at)?,
                };
            }
            io_wait(&mut io, s.qd, |_, c| {
                exact(
                    "driver read or write",
                    c.result.map_or(-1, i64::from),
                    s.size,
                )
            })?;
        }
        Ok(batches * s.qd)
    });
    Ok(())
}
