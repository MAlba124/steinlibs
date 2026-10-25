//! `io_uring` backend. One SQE per operation.

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::time::Duration;

use io_uring::types::{BufRingEntry, Fixed, SubmitArgs, Timespec};
use io_uring::{IoUring, opcode, squeue, types};
use rustix::io::Errno;
use rustix::net::SocketAddrAny;

use super::slot::{Side, Slot, State};
use super::{Cqe, F_BUFFER, F_POSTED, FOREVER, Fd, Groups, Key, Mem, Op, RingFlags, Setup};
use crate::error::IoError;

/// User word of the queue's own cancel entries.
const OWN: u64 = u64::MAX;
/// Result marking a posted value.
const POSTED: i32 = i32::MIN;
/// CQ entries per SQ entry. Multishot posts many.
const CQ_FACTOR: u32 = 4;
/// `FUTEX2_SIZE_U32 | FUTEX2_PRIVATE`.
const FUTEX2_U32_PRIVATE: u32 = 0x02 | 128;
const NOSIGNAL: u32 = libc::MSG_NOSIGNAL as u32;
const ACCEPT_FLAGS: i32 = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;

/// What a reap reads and frees, borrowed from the queue.
pub(super) struct Reap<'q> {
    pub(super) slots: &'q mut [Slot],
    pub(super) sides: &'q crate::pinned::Pinned<Side>,
    pub(super) groups: &'q mut Groups,
    pub(super) free: &'q mut u32,
    pub(super) retired: &'q mut u32,
    pub(super) draining: bool,
}

pub(super) struct Ring {
    ring: IoUring,
    pub(super) flags: RingFlags,
    /// Registered buffer slots. `fixed_used` is the taken bitmap.
    fixed: u16,
    fixed_used: u64,
}

impl Ring {
    pub(super) fn new(setup: &Setup, slots: u32) -> Result<Ring, IoError> {
        let entries = slots.clamp(8, 4096).next_power_of_two();
        let (ring, flags) = build(setup, entries).map_err(errno_of)?;
        // Requires EXT_ARG (5.11) and RW_CUR_POS (5.6).
        if !ring.params().is_feature_ext_arg() || !ring.params().is_feature_rw_cur_pos() {
            return Err(IoError::Kernel(Errno::NOSYS));
        }
        let mut r = Ring {
            ring,
            flags,
            fixed: 0,
            fixed_used: 0,
        };
        let want = setup.fixed_buffers.min(64);
        if want > 0
            && r.ring
                .submitter()
                .register_buffers_sparse(want as u32)
                .is_ok()
        {
            r.fixed = want;
        }
        Ok(r)
    }

