//! Readiness backend emulating the ring.
//!
//! File ops run at submission. Socket and pipe ops are tried at submission,
//! then queued per descriptor (FIFO) and retried on its edge-triggered signal.
//! Multishot ops deliver once, then go on a retry list so the edge is drained,
//! by a reap with room left or by the next tick.
//!
//! No try blocks the loop on a blocking descriptor. Sockets get `MSG_DONTWAIT`, a read
//! or write at the position `RWF_NOWAIT`, and an accept on a blocking listener waits for
//! readable first. A connect or splice sets `O_NONBLOCK` on a blocking end for the call
//! only, which whoever shares the file description sees meanwhile. A regular file cannot
//! be waited on, so its read or write at the position blocks as a file op does.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use rustix::event::epoll;
use rustix::io::Errno;

use super::slot::{Blocks, List, State, Watch};
use super::{
    Cqe, F_BUFFER, F_MORE, FOREVER, Fd, Mem, NONE, Op, POSITION, Queue, RawBuf, ring_entry,
};
use crate::error::IoError;

/// Events per wait. The rest come on the next.
const EVENTS: usize = 64;

/// Timer epoll tag, above any fd.
const TIMER: u64 = u64::MAX;

pub(super) struct Poll {
    ep: OwnedFd,
    /// Armed to the earliest timeout so the epoll fd wakes for it.
    timer: OwnedFd,
}

impl Poll {
    pub(super) fn new() -> Result<Poll, IoError> {
        use rustix::time::{TimerfdClockId, TimerfdFlags, timerfd_create};
        let ep = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(IoError::Kernel)?;
        let flags = TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK;
        let timer = timerfd_create(TimerfdClockId::Monotonic, flags).map_err(IoError::Kernel)?;
        let data = epoll::EventData::new_u64(TIMER);
        epoll::add(&ep, &timer, data, epoll::EventFlags::IN).map_err(IoError::Kernel)?;
        Ok(Poll { ep, timer })
    }

    /// Arm for `at`, or disarm. Past deadlines arm 1ns out since zero disarms.
    fn arm(&self, at: Option<Instant>) {
        use rustix::time::{Itimerspec, TimerfdTimerFlags, Timespec, timerfd_settime};
        let after = at.map_or(Duration::ZERO, |at| {
            at.saturating_duration_since(Instant::now())
                .max(Duration::from_nanos(1))
        });
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let value = Timespec {
            tv_sec: after.as_secs() as i64,
            tv_nsec: after.subsec_nanos() as i64,
        };
        let spec = Itimerspec {
            it_interval: zero,
            it_value: value,
        };
        // On failure the tick's own timeout still fires it.
        let _ = timerfd_settime(&self.timer, TimerfdTimerFlags::empty(), &spec);
    }

    pub(super) fn as_fd(&self) -> BorrowedFd<'_> {
        self.ep.as_fd()
    }
}

enum Try {
    /// Would block.
    Again,
    /// Finished with result and flags.
    Done(i32, u32),
    /// Multishot delivered once, still armed.
    More(i32, u32),
}

impl Queue {
    pub(super) fn start_epoll(&mut self, i: u32) {
        let op = self.slots[i as usize].op;
        match op {
            Op::Timeout { after } => {
                // Clamped first, so no deadline overflows the clock and panics.
                let Some(at) = Instant::now().checked_add(after.min(FOREVER)) else {
                    return self.finish(i, -Errno::INVAL.raw_os_error(), 0, true);
                };
                self.slots[i as usize].at = Some(at);
                self.timers += 1;
                self.arm_timer();
            }
            Op::FutexWait { .. } | Op::Post { .. } => {
                self.finish(i, -Errno::OPNOTSUPP.raw_os_error(), 0, true)
            }
            Op::Read { at, .. } | Op::Write { at, .. } if at != POSITION => {
                self.finish(i, file(op), 0, true)
            }
            Op::Close { fd } => {
                self.forget(fd.raw());
                self.finish(i, sys::close(fd.raw()), 0, true);
            }
            Op::Nop
            | Op::Open { .. }
            | Op::Statx { .. }
            | Op::Rename { .. }
            | Op::Unlink { .. } => {
                self.finish(i, file(op), 0, true);
            }
            Op::Fsync { .. }
            | Op::SyncFileRange { .. }
            | Op::Fadvise { .. }
            | Op::Fallocate { .. }
            | Op::Truncate { .. } => {
                self.finish(i, file(op), 0, true);
            }
            Op::SetSockOpt { .. } => self.finish(i, file(op), 0, true),
            Op::Connect { fd, to } => self.connect(i, fd, to),
            _ => self.stream(i),
        }
    }

    /// Tried now unless an older op waits on the same side, to keep order.
    fn stream(&mut self, i: u32) {
        let (fd, write) = side_of(&self.slots[i as usize].op);
        let Some(fd) = self.watched(i, fd) else {
            return;
        };
        let empty = self.list(fd, write).is_empty();
        if empty {
            match self.attempt(i) {
                Try::Again => {}
                Try::Done(r, f) => return self.finish(i, r, f, true),
                // Still waits, see `rearm`.
                Try::More(r, f) => self.finish(i, r, f | F_MORE, false),
            }
        }
        self.wait(i, fd, write);
    }

    /// Connect now. In progress waits for writable.
    fn connect(&mut self, i: u32, fd: Fd, to: std::net::SocketAddr) {
        let Some(raw) = self.watched(i, fd) else {
            return;
        };
        match sys::unblocked([raw, -1], || sys::connect(raw, to)) {
            0 => self.finish(i, 0, 0, true),
            r if r == -Errno::INPROGRESS.raw_os_error() || r == -Errno::ALREADY.raw_os_error() => {
                self.wait(i, raw, true)
            }
            r => self.finish(i, r, 0, true),
        }
    }

