//! Cross-thread wakeups. Values posted into another driver's completion queue.
//! `MSG_RING` between rings, an 8-byte-per-value pipe for an epoll target.
//!
//! A [`Waker`] wakes a driver from any thread, one without a driver of its own too. An
//! eventfd the driver polls, its wakes merged into one [`Kind::Wake`] until the next look.

use rustix::event::{EventfdFlags, eventfd};
use rustix::pipe::PipeFlags;

use super::*;

/// Handle to send to another thread. Owns its fd, so it may outlive the target.
#[derive(Debug)]
pub struct Mailbox {
    fd: OwnedFd,
    ring: bool,
}

/// A [`Mailbox`] adopted by a sending driver. Made only by [`Io::peer`], and a post
/// goes only to a peer the driver still has, so no copy reaches another descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    fd: RawFd,
    ring: bool,
}

impl Peer {
    /// The descriptor, to close with [`Io::close`].
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// A ring target, posted to with `MSG_RING`, else an epoll target's pipe.
    pub fn is_ring(&self) -> bool {
        self.ring
    }
}

/// Values read off the pipe per batch.
const MAIL_BATCH: usize = 64;

/// Wakes one driver from any thread. Clones share the eventfd, which outlives the driver,
/// so a wake after it is gone does nothing.
#[derive(Clone, Debug)]
pub struct Waker(Arc<OwnedFd>);

impl Waker {
    /// The driver gets a [`Kind::Wake`] completion, one for every wake before it looks.
    pub fn wake(&self) {
        // A full counter is a wake already pending.
        let _ = rustix::io::write(&*self.0, &1u64.to_ne_bytes());
    }
}

impl Io {
    /// One per sender, made on the owning thread. On a ring it is a dup of the ring fd,
    /// and whoever holds that can post any completion with `MSG_RING` from raw `io_uring`
    /// code, a forged final one for an op in flight included, which frees memory the
    /// kernel still uses. Safe code of this crate only posts values, so hand a mailbox only
    /// to code you trust as much as this driver.
    pub fn mailbox(&mut self) -> Result<Mailbox, IoError> {
        if self.q.backend() == Backend::Uring {
            let fd = rustix::io::fcntl_dupfd_cloexec(self.as_fd(), 0).map_err(IoError::Kernel)?;
            return Ok(Mailbox { fd, ring: true });
        }
        if self.mail.is_none() {
            self.start_mail()?;
        }
        let Some((_, tx)) = &self.mail else {
            return Err(IoError::Kernel(Errno::BADF));
        };
        let fd = rustix::io::fcntl_dupfd_cloexec(tx, 0).map_err(IoError::Kernel)?;
        Ok(Mailbox { fd, ring: false })
    }

    fn start_mail(&mut self) -> Result<(), IoError> {
        let flags = PipeFlags::CLOEXEC | PipeFlags::NONBLOCK;
        let (rx, tx) = rustix::pipe::pipe_with(flags).map_err(IoError::Kernel)?;
        let fd = rx.as_raw_fd();
        self.mail = Some((rx, tx));
        self.arm_mail(fd).inspect_err(|_| self.drop_mail())
    }

    /// Stop polling the mailbox pipe. The next [`Io::mailbox`] starts a new one.
    fn drop_mail(&mut self) {
        if let Some((rx, _)) = self.mail.take() {
            // Readiness backend watches by number, which gets reused.
            self.q.forget(rx.as_raw_fd());
        }
    }

    /// Stop polling the waker's eventfd. The next [`Io::waker`] starts a new one.
    fn drop_wake(&mut self) {
        if let Some(fd) = self.wake.take() {
            self.q.forget(fd.as_raw_fd());
        }
    }

    fn arm_mail(&mut self, fd: RawFd) -> Result<(), IoError> {
        let poll = queue::Op::Poll {
            fd: raw(fd),
            events: libc::POLLIN as u32,
            multi: false,
        };
        // Never in a caller's chain, see `Queue::push_apart`.
        self.q.push_apart(poll, MAIL).map(drop)
    }

    /// A waker for this driver, its eventfd made and polled on the first call.
    pub fn waker(&mut self) -> Result<Waker, IoError> {
        if self.wake.is_none() {
            let flags = EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK;
            let fd = eventfd(0, flags).map_err(IoError::Kernel)?;
            self.arm_wake(fd.as_raw_fd())?;
            self.wake = Some(fd);
        }
        let Some(fd) = &self.wake else {
            return Err(IoError::Kernel(Errno::BADF));
        };
        let theirs = rustix::io::fcntl_dupfd_cloexec(fd, 0).map_err(IoError::Kernel)?;
        Ok(Waker(Arc::new(theirs)))
    }

    fn arm_wake(&mut self, fd: RawFd) -> Result<(), IoError> {
        let poll = queue::Op::Poll {
            fd: raw(fd),
            events: libc::POLLIN as u32,
            multi: false,
        };
        self.q.push_apart(poll, WAKE).map(drop)
    }

