//! File ops. One ring entry each. On epoll they are blocking syscalls made at submission.

use rustix::fs::{AtFlags, Mode, OFlags, StatxFlags};

use super::*;
use crate::{dir, file};

/// Memory handed over by [`Io::add_region`], indexed in add order.
/// Held for the driver's life. Access via [`Io::region_mut`] or [`Io::region_at_mut`],
/// refused for ranges an operation in flight names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region(pub u16);

const O_CLOEXEC: OFlags = OFlags::CLOEXEC;
/// Mode for created files, before umask.
const APPEND_MODE: u32 = 0o644;
/// `struct statx` room at the head of a stat buffer, path follows.
const STATX_ROOM: u32 = queue::STATX_LEN as u32;

impl Io {
    /// Open read-only. Completes with an fd the driver owns.
    /// `path` resolves against cwd and must fit in one buffer.
    pub fn open(&mut self, path: &[u8]) -> Result<OpId, IoError> {
        self.open_with(CWD, path, OFlags::RDONLY, Mode::empty())
    }

    /// Open for append, create if missing. Writes at [`APPEND`] always land at the end.
    pub fn open_append(&mut self, path: &[u8]) -> Result<OpId, IoError> {
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND;
        self.open_with(CWD, path, flags, Mode::from_raw_mode(APPEND_MODE))
    }

    /// Open read-write at offsets, create if missing.
    pub fn open_rw(&mut self, path: &[u8]) -> Result<OpId, IoError> {
        self.open_with(
            CWD,
            path,
            OFlags::RDWR | OFlags::CREATE,
            Mode::from_raw_mode(APPEND_MODE),
        )
    }

    /// Open relative to an owned dir fd or [`CWD`]. `O_CLOEXEC` is always added.
    pub fn open_with(
        &mut self,
        dir: RawFd,
        path: &[u8],
        flags: OFlags,
        mode: Mode,
    ) -> Result<OpId, IoError> {
        let bid = self.path_buf(path, 0)?;
        let (id, bid) = self.insert_path(Kind::Open, bid)?;
        let path = self.buffer(bid, 0, path.len() as u32 + 1);
        let flags = flags | O_CLOEXEC;
        self.submit(
            id,
            queue::Op::Open {
                dir,
                path,
                flags,
                mode,
            },
        )?;
        Ok(id)
    }

    /// Positional read into a driver buffer. `Ok(0)` is EOF.
    pub fn read_at(&mut self, fd: RawFd, offset: u64) -> Result<OpId, IoError> {
        let bid = self.bufs.take_lent().map_err(|_| IoError::BuffersFull)?;
        let mut op = Op::new(Kind::Read, fd).with_buf(bid);
        op.offset = offset;
        let id = self.insert_lent(op, bid)?;
        let mem = self.buffer(bid, 0, u32::MAX);
        self.submit(
            id,
            queue::Op::Read {
                fd: raw(fd),
                mem,
                at: offset,
            },
        )?;
        Ok(id)
    }