    /// The raw fd, the watch table grown to cover it. Else finish the op, with `EMFILE`
    /// for a queue without a table, which waits on no descriptor, or `EBADF`. The table
    /// grows only for an open descriptor, so a bogus number allocates nothing.
    fn watched(&mut self, i: u32, fd: Fd) -> Option<RawFd> {
        match fd {
            Fd::Raw(_) if self.watches.is_empty() => {
                self.finish(i, -Errno::MFILE.raw_os_error(), 0, true);
                None
            }
            Fd::Raw(fd) if fd >= 0 && (fd as usize) < self.watches.len() => Some(fd),
            Fd::Raw(fd) if fd >= 0 && sys::is_open(fd) => {
                // Rare, the table starts sized for the expected descriptors.
                let n = (fd as usize + 1).next_power_of_two();
                self.watches.resize(n, Watch::EMPTY);
                Some(fd)
            }
            _ => {
                self.finish(i, -Errno::BADF.raw_os_error(), 0, true);
                None
            }
        }
    }

    fn list(&mut self, fd: RawFd, write: bool) -> List {
        let w = &self.watches[fd as usize];
        if write { w.write } else { w.read }
    }

    fn set_list(&mut self, fd: RawFd, write: bool, l: List) {
        let w = &mut self.watches[fd as usize];
        if write { w.write = l } else { w.read = l }
    }

    /// Append to the descriptor's wait list and register it with epoll.
    fn wait(&mut self, i: u32, fd: RawFd, write: bool) {
        if let Err(e) = self.add(fd) {
            if self.slots[i as usize].state == State::Flying {
                self.finish(i, -e.raw_os_error(), 0, true);
            }
            return;
        }
        let mut l = self.list(fd, write);
        match l.tail {
            NONE => l.head = i,
            t => self.slots[t as usize].wait_next = i,
        }
        l.tail = i;
        self.slots[i as usize].wait_next = NONE;
        self.set_list(fd, write, l);
    }

    fn add(&mut self, fd: RawFd) -> Result<(), Errno> {
        if self.watches[fd as usize].added {
            return Ok(());
        }
        let super::Engine::Epoll(p) = &self.engine else {
            return Ok(());
        };
        let flags = epoll::EventFlags::IN
            | epoll::EventFlags::OUT
            | epoll::EventFlags::RDHUP
            | epoll::EventFlags::ET;
        let flags = flags.bits();
        match sys::epoll_ctl(&p.ep, libc::EPOLL_CTL_ADD, fd, flags) {
            Ok(()) => {}
            Err(Errno::EXIST) => sys::epoll_ctl(&p.ep, libc::EPOLL_CTL_MOD, fd, flags)?,
            Err(e) => return Err(e),
        }
        self.watches[fd as usize].added = true;
        Ok(())
    }

    fn unlink(&mut self, i: u32) {
        let (fd, write) = side_of(&self.slots[i as usize].op);
        let Fd::Raw(fd) = fd else { return };
        if fd < 0 || fd as usize >= self.watches.len() {
            return;
        }
        let mut prev = NONE;
        let mut at = self.list(fd, write).head;
        while at != NONE && at != i {
            prev = at;
            at = self.slots[at as usize].wait_next;
        }
        if at == i {
            self.unlink_after(fd, write, prev, i);
        }
    }

    fn unlink_after(&mut self, fd: RawFd, write: bool, prev: u32, i: u32) {
        let next = self.slots[i as usize].wait_next;
        let mut l = self.list(fd, write);
        match prev {
            NONE => l.head = next,
            p => self.slots[p as usize].wait_next = next,
        }
        if l.tail == i {
            l.tail = prev;
        }
        self.slots[i as usize].wait_next = NONE;
        self.set_list(fd, write, l);
    }

    /// Drop `epoll` state for `fd` and cancel its waiters. Call after closing an
    /// fd yourself so a reused number is watched afresh. No-op on the ring.
    pub fn forget(&mut self, fd: RawFd) {
        if !matches!(self.engine, super::Engine::Epoll(_))
            || fd < 0
            || fd as usize >= self.watches.len()
        {
            return;
        }
        for write in [false, true] {
            loop {
                let i = self.list(fd, write).head;
                if i == NONE {
                    break;
                }
                self.unlink_after(fd, write, NONE, i);
                self.end(i, -Errno::CANCELED.raw_os_error());
            }
        }
        if let super::Engine::Epoll(p) = &self.engine {
            // Gone already is fine.
            let _ = sys::epoll_ctl(&p.ep, libc::EPOLL_CTL_DEL, fd, 0);
        }
        self.watches[fd as usize] = Watch::EMPTY;
    }

    /// Whether epoll watches `fd`, for tests of `forget`.
    #[cfg(test)]
    pub(crate) fn is_watched(&self, fd: RawFd) -> bool {
        self.watches.get(fd as usize).is_some_and(|w| w.added)
    }

    /// End an op that may still have a delivery queued.
    fn end(&mut self, i: u32, res: i32) {
        match self.slots[i as usize].state {
            State::Flying => self.finish(i, res, 0, true),
            // Ends when the queued delivery is taken.
            State::Queued => self.slots[i as usize].cancel = true,
            _ => {}
        }
    }

    /// Queue a completion. A final one ends the operation once it is taken.
    pub(super) fn finish(&mut self, i: u32, res: i32, flags: u32, last: bool) {
        if last {
            // Ended, so its retry must not run once the slot is reused.
            self.unretry(i);
        }
        let s = &mut self.slots[i as usize];
        (s.res, s.flags, s.next) = (res, flags, NONE);
        s.state = if last { State::Done } else { State::Queued };
        let tail = self.done.tail;
        match tail {
            NONE => self.done.head = i,
            t => self.slots[t as usize].next = i,
        }
        self.done.tail = i;
    }

