//! Completion queue over owned slots and caller-vouched memory. `io_uring` or `epoll`.
//!
//! Every operation ends with exactly one completion without [`F_MORE`]. Its slot and
//! the memory it names belong to the kernel until that completion is taken.
//! Ring-only operations complete with `EOPNOTSUPP` on `epoll`.
//!
//! The queue owns the memory the kernel reads by address, its slot headers and ring pages.
//! Leaking a queue leaks that memory with it, so the kernel never writes freed memory. A drop
//! whose drain times out leaks it on purpose.

use std::net::SocketAddr;
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::error::IoError;
use crate::pinned::Pinned;

mod epoll;
mod hold;
mod slot;
mod uring;

pub use io_uring::types::BufRingEntry;
pub use slot::Owned;

use hold::Holds;
use slot::{Behind, Side, Slot, State, Watch};

/// No slot, end of a list.
pub(crate) const NONE: u32 = u32::MAX;
/// Offset meaning the descriptor's own position.
pub const POSITION: u64 = u64::MAX;
/// Dir fd for paths relative to the working directory. `AT_FDCWD`.
pub const CWD: RawFd = -100;
pub const REGIONS: usize = 16;
pub const GROUPS: usize = 64;
/// More completions follow for the same operation.
pub const F_MORE: u32 = 1 << 1;
/// Took a provided buffer, index in the top 16 bits.
pub const F_BUFFER: u32 = 1 << 0;
/// A value posted by [`Op::Post`]. Not a kernel flag, clear of kernel bits.
pub const F_POSTED: u32 = 1 << 15;
/// Longest data length of one [`Op::Read`], [`Op::Write`], [`Op::Recv`] or [`Op::RecvAll`],
/// the kernel's `MAX_RW_COUNT`. Longer ones are refused, so every result fits an `i32`.
pub const MAX_RW_COUNT: u32 = 0x7fff_f000;
/// Longest wait, about 136 years. Longer timeouts and ticks wait this long.
const FOREVER: Duration = Duration::from_secs(u32::MAX as u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fd {
    Raw(RawFd),
    /// Index into the [`Queue::register_files`] table.
    Fixed(u32),
}

/// Memory an operation reads or writes. Made only by [`Queue::mem`] or [`Mem::raw`].
///
/// One from [`Queue::mem`] carries its region's stamp, and a push names it only while that
/// region is still registered with the same queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mem {
    ptr: *mut u8,
    len: u32,
    /// Registered buffer index, or `u16::MAX`.
    fixed: u16,
    /// Region stamp in the high bits, region index in the low byte. Zero for [`Mem::raw`].
    tag: u64,
}

/// Region stamps, unique for the process's life so no stale `Mem` matches a new region.
static STAMPS: AtomicU64 = AtomicU64::new(1);

impl Mem {
    pub const EMPTY: Mem = Mem {
        ptr: core::ptr::null_mut(),
        len: 0,
        fixed: u16::MAX,
        tag: 0,
    };

    /// # Safety
    ///
    /// The `len` bytes at `ptr` stay valid until every operation naming them has
    /// its final completion taken, forever if the queue drops before that. Memory handed
    /// to [`Queue::provide`] counts as named until a completion hands its buffer back, or
    /// if its group is removed first, until every op that took from the group ended. No
    /// conflicting access meanwhile, and none by the queue either, so no op in flight
    /// writes bytes a path op names when it is pushed, as the push reads the path's end.
    pub unsafe fn raw(ptr: *mut u8, len: usize) -> Mem {
        Mem {
            ptr,
            len: len.min(u32::MAX as usize) as u32,
            fixed: u16::MAX,
            tag: 0,
        }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn fixed(&self) -> Option<u16> {
        (self.fixed != u16::MAX).then_some(self.fixed)
    }

    /// `len` bytes from `at`, clamped.
    pub fn part(self, at: usize, len: usize) -> Mem {
        let at = at.min(self.len as usize);
        let len = len.min(self.len as usize - at);
        Mem {
            ptr: self.ptr.wrapping_add(at),
            len: len as u32,
            fixed: self.fixed,
            tag: self.tag,
        }
    }
}

/// Caller memory handed over once, by [`Queue::register`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region(pub u16);

/// A provided buffer ring from [`Queue::add_group`]. The kernel's buffer group id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group(pub u16);

/// Slot and generation of an operation in flight. Stale keys name nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub u64);

impl Key {
    fn new(slot: u32, generation: u32) -> Key {
        Key(((generation as u64) << 32) | slot as u64)
    }

    pub fn slot(self) -> u32 {
        self.0 as u32
    }

    pub fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cqe {
    pub user: u64,
    /// Bytes moved, a descriptor, or negated errno.
    pub res: i32,
    pub flags: u32,
    /// Peer of [`Op::RecvFrom`] or one-shot [`Op::Accept`], if an internet address.
    pub addr: Option<SocketAddr>,
}

impl Cqe {
    pub const EMPTY: Cqe = Cqe {
        user: 0,
        res: 0,
        flags: 0,
        addr: None,
    };
}

impl Cqe {
    pub fn more(&self) -> bool {
        self.flags & F_MORE != 0
    }

    /// Provided buffer index taken.
    pub fn buffer(&self) -> Option<u16> {
        (self.flags & F_BUFFER != 0).then_some((self.flags >> 16) as u16)
    }

    /// A posted value, carried in `user`.
    pub fn is_posted(&self) -> bool {
        self.flags & F_POSTED != 0
    }