    /// Read into a caller buffer at `at`. Driver owns it until completion.
    /// Filled length counts the prefix. Failure or cancel releases the buffer, an error
    /// here too, see [`Io::send`]. A range past the buffer size is [`IoError::TooLong`].
    pub fn read_at_into(
        &mut self,
        fd: RawFd,
        offset: u64,
        buf: BufId,
        at: u32,
        len: u32,
    ) -> Result<OpId, IoError> {
        let buf = self.fits(buf, at, len)?;
        let mut op = Op::new(Kind::Read, fd);
        op.offset = offset;
        // Reuse `sent`/`len` for the landing range.
        op.sent = at;
        op.len = len;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, at, len);
        self.submit(
            id,
            queue::Op::Read {
                fd: raw(fd),
                mem,
                at: offset,
            },
        )?;
        Ok(id)
    }

    /// One-shot read on an unseekable fd (eventfd, pipe). Rearm after each completion.
    pub fn read_stream(&mut self, fd: RawFd) -> Result<OpId, IoError> {
        let bid = self.bufs.take_lent().map_err(|_| IoError::BuffersFull)?;
        let id = self.insert_lent(Op::new(Kind::Read, fd).with_buf(bid), bid)?;
        let mem = self.buffer(bid, 0, u32::MAX);
        self.submit(
            id,
            queue::Op::Read {
                fd: raw(fd),
                mem,
                at: queue::POSITION,
            },
        )?;
        Ok(id)
    }

    /// Write at `offset`, or the fd position for [`APPEND`]. Driver owns `buf` as for
    /// [`Io::send`]. Short writes are continued, so completion means all of `len` went out.
    pub fn write(&mut self, fd: RawFd, buf: BufId, len: u32, offset: u64) -> Result<OpId, IoError> {
        let buf = self.fits(buf, 0, len)?;
        let mut op = Op::new(Kind::Write, fd);
        op.len = len;
        op.offset = offset;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, 0, op.len);
        self.submit(
            id,
            queue::Op::Write {
                fd: raw(fd),
                mem,
                at: offset,
            },
        )?;
        Ok(id)
    }

    /// Completes with a buffer [`crate::file::meta`] decodes. Follows symlinks.
    pub fn stat(&mut self, path: &[u8]) -> Result<OpId, IoError> {
        let bid = self.path_buf(path, STATX_ROOM)?;
        let (id, bid) = self.insert_path_on(Kind::Stat, -1, bid)?;
        let out = self.buffer(bid, 0, STATX_ROOM);
        let path = self.buffer(bid, STATX_ROOM, path.len() as u32 + 1);
        self.submit(
            id,
            queue::Op::Statx {
                dir: CWD,
                path,
                out,
                follow: true,
            },
        )?;
        Ok(id)
    }

    /// [`Io::stat`] of an owned fd.
    pub fn stat_fd(&mut self, fd: RawFd) -> Result<OpId, IoError> {
        let bid = self.bufs.take().map_err(|_| IoError::BuffersFull)?;
        let (id, bid) = self.insert_path_on(Kind::Stat, fd, bid)?;
        let out = self.buffer(bid, 0, STATX_ROOM);
        self.submit(
            id,
            queue::Op::Statx {
                dir: fd,
                path: Mem::EMPTY,
                out,
                follow: true,
            },
        )?;
        Ok(id)
    }

    /// Synchronous fd stat, for a stat that waited on its open.
    pub(super) fn stat_fd_now(&mut self, id: OpId, fd: RawFd, bid: u16) {
        let got = match borrow(&self.owned, fd) {
            Some(f) => rustix::fs::statx(f, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS),
            None => Err(Errno::BADF),
        };
        match got {
            Ok(sx) => {
                // No kernel entry holds it, the stat ran here.
                let buf = self.landed(bid, file::META_LEN);
                file::serialize(&sx, self.bufs.bytes_mut(&buf.id));
                self.retire(id, Ok(file::META_LEN), Some(buf));
            }
            Err(e) => self.fail_read(id, bid, e),
        }
    }

    /// Completes with `Ok(0)`.
    pub fn fallocate(&mut self, fd: RawFd, offset: u64, len: u64) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Fallocate, fd);
        op.offset = offset;
        let id = self.insert(op)?;
        self.submit(
            id,
            queue::Op::Fallocate {
                fd: raw(fd),
                at: offset,
                len,
            },
        )?;
        Ok(id)
    }

    /// Hand memory over for the driver's life. Max eight regions.
    pub fn add_region(&mut self, mem: Box<[u8]>) -> Result<Region, IoError> {
        if self.regions.len() == self.regions.capacity() {
            return Err(IoError::RegionsFull);
        }
        self.regions.push(Pinned::new(mem));
        Ok(Region(self.regions.len() as u16 - 1))
    }

    /// The whole region. Empty for an unknown region or while a read into it is in flight.
    pub fn region(&self, r: Region) -> &[u8] {
        let len = self.regions.get(r.0 as usize).map_or(0, Pinned::len);
        self.region_at(r, 0, len)
    }

    /// The whole region to fill. Empty for an unknown region or while any op on it is in flight.
    pub fn region_mut(&mut self, r: Region) -> &mut [u8] {
        let len = self.regions.get(r.0 as usize).map_or(0, Pinned::len);
        self.region_at_mut(r, 0, len)
    }

    /// `len` bytes at `at`. Empty if out of range or a read in flight writes any of them.
    pub fn region_at(&self, r: Region, at: usize, len: usize) -> &[u8] {
        match self.free_range(r, at, len, false) {
            // SAFETY: in the region, which the driver owns, and no op in flight writes
            // these bytes. A mutable view needs `&mut self`, which this borrow excludes.
            Some(p) => unsafe { core::slice::from_raw_parts(p, len) },
            None => &[],
        }
    }

    /// `len` bytes at `at` to fill. Empty if out of range or any op in flight names them.
    pub fn region_at_mut(&mut self, r: Region, at: usize, len: usize) -> &mut [u8] {
        match self.free_range(r, at, len, true) {
            // SAFETY: as in `region_at`, no op in flight reads or writes these bytes, and
            // `&mut self` makes this the only view.
            Some(p) => unsafe { core::slice::from_raw_parts_mut(p, len) },
            None => &mut [],
        }
    }

    /// Start of a range no op in flight conflicts with. A write conflicts with any op.
    fn free_range(&self, r: Region, at: usize, len: usize, write: bool) -> Option<*mut u8> {
        let m = self.regions.get(r.0 as usize)?;
        let end = at.checked_add(len).filter(|end| *end <= m.len())?;
        let busy = self
            .spans
            .iter()
            .any(|s| s.region == r.0 && s.at < end && at < s.end && (write || s.writes));
        (!busy).then(|| m.at(at))
    }

    fn span_of(&self, r: Region, at: usize, len: u32) -> Result<Mem, IoError> {
        let size = self.regions.get(r.0 as usize).map_or(0, Pinned::len);
        let known = (r.0 as usize) < self.regions.len();
        if !known || at.checked_add(len as usize).is_none_or(|end| end > size) {
            return Err(IoError::OutOfRegion {
                at,
                len: len as usize,
                size,
            });
        }
        let base = self.regions[r.0 as usize].at(at);
        // SAFETY: in range, checked above. The driver owns the region and frees it only
        // after the queue drained, or never. The range is a span until the op's last
        // completion, so the caller cannot reach it meanwhile.
        Ok(unsafe { Mem::raw(base, len as usize) })
    }

    /// Insert and submit a region op, its range a span until the last completion.
    fn submit_span(&mut self, op: Op, r: Region, at: usize, q: queue::Op) -> Result<OpId, IoError> {
        let id = self.insert(op)?;
        self.spans.push(Span {
            user: id.to_user_data(),
            region: r.0,
            at,
            end: at + op.len as usize,
            writes: op.kind == Kind::ReadMem,
        });
        self.submit(id, q)?;
        Ok(id)
    }

    /// Single read into a region. Short reads are reported, not continued. Zero is EOF.
    pub fn read_mem(
        &mut self,
        fd: RawFd,
        offset: u64,
        r: Region,
        at: usize,
        len: u32,
    ) -> Result<OpId, IoError> {
        let mem = self.span_of(r, at, len)?;
        let op = mem_op(Kind::ReadMem, fd, offset, r, at, len);
        let read = queue::Op::Read {
            fd: raw(fd),
            mem,
            at: offset,
        };
        self.submit_span(op, r, at, read)
    }

    /// Single write from a region, [`APPEND`] for fd position. Short writes are reported, not continued.
    pub fn write_mem(
        &mut self,
        fd: RawFd,
        offset: u64,
        r: Region,
        at: usize,
        len: u32,
    ) -> Result<OpId, IoError> {
        let mem = self.span_of(r, at, len)?;
        let op = mem_op(Kind::WriteMem, fd, offset, r, at, len);
        let write = queue::Op::Write {
            fd: raw(fd),
            mem,
            at: offset,
        };
        self.submit_span(op, r, at, write)
    }

    /// `fsync`, or `fdatasync` with `data`. Completes with `Ok(0)`.
    pub fn sync(&mut self, fd: RawFd, data: bool) -> Result<OpId, IoError> {
        let id = self.insert(Op::new(Kind::Sync, fd))?;
        self.submit(
            id,
            queue::Op::Fsync {
                fd: raw(fd),
                data_only: data,
            },
        )?;
        Ok(id)
    }

    /// Completes with `Ok(0)`. Ring entry needs kernel 6.9.
    pub fn truncate(&mut self, fd: RawFd, len: u64) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Truncate, fd);
        op.offset = len;
        let id = self.insert(op)?;
        self.submit(id, queue::Op::Truncate { fd: raw(fd), len })?;
        Ok(id)
    }

    /// Both paths must fit one buffer together. Completes with `Ok(0)`.
    pub fn rename(
        &mut self,
        from_dir: RawFd,
        from: &[u8],
        to_dir: RawFd,
        to: &[u8],
    ) -> Result<OpId, IoError> {
        let max = (self.bufs.size() as usize).saturating_sub(2);
        if from.len() + to.len() > max {
            return Err(IoError::PathTooLong {
                len: from.len() + to.len(),
                max,
            });
        }
        let bid = self.path_buf(from, 0)?;
        let second = from.len() + 1;
        // Still the caller's, lent once both paths are in.
        let bytes = self.bufs.bytes_mut(&bid);
        bytes[second..second + to.len()].copy_from_slice(to);
        bytes[second + to.len()] = 0;
        let (id, bid) = self.insert_path(Kind::Rename, bid)?;
        let a = self.buffer(bid, 0, second as u32);
        let b = self.buffer(bid, second as u32, to.len() as u32 + 1);
        self.submit(
            id,
            queue::Op::Rename {
                from_dir,
                from: a,
                to_dir,
                to: b,
            },
        )?;
        Ok(id)
    }

    /// `rmdir` with `is_dir`. Completes with `Ok(0)`.
    pub fn unlink(&mut self, dir: RawFd, path: &[u8], is_dir: bool) -> Result<OpId, IoError> {
        let bid = self.path_buf(path, 0)?;
        let (id, bid) = self.insert_path(Kind::Unlink, bid)?;
        let path = self.buffer(bid, 0, path.len() as u32 + 1);
        self.submit(
            id,
            queue::Op::Unlink {
                dir,
                path,
                rmdir: is_dir,
            },
        )?;
        Ok(id)
    }

    /// Pack whole entries into a region range for [`crate::dir::entries`]. Zero means done.
    /// `EINVAL` if one name does not fit. Blocks, no async getdents exists.
    /// [`IoError::RegionBusy`] if an op in flight names the range.
    pub fn list(&mut self, fd: RawFd, r: Region, at: usize, len: u32) -> Result<OpId, IoError> {
        self.span_of(r, at, len)?;
        let Some(p) = self.free_range(r, at, len as usize, true) else {
            return Err(IoError::RegionBusy {
                at,
                len: len as usize,
            });
        };
        let id = self.insert(Op::new(Kind::List, fd))?;
        // SAFETY: in the region and clear of every op in flight, checked above.
        let out = unsafe { core::slice::from_raw_parts_mut(p, len as usize) };
        let got = match borrow(&self.owned, fd) {
            Some(d) => dir::pack(d, out),
            None => Err(Errno::BADF),
        };
        self.retire(id, got.map(|n| n as u32), None);
        Ok(id)
    }

    /// Caller-held buffer with NUL-terminated `path` at `at`.
    fn path_buf(&mut self, path: &[u8], at: u32) -> Result<BufId, IoError> {
        let start = at as usize;
        let max = (self.bufs.size() as usize).saturating_sub(start + 1);
        if path.len() > max {
            return Err(IoError::PathTooLong {
                len: path.len(),
                max,
            });
        }
        let bid = self.bufs.take().map_err(|_| IoError::BuffersFull)?;
        let bytes = self.bufs.bytes_mut(&bid);
        bytes[start..start + path.len()].copy_from_slice(path);
        bytes[start + path.len()] = 0;
        Ok(bid)
    }

    /// Frees `bid` if there is no slot.
    fn insert_lent(&mut self, op: Op, bid: u16) -> Result<OpId, IoError> {
        self.insert(op).inspect_err(|_| {
            self.free_buf(bid);
        })
    }

    fn insert_path(&mut self, kind: Kind, bid: BufId) -> Result<(OpId, u16), IoError> {
        self.insert_path_on(kind, -1, bid)
    }

    /// Lends a path buffer the driver filled to the op. Returns the op and the buffer's index.
    fn insert_path_on(
        &mut self,
        kind: Kind,
        fd: RawFd,
        bid: BufId,
    ) -> Result<(OpId, u16), IoError> {
        let i = self.lend(bid)?;
        let id = self.insert_lent(Op::new(kind, fd).with_buf(i), i)?;
        Ok((id, i))
    }

    /// Op whose buffer only held kernel input.
    pub(super) fn done_path(&mut self, id: OpId, op: Op, res: i32) {
        if let Some(b) = op.buf {
            self.free_buf(b);
        }
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = None;
        }
        self.done_plain(id, res);
    }

    pub(super) fn done_open(&mut self, id: OpId, op: Op, res: i32) {
        if let Some(b) = op.buf {
            self.free_buf(b);
        }
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = None;
        }
        if res < 0 {
            let e = Errno::from_raw_os_error(-res);
            self.retire(id, Err(e), None);
            return self.stat_landed(op, Err(e));
        }
        // SAFETY: fresh from the kernel, nothing else has its number.
        let fd = unsafe { OwnedFd::from_raw_fd(res) };
        self.adopt(id, fd);
        self.stat_landed(op, Ok(res));
    }

    fn adopt(&mut self, id: OpId, fd: OwnedFd) {
        let raw = fd.as_raw_fd();
        self.keep(raw, fd);
        self.set_fd(id, raw);
        self.retire(id, Ok(raw as u32), None);
    }

    /// File read or stat. One buffer, one delivery.
    pub(super) fn done_file(&mut self, id: OpId, op: Op, res: i32) {
        let Some(bid) = op.buf else {
            return;
        };
        if res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-res));
        }
        let len = match op.kind {
            Kind::Stat => file::META_LEN,
            _ => res as u32,
        };
        // Read at an offset counts the prefix as filled.
        let filled = if op.kind == Kind::Read {
            op.sent + len
        } else {
            len
        };
        let buf = self.landed(bid, filled);
        self.retire(id, Ok(len), Some(buf));
    }
}

fn mem_op(kind: Kind, fd: RawFd, offset: u64, r: Region, at: usize, len: u32) -> Op {
    let mut op = Op::new(kind, fd);
    op.offset = offset;
    op.aux = u32::from(r.0);
    op.sent = at as u32;
    op.len = len;
    op
}