    pub(super) fn cancel_epoll(&mut self, i: u32) {
        let s = &mut self.slots[i as usize];
        match s.op {
            Op::Timeout { .. } => {
                if s.at.take().is_some() {
                    self.timers -= 1;
                    self.finish(i, -Errno::CANCELED.raw_os_error(), 0, true);
                    self.arm_timer();
                }
            }
            _ if s.state == State::Flying => {
                // `finish` takes it off the retry list too.
                self.unlink(i);
                self.finish(i, -Errno::CANCELED.raw_os_error(), 0, true);
            }
            // A delivery is queued. `rearm` ends it.
            _ => {}
        }
    }

    pub(super) fn reap_epoll(&mut self, out: &mut [Cqe]) -> usize {
        let mut n = self.reap_done(out, 0);
        // A multishot that delivered may have more waiting on its edge. Taking it
        // here drains a burst in one reap, where a tick would take one a wait.
        while n < out.len() && !self.retry.is_empty() {
            self.run_retries();
            let took = self.reap_done(out, n);
            if took == n {
                break;
            }
            n = took;
        }
        n
    }

    /// Move finished completions into `out` from `n` on. Returns the new count.
    fn reap_done(&mut self, out: &mut [Cqe], mut n: usize) -> usize {
        while n < out.len() && self.done.head != NONE {
            let i = self.done.head;
            let s = &self.slots[i as usize];
            self.done.head = s.next;
            if self.done.head == NONE {
                self.done.tail = NONE;
            }
            // SAFETY: no kernel holds a side on `epoll`, syscalls use it only while they run.
            let side = unsafe { &*self.sides.at(i as usize) };
            out[n] = Cqe {
                user: s.user,
                res: s.res,
                flags: s.flags,
                addr: super::slot::peer(&s.op, side, s.res),
            };
            n += 1;
            if self.draining {
                super::orphan(&s.op, s.res);
            }
            match s.state {
                State::Done => self.release(i),
                _ => self.rearm(i),
            }
        }
        n
    }

    /// A multishot delivery was taken. End if cancelled, else retry next tick.
    fn rearm(&mut self, i: u32) {
        let s = &mut self.slots[i as usize];
        s.state = State::Flying;
        if s.cancel {
            self.unlink(i);
            self.finish(i, -Errno::CANCELED.raw_os_error(), 0, true);
            return;
        }
        // Multishot poll reports edges, as on the ring. A slot is on the list at most once.
        if matches!(s.op, Op::Poll { .. }) || s.on_retry {
            return;
        }
        (s.on_retry, s.retry_next) = (true, NONE);
        match self.retry.tail {
            NONE => self.retry.head = i,
            t => self.slots[t as usize].retry_next = i,
        }
        self.retry.tail = i;
    }

    /// Take `i` off the retry list, if on it.
    fn unretry(&mut self, i: u32) {
        if !self.slots[i as usize].on_retry {
            return;
        }
        let (mut prev, mut at) = (NONE, self.retry.head);
        while at != NONE && at != i {
            prev = at;
            at = self.slots[at as usize].retry_next;
        }
        let next = self.slots[i as usize].retry_next;
        if at == i {
            match prev {
                NONE => self.retry.head = next,
                p => self.slots[p as usize].retry_next = next,
            }
            if self.retry.tail == i {
                self.retry.tail = prev;
            }
        }
        let s = &mut self.slots[i as usize];
        (s.on_retry, s.retry_next) = (false, NONE);
    }

    fn run_retries(&mut self) {
        let mut i = core::mem::replace(&mut self.retry, List::EMPTY).head;
        while i != NONE {
            let s = &mut self.slots[i as usize];
            let next = s.retry_next;
            // Off the list before the try, so a finish does not look for it there.
            (s.on_retry, s.retry_next) = (false, NONE);
            if s.state == State::Flying {
                match self.attempt(i) {
                    Try::Again => {}
                    Try::More(r, f) => self.finish(i, r, f | F_MORE, false),
                    Try::Done(r, f) => {
                        self.unlink(i);
                        self.finish(i, r, f, true);
                    }
                }
            }
            i = next;
        }
    }

