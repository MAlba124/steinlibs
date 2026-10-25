//! Socket ops, sockopts, cmsg send/recv, pipes, splice and linked chains.

use rustix::pipe::PipeFlags;

use super::*;
use crate::msg;

impl Io {
    /// The socket is driver-owned and named by [`Op::fd`].
    pub fn connect(&mut self, addr: SocketAddr) -> Result<OpId, IoError> {
        let (fd, started) = net::tcp_connect(addr).map_err(IoError::Kernel)?;
        self.start_connect(fd, started)
    }

    /// Unix stream connect, as [`Io::connect`].
    pub fn connect_unix(&mut self, to: net::UnixAddr<'_>) -> Result<OpId, IoError> {
        let (fd, started) = net::unix_connect(to).map_err(IoError::Kernel)?;
        self.start_connect(fd, started)
    }

    /// The socket is closed again if no op takes it, the caller never saw its number.
    fn start_connect(&mut self, fd: OwnedFd, started: net::Started) -> Result<OpId, IoError> {
        let raw_fd = fd.as_raw_fd();
        self.keep(raw_fd, fd);
        let id = match self.insert(Op::new(Kind::Connect, raw_fd)) {
            Ok(id) => id,
            Err(e) => {
                self.close(raw_fd);
                return Err(e);
            }
        };
        match started {
            // Settled in the syscall. No readiness event will come.
            net::Started::Done => self.retire(id, Ok(0), None),
            net::Started::Failed(e) => self.retire(id, Err(e), None),
            // In progress, poll for writable.
            net::Started::Pending => {
                let poll = queue::Op::Poll {
                    fd: raw(raw_fd),
                    events: libc::POLLOUT as u32,
                    multi: false,
                };
                if let Err(e) = self.submit(id, poll) {
                    self.close(raw_fd);
                    return Err(e);
                }
            }
        }
        Ok(id)
    }

    /// Synchronous, a UDP connect cannot block.
    pub fn connect_udp(&mut self, addr: SocketAddr) -> Result<RawFd, IoError> {
        let fd = net::udp_connect(addr).map_err(IoError::Kernel)?;
        Ok(self.own(fd))
    }

    /// Unconnected UDP socket for [`Io::recv_from`] and [`Io::send_to`].
    pub fn bind_udp(&mut self, addr: SocketAddr) -> Result<RawFd, IoError> {
        let fd = net::udp_bind(addr).map_err(IoError::Kernel)?;
        Ok(self.own(fd))
    }

    /// Synchronous bind and listen.
    pub fn listen(&mut self, addr: SocketAddr) -> Result<RawFd, IoError> {
        let fd = net::tcp_listen(addr).map_err(IoError::Kernel)?;
        Ok(self.own(fd))
    }