    pub(super) fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the ring's descriptor lives as long as `self`.
        unsafe { BorrowedFd::borrow_raw(self.ring.as_raw_fd()) }
    }

    /// `side` belongs to a free slot, so nothing else touches it until the completion.
    pub(super) fn push(
        &mut self,
        op: Op,
        side: &mut Side,
        key: Key,
        link: bool,
    ) -> Result<(), IoError> {
        let mut e = encode(op, side)?.user_data(key.0);
        if link {
            e = e.flags(squeue::Flags::IO_LINK);
        }
        self.queue(&e)
    }

    pub(super) fn cancel(&mut self, key: Key) -> Result<(), IoError> {
        let e = opcode::AsyncCancel::new(key.0).build().user_data(OWN);
        self.queue(&e)
    }

    /// A nop of the queue's own without `IO_LINK`, which ends an open chain.
    pub(super) fn end_chain(&mut self) -> Result<(), IoError> {
        let e = opcode::Nop::new().build().user_data(OWN);
        self.queue(&e)
    }

    fn queue(&mut self, e: &squeue::Entry) -> Result<(), IoError> {
        // SAFETY: entries point at pinned slot headers or `Mem`-vouched memory,
        // both outliving the completion.
        if unsafe { self.ring.submission().push(e) }.is_ok() {
            return Ok(());
        }
        // SQ full, submit to make room.
        self.ring.submit().map_err(errno_of)?;
        // SAFETY: as above.
        unsafe { self.ring.submission().push(e) }.map_err(|_| IoError::OpsFull)
    }

    /// Submit, and again if deferred task work is pending.
    pub(super) fn submit(&mut self) -> Result<(), IoError> {
        for _ in 0..2 {
            match self.ring.submit() {
                Ok(_) => {}
                Err(e) if quiet(&e) => {}
                Err(e) => return Err(errno_of(e)),
            }
            if !self.ring.submission().taskrun() {
                break;
            }
        }
        Ok(())
    }

    pub(super) fn unsubmitted(&mut self) -> bool {
        !self.ring.submission().is_empty()
    }

    pub(super) fn overflow(&mut self) -> u32 {
        self.ring.completion().overflow()
    }

    pub(super) fn backlog(&mut self) -> bool {
        self.ring.submission().cq_overflow()
    }

    pub(super) fn wait(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        if !self.ring.completion().is_empty() {
            return self.submit();
        }
        let r = match timeout {
            Some(d) => {
                let ts = Timespec::from(d.min(FOREVER));
                let args = SubmitArgs::new().timespec(&ts);
                self.ring.submitter().submit_with_args(1, &args)
            }
            None => self.ring.submit_and_wait(1),
        };
        match r {
            Ok(_) => Ok(()),
            Err(e) if quiet(&e) => Ok(()),
            Err(e) => Err(errno_of(e)),
        }
    }

    pub(super) fn pending(&mut self) -> bool {
        if !matches!(self.flags, RingFlags::Deferred | RingFlags::Coop) {
            return true;
        }
        let sq = self.ring.submission();
        if !sq.is_empty() || sq.taskrun() || sq.cq_overflow() {
            return true;
        }
        drop(sq);
        !self.ring.completion().is_empty()
    }

    pub(super) fn ready(&mut self) -> bool {
        !self.ring.completion().is_empty()
    }

    /// Fill `out`, freeing slots on final completions. Returns (written, freed).
    pub(super) fn reap(&mut self, at: &mut Reap<'_>, out: &mut [Cqe]) -> (usize, u32) {
        let (mut n, mut freed) = (0, 0);
        let mut cq = self.ring.completion();
        while n < out.len() {
            let Some(e) = cq.next() else { break };
            let (ud, res, flags) = (e.user_data(), e.result(), e.flags());
            if res == POSTED {
                out[n] = Cqe {
                    user: ud,
                    res: 0,
                    flags: F_POSTED,
                    addr: None,
                };
                n += 1;
                continue;
            }
            let key = Key(ud);
            let i = key.slot() as usize;
            let Some(s) = at.slots.get_mut(i) else {
                continue;
            };
            if ud == OWN || s.generation != key.generation() || s.state != State::Flying {
                continue;
            }
            let more = io_uring::cqueue::more(flags);
            let res = if s.after_chain {
                // The chain ended, so the close the nop stood for runs now.
                super::close_after(&s.op);
                0
            } else if s.void {
                -libc::ECANCELED
            } else {
                res
            };
            if flags & F_BUFFER != 0 {
                at.groups.handed_back(&s.op, (flags >> 16) as u16);
            }
            let addr = if more {
                None
            } else {
                // SAFETY: a final completion, so the kernel is done with the side.
                super::slot::peer(&s.op, unsafe { &*at.sides.at(i) }, res)
            };
            out[n] = Cqe {
                user: s.user,
                res,
                flags,
                addr,
            };
            n += 1;
            if at.draining {
                super::orphan(&s.op, res);
            }
            if more {
                continue;
            }
            // A close is done with its fd whatever `res` says, see `Queue::close`.
            at.groups.ended(&s.op);
            super::free_slot(at.slots, at.free, at.retired, key.slot());
            freed += 1;
        }
        (n, freed)
    }

    /// Register `len` bytes at `base` as a fixed buffer, or `u16::MAX`. Max 1 GiB.
    pub(super) fn fix(&mut self, base: *mut u8, len: usize) -> u16 {
        let Some(at) = (0..self.fixed).find(|i| self.fixed_used & (1 << i) == 0) else {
            return u16::MAX;
        };
        if len > 1 << 30 {
            return u16::MAX;
        }
        let iov = libc::iovec {
            iov_base: base.cast(),
            iov_len: len,
        };
        // SAFETY: the region is valid until unregistered or the ring closes.
        match unsafe {
            self.ring
                .submitter()
                .register_buffers_update(at as u32, &[iov], None)
        } {
            Ok(_) => {
                self.fixed_used |= 1 << at;
                at
            }
            Err(_) => u16::MAX,
        }
    }

    pub(super) fn unfix(&mut self, at: u16) {
        if at >= self.fixed {
            return;
        }
        let none = libc::iovec {
            iov_base: core::ptr::null_mut(),
            iov_len: 0,
        };
        // SAFETY: an empty entry clears the slot.
        let _ = unsafe {
            self.ring
                .submitter()
                .register_buffers_update(at as u32, &[none], None)
        };
        self.fixed_used &= !(1 << at);
    }

    pub(super) fn add_group(
        &mut self,
        ring: *mut BufRingEntry,
        entries: u16,
        bgid: u16,
    ) -> Result<(), IoError> {
        // SAFETY: page aligned and `entries` zeroed records, checked by `Queue::add_group`
        // or vouched for by its caller. It stays valid while an op can take from it, by
        // queue ownership for store pages or by `add_group`'s contract.
        unsafe {
            self.ring
                .submitter()
                .register_buf_ring_with_flags(ring as u64, entries, bgid, 0)
        }
        .map_err(errno_of)
    }

    pub(super) fn remove_group(&mut self, bgid: u16) -> Result<(), IoError> {
        self.ring
            .submitter()
            .unregister_buf_ring(bgid)
            .map_err(errno_of)
    }

    pub(super) fn register_files(&mut self, n: u32) -> Result<(), IoError> {
        self.ring
            .submitter()
            .register_files_sparse(n)
            .map_err(errno_of)
    }

    pub(super) fn set_file(&mut self, index: u32, fd: RawFd) -> Result<(), IoError> {
        self.ring
            .submitter()
            .register_files_update(index, &[fd])
            .map(drop)
            .map_err(errno_of)
    }
}

