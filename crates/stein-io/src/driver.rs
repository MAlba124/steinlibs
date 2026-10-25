//! The driver. Buffers, operation table and owned descriptors over [`crate::queue`].

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use rustix::io::Errno;

use crate::buf::{Buf, BufId, Bufs, Own};
use crate::error::IoError;
use crate::net;
use crate::op::{Completion, Kind, Op, OpId};
use crate::pinned::Pinned;
use crate::queue::{self, Cqe, Fd, Mem, Queue, Setup, Want};
pub use crate::queue::{Backend, CWD, RingFlags};
pub use mail::Waker;
use table::Table;

/// Memory a [`Io::send_shared`] reads from. The driver holds a reference until the send's
/// last completion, so the caller may drop its own at once.
pub type Shared = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// What the driver allocates once at start. It owns all of it, the arena included, so
/// leaking a driver leaks memory the kernel may still use rather than freeing it.
pub struct Storage {
    /// Operations in flight at once.
    pub ops: usize,
    /// Buffers in the arena, each of the size [`Io::new`] takes.
    pub bufs: usize,
    /// See [`Storage::queue_for`].
    pub queue: queue::Owned,
}

impl Storage {
    /// `ops` operations over `bufs` buffers, with [`Storage::queue_for`].
    pub fn new(ops: usize, bufs: usize) -> Storage {
        Storage {
            ops,
            bufs,
            queue: Storage::queue_for(ops, bufs),
        }
    }

    /// Queue storage for `ops` operations over `bufs` buffers.
    /// Two slots per op since a cancelled one keeps its entry until the kernel lets go.
    pub fn queue_for(ops: usize, bufs: usize) -> queue::Owned {
        let pages = queue::ring_pages(ring_size(bufs) as usize);
        queue::Owned::with_rings(2 * ops + SPARE_SLOTS, FD_ROOM, pages)
    }
}

/// Memory a [`Kind::SendShared`] reads, as it was when the send started.
struct Held {
    /// Keeps `ptr` alive.
    data: Shared,
    ptr: *const u8,
    len: usize,
}

/// A region range an operation in flight names, keyed by its user data.
#[derive(Clone, Copy, Debug)]
struct Span {
    user: u64,
    region: u16,
    at: usize,
    end: usize,
    /// The kernel writes it, a read into the region.
    writes: bool,
}

pub struct Io {
    /// Drained by the drop below before any field goes. The ring's kernel context may
    /// outlive the queue through a dup of its fd, [`Io::mailbox`] hands one out, but once
    /// the drain succeeds nothing is in flight and safe code cannot submit to an orphaned
    /// ring, so nothing below is touched by the kernel again. A drain that times out leaks.
    q: Queue,
    ops: Table,
    bufs: Bufs<'static>,
    /// Provided buffer ring over the first buffers of the arena.
    group: Option<queue::Group>,
    /// Unclaimed buffers in the ring.
    ring_free: u16,
    /// Closes go out as batch entries, see [`Io::close`].
    ring_close: bool,
    /// Adopted mailboxes. A post goes only to one of these, and a ring one, which a ring
    /// close refuses, closes here.
    peers: Vec<Peer>,
    /// Buffers the kernel may still write after a cancel, keyed by user data.
    /// Kind kept so an open or accept that completed anyway gets its fd closed.
    quarantine: Vec<(u64, Option<u16>, Kind)>,
    /// Receives that could not arm because the arena was empty.
    rearm: Vec<OpId>,
    /// Owned descriptors indexed by number.
    owned: Vec<Option<OwnedFd>>,
    done: VecDeque<Completion>,
    /// Live ops per descriptor, lets close skip the pool walk.
    live_on: Vec<u32>,
    /// Finished ops still holding their slot, see [`Op::done`].
    retired: usize,
    /// Memory held for the driver's life, by [`Region`].
    regions: Vec<Pinned<u8>>,
    /// Region ranges in flight, so the caller cannot touch them.
    spans: Vec<Span>,
    /// Epoll mailbox pipe. Read end is polled, write end is duped for senders.
    mail: Option<(OwnedFd, OwnedFd)>,
    /// Reentrancy guard for [`Io::harvest`].
    harvesting: bool,
    /// Queue key per slot for cancel, [`NO_KEY`] if none. Separate column keeps `Op` four words.
    keys: Vec<u64>,
    /// Memory a [`Kind::SendShared`] reads, per slot.
    held: Vec<Option<Held>>,
    /// Held memory the kernel may still read after a cancel, keyed by user data.
    held_quarantine: Vec<(u64, Shared)>,
    /// The eventfd [`Waker`]s write, polled under [`WAKE`].
    wake: Option<OwnedFd>,
    /// How long the drop waits for the queue to drain.
    drain: Duration,
    /// Harvest scratch, kept so a harvest writes no fresh array.
    scratch: Box<[Cqe]>,
    /// Descriptors that ops which joined a chain name, by user data, until their final
    /// completion, cancelled or not. See [`Queue::in_chain`].
    chain_fds: Vec<(u64, RawFd)>,
    /// Closes waiting until no op in `chain_fds` names their descriptor.
    parked: Vec<(RawFd, OwnedFd)>,
}