    pub(super) fn tick_epoll(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.run_retries();
        let mut wait = if self.done.head != NONE {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        if let Some(at) = self.earliest() {
            let left = at.saturating_duration_since(Instant::now());
            wait = Some(wait.map_or(left, |w| w.min(left)));
        }
        // Clamped, past it the second goes negative and the wait fails.
        let ts = wait
            .map(|d| d.min(FOREVER))
            .map(|d| rustix::time::Timespec {
                tv_sec: d.as_secs() as i64,
                tv_nsec: d.subsec_nanos() as i64,
            });
        let mut events = [MaybeUninit::<epoll::Event>::uninit(); EVENTS];
        let mut ready = [(0u64, epoll::EventFlags::empty()); EVENTS];
        let n = {
            let super::Engine::Epoll(p) = &self.engine else {
                return Ok(());
            };
            match epoll::wait(&p.ep, &mut events[..], ts.as_ref()) {
                Ok((got, _)) => {
                    for (r, e) in ready.iter_mut().zip(got.iter()) {
                        *r = (e.data.u64(), e.flags);
                    }
                    got.len()
                }
                Err(Errno::INTR) => 0,
                Err(e) => return Err(IoError::Kernel(e)),
            }
        };
        let mut timer = false;
        for &(tag, flags) in &ready[..n] {
            if tag == TIMER {
                timer = true;
                continue;
            }
            let fd = tag as RawFd;
            let broken = flags.intersects(epoll::EventFlags::ERR | epoll::EventFlags::HUP);
            if broken || flags.intersects(epoll::EventFlags::IN | epoll::EventFlags::RDHUP) {
                self.run(fd, false);
            }
            if broken || flags.contains(epoll::EventFlags::OUT) {
                self.run(fd, true);
            }
        }
        self.fire_timers(timer);
        Ok(())
    }

    /// Try one side's waiters oldest first until one would block.
    fn run(&mut self, fd: RawFd, write: bool) {
        if fd < 0 || fd as usize >= self.watches.len() {
            return;
        }
        let mut prev = NONE;
        let mut i = self.list(fd, write).head;
        while i != NONE {
            let next = self.slots[i as usize].wait_next;
            if self.slots[i as usize].state == State::Queued {
                // Last delivery not yet taken.
                (prev, i) = (i, next);
                continue;
            }
            match self.attempt(i) {
                Try::Again => break,
                Try::More(r, f) => {
                    self.finish(i, r, f | F_MORE, false);
                    prev = i;
                }
                Try::Done(r, f) => {
                    self.unlink_after(fd, write, prev, i);
                    self.finish(i, r, f, true);
                }
            }
            i = next;
        }
    }

    pub(super) fn earliest(&mut self) -> Option<Instant> {
        if self.timers == 0 {
            return None;
        }
        self.slots.iter().filter_map(|s| s.at).min()
    }

    fn arm_timer(&mut self) {
        let at = self.earliest();
        if let super::Engine::Epoll(p) = &self.engine {
            p.arm(at);
        }
    }

    /// Fire due timeouts earliest first and drain the timerfd.
    fn fire_timers(&mut self, fired: bool) {
        let mut sink = [0u8; 8];
        let expired = match &self.engine {
            super::Engine::Epoll(p) if fired => rustix::io::read(&p.timer, &mut sink).is_ok(),
            _ => false,
        };
        let waiting = self.timers;
        self.fire_due();
        if expired || self.timers != waiting {
            self.arm_timer();
        }
    }

    fn fire_due(&mut self) {
        let now = Instant::now();
        while self.timers > 0 {
            let due = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.at.filter(|at| *at <= now).map(|at| (at, i)));
            let Some((_, i)) = due.min() else { return };
            self.slots[i].at = None;
            self.timers -= 1;
            self.finish(i as u32, -Errno::TIME.raw_os_error(), 0, true);
        }
    }

    /// One non-blocking try.
    fn attempt(&mut self, i: u32) -> Try {
        let op = self.slots[i as usize].op;
        match op {
            Op::Recv { fd, mem } => once(sys::recv(raw(fd), mem)),
            Op::Send { fd, mem } => once(sys::send(raw(fd), mem)),
            Op::SendTo { fd, mem, to } => once(sys::send_to(raw(fd), mem, to)),
            Op::RecvFrom { fd, mem } => {
                // SAFETY: as in `reap_done`, no kernel holds this side.
                let side = unsafe { &mut *self.sides.at(i as usize) };
                once(sys::recv_from(
                    raw(fd),
                    mem,
                    &mut side.name,
                    &mut side.hdr.msg_namelen,
                ))
            }
            Op::RecvAll { fd, mem } => self.recv_all(i, raw(fd), mem),
            Op::SendMsg {
                fd,
                mem,
                control,
                to,
            } => once(sys::send_msg(raw(fd), mem, control, to)),
            Op::Splice { from, at, to, len } => {
                // A source at an offset is a file, which never waits.
                let src = if at == POSITION { raw(from) } else { -1 };
                let r = sys::unblocked([src, raw(to)], || sys::splice(raw(from), at, raw(to), len));
                self.spliced(r, raw(to))
            }
            Op::RecvMsgMulti {
                fd,
                group,
                name,
                control,
            } => self.recvmsg_group(raw(fd), group.0 as usize, name, control),
            Op::Read { fd, mem, .. } => self.at_position(raw(fd), mem, false),
            Op::Write { fd, mem, .. } => self.at_position(raw(fd), mem, true),
            Op::Accept { fd, .. } if !self.acceptable(raw(fd)) => Try::Again,
            Op::Accept { fd, multi: true } => repeat(sys::accept(raw(fd), None), true, 0),
            Op::Accept { fd, multi: false } => {
                // SAFETY: as in `reap_done`.
                let side = unsafe { &mut *self.sides.at(i as usize) };
                once(sys::accept(
                    raw(fd),
                    Some((&mut side.name, &mut side.namelen)),
                ))
            }
            Op::Connect { fd, .. } => once(sys::connected(raw(fd))),
            Op::Poll { fd, events, multi } => match sys::poll(raw(fd), events) {
                0 => Try::Again,
                r => repeat(r, multi, 0),
            },
            Op::RecvMulti { fd, group } => self.recv_group(raw(fd), group.0 as usize),
            _ => Try::Done(-Errno::INVAL.raw_os_error(), 0),
        }
    }

    /// A splice's try. `EAGAIN` with room at `to` means the source is empty.
    fn spliced(&self, r: i32, to: RawFd) -> Try {
        if r == -Errno::AGAIN.raw_os_error() && sys::poll(to, libc::POLLOUT as u32) != 0 {
            return Try::Done(r, 0);
        }
        once(r)
    }