    pub fn result(&self) -> Result<u32, Errno> {
        match self.res {
            r if r < 0 => Err(Errno::from_raw_os_error(-r)),
            r => Ok(r as u32),
        }
    }
}

/// A descriptor a [`Op::Close`] owns. Made only by [`Queue::close`], from an [`OwnedFd`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closing(RawFd);

impl Closing {
    pub fn raw(&self) -> RawFd {
        self.0
    }
}

/// An operation, copied into its slot.
///
/// No ordering between operations on one descriptor on the ring. Keep one
/// receive and one send in flight per stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Completes with zero.
    Nop,
    /// Read at `at`, or the position for [`POSITION`].
    Read {
        fd: Fd,
        mem: Mem,
        at: u64,
    },
    Write {
        fd: Fd,
        mem: Mem,
        at: u64,
    },
    Recv {
        fd: Fd,
        mem: Mem,
    },
    /// Multishot into buffers of `group`.
    RecvMulti {
        fd: Fd,
        group: Group,
    },
    /// Send what fits. No `SIGPIPE`.
    Send {
        fd: Fd,
        mem: Mem,
    },
    SendTo {
        fd: Fd,
        mem: Mem,
        to: SocketAddr,
    },
    /// Source comes back in [`Cqe::addr`].
    RecvFrom {
        fd: Fd,
        mem: Mem,
    },
    /// Fill all of `mem` from a stream, or up to its end. One completion.
    RecvAll {
        fd: Fd,
        mem: Mem,
    },
    /// Multishot into buffers of `group`, laid out as `io_uring_recvmsg_out`.
    /// Header, name room if `name`, `control` bytes, payload. Result covers all of it.
    RecvMsgMulti {
        fd: Fd,
        group: Group,
        name: bool,
        control: u32,
    },
    /// Send with encoded control messages, to `to` or the connected peer. No `SIGPIPE`.
    SendMsg {
        fd: Fd,
        mem: Mem,
        control: Mem,
        to: Option<SocketAddr>,
    },
    /// Non-blocking, close-on-exec. Result is the caller's new descriptor. One-shot
    /// fills [`Cqe::addr`].
    Accept {
        fd: Fd,
        multi: bool,
    },
    Connect {
        fd: Fd,
        to: SocketAddr,
    },
    /// Wait for `POLL*` `events`. Result is what fired.
    Poll {
        fd: Fd,
        events: u32,
        multi: bool,
    },
    /// `path` is NUL terminated.
    Open {
        dir: RawFd,
        path: Mem,
        flags: OFlags,
        mode: Mode,
    },
    /// Writes a `struct statx` to `out`, see [`statx_of`]. Empty path means `dir` itself.
    /// A symlink at the end of `path` is stated itself unless `follow`.
    Statx {
        dir: RawFd,
        path: Mem,
        out: Mem,
        follow: bool,
    },
    Rename {
        from_dir: RawFd,
        from: Mem,
        to_dir: RawFd,
        to: Mem,
    },
    Unlink {
        dir: RawFd,
        path: Mem,
        rmdir: bool,
    },
    /// Pushed by [`Queue::close`] only.
    Close {
        fd: Closing,
    },
    Fsync {
        fd: Fd,
        data_only: bool,
    },
    /// `sync_file_range(2)` of `len` bytes at `at`, `SYNC_FILE_RANGE_*` `flags`.
    SyncFileRange {
        fd: Fd,
        at: u64,
        len: u32,
        flags: u32,
    },
    /// `posix_fadvise(2)` of `len` bytes at `at`, zero meaning to the end.
    Fadvise {
        fd: Fd,
        at: u64,
        len: u32,
        advice: i32,
    },
    Fallocate {
        fd: Fd,
        at: u64,
        len: u64,
    },
    /// Needs kernel 6.9 on the ring.
    Truncate {
        fd: Fd,
        len: u64,
    },
    /// Up to `len` bytes, one side a pipe. Short moves are not resubmitted. On
    /// `epoll` an empty `from` gives `EAGAIN` and a full `to` is waited on.
    Splice {
        from: Fd,
        at: u64,
        to: Fd,
        len: u32,
    },
    SetSockOpt {
        fd: Fd,
        level: u32,
        name: u32,
        value: Mem,
    },
    /// Completes with `ETIME`.
    Timeout {
        after: Duration,
    },
    /// Private futex wait on the `u32` at `word`. Ring only.
    FutexWait {
        word: Mem,
        value: u32,
    },
    /// Post `value` to another ring's completion queue with [`F_POSTED`]. Ring only.
    Post {
        ring: RawFd,
        value: u64,
    },
}

/// The backend a queue runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Readiness, emulating completion.
    Epoll,
    Uring,
}

/// How a ring was set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingFlags {
    /// Not a ring.
    None,
    /// `SINGLE_ISSUER` and `DEFER_TASKRUN`. Kernel 6.1.
    Deferred,
    /// `COOP_TASKRUN` only. Kernel 5.19.
    Coop,
    /// Only a larger completion queue.
    Plain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The ring, else `epoll`.
    Any,
    Uring,
    Epoll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub want: Want,
    /// Only the creating thread submits and waits. Falls back on older kernels,
    /// see [`Queue::ring_flags`].
    pub single_issuer: bool,
    /// Registered buffer slots for fixed regions.
    pub fixed_buffers: u16,
}

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            want: Want::Any,
            single_issuer: false,
            fixed_buffers: 0,
        }
    }
}

/// A failed start, returning the store.
pub struct Refused {
    pub store: Owned,
    pub error: IoError,
}

impl core::fmt::Debug for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Refused({})", self.error)
    }
}

/// A close that never reached the queue, returning the descriptor.
#[derive(Debug)]
pub struct CloseRefused {
    pub fd: OwnedFd,
    pub error: IoError,
}

#[derive(Clone, Copy, Debug)]
struct RegionEntry {
    base: *mut u8,
    len: usize,
    fixed: u16,
    /// What a [`Mem`] of this region carries.
    tag: u64,
}

#[derive(Clone, Copy, Debug)]
struct GroupEntry {
    ring: *mut BufRingEntry,
    entries: u16,
    tail: u16,
    /// Next entry `epoll` takes, as the kernel's head. Unused on the ring.
    head: u16,
    /// Removed, kept until its ops end since they may still hold its buffers.
    removed: bool,
}

/// `struct io_uring_buf`, written field by field so no reference covers the tail the
/// kernel reads in the first entry.
#[repr(C)]
struct RawBuf {
    addr: u64,
    len: u32,
    bid: u16,
    /// The ring's tail in the first entry.
    resv: u16,
}

const _: () = assert!(size_of::<RawBuf>() == size_of::<BufRingEntry>());

// One per queue.
#[allow(clippy::large_enum_variant)]
enum Engine {
    Uring(uring::Ring),
    Epoll(epoll::Poll),
}

pub struct Queue {
    /// Dropped first. A dup of the ring fd, from `Io::mailbox` or a clone of
    /// [`Queue::as_fd`], keeps the kernel context alive past it, so the order alone proves
    /// nothing. What does is the drain in the drop. Once it succeeds nothing is in flight,
    /// and safe code cannot submit to a ring whose queue is gone, so the kernel never
    /// touches the memory below again. A drain that timed out leaks that memory.
    engine: Engine,
    /// Headers the kernel reads and writes by address, one per slot.
    sides: Pinned<Side>,
    /// Provided buffer ring pages, see [`Queue::add_ring`].
    rings: Pinned<RingPage>,
    slots: Box<[Slot]>,
    /// `epoll` state per fd number, grown for higher ones.
    watches: Vec<Watch>,
    /// Free slot list head.
    free: u32,
    live: u32,
    /// Slots whose generation ran out, never reused so no stale key names a new op.
    retired: u32,
    /// Readiness completions, oldest first.
    done: slot::List,
    /// Multishot ops to retry. An edge may have had more data than they took.
    retry: slot::List,
    /// Pending readiness timeouts.
    timers: u32,
    regions: [Option<RegionEntry>; REGIONS],
    groups: Groups,
    /// Store taken or abandoned, the drop leaves it alone.
    taken: bool,
    /// See [`Queue::link`].
    link: bool,
    /// The last ring entry carries `IO_LINK`, so the next one joins its chain.
    tail_linked: bool,
    /// Slots that may have something waiting behind an open chain, see [`Behind`].
    /// May overcount, a flush counts again.
    behind: u32,
    /// Store ring pages handed to groups.
    ring_pages: usize,
    /// In [`Queue::quiesce`], descriptors of discarded completions get closed.
    draining: bool,
}