impl Io {
    /// Prefers `io_uring`, falls back to `epoll`.
    /// Decided at runtime since seccomp and containers often disable `io_uring`.
    pub fn new(store: Storage, buf_size: u32) -> Result<Io, IoError> {
        Io::start(
            store,
            buf_size,
            Setup {
                want: Want::Any,
                ..Setup::default()
            },
        )
    }

    /// Start on a named backend, no fallback.
    pub fn with_backend(store: Storage, buf_size: u32, want: Backend) -> Result<Io, IoError> {
        let want = match want {
            Backend::Epoll => Want::Epoll,
            Backend::Uring => Want::Uring,
        };
        Io::start(
            store,
            buf_size,
            Setup {
                want,
                ..Setup::default()
            },
        )
    }

    /// Driver created and driven by one thread. Tries `SINGLE_ISSUER` + `DEFER_TASKRUN`,
    /// degrading per kernel down to `epoll`. See [`Io::ring_flags`].
    /// Every submit and tick must come from the creating thread.
    /// On a ring, [`Io::close`] becomes a batch entry.
    pub fn single_issuer(store: Storage, buf_size: u32) -> Result<Io, IoError> {
        let mut io = Io::start(
            store,
            buf_size,
            Setup {
                want: Want::Any,
                single_issuer: true,
                fixed_buffers: 0,
            },
        )?;
        io.ring_close = io.q.backend() == Backend::Uring;
        Ok(io)
    }

    pub fn ring_flags(&self) -> RingFlags {
        self.q.ring_flags()
    }

    // Tables sized once here, no op grows them.
    fn start(store: Storage, buf_size: u32, setup: Setup) -> Result<Io, IoError> {
        if store.ops >= u32::MAX as usize {
            return Err(IoError::TooManySlots(store.ops));
        }
        let bufs = Bufs::owned(store.bufs, buf_size)?;
        let ops = Table::new(store.ops);
        let mut q = Queue::new(store.queue, setup).map_err(|r| r.error)?;
        let cap = ops.capacity();
        // Cancelled ops keep their queue slot until the kernel lets go, so up to one each.
        let slots = q.room();
        let mut io = Io {
            q,
            ops,
            bufs,
            group: None,
            ring_free: 0,
            ring_close: false,
            peers: Vec::new(),
            quarantine: Vec::with_capacity(slots),
            rearm: Vec::with_capacity(cap),
            owned: Vec::with_capacity(FD_ROOM),
            done: VecDeque::with_capacity(cap * 2),
            live_on: vec![0; FD_ROOM],
            retired: 0,
            regions: Vec::with_capacity(REGIONS),
            spans: Vec::with_capacity(slots),
            mail: None,
            harvesting: false,
            keys: vec![NO_KEY; cap],
            held: (0..cap).map(|_| None).collect(),
            held_quarantine: Vec::with_capacity(cap),
            wake: None,
            drain: DRAIN,
            scratch: vec![Cqe::EMPTY; HARVEST].into_boxed_slice(),
            chain_fds: Vec::with_capacity(cap * 2),
            parked: Vec::with_capacity(cap),
        };
        io.start_ring();
        Ok(io)
    }

    /// Best-effort provided buffer ring for receives. Needs kernel 5.19.
    fn start_ring(&mut self) {
        let entries = ring_size(self.bufs.count());
        if entries == 0 || self.bufs.reserve(entries).is_err() {
            return;
        }
        let Ok(g) = self.q.add_ring(entries) else {
            // Unsupported or no room. Buffers go back to the free list.
            let _ = self.bufs.reserve(0);
            return;
        };
        for i in 0..entries {
            let m = self.buffer(i, 0, u32::MAX);
            let _ = self.q.provide(g, i, m);
        }
        self.group = Some(g);
        self.ring_free = entries;
    }

    pub fn backend(&self) -> Backend {
        self.q.backend()
    }