/// Build the best ring the kernel accepts, falling back from deferred to coop to plain.
fn build(setup: &Setup, entries: u32) -> std::io::Result<(IoUring, RingFlags)> {
    let cq = entries.saturating_mul(CQ_FACTOR);
    if setup.single_issuer {
        let mut b = IoUring::builder();
        b.setup_single_issuer()
            .setup_defer_taskrun()
            .setup_taskrun_flag()
            .setup_cqsize(cq)
            .setup_submit_all();
        if let Ok(r) = b.build(entries) {
            return Ok((r, RingFlags::Deferred));
        }
    }
    let mut b = IoUring::builder();
    b.setup_coop_taskrun()
        .setup_taskrun_flag()
        .setup_cqsize(cq)
        .setup_submit_all();
    if let Ok(r) = b.build(entries) {
        return Ok((r, RingFlags::Coop));
    }
    let mut b = IoUring::builder();
    b.setup_cqsize(cq);
    b.build(entries).map(|r| (r, RingFlags::Plain))
}

/// Build an entry for a raw or fixed fd, bound to `$f`.
macro_rules! on {
    ($fd:expr, $f:ident => $build:expr) => {
        match $fd {
            Fd::Raw(raw) => {
                let $f = types::Fd(raw);
                $build
            }
            Fd::Fixed(i) => {
                let $f = Fixed(i);
                $build
            }
        }
    };
}

