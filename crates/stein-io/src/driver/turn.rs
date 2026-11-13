//! Server-turn helpers that avoid io-wq punts and extra syscalls.
//! Kernel 7.1 (`examples/punt.rs`) always punts statx to a worker. Cached open, read and close run inline.

use super::*;

impl Io {
    /// Stat the fd an in-flight open returns, as a plain call once it lands. No punt.
    /// Completes in the same tick as the open. Open failure propagates, cancel gives `ECANCELED`.
    /// One stat per open. Buffers shorter than [`crate::file::META_LEN`] are [`IoError::TooLong`].
    pub fn stat_opened(&mut self, open: OpId) -> Result<OpId, IoError> {
        self.call(|io| io.stat_opened_in(open))
    }

    fn stat_opened_in(&mut self, open: OpId) -> Result<OpId, IoError> {
        let waited = self.ops.get(open).copied();
        let opening = |o: &Op| matches!(o.kind, Kind::Open | Kind::Open2) && o.aux == 0;
        let Some(o) = waited.filter(opening) else {
            return Err(IoError::NotOpening);
        };
        let max = self.bufs.size() as usize;
        let len = crate::file::META_LEN as usize;
        if max < len {
            return Err(IoError::TooLong { len, max });
        }
        let bid = self.bufs.take_lent().map_err(|_| IoError::BuffersFull)?;
        let mut op = Op::new(Kind::Stat, -1).with_buf(bid);
        op.aux = STAT_WAITS;
        let id = match self.insert(op) {
            Ok(id) => id,
            Err(e) => {
                self.free_buf(bid);
                return Err(e);
            }
        };
        if let Some(slot) = self.ops.get_mut(open) {
            slot.aux = STAT_WAITS;
            slot.offset = id.to_user_data();
        }
        if !o.done {
            return Ok(id);
        }
        // Open already landed, its result is queued.
        let failed = self.done.iter().find(|c| c.op == open);
        match failed.and_then(|c| c.result.err()) {
            Some(e) => self.fail_read(id, bid, e),
            None => self.stat_fd_now(id, o.fd, bid),
        }
        Ok(id)
    }

    /// Answer the stat waiting on this open, if any.
    pub(super) fn stat_landed(&mut self, open: Op, r: Result<RawFd, Errno>) {
        if open.aux != STAT_WAITS {
            return;
        }
        let id = OpId::from_user_data(open.offset);
        let waiting = self.ops.get(id).filter(|o| !o.done);
        let Some(bid) = waiting.and_then(|o| o.buf) else {
            return;
        };
        match r {
            Ok(fd) => self.stat_fd_now(id, fd, bid),
            Err(e) => self.fail_read(id, bid, e),
        }
    }

    /// Cancel an open or its waiting stat. True if nothing is left in the kernel.
    /// The stat has no entry, so its buffer frees at once.
    pub(super) fn cancel_waiting(&mut self, op: Op) -> bool {
        if matches!(op.kind, Kind::Open | Kind::Open2) {
            self.stat_landed(op, Err(Errno::CANCELED));
        }
        if op.kind != Kind::Stat {
            return false;
        }
        if let Some(b) = op.buf {
            self.free_buf(b);
        }
        true
    }

    /// The queue owns the fd from here and never closes it again, nor cancels the close,
    /// see [`Queue::close`]. An unqueued close falls back to a plain close here. Ring fds
    /// never come here, see [`Io::close`].
    pub(super) fn close_in_batch(&mut self, fd: RawFd, file: OwnedFd) {
        // A refused push hands the fd back and dropping it closes it.
        let _ = self.q.close(file, CLOSING | u64::from(fd as u32));
    }

    /// Whether a non-waiting [`Io::tick`] could find work, checked without a syscall.
    /// Always true on epoll or a ring without the deferred-work flag.
    pub fn pending(&mut self) -> bool {
        if !self.done.is_empty() {
            return true;
        }
        // Bufferless receives rearm on tick once a buffer of their kind frees.
        let rearms = self.rearm.iter().any(|id| self.can_rearm(*id));
        rearms || self.q.pending()
    }

    /// A receive waiting for a buffer finds one. A multishot one on the provided ring
    /// needs a ring buffer, any other a free list one.
    pub(super) fn can_rearm(&self, id: OpId) -> bool {
        let Some(op) = self.ops.get(id) else {
            return false;
        };
        let multi = op.kind == Kind::RecvMsg || (op.kind == Kind::Recv && op.multishot);
        if multi && self.group.is_some() {
            self.ring_free > 0
        } else {
            !self.bufs.is_empty()
        }
    }
}