impl Queue {
    /// Start a queue over `store`. Refuses more slots than a key can name.
    pub fn new(store: Owned, setup: Setup) -> Result<Queue, Refused> {
        let n = store.slots.len();
        if n == 0 || n >= NONE as usize || store.sides.len() != n {
            return Err(Refused {
                store,
                error: IoError::TooManySlots(n),
            });
        }
        let engine = match start(&setup, n as u32) {
            Ok(engine) => engine,
            Err(error) => return Err(Refused { store, error }),
        };
        let Owned {
            mut slots,
            sides,
            mut watches,
            rings,
        } = store;
        for (i, s) in slots.iter_mut().enumerate() {
            *s = Slot::new();
            s.next = if i + 1 < n { i as u32 + 1 } else { NONE };
        }
        // Registrations belong to the epoll a store came from, not this queue's.
        watches.fill(Watch::EMPTY);
        Ok(Queue {
            engine,
            sides: Pinned::new(sides),
            rings: Pinned::new(rings),
            slots,
            watches: watches.into_vec(),
            free: 0,
            live: 0,
            retired: 0,
            done: slot::List::EMPTY,
            retry: slot::List::EMPTY,
            timers: 0,
            regions: [None; REGIONS],
            groups: Groups {
                entry: [None; GROUPS],
                ops: [0; GROUPS],
                holds: core::array::from_fn(|_| Holds::new()),
            },
            taken: false,
            link: false,
            tail_linked: false,
            behind: 0,
            ring_pages: 0,
            draining: false,
        })
    }

    pub fn backend(&self) -> Backend {
        match self.engine {
            Engine::Uring(_) => Backend::Uring,
            Engine::Epoll(_) => Backend::Epoll,
        }
    }

    /// [`RingFlags::None`] on `epoll`.
    pub fn ring_flags(&self) -> RingFlags {
        match &self.engine {
            Engine::Uring(r) => r.flags,
            Engine::Epoll(_) => RingFlags::None,
        }
    }