    /// A read or write at the position that never blocks the loop. A regular file, which
    /// epoll cannot wait on, blocks as a file op. Without `RWF_NOWAIT` support the try
    /// runs once the descriptor polls ready, a write at most `PIPE_BUF` long so a pipe
    /// with room takes all of it.
    fn at_position(&mut self, fd: RawFd, mem: Mem, write: bool) -> Try {
        let r = sys::nowait(fd, mem, write);
        let unsupported =
            r == -Errno::OPNOTSUPP.raw_os_error() || r == -Errno::NOSYS.raw_os_error();
        if r != -Errno::AGAIN.raw_os_error() && !unsupported {
            return Try::Done(r, 0);
        }
        // Added to epoll means pollable, so no stat is needed past the first wait.
        let added = self.watches.get(fd as usize).is_some_and(|w| w.added);
        if !added && sys::regular(fd) {
            return once(sys::rw(fd, mem, write));
        }
        if !unsupported {
            return Try::Again;
        }
        let events = if write { libc::POLLOUT } else { libc::POLLIN };
        match sys::poll(fd, events as u32) {
            0 => Try::Again,
            r if r < 0 => Try::Done(r, 0),
            _ if write => once(sys::rw(fd, mem.part(0, libc::PIPE_BUF), true)),
            _ => once(sys::rw(fd, mem, false)),
        }
    }

    /// An accept can be tried without blocking. A blocking listener waits for readable.
    fn acceptable(&mut self, fd: RawFd) -> bool {
        let Some(w) = self.watches.get_mut(fd as usize) else {
            return true;
        };
        if w.blocks == Blocks::Unknown {
            w.blocks = match sys::blocks(fd) {
                true => Blocks::Yes,
                false => Blocks::No,
            };
        }
        w.blocks == Blocks::No || sys::poll(fd, libc::POLLIN as u32) != 0
    }

    fn recv_all(&mut self, i: u32, fd: RawFd, mem: Mem) -> Try {
        loop {
            let have = self.slots[i as usize].progress;
            let r = sys::recv(fd, mem.part(have as usize, mem.len()));
            match r {
                r if r == -Errno::AGAIN.raw_os_error() => return Try::Again,
                r if r < 0 && have == 0 => return Try::Done(r, 0),
                r if r <= 0 => return Try::Done(have as i32, 0),
                r if have as usize + r as usize >= mem.len() => {
                    return Try::Done(mem.len() as i32, 0);
                }
                r => self.slots[i as usize].progress = have + r as u32,
            }
        }
    }

    /// The group's next buffer, as the kernel would take it. Ends the op with `ENOBUFS`
    /// for an empty or removed group.
    fn next_buffer(&self, group: usize) -> Result<(Mem, u16), Try> {
        match self.groups.entry.get(group) {
            Some(Some(g)) if g.removed || g.head == g.tail => {
                Err(Try::Done(-Errno::NOBUFS.raw_os_error(), 0))
            }
            Some(Some(g)) => Ok(ring_entry(
                g.ring.cast::<RawBuf>(),
                g.head & (g.entries - 1),
            )),
            _ => Err(Try::Done(-Errno::INVAL.raw_os_error(), 0)),
        }
    }

    /// The group's next buffer was filled, so it is taken and handed back with the result.
    fn take_buffer(&mut self, group: usize, bid: u16) {
        if let Some(Some(g)) = self.groups.entry.get_mut(group) {
            g.head = g.head.wrapping_add(1);
        }
        if let Some(h) = self.groups.holds.get_mut(group) {
            h.remove(bid);
        }
    }

    /// Multishot recvmsg into the group's next buffer, in the ring's layout.
    fn recvmsg_group(&mut self, fd: RawFd, group: usize, name: bool, control: u32) -> Try {
        let (mem, bid) = match self.next_buffer(group) {
            Ok(b) => b,
            Err(t) => return t,
        };
        match sys::recv_msg_out(fd, mem, name, control) {
            r if r == -Errno::AGAIN.raw_os_error() => Try::Again,
            r if r < 0 => Try::Done(r, 0),
            r => {
                self.take_buffer(group, bid);
                let flags = F_BUFFER | (bid as u32) << 16;
                if sys::payload_of(mem) == 0 {
                    Try::Done(r, flags)
                } else {
                    Try::More(r, flags)
                }
            }
        }
    }

    /// Multishot recv into the group's next buffer. Consumed only on data.
    fn recv_group(&mut self, fd: RawFd, group: usize) -> Try {
        let (mem, bid) = match self.next_buffer(group) {
            Ok(b) => b,
            Err(t) => return t,
        };
        match sys::recv(fd, mem) {
            r if r == -Errno::AGAIN.raw_os_error() => Try::Again,
            0 => Try::Done(0, 0),
            r if r < 0 => Try::Done(r, 0),
            r => {
                self.take_buffer(group, bid);
                Try::More(r, F_BUFFER | (bid as u32) << 16)
            }
        }
    }
}

/// Descriptor an op waits on, and whether on the write side.
fn side_of(op: &Op) -> (Fd, bool) {
    match *op {
        Op::Send { fd, .. }
        | Op::SendTo { fd, .. }
        | Op::SendMsg { fd, .. }
        | Op::Write { fd, .. }
        | Op::Connect { fd, .. }
        | Op::Splice { to: fd, .. } => (fd, true),
        Op::Poll { fd, events, .. } => (fd, events & libc::POLLIN as u32 == 0),
        Op::Recv { fd, .. }
        | Op::RecvMulti { fd, .. }
        | Op::RecvFrom { fd, .. }
        | Op::RecvAll { fd, .. }
        | Op::RecvMsgMulti { fd, .. }
        | Op::Read { fd, .. }
        | Op::Accept { fd, .. } => (fd, false),
        _ => (Fd::Fixed(NONE), false),
    }
}

fn raw(fd: Fd) -> RawFd {
    match fd {
        Fd::Raw(fd) => fd,
        Fd::Fixed(_) => -1,
    }
}