    /// Readable exactly when [`Io::tick`] has work. For embedding in another event loop.
    /// On a ring it is the ring fd, so do not hand a dup of it to untrusted code, see
    /// [`Io::mailbox`].
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.q.as_fd()
    }

    pub fn bufs(&self) -> &Bufs<'static> {
        &self.bufs
    }

    /// Take a buffer to fill and hand to [`Io::send`], [`Io::send_to`] or [`Io::write`],
    /// or to receive into with [`Io::recv_into`].
    pub fn alloc(&mut self) -> Result<BufId, IoError> {
        self.bufs.take().map_err(|_| IoError::BuffersFull)
    }

    /// Empty unless `id` is of this driver, from [`Io::alloc`] or a completion.
    pub fn buf_mut(&mut self, id: &BufId) -> &mut [u8] {
        self.bufs.bytes_mut(id)
    }

    /// Empty unless `b` is of this driver.
    pub fn filled(&self, b: &Buf) -> &[u8] {
        self.bufs.filled(b)
    }

    /// Copy a completion's bytes into `dst` at `at`, truncated to fit. Zero unless both
    /// are of this driver.
    pub fn copy_into(&mut self, dst: &BufId, at: usize, src: &Buf) -> usize {
        self.bufs.copy_into(dst, at, src)
    }

    /// Copy a range between buffers, or within one. Zero unless both are of this driver.
    pub fn copy_range(
        &mut self,
        dst: &BufId,
        dst_at: usize,
        src: &BufId,
        src_at: usize,
        len: usize,
    ) -> usize {
        self.bufs.copy_range(dst, dst_at, src, src_at, len)
    }

    /// Return a buffer, a [`Buf`] or a [`BufId`]. Must be called or the arena runs dry.
    /// False for one of another driver, which stays lost to its own.
    pub fn release(&mut self, b: impl Into<BufId>) -> bool {
        let id = b.into();
        let (i, ring) = (id.index(), self.bufs.in_ring(&id));
        if !self.bufs.release(id) {
            return false;
        }
        if ring {
            self.give_back(i);
        }
        true
    }

    /// Offer ring buffer `i` to the kernel again.
    fn give_back(&mut self, i: u16) {
        let Some(g) = self.group else {
            return;
        };
        let m = self.buffer(i, 0, u32::MAX);
        if self.q.provide(g, i, m).is_ok() {
            self.ring_free = self.ring_free.saturating_add(1);
        }
    }

    /// Provided ring buffers, zero if receives pick a buffer at submission.
    pub fn provided_buffers(&self) -> usize {
        self.bufs.reserved() as usize
    }

    /// Buffers not held by the caller, free list plus ring.
    pub fn available(&self) -> usize {
        self.bufs.available() + self.ring_free as usize
    }

    /// Excludes ops whose completion is queued but not yet taken.
    pub fn in_flight(&self) -> usize {
        self.ops.len() - self.retired
    }

    /// False for completed, cancelled, or unknown ops.
    pub fn is_live(&self, id: OpId) -> bool {
        self.ops.get(id).is_some_and(|o| !o.done)
    }

    pub fn op_fd(&self, id: OpId) -> Option<RawFd> {
        self.ops.get(id).map(|o| o.fd).filter(|&fd| fd >= 0)
    }

    pub fn timeout(&mut self, after: Duration) -> Result<OpId, IoError> {
        let id = self.insert(Op::new(Kind::Timeout, -1))?;
        self.submit(id, queue::Op::Timeout { after })?;
        Ok(id)
    }

    /// Adopt an externally opened descriptor. Returns its number.
    pub fn own(&mut self, fd: OwnedFd) -> RawFd {
        let raw = fd.as_raw_fd();
        self.keep(raw, fd);
        raw
    }

    /// Presize per-descriptor tables for `n` descriptors, the readiness watch table too.
    pub fn reserve_fds(&mut self, n: usize) {
        self.q.reserve_fds(n);
        if self.owned.len() < n {
            self.owned.resize_with(n, || None);
        }
        self.grow_live_on(n);
    }

    /// Borrow an owned descriptor for synchronous calls like [`crate::dir::list`].
    pub fn fd(&self, fd: RawFd) -> Option<BorrowedFd<'_>> {
        borrow(&self.owned, fd)
    }

    pub fn local_addr(&self, fd: RawFd) -> Option<SocketAddr> {
        net::local_addr(borrow(&self.owned, fd)?).ok()
    }

    /// Frees the slot now, late events hit a stale handle and drop.
    /// A buffer the kernel may still write is quarantined until its event lands.
    pub fn cancel(&mut self, id: OpId) -> bool {
        // Before the slot goes, a send's memory stays until its event lands.
        let held = self.take_held(id);
        let Some(op) = self.take_op(id) else {
            return false;
        };
        if op.done {
            // Already finished, its completion stays queued.
            self.retired -= 1;
            return true;
        }
        if let Some(h) = held {
            self.held_quarantine.push((id.to_user_data(), h));
        }
        self.rearm.retain(|r| *r != id);
        if op.aux == STAT_WAITS && self.cancel_waiting(op) {
            return true;
        }
        if op.buf.is_some() || gives_fd(op.kind) {
            self.quarantine.push((id.to_user_data(), op.buf, op.kind));
        }
        if let Some(key) = self.key_of(id) {
            self.q.cancel(key);
        }
        true
    }

    fn key_of(&self, id: OpId) -> Option<queue::Key> {
        self.keys
            .get(id.slot() as usize)
            .copied()
            .filter(|k| *k != NO_KEY)
            .map(queue::Key)
    }

    fn set_key(&mut self, id: OpId, key: u64) {
        if let Some(k) = self.keys.get_mut(id.slot() as usize) {
            *k = key;
        }
    }

    /// Cancels all ops on `fd` first since the number gets reused.
    /// On a ring the close goes in the next batch, silently, and is never cancelled, always
    /// on a [`Io::single_issuer`] one and else while pushed entries wait to be submitted, so
    /// those find the file and not the number reused. One behind an [`Io::chain`] runs once
    /// the chain ends, failed or not. While an op that joined a chain names `fd`, cancelled
    /// or not, the close waits for its last completion, as it may look the number up only
    /// when it starts. A ring [`Peer`] closes here at once, as the ring refuses to close an
    /// `io_uring` descriptor. So would one handed to [`Io::own`], which then leaks, so close
    /// those yourself.
    pub fn close(&mut self, fd: RawFd) -> bool {
        self.cancel_ops_on(fd);
        // Readiness backend watches by number, which gets reused.
        self.q.forget(fd);
        let Some(file) = self.owned.get_mut(fd as usize).and_then(Option::take) else {
            return false;
        };
        let peer = self.peers.iter().position(|p| p.fd() == fd);
        let ring_peer = peer.is_some_and(|i| self.peers[i].is_ring());
        if let Some(i) = peer {
            self.peers.swap_remove(i);
        }
        if ring_peer {
            // Entries naming it reach the kernel first.
            if self.q.unsubmitted() {
                let _ = self.q.submit();
            }
            drop(file);
            return true;
        }
        if self.chain_fds.iter().any(|(_, f)| *f == fd) {
            self.parked.push((fd, file));
            return true;
        }
        self.close_now(fd, file);
        true
    }

    /// Close where no pushed entry can find the number reused.
    fn close_now(&mut self, fd: RawFd, file: OwnedFd) {
        if self.ring_close || self.q.unsubmitted() {
            return self.close_in_batch(fd, file);
        }
        drop(file);
    }

    /// Note what a chained op names, see `chain_fds`.
    fn note_chained(&mut self, id: OpId) {
        let Some(op) = self.ops.get(id).copied() else {
            return;
        };
        let user = id.to_user_data();
        let aux = if op.kind == Kind::Splice {
            op.aux as RawFd
        } else {
            -1
        };
        for fd in [op.fd, aux] {
            if fd >= 0 {
                self.chain_fds.push((user, fd));
            }
        }
    }

    /// An op ended, so the closes only it held up run.
    fn unchain(&mut self, user: u64) {
        let before = self.chain_fds.len();
        self.chain_fds.retain(|(u, _)| *u != user);
        if self.chain_fds.len() == before {
            return;
        }
        let mut i = 0;
        while i < self.parked.len() {
            let fd = self.parked[i].0;
            if self.chain_fds.iter().any(|(_, f)| *f == fd) {
                i += 1;
                continue;
            }
            let (fd, file) = self.parked.swap_remove(i);
            self.close_now(fd, file);
        }
    }

    /// Cursor walk since cancel removes from the pool being walked. An fd past `live_on`
    /// has uncounted ops, so it is walked too.
    fn cancel_ops_on(&mut self, fd: RawFd) {
        if fd < 0 || self.live_on.get(fd as usize).is_some_and(|n| *n == 0) {
            return;
        }
        let mut slot = 0;
        while let Some(id) = self.ops.next_live(slot) {
            slot = id.slot() + 1;
            if self.ops.get(id).is_some_and(|op| op.fd == fd && !op.done) {
                self.cancel(id);
            }
        }
    }

    /// Lend a caller's buffer to an op, refused for one of another driver.
    fn lend(&mut self, buf: BufId) -> Result<u16, IoError> {
        let i = buf.index();
        self.bufs.lend(buf).ok_or(IoError::NotHeld(i))
    }

    /// [`Io::insert`] of an op on a caller's buffer, lent here. Freed if no slot, as the
    /// caller handed it over. Returns the op and the buffer's index.
    fn insert_on(&mut self, op: Op, buf: BufId) -> Result<(OpId, u16), IoError> {
        let i = self.lend(buf)?;
        let id = self
            .insert(op.with_buf(i))
            .inspect_err(|_| self.free_buf(i))?;
        Ok((id, i))
    }

    /// `buf` back if one buffer holds `len` bytes at `at`. Else it is freed as for a
    /// failed call and the error is [`IoError::TooLong`] with the end as its length.
    fn fits(&mut self, buf: BufId, at: u32, len: u32) -> Result<BufId, IoError> {
        let end = at as usize + len as usize;
        let max = self.bufs.size() as usize;
        if end <= max {
            return Ok(buf);
        }
        self.drop_handed(buf);
        Err(IoError::TooLong { len: end, max })
    }

    /// Free a caller's buffer handed to a call that failed before lending it. One of
    /// another driver stays where it is.
    fn drop_handed(&mut self, buf: BufId) {
        if self.bufs.owner(&buf) == Some(Own::Caller) {
            self.free_buf(buf.index());
        }
    }

    /// Submit queued work without waiting.
    /// Required before waiting on [`Io::as_fd`] yourself, or the wait hangs on unsubmitted work.
    pub fn flush(&mut self) -> Result<(), IoError> {
        self.q.submit()
    }

    pub fn next_completion(&mut self) -> Option<Completion> {
        let c = self.done.pop_front()?;
        if c.is_final() && self.ops.get(c.op).is_some_and(|o| o.done) {
            self.take_op(c.op);
            self.retired -= 1;
        }
        Some(c)
    }

    /// Wait for activity. Returns queued completion count.
    pub fn tick(&mut self, timeout: Option<Duration>) -> Result<usize, IoError> {
        // Undrained completions return at once, so the caller never blocks with work pending.
        if !self.done.is_empty() {
            return Ok(self.done.len());
        }
        self.retry_rearms();
        self.q.tick(timeout)?;
        self.harvest();
        Ok(self.done.len())
    }

    fn harvest(&mut self) {
        // Handlers may submit and finish more. This loop takes those in order, no nested harvest.
        if self.harvesting {
            return;
        }
        self.harvesting = true;
        let mut got = core::mem::take(&mut self.scratch);
        loop {
            let n = self.q.reap(&mut got);
            if n == 0 {
                break;
            }
            for c in &got[..n] {
                self.complete(*c);
            }
        }
        self.scratch = got;
        self.harvesting = false;
    }

    /// Readiness backend may complete file ops inline, harvested at once.
    fn submit(&mut self, id: OpId, op: queue::Op) -> Result<(), IoError> {
        self.push_op(id, op, false)
    }

    /// [`Io::submit`] on the driver's own behalf, a re-arm or the rest of a short write,
    /// which never joins a caller's [`Io::chain`], see [`Queue::push_apart`].
    fn resubmit(&mut self, id: OpId, op: queue::Op) -> Result<(), IoError> {
        self.push_op(id, op, true)
    }

    fn push_op(&mut self, id: OpId, op: queue::Op, own: bool) -> Result<(), IoError> {
        let chained = !own && self.q.in_chain();
        let user = id.to_user_data();
        let pushed = if own {
            self.q.push_apart(op, user)
        } else {
            self.q.push(op, user)
        };
        match pushed {
            Ok(key) => {
                self.submitted(id, key, chained);
                Ok(())
            }
            Err(e) => {
                // Never reached the kernel. Slot and buffer freed, the caller handed it over.
                self.unspan(id.to_user_data());
                if let Some(o) = self.take_op(id) {
                    self.retired -= usize::from(o.done);
                    if let Some(b) = o.buf {
                        self.free_buf(b);
                    }
                }
                Err(e)
            }
        }
    }

    fn submitted(&mut self, id: OpId, key: queue::Key, chained: bool) {
        self.set_key(id, key.0);
        if chained {
            self.note_chained(id);
        }
        self.settle_now();
    }

    /// Harvest what the readiness backend finished inline.
    fn settle_now(&mut self) {
        if self.q.backend() == Backend::Epoll && self.q.ready() {
            self.harvest();
        }
    }

    fn complete(&mut self, c: Cqe) {
        if c.is_posted() {
            return self.deliver_msg(c.user);
        }
        if c.user >= CLOSING {
            // Driver's own close or the mailbox poll.
            return self.own_entry(c);
        }
        if !c.more() && !self.chain_fds.is_empty() {
            self.unchain(c.user);
        }
        if !c.more() && !self.spans.is_empty() {
            self.unspan(c.user);
        }
        let id = OpId::from_user_data(c.user);
        if !self.is_live(id) {
            // Cancelled or finished. The generation makes this a miss, not a hit on the reused slot.
            self.reap_stale(c);
            return;
        }
        let Some(op) = self.ops.get(id).copied() else {
            return;
        };
        if !c.more() {
            self.set_key(id, NO_KEY);
        }
        self.apply(id, op, c);
    }

    /// Release what a forgotten op's completion still holds.
    fn reap_stale(&mut self, c: Cqe) {
        // A cancelled receive may still have been handed a ring buffer.
        self.offer_again(c.flags);
        if c.more() {
            return;
        }
        if let Some(i) = self.held_quarantine.iter().position(|(u, _)| *u == c.user) {
            self.held_quarantine.swap_remove(i);
        }
        let Some(i) = self.quarantine.iter().position(|(u, _, _)| *u == c.user) else {
            return;
        };
        let (_, b, kind) = self.quarantine.swap_remove(i);
        if let Some(b) = b {
            self.free_buf(b);
        }
        if gives_fd(kind) && c.res >= 0 {
            // Open or accept beat its cancel. Close the orphan fd or it leaks.
            // SAFETY: fresh from the kernel, nothing else knows its number.
            drop(unsafe { OwnedFd::from_raw_fd(c.res) });
        }
    }

    /// The region range of an op that ended, free for the caller again.
    fn unspan(&mut self, user: u64) {
        if let Some(i) = self.spans.iter().position(|s| s.user == user) {
            self.spans.swap_remove(i);
        }
    }

    fn apply(&mut self, id: OpId, op: Op, c: Cqe) {
        let res = c.res;
        match op.kind {
            Kind::Timeout => self.done_timeout(id, res),
            Kind::Connect => self.done_connect(id, op, res),
            Kind::Recv => self.done_recv(id, op, c),
            Kind::Open => self.done_open(id, op, res),
            Kind::Read | Kind::Stat => self.done_file(id, op, res),
            Kind::Accept => self.done_accept(id, op, c),
            Kind::RecvFrom => self.done_recvfrom(id, op, c),
            Kind::SendTo | Kind::SendMsg => self.done_sendto(id, op, res),
            Kind::Send | Kind::Write => self.done_send(id, op, res),
            Kind::SendShared => self.done_send_shared(id, op, res),
            _ => self.apply_rest(id, op, c),
        }
    }

    /// Cold kinds, out of line so [`Io::apply`] stays small enough to inline (measured).
    #[inline(never)]
    fn apply_rest(&mut self, id: OpId, op: Op, c: Cqe) {
        match op.kind {
            Kind::Fallocate
            | Kind::Sync
            | Kind::Truncate
            | Kind::ReadMem
            | Kind::WriteMem
            | Kind::Splice
            | Kind::Post => self.done_plain(id, c.res),
            // Poll result is the event mask, not a count.
            Kind::Ready => self.done_plain(id, c.res.min(0)),
            Kind::Rename | Kind::Unlink | Kind::SetSockOpt => self.done_path(id, op, c.res),
            Kind::RecvInto => self.done_recv_into(id, op, c.res),
            Kind::RecvMsg => self.done_recvmsg(id, op, c),
            _ => (),
        }
    }

    fn done_plain(&mut self, id: OpId, res: i32) {
        self.retire(id, result_of(res), None);
    }

    /// `-ETIME` is success.
    fn done_timeout(&mut self, id: OpId, res: i32) {
        let r = if res == -Errno::TIME.raw_os_error() || res >= 0 {
            Ok(0)
        } else {
            Err(Errno::from_raw_os_error(-res))
        };
        self.retire(id, r, None);
    }

    /// Queue the completion, keep the slot until it is taken.
    fn retire(&mut self, id: OpId, result: Result<u32, Errno>, buf: Option<Buf>) {
        self.retire_with(id, result, buf, None);
    }

    fn retire_with(
        &mut self,
        id: OpId,
        result: Result<u32, Errno>,
        buf: Option<Buf>,
        addr: Option<SocketAddr>,
    ) {
        let Some(slot) = self.ops.get_mut(id) else {
            return;
        };
        if slot.done {
            return;
        }
        // Buffer moves to the completion so the record cannot release it twice.
        slot.done = true;
        slot.buf = None;
        let op = *slot;
        self.retired += 1;
        self.done.push_back(Completion {
            op: id,
            kind: op.kind,
            fd: op.fd,
            result,
            buf,
            addr,
            more: false,
        });
    }

    /// Delivery from a multishot op that stays armed.
    fn deliver_more(
        &mut self,
        id: OpId,
        op: Op,
        result: Result<u32, Errno>,
        buf: Option<Buf>,
        addr: Option<SocketAddr>,
    ) {
        self.done.push_back(Completion {
            op: id,
            kind: op.kind,
            fd: op.fd,
            result,
            buf,
            addr,
            more: true,
        });
    }

    fn fail_read(&mut self, id: OpId, bid: u16, e: Errno) {
        self.free_buf(bid);
        self.retire(id, Err(e), None);
    }

    /// Buffer `i` handed to the caller with `len` bytes filled.
    fn landed(&mut self, i: u16, len: u32) -> Buf {
        Buf {
            id: self.bufs.land(i),
            len,
        }
    }

    /// Free a buffer the caller or an op held. One of the provided ring goes back to it.
    pub(super) fn free_buf(&mut self, bid: u16) {
        if self.bufs.free(bid) && self.bufs.ring_at(bid) {
            self.give_back(bid);
        }
    }

    /// A re-submit of a live op failed, which dropped it. Its caller still gets a final.
    pub(super) fn resubmit_failed(&mut self, id: OpId, op: Op, r: Result<(), IoError>) {
        let Err(e) = r else {
            return;
        };
        let e = match e {
            IoError::Kernel(e) => e,
            _ => Errno::NOBUFS,
        };
        self.done.push_back(Completion {
            op: id,
            kind: op.kind,
            fd: op.fd,
            result: Err(e),
            buf: None,
            addr: None,
            more: false,
        });
    }

    /// Buffer range as a queue `Mem`, clamped to the buffer.
    fn buffer(&self, bid: u16, at: u32, len: u32) -> Mem {
        let Some((base, size)) = self.bufs.raw(bid) else {
            return Mem::EMPTY;
        };
        let at = (at as usize).min(size);
        let len = (len as usize).min(size - at);
        // SAFETY: the driver owns the arena and frees it only after the queue drained, or
        // never. The buffer is the kernel's, lent to the op until its last completion or
        // quarantined after a cancel, or in the provided ring, so the caller cannot reach it.
        // One op holds a buffer at a time, so none in flight writes a path buffer.
        unsafe { Mem::raw(base.wrapping_add(at), len) }
    }

    fn insert(&mut self, op: Op) -> Result<OpId, IoError> {
        let id = self.ops.insert(op).ok_or(IoError::OpsFull)?;
        self.set_key(id, NO_KEY);
        self.count_on(op.fd, true);
        Ok(id)
    }

    /// One more or one less live op on `fd`. Every change of an op's fd goes through here
    /// or [`Io::set_fd`], so [`Io::close`] sees each op on the fd it closes. An fd past
    /// `live_on` is not counted, so a bogus number allocates nothing, and is counted once
    /// [`Io::grow_live_on`] covers it.
    fn count_on(&mut self, fd: RawFd, more: bool) {
        let Some(n) = usize::try_from(fd)
            .ok()
            .and_then(|i| self.live_on.get_mut(i))
        else {
            return;
        };
        *n = if more { *n + 1 } else { n.saturating_sub(1) };
    }

    /// Cover descriptors below `n`, counting the live ops on the new ones. Grown only for
    /// descriptors the driver owns or was told to expect, so bounded by real ones.
    fn grow_live_on(&mut self, n: usize) {
        let from = self.live_on.len();
        if n <= from {
            return;
        }
        self.live_on.resize(n, 0);
        let mut slot = 0;
        while let Some(id) = self.ops.next_live(slot) {
            slot = id.slot() + 1;
            let fd = self.ops.get(id).map_or(-1, |o| o.fd);
            if let Some(c) = usize::try_from(fd).ok().filter(|i| (from..n).contains(i)) {
                self.live_on[c] += 1;
            }
        }
    }

    /// Point a live op at another fd, moving its count.
    fn set_fd(&mut self, id: OpId, fd: RawFd) {
        let Some(old) = self.ops.get(id).map(|o| o.fd) else {
            return;
        };
        if old == fd {
            return;
        }
        self.count_on(old, false);
        self.count_on(fd, true);
        if let Some(slot) = self.ops.get_mut(id) {
            slot.fd = fd;
        }
    }

    /// Mirror of [`Io::insert`]. All removals go here to keep `live_on` right.
    /// The generation is checked first, so a stale or forged id touches no slot column.
    fn take_op(&mut self, id: OpId) -> Option<Op> {
        let op = self.ops.remove(id)?;
        if let Some(h) = self.held.get_mut(id.slot() as usize) {
            *h = None;
        }
        self.count_on(op.fd, false);
        Some(op)
    }

    /// The memory slot `id` holds, if it is that op's.
    fn take_held(&mut self, id: OpId) -> Option<Shared> {
        self.ops.get(id)?;
        self.held
            .get_mut(id.slot() as usize)?
            .take()
            .map(|h| h.data)
    }

    fn keep(&mut self, raw: RawFd, fd: OwnedFd) {
        let i = raw as usize;
        if self.owned.len() <= i {
            self.owned.resize_with(i + 1, || None);
        }
        self.grow_live_on(i + 1);
        self.owned[i] = Some(fd);
    }

    /// Arm receives that had no buffer at submission.
    fn retry_rearms(&mut self) {
        if self.rearm.is_empty() || (self.bufs.is_empty() && self.ring_free == 0) {
            return;
        }
        for _ in 0..self.rearm.len() {
            let Some(id) = self.rearm.pop() else { break };
            self.retry_one(id);
        }
    }

    fn retry_one(&mut self, id: OpId) {
        let Some(op) = self.ops.get(id).copied() else {
            return;
        };
        let r = match op.kind {
            Kind::RecvFrom => self.arm_recvfrom(id, op.fd, true),
            Kind::RecvMsg => self.arm_recvmsg(id, op.fd, true),
            _ => self.arm_recv(id, op.fd, true),
        };
        self.resubmit_failed(id, op, r);
    }
}