    /// Readable when [`Queue::tick`] has work. On `epoll` timeouts are not behind it,
    /// see [`Queue::next_deadline`].
    ///
    /// On the ring this is the ring fd. Whoever holds a dup of it can post any completion
    /// with `MSG_RING` from raw `io_uring` code, a forged final one included, which would
    /// free a slot the kernel still uses. Safe code of this crate posts only with
    /// [`F_POSTED`], so hand a dup only to code you trust as much as this queue.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.engine {
            Engine::Uring(r) => r.as_fd(),
            Engine::Epoll(p) => p.as_fd(),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.live as usize
    }

    pub fn room(&mut self) -> usize {
        self.slots.len() - self.live as usize - self.retired as usize
    }

    /// Presize the `epoll` watch table for descriptors below `n`, which also lets a queue
    /// made without one wait on descriptors. No-op on the ring.
    pub fn reserve_fds(&mut self, n: usize) {
        if matches!(self.engine, Engine::Epoll(_)) && self.watches.len() < n {
            self.watches.resize(n, Watch::EMPTY);
        }
    }

    /// Queue an operation tagged `user`. On the ring nothing is submitted until
    /// [`Queue::submit`] or [`Queue::tick`]. On `epoll` file ops run here.
    pub fn push(&mut self, op: Op, user: u64) -> Result<Key, IoError> {
        self.push_with(op, user, None, false)
    }

    /// [`Queue::push`] of an entry on the caller's own behalf, a re-arm say, that never
    /// joins a [`Queue::link`] chain and leaves a pending link to the next push. While a
    /// chain is open it waits in its slot and goes out once the chain ends.
    pub fn push_apart(&mut self, op: Op, user: u64) -> Result<Key, IoError> {
        self.push_with(op, user, None, true)
    }

    /// Push a [`Op::SendMsg`] with `control` copied into the slot. At most [`CONTROL_ROOM`] bytes.
    pub fn push_control(&mut self, op: Op, control: &[u8], user: u64) -> Result<Key, IoError> {
        if control.len() > CONTROL_ROOM {
            return Err(IoError::ControlTooLong {
                len: control.len(),
                max: CONTROL_ROOM,
            });
        }
        self.push_with(op, user, Some(control), false)
    }

    /// Close `fd` as an operation. The queue owns it from here and never closes it again,
    /// whatever the completion says, since the kernel may have taken the number out of the
    /// table before a `->flush` error, `ECANCELED` included, and another open may reuse it.
    /// So a close is never cancelled, see [`Queue::cancel`]. One that never ran leaks its
    /// descriptor. That is one still in flight when a drain times out, and on the ring an
    /// `io_uring` fd, which the ring refuses to close. Close those yourself. A close behind
    /// a [`Queue::link`] chain runs once the chain ends, whatever its outcome.
    pub fn close(&mut self, fd: OwnedFd, user: u64) -> Result<Key, CloseRefused> {
        let raw = fd.into_raw_fd();
        self.push_with(Op::Close { fd: Closing(raw) }, user, None, false)
            .map_err(|error| CloseRefused {
                // SAFETY: just taken from an `OwnedFd`, and the op never ran.
                fd: unsafe { OwnedFd::from_raw_fd(raw) },
                error,
            })
    }

    fn push_with(
        &mut self,
        mut op: Op,
        user: u64,
        control: Option<&[u8]>,
        apart: bool,
    ) -> Result<Key, IoError> {
        self.check(&op)?;
        let i = self.free;
        if i == NONE {
            return Err(IoError::OpsFull);
        }
        let side = self.sides.at(i as usize);
        if let (Some(bytes), Op::SendMsg { control: room, .. }) = (control, &mut op) {
            // SAFETY: slot `i` is free, so the kernel holds no pointer into its side.
            let c = unsafe { &mut (*side).control.0 };
            c[..bytes.len()].copy_from_slice(bytes);
            // SAFETY: the slot's own room, untouched while the op is in flight.
            *room = unsafe { Mem::raw(c.as_mut_ptr(), bytes.len()) };
        }
        // A close in a chain is cancelled with it and never runs, so a nop takes its place
        // and the close runs when the nop lands.
        let after = self.tail_linked && matches!(op, Op::Close { .. });
        // Apart from an open chain it waits, so what the ring would refuse is refused now.
        let wait = apart && self.tail_linked;
        if wait {
            // SAFETY: as above, slot `i` is free.
            uring::encode(op, unsafe { &mut *side })?;
        }
        let s = &mut self.slots[i as usize];
        self.free = s.next;
        (s.op, s.user, s.state, s.cancel, s.next, s.wait_next) =
            (op, user, State::Flying, false, NONE, NONE);
        (s.progress, s.after_chain, s.void) = (0, after, false);
        s.behind = if wait { Behind::Op } else { Behind::None };
        let key = Key::new(i, s.generation);
        self.live += 1;
        if let Some(n) = group_of(&op).and_then(|g| self.groups.ops.get_mut(g)) {
            *n += 1;
        }
        let link = !apart && core::mem::take(&mut self.link);
        let mut closed = false;
        let pushed = match &mut self.engine {
            Engine::Uring(_) if wait => {
                self.behind += 1;
                Ok(())
            }
            Engine::Uring(r) => {
                let sent = if after { Op::Nop } else { op };
                // SAFETY: as above, slot `i` was free.
                let pushed = r.push(sent, unsafe { &mut *side }, key, link);
                if pushed.is_ok() && !apart {
                    closed = self.tail_linked && !link;
                    self.tail_linked = link;
                }
                pushed
            }
            Engine::Epoll(_) => {
                self.start_epoll(i);
                Ok(())
            }
        };
        if let Err(e) = pushed {
            self.release(i);
            return Err(e);
        }
        if closed {
            self.flush_behind();
        }
        Ok(key)
    }

    /// A submit or tick ends a chain left open with a nop, so what waits behind it goes out.
    fn close_chain(&mut self) {
        if let Engine::Uring(r) = &mut self.engine
            && self.tail_linked
            && r.end_chain().is_ok()
        {
            self.tail_linked = false;
        }
        self.flush_behind();
    }

    /// Push what waited for a chain to close. What the ring refuses waits for the next
    /// submit or tick.
    fn flush_behind(&mut self) {
        let Engine::Uring(r) = &mut self.engine else {
            return;
        };
        if self.behind == 0 || self.tail_linked {
            return;
        }
        for i in 0..self.slots.len() {
            let s = &mut self.slots[i];
            let key = Key::new(i as u32, s.generation);
            let pushed = match s.behind {
                Behind::None => continue,
                Behind::Op => {
                    let sent = if s.void { Op::Nop } else { s.op };
                    // SAFETY: the slot's own side, which no entry of the kernel names yet.
                    r.push(sent, unsafe { &mut *self.sides.at(i) }, key, false)
                }
                Behind::Cancel => r.cancel(key),
            };
            if pushed.is_err() {
                return;
            }
            s.behind = Behind::None;
        }
        self.behind = 0;
    }

    /// The next push joins a [`Queue::link`] chain, as the last ring entry carries
    /// `IO_LINK`. Such an op may look its descriptors up only when it starts, once the ops
    /// before it ended, so their numbers must stay open until it does. A
    /// [`Queue::push_apart`] never joins.
    pub fn in_chain(&self) -> bool {
        self.tail_linked
    }

    /// Entries pushed but not handed to the kernel yet, which name descriptors by number,
    /// those waiting behind a chain too. Always false on `epoll`.
    pub fn unsubmitted(&mut self) -> bool {
        match &mut self.engine {
            Engine::Uring(r) => r.unsubmitted() || self.behind > 0,
            Engine::Epoll(_) => false,
        }
    }

    /// Link the next push to the one after it. The second fails with `ECANCELED`
    /// if the first fails. Ignored on `epoll`, which runs in push order. A
    /// [`Queue::close`] as the second waits for the first and runs whatever its outcome.
    /// A chain ends at the first push not linked to the next, or at the next
    /// [`Queue::submit`] or [`Queue::tick`], which end one left open with a nop. Cancels
    /// and [`Queue::push_apart`] entries never join it, they wait until it ends.
    pub fn link(&mut self) {
        self.link = matches!(self.engine, Engine::Uring(_));
    }

    /// Completions the kernel dropped. Always zero on `epoll`.
    pub fn overflow(&mut self) -> u32 {
        match &mut self.engine {
            Engine::Uring(r) => r.overflow(),
            Engine::Epoll(_) => 0,
        }
    }

    /// Completions waiting in the kernel for CQ room.
    pub fn backlog(&mut self) -> bool {
        match &mut self.engine {
            Engine::Uring(r) => r.backlog(),
            Engine::Epoll(_) => false,
        }
    }

    /// Provided buffer ring of `entries` (power of two) in the store's ring pages.
    pub fn add_ring(&mut self, entries: u16) -> Result<Group, IoError> {
        let pages = (entries as usize * RING_ENTRY).div_ceil(PAGE);
        if self.ring_pages + pages > self.rings.len() {
            return Err(IoError::BadArena {
                bytes: self.rings.len() * PAGE,
                buffer: entries as usize * RING_ENTRY,
            });
        }
        let at = self.rings.at(self.ring_pages);
        // SAFETY: `pages` pages from `at` lie in the store's ring memory, checked above,
        // and no group took them yet, so neither the kernel nor a reference touches them.
        unsafe { core::ptr::write_bytes(at, 0, pages) };
        // SAFETY: page aligned by `RingPage`'s type, `entries` zeroed records fit the
        // pages above, owned by the queue and freed only after its drop drained it, which
        // leaves nothing in flight to take from the group, or never when the drain timed
        // out. See the `engine` field for why a ring outliving the queue changes nothing.
        let g = unsafe { self.add_group(at.cast::<BufRingEntry>(), entries) }?;
        self.ring_pages += pages;
        Ok(g)
    }

    /// Request cancellation. The op's own completion reports the outcome. False for stale
    /// keys and for a [`Op::Close`], which always runs, see [`Queue::close`]. While a
    /// [`Queue::link`] chain is open the cancel waits for it to end, and an op still
    /// waiting behind it never goes out and ends with `ECANCELED`.
    pub fn cancel(&mut self, key: Key) -> bool {
        let Some(s) = self.slots.get(key.slot() as usize) else {
            return false;
        };
        let armed = matches!(s.state, State::Flying | State::Queued);
        let close = matches!(s.op, Op::Close { .. });
        if s.generation != key.generation() || !armed || s.cancel || close {
            return false;
        }
        let asked = match &mut self.engine {
            Engine::Uring(r) => {
                let s = &mut self.slots[key.slot() as usize];
                match s.behind {
                    // Never went out, a nop goes in its place.
                    Behind::Op => {
                        s.void = true;
                        true
                    }
                    // In the chain it would wait behind it, or fail with it.
                    _ if self.tail_linked => {
                        s.behind = Behind::Cancel;
                        self.behind += 1;
                        true
                    }
                    _ => r.cancel(key).is_ok(),
                }
            }
            Engine::Epoll(_) => {
                // Before, so a queued delivery ends when taken. See `rearm`.
                self.slots[key.slot() as usize].cancel = true;
                self.cancel_epoll(key.slot());
                true
            }
        };
        // Only once asked, so a failed push leaves the key cancellable.
        if asked {
            self.slots[key.slot() as usize].cancel = true;
        }
        asked
    }

    /// Submit without waiting, ending a [`Queue::link`] chain left open. On `epoll` a
    /// zero-timeout poll.
    pub fn submit(&mut self) -> Result<(), IoError> {
        self.close_chain();
        match &mut self.engine {
            Engine::Uring(r) => r.submit(),
            Engine::Epoll(_) => self.tick_epoll(Some(Duration::ZERO)),
        }
    }

    /// Submit and wait for one completion or `timeout`. Signals and timeouts are not errors.
    /// Ends a [`Queue::link`] chain left open.
    pub fn tick(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.close_chain();
        match &mut self.engine {
            Engine::Uring(r) => r.wait(timeout),
            Engine::Epoll(_) => self.tick_epoll(timeout),
        }
    }

    /// Whether a non-waiting tick could find a completion. Conservative on `epoll`.
    pub fn pending(&mut self) -> bool {
        match &mut self.engine {
            Engine::Uring(r) => self.behind > 0 || r.pending(),
            Engine::Epoll(_) => self.live > 0,
        }
    }

    /// A completion can be taken without a tick.
    pub fn ready(&mut self) -> bool {
        match &mut self.engine {
            Engine::Uring(r) => r.ready(),
            Engine::Epoll(_) => !self.done.is_empty(),
        }
    }

    pub fn pop(&mut self) -> Option<Cqe> {
        let mut one = [Cqe::EMPTY];
        (self.reap(&mut one) == 1).then_some(one[0])
    }

    /// Fill `out` with available completions. Returns the count.
    pub fn reap(&mut self, out: &mut [Cqe]) -> usize {
        match &mut self.engine {
            Engine::Uring(r) => {
                let mut at = uring::Reap {
                    slots: &mut self.slots,
                    sides: &self.sides,
                    groups: &mut self.groups,
                    free: &mut self.free,
                    retired: &mut self.retired,
                    draining: self.draining,
                };
                let (n, freed) = r.reap(&mut at, out);
                self.live -= freed;
                n
            }
            Engine::Epoll(_) => self.reap_epoll(out),
        }
    }

    /// Hand over memory for ops to name by range. Registered as fixed if `fixed` and room.
    ///
    /// # Safety
    ///
    /// Valid until the region is unregistered or the queue drops, and past a drop while
    /// an operation naming it is in flight, forever if the drop's drain times out. A
    /// buffer of it offered to [`Queue::provide`] counts as named, as in [`Mem::raw`]. No
    /// conflicting access to a range while an op on it is in flight or it sits in a group.
    pub unsafe fn register(
        &mut self,
        base: *mut u8,
        len: usize,
        fixed: bool,
    ) -> Result<Region, IoError> {
        let Some(at) = self.regions.iter().position(Option::is_none) else {
            return Err(IoError::RegionsFull);
        };
        let fixed = match &mut self.engine {
            Engine::Uring(r) if fixed => r.fix(base, len),
            _ => u16::MAX,
        };
        let tag = (STAMPS.fetch_add(1, Ordering::Relaxed) << 8) | at as u64;
        self.regions[at] = Some(RegionEntry {
            base,
            len,
            fixed,
            tag,
        });
        Ok(Region(at as u16))
    }

    /// Later pushes refuse a [`Mem`] of it. [`IoError::RegionBusy`] names a range of it
    /// this queue may still have the kernel touch, and the region stays. That is one an op
    /// in flight names, or a buffer offered to a group and not handed back by a completion
    /// yet, a removed group's too until its ops ended. Once this returns `Ok` nothing this
    /// queue lent reaches the region, so `register`'s promise ends here. Memory lent some
    /// other way, by [`Mem::raw`] or another queue, stays under that contract.
    pub fn unregister(&mut self, region: Region) -> Result<(), IoError> {
        let Some(entry) = self.regions.get(region.0 as usize).copied().flatten() else {
            return Ok(());
        };
        let base = entry.base as usize;
        let busy = self.busy(base, base.saturating_add(entry.len), true);
        if let Some((at, len)) = busy {
            return Err(IoError::RegionBusy {
                at: at.saturating_sub(base),
                len,
            });
        }
        self.regions[region.0 as usize] = None;
        if let Engine::Uring(r) = &mut self.engine {
            r.unfix(entry.fixed);
        }
        Ok(())
    }

    /// First range overlapping `[lo, hi)` the kernel may touch, as address and length.
    /// Ranges ops in flight write, or name at all with `any`, and every buffer a group
    /// holds, taken by the kernel or not, until a completion hands its bid back, in any
    /// order. A removed group's count until its ops ended.
    fn busy(&self, lo: usize, hi: usize, any: bool) -> Option<(usize, usize)> {
        let hit =
            |p: usize, n: usize| (n > 0 && p < hi && lo < p.saturating_add(n)).then_some((p, n));
        let live = self
            .slots
            .iter()
            .filter(|s| matches!(s.state, State::Flying | State::Queued));
        for s in live {
            let named = if any {
                mems(&s.op)
            } else {
                [written(&s.op), Mem::EMPTY]
            };
            if let Some(r) = named
                .iter()
                .find_map(|m| hit(m.ptr as usize, m.len as usize))
            {
                return Some(r);
            }
        }
        let groups = self.groups.entry.iter().zip(&self.groups.holds);
        groups
            .filter(|(e, _)| e.is_some())
            .find_map(|(_, h)| h.hit(lo, hi))
    }

    /// A region path clear of every range the kernel may write, since the push reads its
    /// end. Raw paths are vouched for by [`Mem::raw`]'s caller.
    fn path_clear(&self, m: &Mem) -> Result<(), IoError> {
        let Some(r) = self.regions.get((m.tag & 0xff) as usize).copied().flatten() else {
            return Ok(());
        };
        if m.tag == 0 || m.len == 0 || r.tag != m.tag {
            return Ok(());
        }
        let lo = m.ptr as usize;
        match self.busy(lo, lo + m.len as usize, false) {
            Some(_) => Err(IoError::RegionBusy {
                at: lo - r.base as usize,
                len: m.len as usize,
            }),
            None => Ok(()),
        }
    }

    pub fn is_fixed(&self, region: Region) -> bool {
        self.regions
            .get(region.0 as usize)
            .copied()
            .flatten()
            .is_some_and(|r| r.fixed != u16::MAX)
    }

    pub fn mem(&self, region: Region, at: usize, len: usize) -> Result<Mem, IoError> {
        let Some(r) = self.regions.get(region.0 as usize).copied().flatten() else {
            return Err(IoError::OutOfRegion { at, len, size: 0 });
        };
        if at.checked_add(len).is_none_or(|end| end > r.len) || len > u32::MAX as usize {
            return Err(IoError::OutOfRegion {
                at,
                len,
                size: r.len,
            });
        }
        Ok(Mem {
            ptr: r.base.wrapping_add(at),
            len: len as u32,
            fixed: r.fixed,
            tag: r.tag,
        })
    }

    /// Provided buffer ring in caller memory. `entries` is a power of two, at most
    /// 32768. Empty until [`Queue::provide`]. Takes the first index no group holds and no
    /// op in flight was pushed against, so an earlier group's completions never count
    /// against this one. [`IoError::RegionsFull`] when there is none.
    ///
    /// # Safety
    ///
    /// `ring` is page aligned, holds `entries` zeroed entries, and is not touched by the
    /// caller. It stays valid until the queue drops, forever if an operation that takes
    /// from it is still in flight then.
    pub unsafe fn add_group(
        &mut self,
        ring: *mut BufRingEntry,
        entries: u16,
    ) -> Result<Group, IoError> {
        if entries == 0
            || !entries.is_power_of_two()
            || entries > 1 << 15
            || ring.align_offset(4096) != 0
        {
            return Err(IoError::BadArena {
                bytes: entries as usize,
                buffer: 0,
            });
        }
        let free = |i: &usize| self.groups.entry[*i].is_none() && self.groups.ops[*i] == 0;
        let Some(at) = (0..GROUPS).find(free) else {
            return Err(IoError::RegionsFull);
        };
        if let Engine::Uring(r) = &mut self.engine {
            r.add_group(ring, entries, at as u16)?;
        }
        self.groups.entry[at] = Some(GroupEntry {
            ring,
            entries,
            tail: 0,
            head: 0,
            removed: false,
        });
        self.groups.holds[at].reset(entries);
        Ok(Group(at as u16))
    }

    /// Drop a group. Ops in flight that take from it end with `ENOBUFS`, and buffers
    /// they already took stay the caller's to keep valid until their completions. Its
    /// buffers count as held, see [`Queue::unregister`], and its index stays taken until
    /// those ops ended.
    pub fn remove_group(&mut self, group: Group) -> Result<(), IoError> {
        let g = group.0 as usize;
        let Some(entry) = self.groups.entry.get_mut(g).and_then(Option::as_mut) else {
            return Err(IoError::NoBufRing);
        };
        if entry.removed {
            return Err(IoError::NoBufRing);
        }
        entry.removed = true;
        if self.groups.ops[g] == 0 {
            self.groups.drop_index(g);
        }
        match &mut self.engine {
            Engine::Uring(r) => r.remove_group(group.0),
            Engine::Epoll(_) => Ok(()),
        }
    }

    /// Offer `mem` to a group as buffer `bid`. Visible at once. [`IoError::BuffersFull`]
    /// while every entry holds a buffer no completion has handed back yet, and
    /// [`IoError::Loaned`] while `bid` itself is held, so each held bid names one buffer.
    pub fn provide(&mut self, group: Group, bid: u16, mem: Mem) -> Result<(), IoError> {
        self.vouched(&mem)?;
        let i = group.0 as usize;
        let Some(g) = self.groups.entry.get_mut(i).and_then(Option::as_mut) else {
            return Err(IoError::NoBufRing);
        };
        let holds = &mut self.groups.holds[i];
        if g.removed {
            return Err(IoError::NoBufRing);
        }
        if holds.len() >= g.entries as usize {
            return Err(IoError::BuffersFull);
        }
        if !holds.insert(bid, mem.ptr as usize, mem.len) {
            return Err(IoError::Loaned(bid));
        }
        let base = g.ring.cast::<RawBuf>();
        let e = base.wrapping_add((g.tail & (g.entries - 1)) as usize);
        // SAFETY: `e` is masked into the ring of `entries` records. Every entry the kernel
        // has not taken yet is still held, as only a completion of this group's own ops
        // hands a bid back, `add_group` reusing no index an earlier group's op is still in
        // flight on. Fewer than `entries` were held, so the kernel reads none past the tail
        // and this one is free. Fields are written through the raw pointer, so nothing
        // covers the first entry's tail.
        unsafe {
            (&raw mut (*e).addr).write(mem.ptr as u64);
            (&raw mut (*e).len).write(mem.len);
            (&raw mut (*e).bid).write(bid);
        }
        g.tail = g.tail.wrapping_add(1);
        // Release, so the entry is visible before the tail.
        // SAFETY: the tail is the first record's `resv`, an aligned u16 inside the ring that
        // the kernel only reads, atomically, and nothing else writes.
        unsafe { core::sync::atomic::AtomicU16::from_ptr(&raw mut (*base).resv) }
            .store(g.tail, Ordering::Release);
        Ok(())
    }

    /// Register an empty file table of `n` for [`Fd::Fixed`]. Ring only.
    pub fn register_files(&mut self, n: u32) -> Result<(), IoError> {
        match &mut self.engine {
            Engine::Uring(r) => r.register_files(n),
            Engine::Epoll(_) => Err(IoError::Unsupported),
        }
    }

    /// Set or clear (`-1`) a file table entry. The table holds its own reference.
    pub fn set_file(&mut self, index: u32, fd: RawFd) -> Result<(), IoError> {
        match &mut self.engine {
            Engine::Uring(r) => r.set_file(index, fd),
            Engine::Epoll(_) => Err(IoError::Unsupported),
        }
    }

    /// Earliest `epoll` timeout, for callers waiting on [`Queue::as_fd`]. `None` on the ring.
    pub fn next_deadline(&mut self) -> Option<std::time::Instant> {
        match self.engine {
            Engine::Uring(_) => None,
            Engine::Epoll(_) => self.earliest(),
        }
    }

    /// Cancel everything but closes, which always run, and wait up to `limit`, discarding
    /// completions. Descriptors that discarded accepts and opens return are closed. True
    /// when nothing is left.
    pub fn quiesce(&mut self, limit: Duration) -> bool {
        if self.live == 0 {
            return true;
        }
        self.cancel_all();
        self.draining = true;
        let start = std::time::Instant::now();
        let mut sink = [Cqe::EMPTY; 32];
        while self.live > 0 && start.elapsed() < limit {
            if self.tick(Some(Duration::from_millis(5))).is_err() {
                break;
            }
            while self.reap(&mut sink) > 0 {}
        }
        self.draining = false;
        self.live == 0
    }

    /// Cancel, drain and return the store. `None` if draining timed out, and then
    /// the store leaks since the kernel may still write it.
    pub fn into_store(mut self) -> Option<Owned> {
        if !self.quiesce(DRAIN) {
            self.abandon();
            return None;
        }
        self.taken = true;
        let sides = core::mem::replace(&mut self.sides, Pinned::new(Box::new([])));
        let rings = core::mem::replace(&mut self.rings, Pinned::new(Box::new([])));
        Some(Owned {
            slots: core::mem::take(&mut self.slots),
            sides: sides.into_box()?,
            watches: core::mem::take(&mut self.watches).into_boxed_slice(),
            rings: rings.into_box()?,
        })
    }

    /// Leak the kernel memory and skip the drain on drop. For a drain that timed out.
    fn abandon(&mut self) {
        self.sides.leak();
        self.rings.leak();
        self.taken = true;
    }

    /// For a driver whose own drain timed out, so the drop does not wait again.
    #[cfg(feature = "driver")]
    pub(crate) fn give_up(&mut self) {
        self.abandon();
    }

    fn cancel_all(&mut self) {
        for i in 0..self.slots.len() as u32 {
            let s = &self.slots[i as usize];
            if matches!(s.state, State::Flying | State::Queued) {
                let key = Key::new(i, s.generation);
                self.cancel(key);
            }
        }
        let _ = self.submit();
    }

    /// Free a slot and bump its generation.
    fn release(&mut self, i: u32) {
        self.groups.ended(&self.slots[i as usize].op);
        free_slot(&mut self.slots, &mut self.free, &mut self.retired, i);
        self.live -= 1;
    }

    /// A `Mem` of a region must name one this queue still has, by its stamp.
    fn vouched(&self, m: &Mem) -> Result<(), IoError> {
        if m.tag == 0 {
            return Ok(());
        }
        let at = (m.tag & 0xff) as usize;
        match self.regions.get(at).copied().flatten() {
            Some(r) if r.tag == m.tag => Ok(()),
            _ => Err(IoError::StaleMem),
        }
    }

    /// Backend-independent validation before an op takes a slot.
    fn check(&self, op: &Op) -> Result<(), IoError> {
        for m in mems(op) {
            self.vouched(&m)?;
        }
        for m in paths(op) {
            self.path_clear(&m)?;
        }
        check_shape(op)
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        if self.taken {
            return;
        }
        if !self.quiesce(DRAIN) {
            // The kernel may still write it.
            self.abandon();
        }
    }
}