/// Headers the kernel reads go into the slot's side.
pub(super) fn encode(op: Op, side: &mut Side) -> Result<squeue::Entry, IoError> {
    Ok(match op {
        Op::Nop => opcode::Nop::new().build(),
        Op::Read { fd, mem, at } => read(fd, mem, at),
        Op::Write { fd, mem, at } => write(fd, mem, at),
        Op::Recv { fd, mem } => on!(fd, f => opcode::Recv::new(f, mem.ptr, mem.len).build()),
        Op::RecvMulti { fd, group } => on!(fd, f => opcode::RecvMulti::new(f, group.0).build()),
        Op::Send { fd, mem } => {
            on!(fd, f => opcode::Send::new(f, mem.ptr, mem.len).flags(NOSIGNAL as i32).build())
        }
        Op::SendTo { fd, mem, to } => {
            message(side, mem, Mem::EMPTY);
            side.namelen = name(side, to);
            side.hdr.msg_name =
                (&mut side.name as *mut rustix::net::addr::SocketAddrStorage).cast();
            side.hdr.msg_namelen = side.namelen;
            let hdr: *const libc::msghdr = &side.hdr;
            on!(fd, f => opcode::SendMsg::new(f, hdr).flags(NOSIGNAL).build())
        }
        Op::RecvFrom { fd, mem } => {
            message(side, mem, Mem::EMPTY);
            side.hdr.msg_name =
                (&mut side.name as *mut rustix::net::addr::SocketAddrStorage).cast();
            side.hdr.msg_namelen = super::ADDR_LEN as libc::socklen_t;
            let hdr: *mut libc::msghdr = &mut side.hdr;
            on!(fd, f => opcode::RecvMsg::new(f, hdr).build())
        }
        Op::RecvAll { fd, mem } => {
            on!(fd, f => opcode::Recv::new(f, mem.ptr, mem.len).flags(libc::MSG_WAITALL).build())
        }
        Op::RecvMsgMulti {
            fd,
            group,
            name,
            control,
        } => {
            message(side, Mem::EMPTY, Mem::EMPTY);
            (side.hdr.msg_iov, side.hdr.msg_iovlen) = (core::ptr::null_mut(), 0);
            side.hdr.msg_namelen = if name {
                super::ADDR_LEN as libc::socklen_t
            } else {
                0
            };
            side.hdr.msg_controllen = control as usize;
            let hdr: *const libc::msghdr = &side.hdr;
            on!(fd, f => opcode::RecvMsgMulti::new(f, hdr, group.0).build())
        }
        Op::SendMsg {
            fd,
            mem,
            control,
            to,
        } => {
            message(side, mem, Mem::EMPTY);
            if let Some(to) = to {
                side.namelen = name(side, to);
                side.hdr.msg_name =
                    (&mut side.name as *mut rustix::net::addr::SocketAddrStorage).cast();
                side.hdr.msg_namelen = side.namelen;
            }
            side.hdr.msg_control = control.ptr.cast();
            side.hdr.msg_controllen = control.len as usize;
            let hdr: *const libc::msghdr = &side.hdr;
            on!(fd, f => opcode::SendMsg::new(f, hdr).flags(NOSIGNAL).build())
        }
        Op::Accept { fd, multi: false } => {
            side.namelen = super::ADDR_LEN as libc::socklen_t;
            let addr: *mut libc::sockaddr =
                (&mut side.name as *mut rustix::net::addr::SocketAddrStorage).cast();
            let len: *mut libc::socklen_t = &mut side.namelen;
            on!(fd, f => opcode::Accept::new(f, addr, len).flags(ACCEPT_FLAGS).build())
        }
        Op::Accept { fd, multi: true } => {
            on!(fd, f => opcode::AcceptMulti::new(f).flags(ACCEPT_FLAGS).build())
        }
        Op::Connect { fd, to } => {
            let len = name(side, to);
            let addr: *const libc::sockaddr =
                (&side.name as *const rustix::net::addr::SocketAddrStorage).cast();
            on!(fd, f => opcode::Connect::new(f, addr, len).build())
        }
        Op::Poll { fd, events, multi } => {
            on!(fd, f => opcode::PollAdd::new(f, events).multi(multi).build())
        }
        _ => return encode_file(op, side),
    })
}