    /// Drains the eventfd, delivers one [`Kind::Wake`] and polls again. A failed poll, or
    /// a poll that cannot be pushed again, is delivered as the error and the eventfd goes,
    /// so no later wake is lost unseen. The next [`Io::waker`] starts over.
    fn woken(&mut self, res: i32) {
        let Some(f) = &self.wake else {
            return;
        };
        let fd = f.as_raw_fd();
        let mut result = if res < 0 {
            Err(Errno::from_raw_os_error(-res))
        } else {
            let mut n = [0u8; 8];
            let _ = rustix::io::read(f, &mut n);
            Ok(0)
        };
        if result.is_ok() {
            result = self.arm_wake(fd).map(|()| 0).map_err(errno_of);
        }
        if result.is_err() {
            self.drop_wake();
        }
        // Marked `more` so nothing looks up its fake op id.
        self.done.push_back(Completion {
            op: OpId::from_user_data(WAKE),
            kind: Kind::Wake,
            fd: -1,
            result,
            buf: None,
            addr: None,
            more: true,
        });
    }

    /// Adopt a mailbox. Close with [`Io::close`] on [`Peer::fd`].
    pub fn peer(&mut self, m: Mailbox) -> Peer {
        let fd = self.own(m.fd);
        let p = Peer { fd, ring: m.ring };
        self.peers.push(p);
        p
    }

    /// Deliver `value` as a [`Kind::Msg`] on the peer, waking it. Completes here with `Ok(0)`.
    /// Ordered per sender. A ring target never reports full, kernel buffers unboundedly
    /// (kernel 7.1). A pipe target holds 8192 values, then `EAGAIN`.
    /// Bound the real queue yourself, the post is only the wake.
    /// Epoll driver to ring target fails with `EOPNOTSUPP`. Self-post is allowed.
    /// [`IoError::Kernel`] with `EBADF` for a peer already closed.
    pub fn post(&mut self, to: Peer, value: u64) -> Result<OpId, IoError> {
        if !self.peers.contains(&to) {
            return Err(IoError::Kernel(Errno::BADF));
        }
        let id = self.insert(Op::new(Kind::Post, to.fd))?;
        if !to.ring {
            let r = match borrow(&self.owned, to.fd) {
                Some(pipe) => write_value(pipe, value),
                None => Err(Errno::BADF),
            };
            self.retire(id, r.map(|()| 0), None);
            return Ok(id);
        }
        self.submit(id, queue::Op::Post { ring: to.fd, value })?;
        Ok(id)
    }

    /// Marked `more` so nothing looks up its fake op id.
    pub(super) fn deliver_msg(&mut self, value: u64) {
        self.deliver_mail(value, Ok(0));
    }

    fn deliver_mail(&mut self, value: u64, result: Result<u32, Errno>) {
        self.done.push_back(Completion {
            op: OpId::from_user_data(value),
            kind: Kind::Msg,
            fd: -1,
            result,
            buf: None,
            addr: None,
            more: true,
        });
    }

    /// Driver's own close or mailbox poll. A close needs nothing, the queue owns its fd.
    pub(super) fn own_entry(&mut self, c: Cqe) {
        if c.user == WAKE {
            return self.woken(c.res);
        }
        if c.user != MAIL {
            return;
        }
        if c.res < 0 {
            // Not polled again, so no spin on a poll that fails at once. No value.
            self.drop_mail();
            return self.deliver_mail(MAIL, Err(Errno::from_raw_os_error(-c.res)));
        }
        self.read_mail();
        let Some(fd) = self.mail.as_ref().map(|(rx, _)| rx.as_raw_fd()) else {
            return;
        };
        if let Err(e) = self.arm_mail(fd) {
            // Values sent from here on would wait unseen, so the mailbox goes.
            self.drop_mail();
            self.deliver_mail(MAIL, Err(errno_of(e)));
        }
    }

    /// Reads at most what `done` has room for. The rest waits in the pipe.
    fn read_mail(&mut self) {
        let room = (self.done.capacity() - self.done.len()).clamp(1, MAIL_BATCH);
        let mut raw = [0u8; MAIL_BATCH * 8];
        let Some((rx, _)) = &self.mail else {
            return;
        };
        let Ok(n) = rustix::io::read(rx, &mut raw[..room * 8]) else {
            return;
        };
        for word in raw[..n].as_chunks::<8>().0 {
            self.deliver_msg(u64::from_ne_bytes(*word));
        }
    }

    /// Completions dropped for lack of kernel memory. Always zero on epoll.
    pub fn cq_overflow(&mut self) -> u32 {
        self.q.overflow()
    }

    /// Completions held in the kernel for CQ room. They arrive on the next [`Io::tick`].
    pub fn cq_backlog(&mut self) -> bool {
        self.q.backlog()
    }
}

/// The errno a failed push reports, `ENOBUFS` for a full queue.
fn errno_of(e: IoError) -> Errno {
    match e {
        IoError::Kernel(e) => e,
        _ => Errno::NOBUFS,
    }
}

/// 8-byte pipe writes are atomic.
fn write_value(pipe: BorrowedFd<'_>, value: u64) -> Result<(), Errno> {
    loop {
        match rustix::io::write(pipe, &value.to_ne_bytes()) {
            Err(Errno::INTR) => continue,
            r => return r.map(|_| ()),
        }
    }
}