const DRAIN: Duration = Duration::from_secs(2);

fn start(setup: &Setup, slots: u32) -> Result<Engine, IoError> {
    match setup.want {
        Want::Epoll => Ok(Engine::Epoll(epoll::Poll::new()?)),
        Want::Uring => Ok(Engine::Uring(uring::Ring::new(setup, slots)?)),
        Want::Any => match uring::Ring::new(setup, slots) {
            Ok(r) => Ok(Engine::Uring(r)),
            Err(_) => Ok(Engine::Epoll(epoll::Poll::new()?)),
        },
    }
}

/// The memory an op names, [`Mem::EMPTY`] for none.
fn mems(op: &Op) -> [Mem; 2] {
    let e = Mem::EMPTY;
    match *op {
        Op::Read { mem, .. }
        | Op::Write { mem, .. }
        | Op::Recv { mem, .. }
        | Op::Send { mem, .. }
        | Op::SendTo { mem, .. }
        | Op::RecvFrom { mem, .. }
        | Op::RecvAll { mem, .. } => [mem, e],
        Op::SendMsg { mem, control, .. } => [mem, control],
        Op::Open { path, .. } | Op::Unlink { path, .. } => [path, e],
        Op::Statx { path, out, .. } => [path, out],
        Op::Rename { from, to, .. } => [from, to],
        Op::SetSockOpt { value, .. } => [value, e],
        Op::FutexWait { word, .. } => [word, e],
        _ => [e, e],
    }
}