impl Drop for Io {
    /// Drain the queue, closes included, before the memory it names goes. If the drain
    /// times out the kernel may still use that memory, so it leaks.
    fn drop(&mut self) {
        if self.q.quiesce(self.drain) {
            return;
        }
        self.q.give_up();
        self.bufs.leak();
        for r in &mut self.regions {
            r.leak();
        }
        for h in self.held.drain(..) {
            core::mem::forget(h);
        }
        for (_, h) in self.held_quarantine.drain(..) {
            core::mem::forget(h);
        }
        // An op may still look the number up.
        for (_, f) in self.parked.drain(..) {
            core::mem::forget(f);
        }
    }
}

/// Kinds whose result is a new descriptor.
fn gives_fd(kind: Kind) -> bool {
    matches!(kind, Kind::Open | Kind::Accept)
}

fn result_of(res: i32) -> Result<u32, Errno> {
    if res < 0 {
        Err(Errno::from_raw_os_error(-res))
    } else {
        Ok(res as u32)
    }
}

/// User data at or above this is the driver's own. Handles keep their slot in
/// the high half. A close carries its fd in the low half.
pub(crate) const CLOSING: u64 = (u32::MAX as u64) << 32;
/// User data of the readiness backend mailbox poll.
pub(crate) const MAIL: u64 = u64::MAX;
/// User data of the [`Waker`] eventfd poll. No close carries an fd this high.
pub(crate) const WAKE: u64 = u64::MAX - 1;
/// No queue entry.
pub(crate) const NO_KEY: u64 = u64::MAX;
/// Open `aux`, a stat waits on it with its handle in `offset`.
/// Stat `aux`, it is that stat and has no kernel entry.
const STAT_WAITS: u32 = 1;