fn once(r: i32) -> Try {
    if r == -Errno::AGAIN.raw_os_error() {
        Try::Again
    } else {
        Try::Done(r, 0)
    }
}

/// Result of a maybe-multishot op. Errors always end it.
fn repeat(r: i32, multi: bool, flags: u32) -> Try {
    match r {
        r if r == -Errno::AGAIN.raw_os_error() => Try::Again,
        r if multi && r >= 0 => Try::More(r, flags),
        r => Try::Done(r, flags),
    }
}

/// Run a file op synchronously.
fn file(op: Op) -> i32 {
    match op {
        Op::Nop => 0,
        Op::Read { fd, mem, at } => sys::pread(raw(fd), mem, at),
        Op::Write { fd, mem, at } => sys::pwrite(raw(fd), mem, at),
        Op::Open {
            dir,
            path,
            flags,
            mode,
        } => sys::open(dir, path, flags.bits() as i32, mode.bits()),
        Op::Statx {
            dir,
            path,
            out,
            follow,
        } => sys::statx(dir, path, out, follow),
        Op::Rename {
            from_dir,
            from,
            to_dir,
            to,
        } => sys::rename(from_dir, from, to_dir, to),
        Op::Unlink { dir, path, rmdir } => sys::unlink(dir, path, rmdir),
        Op::Fsync { fd, data_only } => sys::fsync(raw(fd), data_only),
        Op::SyncFileRange { fd, at, len, flags } => sys::sync_range(raw(fd), at, len, flags),
        Op::Fadvise {
            fd,
            at,
            len,
            advice,
        } => sys::fadvise(raw(fd), at, len, advice),
        Op::Fallocate { fd, at, len } => sys::fallocate(raw(fd), at, len),
        Op::Truncate { fd, len } => sys::truncate(raw(fd), len),
        Op::SetSockOpt {
            fd,
            level,
            name,
            value,
        } => sys::set_sockopt(raw(fd), level, name, value),
        _ => -Errno::INVAL.raw_os_error(),
    }
}

/// Raw syscalls returning what the ring would, a count or fd or negated errno.
/// `EINTR` is retried.
mod sys {
    use std::net::SocketAddr;
    use std::os::fd::RawFd;

    use rustix::net::SocketAddrAny;
    use rustix::net::addr::SocketAddrStorage;

    use super::super::Mem;

    fn ret(r: isize) -> i32 {
        if r < 0 {
            -std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            r.min(i32::MAX as isize) as i32
        }
    }

    fn retry(mut f: impl FnMut() -> isize) -> i32 {
        loop {
            let r = ret(f());
            if r != -libc::EINTR {
                return r;
            }
        }
    }

    // SAFETY for every call below: arguments are integers, raw fd numbers and memory
    // within a `Mem`'s length, vouched for by its maker, or locals that outlive the call.
    // A raw number names whatever the caller holds open. A stale one gets `EBADF` or acts
    // on another descriptor, never on memory. `close` takes a number the queue owns, see
    // `Queue::close`. In `recv_msg_out` the name room and control bytes lie inside `m`
    // after its 16-byte header, as `head <= m.len()` is checked first.