/// The memory an op writes, [`Mem::EMPTY`] for none.
fn written(op: &Op) -> Mem {
    match *op {
        Op::Read { mem, .. }
        | Op::Recv { mem, .. }
        | Op::RecvFrom { mem, .. }
        | Op::RecvAll { mem, .. } => mem,
        Op::Statx { out, .. } => out,
        _ => Mem::EMPTY,
    }
}

/// The paths an op names, [`Mem::EMPTY`] for none.
fn paths(op: &Op) -> [Mem; 2] {
    let e = Mem::EMPTY;
    match *op {
        Op::Open { path, .. } | Op::Unlink { path, .. } | Op::Statx { path, .. } => [path, e],
        Op::Rename { from, to, .. } => [from, to],
        _ => [e, e],
    }
}

/// Buffer `at` of a provided ring, read field by field.
fn ring_entry(ring: *mut RawBuf, at: u16) -> (Mem, u16) {
    let e = ring.wrapping_add(at as usize);
    // SAFETY: `at` is masked into the ring of `entries` records, sized by `add_group`'s
    // contract or the store's pages. The kernel only reads entries, and the queue writes
    // them only in `provide`, which `&mut self` keeps apart from every reader.
    let (addr, len, bid) = unsafe {
        (
            (&raw const (*e).addr).read(),
            (&raw const (*e).len).read(),
            (&raw const (*e).bid).read(),
        )
    };
    // SAFETY: vouched for by whoever provided it. A region's buffer keeps its region
    // registered while in the ring, as `unregister` refuses, and a raw one is held by
    // `Mem::raw`'s contract until a completion hands it back.
    (unsafe { Mem::raw(addr as *mut u8, len as usize) }, bid)
}