const REGIONS: usize = 8;
/// Max wait in drop for the queue to drain.
const DRAIN: Duration = Duration::from_secs(2);
/// Completions reaped per batch.
const HARVEST: usize = 64;
/// Queue slots beyond two per op, for the driver's own entries.
const SPARE_SLOTS: usize = 16;

/// Write offset meaning the descriptor's own position.
pub const APPEND: u64 = u64::MAX;

/// Initial per-descriptor table size.
const FD_ROOM: usize = 8192;

/// Half the arena rounded down to a power of two, max `1 << 15`.
/// Zero below 8 buffers. The rest stays for sends.
fn ring_size(count: usize) -> u16 {
    if count < 8 {
        return 0;
    }
    let half = count / 2;
    let n = 1usize << (usize::BITS - 1 - half.leading_zeros());
    n.min(1 << 15) as u16
}

/// None for closed fds, so a stale number is never reused.
fn borrow(owned: &[Option<OwnedFd>], fd: RawFd) -> Option<BorrowedFd<'_>> {
    match owned.get(fd as usize) {
        Some(Some(f)) => Some(f.as_fd()),
        _ => None,
    }
}

fn raw(fd: RawFd) -> Fd {
    Fd::Raw(fd)
}

mod disk;
mod mail;
mod sock;
mod table;
mod turn;

pub use disk::Region;
pub use mail::{Mailbox, Peer};

#[cfg(test)]
mod tests;