    /// `f` with each blocking descriptor of `fds` non-blocking, put back after. Negative
    /// numbers are skipped. A flag that cannot be read or set fails the call, not risking
    /// a block.
    pub(super) fn unblocked(fds: [RawFd; 2], f: impl FnOnce() -> i32) -> i32 {
        let mut old = [-1; 2];
        let mut r = 0;
        for (k, &fd) in fds.iter().enumerate() {
            if fd < 0 || (k == 1 && fd == fds[0]) {
                continue;
            }
            let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if fl < 0 {
                r = ret(-1);
                break;
            }
            if fl & libc::O_NONBLOCK != 0 {
                continue;
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) } < 0 {
                r = ret(-1);
                break;
            }
            old[k] = fl;
        }
        if r == 0 {
            r = f();
        }
        for (&fd, fl) in fds.iter().zip(old) {
            if fl >= 0 {
                // Fails only for a number gone meanwhile, whose flag no longer matters.
                unsafe { libc::fcntl(fd, libc::F_SETFL, fl) };
            }
        }
        r
    }

    /// `connect(2)`, see [`unblocked`] for a blocking socket.
    pub(super) fn connect(fd: RawFd, to: SocketAddr) -> i32 {
        let any = SocketAddrAny::from(to);
        retry(|| unsafe { libc::connect(fd, any.as_ptr().cast(), any.addr_len()) } as isize)
    }

    /// `epoll_ctl(2)` on a raw number, with its own number as the tag.
    pub(super) fn epoll_ctl(
        ep: &std::os::fd::OwnedFd,
        op: i32,
        fd: RawFd,
        events: u32,
    ) -> Result<(), rustix::io::Errno> {
        use std::os::fd::AsRawFd;
        let mut ev = libc::epoll_event {
            events,
            u64: fd as u64,
        };
        match unsafe { libc::epoll_ctl(ep.as_raw_fd(), op, fd, &mut ev) } {
            0 => Ok(()),
            _ => Err(
                rustix::io::Errno::from_io_error(&std::io::Error::last_os_error())
                    .unwrap_or(rustix::io::Errno::IO),
            ),
        }
    }

    pub(super) fn recv(fd: RawFd, m: Mem) -> i32 {
        retry(|| unsafe { libc::recv(fd, m.ptr().cast(), m.len(), libc::MSG_DONTWAIT) })
    }

    pub(super) fn send(fd: RawFd, m: Mem) -> i32 {
        retry(|| unsafe {
            libc::send(
                fd,
                m.ptr().cast(),
                m.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        })
    }

    pub(super) fn send_to(fd: RawFd, m: Mem, to: SocketAddr) -> i32 {
        let any = SocketAddrAny::from(to);
        let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
        retry(|| unsafe {
            libc::sendto(
                fd,
                m.ptr().cast(),
                m.len(),
                flags,
                any.as_ptr().cast(),
                any.addr_len(),
            )
        })
    }

    pub(super) fn recv_from(
        fd: RawFd,
        m: Mem,
        name: &mut SocketAddrStorage,
        len: &mut libc::socklen_t,
    ) -> i32 {
        *len = size_of::<SocketAddrStorage>() as libc::socklen_t;
        let name: *mut libc::sockaddr = (name as *mut SocketAddrStorage).cast();
        retry(|| unsafe {
            libc::recvfrom(
                fd,
                m.ptr().cast(),
                m.len(),
                libc::MSG_DONTWAIT,
                name,
                &mut *len,
            )
        })
    }

    pub(super) fn send_msg(fd: RawFd, m: Mem, control: Mem, to: Option<SocketAddr>) -> i32 {
        let any = to.map(SocketAddrAny::from);
        let mut iov = libc::iovec {
            iov_base: m.ptr().cast(),
            iov_len: m.len(),
        };
        // SAFETY: a header of integers and pointers, all zeros is a value.
        let mut hdr: libc::msghdr = unsafe { core::mem::zeroed() };
        (hdr.msg_iov, hdr.msg_iovlen) = (&mut iov, 1);
        (hdr.msg_control, hdr.msg_controllen) = (control.ptr().cast(), control.len());
        if let Some(any) = &any {
            (hdr.msg_name, hdr.msg_namelen) = (any.as_ptr().cast_mut().cast(), any.addr_len());
        }
        let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
        retry(|| unsafe { libc::sendmsg(fd, &hdr, flags) })
    }

    /// Receive into `m` in `io_uring_recvmsg_out` layout. Returns total bytes.
    pub(super) fn recv_msg_out(fd: RawFd, m: Mem, name: bool, control: u32) -> i32 {
        let name_room = if name {
            size_of::<SocketAddrStorage>()
        } else {
            0
        };
        let head = 16 + name_room + control as usize;
        if m.len() < head || head > i32::MAX as usize {
            return -libc::ENOBUFS;
        }
        // At most what keeps the total an `i32` count.
        let body = m.part(head, i32::MAX as usize - head);
        let mut iov = libc::iovec {
            iov_base: body.ptr().cast(),
            iov_len: body.len(),
        };
        // SAFETY: a header of integers and pointers, all zeros is a value.
        let mut hdr: libc::msghdr = unsafe { core::mem::zeroed() };
        (hdr.msg_iov, hdr.msg_iovlen) = (&mut iov, 1);
        let room = m.ptr().wrapping_add(16);
        (hdr.msg_name, hdr.msg_namelen) = (room.cast(), name_room as libc::socklen_t);
        if control > 0 {
            (hdr.msg_control, hdr.msg_controllen) =
                (room.wrapping_add(name_room).cast(), control as usize);
        }
        let n = retry(|| unsafe { libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT) });
        if n < 0 {
            return n;
        }
        let words = [
            hdr.msg_namelen,
            hdr.msg_controllen as u32,
            n as u32,
            hdr.msg_flags as u32,
        ];
        // SAFETY: `m` holds the header's sixteen bytes, checked above.
        unsafe { core::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), m.ptr(), 16) };
        // `n` is at most the body, so the sum fits, saturated all the same.
        i32::try_from(head + n as usize).unwrap_or(i32::MAX)
    }

    /// Payload length from a [`recv_msg_out`] header.
    pub(super) fn payload_of(m: Mem) -> u32 {
        let mut w = [0u8; 4];
        // SAFETY: the header was written, at least sixteen bytes.
        unsafe { core::ptr::copy_nonoverlapping(m.ptr().wrapping_add(8), w.as_mut_ptr(), 4) };
        u32::from_ne_bytes(w)
    }

    pub(super) fn splice(from: RawFd, at: u64, to: RawFd, len: u32) -> i32 {
        let mut off = at as libc::loff_t;
        let offp = if at == super::POSITION {
            core::ptr::null_mut()
        } else {
            &mut off as *mut libc::loff_t
        };
        let flags = libc::SPLICE_F_NONBLOCK;
        retry(|| unsafe {
            libc::splice(from, offp, to, core::ptr::null_mut(), len as usize, flags)
        })
    }

    pub(super) fn fallocate(fd: RawFd, at: u64, len: u64) -> i32 {
        retry(|| unsafe { libc::fallocate(fd, 0, at as libc::off_t, len as libc::off_t) } as isize)
    }

    pub(super) fn truncate(fd: RawFd, len: u64) -> i32 {
        retry(|| unsafe { libc::ftruncate(fd, len as libc::off_t) } as isize)
    }

    pub(super) fn set_sockopt(fd: RawFd, level: u32, name: u32, v: Mem) -> i32 {
        let len = v.len() as libc::socklen_t;
        retry(
            || unsafe { libc::setsockopt(fd, level as i32, name as i32, v.ptr().cast(), len) }
                as isize,
        )
    }

    /// Plain `read(2)` or `write(2)` at the position, which may block.
    pub(super) fn rw(fd: RawFd, m: Mem, write: bool) -> i32 {
        retry(|| unsafe {
            match write {
                true => libc::write(fd, m.ptr().cast(), m.len()),
                false => libc::read(fd, m.ptr().cast(), m.len()),
            }
        })
    }

    /// `preadv2` or `pwritev2` at the position with `RWF_NOWAIT`, so a blocking pipe or
    /// socket gives `EAGAIN` rather than blocking. `EOPNOTSUPP` for a file without a
    /// non-blocking path.
    pub(super) fn nowait(fd: RawFd, m: Mem, write: bool) -> i32 {
        let iov = libc::iovec {
            iov_base: m.ptr().cast(),
            iov_len: m.len(),
        };
        let nr = if write {
            libc::SYS_pwritev2
        } else {
            libc::SYS_preadv2
        };
        // Offset -1 in both halves is the position, on 32 and 64 bit alike.
        let at: libc::c_long = -1;
        let iov: *const libc::iovec = &iov;
        retry(|| unsafe { libc::syscall(nr, fd, iov, 1, at, at, libc::RWF_NOWAIT) } as isize)
    }

    /// A regular file or block device, which epoll cannot wait on.
    pub(super) fn regular(fd: RawFd) -> bool {
        let mut st = core::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
            return false;
        }
        // SAFETY: `fstat` succeeded and filled it.
        let mode = unsafe { st.assume_init() }.st_mode & libc::S_IFMT;
        mode == libc::S_IFREG || mode == libc::S_IFBLK
    }

    /// Lacks `O_NONBLOCK`. A failed ask counts as blocking, the cautious answer.
    pub(super) fn blocks(fd: RawFd) -> bool {
        let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        fl < 0 || fl & libc::O_NONBLOCK == 0
    }

    /// An open descriptor number.
    pub(super) fn is_open(fd: RawFd) -> bool {
        (unsafe { libc::fcntl(fd, libc::F_GETFD) }) >= 0
    }

    pub(super) fn pread(fd: RawFd, m: Mem, at: u64) -> i32 {
        retry(|| unsafe { libc::pread(fd, m.ptr().cast(), m.len(), at as libc::off_t) })
    }

    pub(super) fn pwrite(fd: RawFd, m: Mem, at: u64) -> i32 {
        retry(|| unsafe { libc::pwrite(fd, m.ptr().cast(), m.len(), at as libc::off_t) })
    }

    pub(super) fn accept(
        fd: RawFd,
        peer: Option<(&mut SocketAddrStorage, &mut libc::socklen_t)>,
    ) -> i32 {
        let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
        let (name, len): (*mut libc::sockaddr, *mut libc::socklen_t) = match peer {
            Some((name, len)) => {
                *len = size_of::<SocketAddrStorage>() as libc::socklen_t;
                ((name as *mut SocketAddrStorage).cast(), len)
            }
            None => (core::ptr::null_mut(), core::ptr::null_mut()),
        };
        retry(|| unsafe { libc::accept4(fd, name, len, flags) } as isize)
    }

    /// `SO_ERROR` of a pending connect.
    pub(super) fn connected(fd: RawFd) -> i32 {
        let mut err: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        let p = (&mut err as *mut libc::c_int).cast();
        match ret(
            unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, p, &mut len) } as isize,
        ) {
            0 => -err,
            r => r,
        }
    }

    /// Ready subset of `events`, or zero.
    pub(super) fn poll(fd: RawFd, events: u32) -> i32 {
        let mut one = libc::pollfd {
            fd,
            events: events as i16,
            revents: 0,
        };
        match retry(|| unsafe { libc::poll(&mut one, 1, 0) } as isize) {
            r if r < 0 => r,
            _ => {
                (one.revents as u16 as i32)
                    & (events as i32 | libc::POLLERR as i32 | libc::POLLHUP as i32)
            }
        }
    }

    fn path(m: Mem) -> *const libc::c_char {
        if m.is_empty() {
            c"".as_ptr()
        } else {
            m.ptr().cast()
        }
    }

    pub(super) fn open(dir: RawFd, p: Mem, flags: i32, mode: u32) -> i32 {
        retry(|| unsafe { libc::openat(dir, path(p), flags, mode) } as isize)
    }

    pub(super) fn statx(dir: RawFd, p: Mem, out: Mem, follow: bool) -> i32 {
        let flags = super::super::uring::statx_flags(p.is_empty(), follow);
        let buf = out.ptr().cast::<libc::statx>();
        retry(|| unsafe { libc::statx(dir, path(p), flags, libc::STATX_BASIC_STATS, buf) } as isize)
    }

    pub(super) fn rename(from_dir: RawFd, from: Mem, to_dir: RawFd, to: Mem) -> i32 {
        retry(|| unsafe { libc::renameat(from_dir, path(from), to_dir, path(to)) } as isize)
    }

    pub(super) fn unlink(dir: RawFd, p: Mem, rmdir: bool) -> i32 {
        let flags = if rmdir { libc::AT_REMOVEDIR } else { 0 };
        retry(|| unsafe { libc::unlinkat(dir, path(p), flags) } as isize)
    }

    pub(super) fn close(fd: RawFd) -> i32 {
        // Not retried, the fd is closed even after EINTR.
        ret(unsafe { libc::close(fd) } as isize)
    }

    pub(super) fn sync_range(fd: RawFd, at: u64, len: u32, flags: u32) -> i32 {
        let (at, len) = (at as libc::off64_t, len as libc::off64_t);
        retry(|| unsafe { libc::sync_file_range(fd, at, len, flags) } as isize)
    }

    /// Returns the error itself, as `posix_fadvise` does not set `errno`.
    pub(super) fn fadvise(fd: RawFd, at: u64, len: u32, advice: i32) -> i32 {
        let (at, len) = (at as libc::off_t, len as libc::off_t);
        -unsafe { libc::posix_fadvise(fd, at, len, advice) }
    }

    pub(super) fn fsync(fd: RawFd, data_only: bool) -> i32 {
        retry(|| unsafe {
            if data_only {
                libc::fdatasync(fd)
            } else {
                libc::fsync(fd)
            }
        } as isize)
    }
}