/// Paths end in NUL, stat and futex memory is large and aligned enough, and no read or
/// write is longer than [`MAX_RW_COUNT`].
fn check_shape(op: &Op) -> Result<(), IoError> {
    let path = |m: &Mem| -> Result<(), IoError> {
        if m.len == 0 {
            return Ok(());
        }
        // SAFETY: in bounds, and vouched for by `Mem::raw`'s caller, which includes no op
        // in flight writing it. A region path `vouched` matched and `path_clear` found
        // clear of every range the kernel may write, so this read races no write.
        let last = unsafe { m.ptr.add(m.len as usize - 1).read() };
        if last != 0 {
            return Err(IoError::PathTooLong {
                len: m.len as usize,
                max: m.len as usize,
            });
        }
        Ok(())
    };
    match op {
        Op::Open { path: p, .. } | Op::Unlink { path: p, .. } => path(p),
        Op::Statx { path: p, out, .. } if out.len() >= STATX_LEN => path(p),
        Op::Statx { out, .. } => Err(IoError::OutOfRegion {
            at: 0,
            len: STATX_LEN,
            size: out.len(),
        }),
        Op::Rename { from, to, .. } => path(from).and(path(to)),
        Op::FutexWait { word, .. } if word.len() < 4 || word.ptr.align_offset(4) != 0 => {
            Err(IoError::OutOfRegion {
                at: 0,
                len: 4,
                size: word.len(),
            })
        }
        Op::Read { mem, .. }
        | Op::Write { mem, .. }
        | Op::Recv { mem, .. }
        | Op::RecvAll { mem, .. }
            if mem.len > MAX_RW_COUNT =>
        {
            Err(IoError::TooLong {
                len: mem.len(),
                max: MAX_RW_COUNT as usize,
            })
        }
        _ => Ok(()),
    }
}