    /// Result is the new driver-owned fd, addr is the peer. `repeating` rearms per connection.
    pub fn accept(&mut self, fd: RawFd, repeating: bool) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Accept, fd);
        op.multishot = repeating;
        let id = self.insert(op)?;
        self.submit(
            id,
            queue::Op::Accept {
                fd: raw(fd),
                multi: false,
            },
        )?;
        Ok(id)
    }

    pub(super) fn done_accept(&mut self, id: OpId, op: Op, c: Cqe) {
        if c.res < 0 {
            return self.retire(id, Err(Errno::from_raw_os_error(-c.res)), None);
        }
        // SAFETY: fresh from the kernel, nothing else has its number.
        let new = unsafe { OwnedFd::from_raw_fd(c.res) };
        let fd = new.as_raw_fd();
        let addr = c.addr.or_else(|| net::peer_addr(&new));
        self.keep(fd, new);
        if !op.multishot {
            return self.retire_with(id, Ok(fd as u32), None, addr);
        }
        self.deliver_more(id, op, Ok(fd as u32), None, addr);
        // Single-shot accept, rearmed.
        let r = self.resubmit(
            id,
            queue::Op::Accept {
                fd: raw(op.fd),
                multi: false,
            },
        );
        self.resubmit_failed(id, op, r);
    }

    /// Driver supplies the buffer. `repeating` stays armed, multishot on the buffer ring.
    pub fn recv(&mut self, fd: RawFd, repeating: bool) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Recv, fd);
        op.multishot = repeating;
        let id = self.insert(op)?;
        self.arm_recv(id, fd, false)?;
        Ok(id)
    }

    /// Arm a receive, a re-arm on the driver's own behalf with `own`.
    pub(super) fn arm_recv(&mut self, id: OpId, fd: RawFd, own: bool) -> Result<(), IoError> {
        let repeating = self.ops.get(id).is_some_and(|o| o.multishot);
        if let Some(group) = self.group.filter(|_| repeating) {
            // Kernel picks a ring buffer on arrival, idle sockets hold none.
            return self.push_op(id, queue::Op::RecvMulti { fd: raw(fd), group }, own);
        }
        let Ok(bid) = self.bufs.take_lent() else {
            // No buffer, rearm when one frees.
            self.rearm.push(id);
            return Ok(());
        };
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = Some(bid);
        }
        let mem = self.buffer(bid, 0, u32::MAX);
        self.push_op(id, queue::Op::Recv { fd: raw(fd), mem }, own)
    }

    pub(super) fn done_recv(&mut self, id: OpId, op: Op, c: Cqe) {
        let Some(bid) = op.buf else {
            // Kernel picked from the ring.
            return self.done_selected(id, op, c);
        };
        if c.res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-c.res));
        }
        let n = c.res as u32;
        let buf = Some(self.landed(bid, n));
        // EOF ends even a repeating receive.
        if !op.multishot || n == 0 {
            return self.retire(id, Ok(n), buf);
        }
        self.deliver_more(id, op, Ok(n), buf, None);
        // Delivered buffer is the caller's now, rearm with a fresh one.
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = None;
        }
        let r = self.arm_recv(id, op.fd, true);
        self.resubmit_failed(id, op, r);
    }

    /// Receive on a kernel-picked ring buffer.
    fn done_selected(&mut self, id: OpId, op: Op, c: Cqe) {
        if c.res < 0 {
            return self.selected_failed(id, c);
        }
        let n = c.res as u32;
        let buf = self.hold(c.buffer(), n);
        if n == 0 {
            // EOF ends a multishot regardless of the more flag.
            return self.retire(id, Ok(0), buf);
        }
        self.deliver_more(id, op, Ok(n), buf, None);
        if !c.more() {
            // Kernel ended the multishot, usually ring dry. Rearm.
            let r = self.arm_recv(id, op.fd, true);
            self.resubmit_failed(id, op, r);
        }
    }

    fn selected_failed(&mut self, id: OpId, c: Cqe) {
        // ENOBUFS ends the submission, not the connection. Rearm later.
        if -c.res == Errno::NOBUFS.raw_os_error() {
            return self.rearm.push(id);
        }
        if !c.more() {
            self.retire(id, Err(Errno::from_raw_os_error(-c.res)), None);
        }
    }

    /// Claim a kernel-picked buffer.
    fn hold(&mut self, picked: Option<u16>, n: u32) -> Option<Buf> {
        let i = picked?;
        if !self.bufs.claim(i) {
            // Already loaned. Delivering would alias it.
            return None;
        }
        self.ring_free = self.ring_free.saturating_sub(1);
        Some(self.landed(i, n))
    }

    /// Return a ring buffer picked for a cancelled op. Never counted out, so not counted in.
    pub(super) fn offer_again(&mut self, flags: u32) {
        let Some(i) = Cqe {
            flags,
            ..Cqe::EMPTY
        }
        .buffer() else {
            return;
        };
        if !self.bufs.ring_at(i) || self.bufs.loaned_at(i) {
            return;
        }
        if let Some(g) = self.group {
            let m = self.buffer(i, 0, u32::MAX);
            let _ = self.q.provide(g, i, m);
        }
    }

    /// Driver owns and releases `buf` from here, caller must not, whatever this returns.
    /// An error frees it, but [`IoError::NotHeld`] when the caller does not hold it.
    /// Short writes are continued. A `len` past the buffer size is [`IoError::TooLong`].
    pub fn send(&mut self, fd: RawFd, buf: BufId, len: u32) -> Result<OpId, IoError> {
        let buf = self.fits(buf, 0, len)?;
        let mut op = Op::new(Kind::Send, fd);
        op.len = len;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, 0, op.len);
        self.submit(id, queue::Op::Send { fd: raw(fd), mem })?;
        Ok(id)
    }

    pub(super) fn done_send(&mut self, id: OpId, op: Op, res: i32) {
        let Some(bid) = op.buf else {
            return;
        };
        if res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-res));
        }
        let sent = op.sent + res as u32;
        if sent >= op.len || res == 0 {
            self.free_buf(bid);
            return self.retire(id, Ok(sent), None);
        }
        // Short write, submit the rest under the same id.
        if let Some(slot) = self.ops.get_mut(id) {
            slot.sent = sent;
        }
        let mem = self.buffer(bid, sent, op.len - sent);
        let fd = raw(op.fd);
        let rest = match op.kind {
            // Explicit offsets advance, APPEND stays.
            Kind::Write if op.offset != APPEND => queue::Op::Write {
                fd,
                mem,
                at: op.offset + sent as u64,
            },
            Kind::Write => queue::Op::Write {
                fd,
                mem,
                at: APPEND,
            },
            _ => queue::Op::Send { fd, mem },
        };
        let r = self.resubmit(id, rest);
        if r.is_err() {
            self.free_buf(bid);
        }
        self.resubmit_failed(id, op, r);
    }

    /// Send `len` bytes of `data` from `at`, read where they lie, no copy into the arena and
    /// no buffer's size limit. The driver holds `data` until the last completion, or after a
    /// cancel until the kernel lets go. Short writes are continued. Up to 4 GiB.
    pub fn send_shared(
        &mut self,
        fd: RawFd,
        data: Shared,
        at: usize,
        len: usize,
    ) -> Result<OpId, IoError> {
        // Read once, so every resubmit sends from the same memory.
        let bytes = (*data).as_ref();
        let (ptr, whole) = (bytes.as_ptr(), bytes.len());
        let at = at.min(whole);
        let len = len.min(whole - at).min(u32::MAX as usize) as u32;
        let mut op = Op::new(Kind::SendShared, fd);
        (op.offset, op.len) = (at as u64, len);
        let id = self.insert(op)?;
        let held = Held {
            data,
            ptr,
            len: whole,
        };
        let mem = held_mem(&held, at, len);
        if let Some(h) = self.held.get_mut(id.slot() as usize) {
            *h = Some(held);
        }
        self.submit(id, queue::Op::Send { fd: raw(fd), mem })?;
        Ok(id)
    }

    pub(super) fn done_send_shared(&mut self, id: OpId, op: Op, res: i32) {
        if res < 0 {
            self.take_held(id);
            return self.retire(id, Err(Errno::from_raw_os_error(-res)), None);
        }
        let sent = op.sent + res as u32;
        if sent >= op.len || res == 0 {
            self.take_held(id);
            return self.retire(id, Ok(sent), None);
        }
        // Short write, the rest under the same id from the same memory.
        let Some(Some(held)) = self.held.get(id.slot() as usize) else {
            return self.retire(id, Ok(sent), None);
        };
        let mem = held_mem(held, op.offset as usize + sent as usize, op.len - sent);
        if let Some(slot) = self.ops.get_mut(id) {
            slot.sent = sent;
        }
        let r = self.resubmit(
            id,
            queue::Op::Send {
                fd: raw(op.fd),
                mem,
            },
        );
        self.resubmit_failed(id, op, r);
    }

    /// [`Io::recv`] with the sender address. Repeating stays armed through empty datagrams.
    pub fn recv_from(&mut self, fd: RawFd, repeating: bool) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::RecvFrom, fd);
        op.multishot = repeating;
        let id = self.insert(op)?;
        self.arm_recvfrom(id, fd, false)?;
        Ok(id)
    }

    pub(super) fn arm_recvfrom(&mut self, id: OpId, fd: RawFd, own: bool) -> Result<(), IoError> {
        let Ok(bid) = self.bufs.take_lent() else {
            self.rearm.push(id);
            return Ok(());
        };
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = Some(bid);
        }
        let mem = self.buffer(bid, 0, u32::MAX);
        self.push_op(id, queue::Op::RecvFrom { fd: raw(fd), mem }, own)
    }

    pub(super) fn done_recvfrom(&mut self, id: OpId, op: Op, c: Cqe) {
        let Some(bid) = op.buf else {
            return;
        };
        if c.res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-c.res));
        }
        let buf = Some(self.landed(bid, c.res as u32));
        if !op.multishot {
            return self.retire_with(id, Ok(c.res as u32), buf, c.addr);
        }
        self.deliver_more(id, op, Ok(c.res as u32), buf, c.addr);
        if let Some(slot) = self.ops.get_mut(id) {
            slot.buf = None;
        }
        let r = self.arm_recvfrom(id, op.fd, true);
        self.resubmit_failed(id, op, r);
    }

    /// One datagram. Driver owns `buf` as for [`Io::send`].
    pub fn send_to(
        &mut self,
        fd: RawFd,
        buf: BufId,
        len: u32,
        to: SocketAddr,
    ) -> Result<OpId, IoError> {
        let buf = self.fits(buf, 0, len)?;
        let mut op = Op::new(Kind::SendTo, fd);
        op.len = len;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, 0, op.len);
        self.submit(
            id,
            queue::Op::SendTo {
                fd: raw(fd),
                mem,
                to,
            },
        )?;
        Ok(id)
    }

    pub(super) fn done_sendto(&mut self, id: OpId, op: Op, res: i32) {
        let Some(bid) = op.buf else {
            return;
        };
        if res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-res));
        }
        self.free_buf(bid);
        self.retire(id, Ok(res as u32), None);
    }

    pub(super) fn done_connect(&mut self, id: OpId, op: Op, res: i32) {
        if res < 0 {
            return self.retire(id, Err(Errno::from_raw_os_error(-res)), None);
        }
        // Refused connects are writable too, so check `SO_ERROR`.
        let r = match borrow(&self.owned, op.fd) {
            Some(sock) => net::connect_result(sock),
            None => Err(Errno::BADF),
        };
        self.retire(id, r.map(|()| 0), None);
    }

    /// Link the next submitted op to the one after. Repeat to extend.
    /// On failure (a short splice counts) the rest get `ECANCELED`. A batched
    /// [`Io::close`] after it runs once the chain ends, failed or not.
    /// Does not survive a full SQ flush. Epoll ignores it and runs all in order. Ends at the
    /// first op not chained on, or when [`Io::flush`] or [`Io::tick`] submits. The driver's
    /// own entries, re-arms, the rest of short writes, cancels and wake polls, never join
    /// it and wait until it ends.
    pub fn chain(&mut self) {
        self.q.link();
    }

    /// `setsockopt(2)` from `buf`. Driver owns `buf` as for [`Io::send`]. Completes with
    /// `Ok(0)`.
    pub fn set_sockopt(
        &mut self,
        fd: RawFd,
        level: i32,
        name: i32,
        buf: BufId,
        len: u32,
    ) -> Result<OpId, IoError> {
        let buf = self.fits(buf, 0, len)?;
        let (id, bid) = self.insert_on(Op::new(Kind::SetSockOpt, fd), buf)?;
        let value = self.buffer(bid, 0, len);
        let op = queue::Op::SetSockOpt {
            fd: raw(fd),
            level: level as u32,
            name: name as u32,
            value,
        };
        self.submit(id, op)?;
        Ok(id)
    }

    /// Receive exactly `len` bytes into a caller buffer at `at`. Short result means EOF.
    /// Filled length counts the prefix. Failure or cancel releases the buffer, an error
    /// here too, see [`Io::send`]. A range past the buffer size is [`IoError::TooLong`].
    pub fn recv_into(&mut self, fd: RawFd, buf: BufId, at: u32, len: u32) -> Result<OpId, IoError> {
        let buf = self.fits(buf, at, len)?;
        let mut op = Op::new(Kind::RecvInto, fd);
        op.sent = at;
        op.len = len;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, at, len);
        self.submit(id, queue::Op::RecvAll { fd: raw(fd), mem })?;
        Ok(id)
    }

    pub(super) fn done_recv_into(&mut self, id: OpId, op: Op, res: i32) {
        let Some(bid) = op.buf else {
            return;
        };
        if res < 0 {
            return self.fail_read(id, bid, Errno::from_raw_os_error(-res));
        }
        let buf = self.landed(bid, op.sent + res as u32);
        self.retire(id, Ok(res as u32), Some(buf));
    }

    /// `sendmsg(2)` with one cmsg of up to [`msg::CMSG_MAX`] bytes. Driver owns `buf` as
    /// for [`Io::send`].
    /// Short sends are not continued.
    pub fn send_msg(
        &mut self,
        fd: RawFd,
        buf: BufId,
        len: u32,
        control: msg::Control<'_>,
    ) -> Result<OpId, IoError> {
        self.send_msg_with(fd, buf, len, control, None)
    }

    /// [`Io::send_msg`] to `to`, for an unconnected socket.
    pub fn send_msg_to(
        &mut self,
        fd: RawFd,
        buf: BufId,
        len: u32,
        control: msg::Control<'_>,
        to: SocketAddr,
    ) -> Result<OpId, IoError> {
        self.send_msg_with(fd, buf, len, control, Some(to))
    }

    fn send_msg_with(
        &mut self,
        fd: RawFd,
        buf: BufId,
        len: u32,
        control: msg::Control<'_>,
        to: Option<SocketAddr>,
    ) -> Result<OpId, IoError> {
        let too_long = IoError::ControlTooLong {
            len: control.data.len(),
            max: msg::CMSG_MAX,
        };
        let mut room = [0u8; msg::CONTROL_ROOM];
        let encoded = (control.data.len() <= msg::CMSG_MAX)
            .then(|| msg::encode(&mut room, control))
            .flatten();
        let Some(n) = encoded else {
            self.drop_handed(buf);
            return Err(too_long);
        };
        let buf = self.fits(buf, 0, len)?;
        let mut op = Op::new(Kind::SendMsg, fd);
        op.len = len;
        let (id, bid) = self.insert_on(op, buf)?;
        let mem = self.buffer(bid, 0, len);
        let send = queue::Op::SendMsg {
            fd: raw(fd),
            mem,
            control: Mem::EMPTY,
            to,
        };
        let chained = self.q.in_chain();
        let key = match self.q.push_control(send, &room[..n], id.to_user_data()) {
            Ok(key) => key,
            Err(e) => {
                self.take_op(id);
                self.free_buf(bid);
                return Err(e);
            }
        };
        self.submitted(id, key, chained);
        Ok(id)
    }

    /// Multishot recvmsg with cmsg room. Buffers parse with [`Io::msg`], result is payload length.
    /// `Ok(0)` and errors are final, caller rearms (e.g. after `EKEYEXPIRED`). `ENOBUFS` rearms itself.
    /// Needs the provided ring, else [`IoError::NoBufRing`].
    pub fn recv_msg(&mut self, fd: RawFd) -> Result<OpId, IoError> {
        self.start_recvmsg(fd, false)
    }

    /// [`Io::recv_msg`] with the sender address, for an unconnected socket.
    /// Buffers parse with [`Io::msg_from`].
    pub fn recv_msg_from(&mut self, fd: RawFd) -> Result<OpId, IoError> {
        self.start_recvmsg(fd, true)
    }

    fn start_recvmsg(&mut self, fd: RawFd, named: bool) -> Result<OpId, IoError> {
        if self.group.is_none() {
            return Err(IoError::NoBufRing);
        }
        let head = if named { msg::HEAD_NAMED } else { msg::HEAD };
        if self.bufs.size() as usize <= head {
            return Err(IoError::BadArena {
                bytes: head,
                buffer: self.bufs.size() as usize,
            });
        }
        let mut op = Op::new(Kind::RecvMsg, fd);
        op.multishot = true;
        op.aux = u32::from(named);
        let id = self.insert(op)?;
        self.arm_recvmsg(id, fd, false)?;
        Ok(id)
    }

    pub(super) fn arm_recvmsg(&mut self, id: OpId, fd: RawFd, own: bool) -> Result<(), IoError> {
        let Some(group) = self.group else {
            return Ok(());
        };
        let name = self.ops.get(id).is_some_and(|o| o.aux == 1);
        let control = msg::CONTROL_ROOM as u32;
        let op = queue::Op::RecvMsgMulti {
            fd: raw(fd),
            group,
            name,
            control,
        };
        self.push_op(id, op, own)
    }

    /// Parse a [`Kind::RecvMsg`] buffer.
    pub fn msg(&self, b: &Buf) -> Option<msg::Msg<'_>> {
        msg::parse(self.bufs.filled(b))
    }

    /// Parse a buffer of [`Io::recv_msg_from`].
    pub fn msg_from(&self, b: &Buf) -> Option<msg::Msg<'_>> {
        msg::parse_named(self.bufs.filled(b))
    }

    pub(super) fn done_recvmsg(&mut self, id: OpId, op: Op, c: Cqe) {
        if c.res < 0 {
            return self.selected_failed(id, c);
        }
        let Some(buf) = self.hold(c.buffer(), c.res as u32) else {
            return;
        };
        let head = if op.aux == 1 {
            msg::HEAD_NAMED
        } else {
            msg::HEAD
        };
        let n = msg::payload_len_at(self.bufs.filled(&buf), head).unwrap_or(0);
        if n == 0 && !c.more() {
            // Empty and final, stream end.
            return self.retire(id, Ok(0), Some(buf));
        }
        self.deliver_more(id, op, Ok(n), Some(buf), None);
        if !c.more() {
            let r = self.arm_recvmsg(id, op.fd, true);
            self.resubmit_failed(id, op, r);
        }
    }

    /// End an armed op without dropping in-flight completions, unlike [`Io::cancel`].
    /// The last one is final, `ECANCELED` unless it ended on its own.
    pub fn stop(&mut self, id: OpId) -> bool {
        let Some(op) = self.ops.get(id).copied().filter(|o| !o.done) else {
            return false;
        };
        if let Some(i) = self.rearm.iter().position(|r| *r == id) {
            // Waiting for a buffer, nothing in flight.
            self.rearm.swap_remove(i);
            self.retire(id, Err(Errno::CANCELED), None);
            return true;
        }
        let Some(key) = self.key_of(id) else {
            return false;
        };
        let _ = op;
        self.q.cancel(key);
        self.settle_now();
        true
    }

    /// Poll for readable, or writable with `write`. Completes with `Ok(0)`.
    pub fn ready(&mut self, fd: RawFd, write: bool) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Ready, fd);
        op.aux = u32::from(write);
        let id = self.insert(op)?;
        let events = if write { libc::POLLOUT } else { libc::POLLIN } as u32;
        self.submit(
            id,
            queue::Op::Poll {
                fd: raw(fd),
                events,
                multi: false,
            },
        )?;
        Ok(id)
    }

    /// Synchronous. Returns driver-owned (read, write) ends. Blocking mode, ring splices wait on it.
    pub fn pipe(&mut self) -> Result<(RawFd, RawFd), IoError> {
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(IoError::Kernel)?;
        Ok((self.own(r), self.own(w)))
    }

    /// Splice up to `len` bytes, one end must be a pipe. `offset` in `from`, or [`APPEND`].
    /// Short counts are not continued, zero is EOF. Owned by `to`, closing `to` cancels it.
    /// Full socket `to` returns short or `EAGAIN`. On epoll an empty `from` gives `EAGAIN`.
    pub fn splice(
        &mut self,
        from: RawFd,
        offset: u64,
        to: RawFd,
        len: u32,
    ) -> Result<OpId, IoError> {
        let mut op = Op::new(Kind::Splice, to);
        op.aux = from as u32;
        op.offset = offset;
        op.len = len;
        let id = self.insert(op)?;
        self.submit(
            id,
            queue::Op::Splice {
                from: raw(from),
                at: offset,
                to: raw(to),
                len,
            },
        )?;
        Ok(id)
    }
}

/// `len` bytes of held memory from `at` as a queue `Mem`, clamped to it.
fn held_mem(h: &Held, at: usize, len: u32) -> Mem {
    let at = at.min(h.len);
    let len = (len as usize).min(h.len - at);
    // SAFETY: `ptr` and `len` are what `as_ref` gave once at the start, so every resubmit
    // names the same bytes. The driver holds the `Shared` behind them in the op's slot until
    // its last completion, in the quarantine after a cancel, or forever if the drain timed
    // out, so the object, and the bytes it lent out of `&self`, outlive every kernel read.
    // A send only reads them, the pointer is mutable only because `Mem` serves both ways.
    unsafe { Mem::raw(h.ptr.wrapping_add(at).cast_mut(), len) }
}