fn encode_file(op: Op, side: &mut Side) -> Result<squeue::Entry, IoError> {
    let cstr = |m: Mem| -> *const libc::c_char {
        if m.len == 0 {
            c"".as_ptr()
        } else {
            m.ptr.cast()
        }
    };
    Ok(match op {
        Op::Open {
            dir,
            path,
            flags,
            mode,
        } => opcode::OpenAt::new(types::Fd(dir), cstr(path))
            .flags(flags.bits() as i32)
            .mode(mode.bits())
            .build(),
        Op::Statx {
            dir,
            path,
            out,
            follow,
        } => {
            let flags = statx_flags(path.len == 0, follow);
            opcode::Statx::new(types::Fd(dir), cstr(path), out.ptr.cast())
                .flags(flags)
                .mask(libc::STATX_BASIC_STATS)
                .build()
        }
        Op::Rename {
            from_dir,
            from,
            to_dir,
            to,
        } => opcode::RenameAt::new(types::Fd(from_dir), cstr(from), types::Fd(to_dir), cstr(to))
            .build(),
        Op::Unlink { dir, path, rmdir } => {
            let flags = if rmdir { libc::AT_REMOVEDIR } else { 0 };
            opcode::UnlinkAt::new(types::Fd(dir), cstr(path))
                .flags(flags)
                .build()
        }
        Op::Close { fd } => opcode::Close::new(types::Fd(fd.raw())).build(),
        Op::Fallocate { fd, at, len } => {
            on!(fd, f => opcode::Fallocate::new(f, len).offset(at).build())
        }
        Op::Truncate { fd, len } => on!(fd, f => opcode::Ftruncate::new(f, len).build()),
        Op::Splice { from, at, to, len } => {
            let off = if at == super::POSITION { -1 } else { at as i64 };
            match (from, to) {
                (Fd::Raw(a), Fd::Raw(b)) => {
                    opcode::Splice::new(types::Fd(a), off, types::Fd(b), -1, len).build()
                }
                _ => return Err(IoError::Unsupported),
            }
        }
        Op::SetSockOpt {
            fd,
            level,
            name,
            value,
        } => on!(fd, f => {
            opcode::SetSockOpt::new(f, level, name, value.ptr.cast(), value.len).build()
        }),
        Op::Fsync { fd, data_only } => {
            let flags = if data_only {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            };
            on!(fd, f => opcode::Fsync::new(f).flags(flags).build())
        }
        Op::SyncFileRange { fd, at, len, flags } => {
            on!(fd, f => opcode::SyncFileRange::new(f, len).offset(at).flags(flags).build())
        }
        Op::Fadvise {
            fd,
            at,
            len,
            advice,
        } => on!(fd, f => {
            opcode::Fadvise::new(f, len as libc::off_t, advice).offset(at).build()
        }),
        Op::Timeout { after } => {
            // Clamped as on `epoll`, past it the kernel reads a negative second.
            side.ts = Timespec::from(after.min(FOREVER));
            let ts: *const Timespec = &side.ts;
            opcode::Timeout::new(ts).build()
        }
        Op::FutexWait { word, value } => opcode::FutexWait::new(
            word.ptr.cast::<u32>(),
            value as u64,
            libc::FUTEX_BITSET_MATCH_ANY as u32 as u64,
            FUTEX2_U32_PRIVATE,
        )
        .build(),
        Op::Post { ring, value } => {
            opcode::MsgRingData::new(types::Fd(ring), POSTED, value, None).build()
        }
        _ => return Err(IoError::Unsupported),
    })
}

pub(super) fn statx_flags(empty: bool, follow: bool) -> i32 {
    let empty = if empty { libc::AT_EMPTY_PATH } else { 0 };
    let nofollow = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    empty | nofollow
}

fn read(fd: Fd, mem: Mem, at: u64) -> squeue::Entry {
    match mem.fixed() {
        Some(i) => on!(fd, f => opcode::ReadFixed::new(f, mem.ptr, mem.len, i).offset(at).build()),
        None => on!(fd, f => opcode::Read::new(f, mem.ptr, mem.len).offset(at).build()),
    }
}

fn write(fd: Fd, mem: Mem, at: u64) -> squeue::Entry {
    match mem.fixed() {
        Some(i) => on!(fd, f => opcode::WriteFixed::new(f, mem.ptr, mem.len, i).offset(at).build()),
        None => on!(fd, f => opcode::Write::new(f, mem.ptr, mem.len).offset(at).build()),
    }
}

/// A msghdr over one buffer, with `name` as the address room.
fn message(side: &mut super::slot::Side, mem: Mem, name: Mem) {
    // SAFETY: a header of integers and pointers, all zeros is a value.
    side.hdr = unsafe { core::mem::zeroed() };
    side.iov = libc::iovec {
        iov_base: mem.ptr.cast(),
        iov_len: mem.len as usize,
    };
    side.hdr.msg_iov = &mut side.iov;
    side.hdr.msg_iovlen = 1;
    side.hdr.msg_name = name.ptr.cast();
    side.hdr.msg_namelen = name.len as libc::socklen_t;
}

/// Write `to` into the slot's address room. Returns its length.
fn name(side: &mut super::slot::Side, to: std::net::SocketAddr) -> libc::socklen_t {
    let any = SocketAddrAny::from(to);
    let len = any.addr_len() as usize;
    // SAFETY: `len` bytes of `any` are initialised and fit the storage.
    unsafe {
        core::ptr::copy_nonoverlapping(
            any.as_ptr().cast::<u8>(),
            &mut side.name as *mut _ as *mut u8,
            len,
        );
    }
    len as libc::socklen_t
}

fn errno_of(e: std::io::Error) -> IoError {
    IoError::Kernel(Errno::from_raw_os_error(e.raw_os_error().unwrap_or(5)))
}

/// Benign wait errors. Timeout, signal, full queue.
fn quiet(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(c) if c == libc::ETIME || c == libc::EINTR || c == libc::EAGAIN || c == libc::EBUSY)
}