/// Group index an op takes provided buffers from.
fn group_of(op: &Op) -> Option<usize> {
    match op {
        Op::RecvMulti { group, .. } | Op::RecvMsgMulti { group, .. } => Some(group.0 as usize),
        _ => None,
    }
}

/// Provided buffer groups, one column entry per index.
struct Groups {
    entry: [Option<GroupEntry>; GROUPS],
    /// Ops in flight pushed against each index. An index is reused only at zero, so no
    /// completion of an earlier group hands back a later one's buffer.
    ops: [u32; GROUPS],
    /// Buffers each index lent the kernel, by bid, until a completion hands them back.
    holds: [Holds; GROUPS],
}

impl Groups {
    /// An op that took from a group ended. A removed group goes with its last op.
    fn ended(&mut self, op: &Op) {
        let Some(g) = group_of(op) else { return };
        let Some(n) = self.ops.get_mut(g) else { return };
        *n = n.saturating_sub(1);
        let removed = self.entry[g].is_some_and(|e| e.removed);
        if *n == 0 && removed {
            self.drop_index(g);
        }
    }

    /// A completion of `op` handed buffer `bid` back.
    fn handed_back(&mut self, op: &Op, bid: u16) {
        if let Some(h) = group_of(op).and_then(|g| self.holds.get_mut(g)) {
            h.remove(bid);
        }
    }

    /// Nothing can take from index `g` any more, so nothing it lent is held.
    fn drop_index(&mut self, g: usize) {
        self.entry[g] = None;
        self.holds[g].clear();
    }
}

/// Free slot `i` and bump its generation. One whose generation wraps retires instead, as a
/// reuse would let a key from 2^32 uses ago name the new op.
fn free_slot(slots: &mut [Slot], free: &mut u32, retired: &mut u32, i: u32) {
    let s = &mut slots[i as usize];
    (s.state, s.op, s.behind) = (State::Free, Op::Nop, Behind::None);
    s.generation = s.generation.wrapping_add(1);
    if s.generation == 0 {
        s.next = NONE;
        *retired += 1;
        return;
    }
    s.next = *free;
    *free = i;
}

/// Close the fd of a close that stood behind a chain as a nop, which ran or was cancelled
/// with the chain, so no close of it ran.
fn close_after(op: &Op) {
    if let Op::Close { fd } = op {
        // SAFETY: the queue owns it since `Queue::close` and never closed it.
        drop(unsafe { OwnedFd::from_raw_fd(fd.raw()) });
    }
}

/// Close a descriptor an accept or open returned that nobody will see.
fn orphan(op: &Op, res: i32) {
    if res >= 0 && matches!(op, Op::Accept { .. } | Op::Open { .. }) {
        // SAFETY: fresh from the kernel and its completion is discarded, so nothing
        // else knows the number.
        drop(unsafe { OwnedFd::from_raw_fd(res) });
    }
}

/// One provided buffer ring page, 256 entries.
#[repr(C, align(4096))]
pub struct RingPage(pub [u8; PAGE]);

impl RingPage {
    pub const ZERO: RingPage = RingPage([0; PAGE]);
}

const PAGE: usize = 4096;
/// `struct io_uring_buf` size.
const RING_ENTRY: usize = 16;

pub const fn ring_pages(entries: usize) -> usize {
    (entries * RING_ENTRY).div_ceil(PAGE)
}

/// Room a [`Op::Statx`] needs, including the kernel's reserve.
pub const STATX_LEN: usize = 256;

/// The `struct statx` a [`Op::Statx`] wrote, if `bytes` holds one.
pub fn statx_of(bytes: &[u8]) -> Option<rustix::fs::Statx> {
    if bytes.len() < size_of::<rustix::fs::Statx>() {
        return None;
    }
    // SAFETY: length checked, plain integers, unaligned read.
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<rustix::fs::Statx>()) })
}

/// Address a [`Op::RecvFrom`] wrote, if an internet one.
pub fn addr_of(bytes: &[u8]) -> Option<SocketAddr> {
    let family = u16::from_ne_bytes(bytes.get(0..2)?.try_into().ok()?);
    let port = u16::from_be_bytes(bytes.get(2..4)?.try_into().ok()?);
    match family as i32 {
        libc::AF_INET => {
            let ip: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
            Some(SocketAddr::from((ip, port)))
        }
        libc::AF_INET6 => {
            let flow = u32::from_ne_bytes(bytes.get(4..8)?.try_into().ok()?);
            let ip: [u8; 16] = bytes.get(8..24)?.try_into().ok()?;
            let scope = u32::from_ne_bytes(bytes.get(24..28)?.try_into().ok()?);
            Some(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip.into(),
                port,
                flow,
                scope,
            )))
        }
        _ => None,
    }
}

/// Inline control room for [`Queue::push_control`].
pub const CONTROL_ROOM: usize = 64;

pub const ADDR_LEN: usize = size_of::<rustix::net::addr::SocketAddrStorage>();

#[cfg(test)]
mod tests;
