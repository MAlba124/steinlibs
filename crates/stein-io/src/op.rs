//! Operation records and their completions.

use core::fmt;
use std::net::SocketAddr;
use std::os::fd::RawFd;

use stein_core::handle::Handle;

use crate::buf::Buf;

/// Names one submitted operation. Round-trips through `user_data`.
pub type OpId = Handle<Op>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Connect,
    /// Into a buffer the driver supplies.
    Recv,
    Send,
    Close,
    /// Owns no descriptor.
    Timeout,
    /// Completes with a new descriptor the driver owns.
    Open,
    /// At an offset, into a buffer the driver supplies.
    Read,
    /// Completes with a buffer [`crate::file::meta`] reads.
    Stat,
    /// At an offset or appended. Short writes are resubmitted.
    Write,
    /// Completes with a new driver-owned descriptor and the peer address.
    Accept,
    RecvFrom,
    SendTo,
    Fallocate,
    /// Data only or data and metadata.
    Sync,
    Truncate,
    Rename,
    Unlink,
    /// Read directory names into a region. Blocks the caller.
    List,
    /// Read a file into a region range.
    ReadMem,
    /// Write a region range to a file.
    WriteMem,
    SetSockOpt,
    /// Receive an exact length into a caller-held buffer.
    RecvInto,
    /// Send with one control message.
    SendMsg,
    /// Receive with control room. Multishot.
    RecvMsg,
    /// One side must be a pipe.
    Splice,
    /// Wait for readable or writable.
    Ready,
    /// Put a value into another driver's completion queue.
    Post,
    /// A posted value. No operation behind it, read with [`Completion::value`].
    Msg,
    /// From caller memory the driver holds until the last completion. Short writes are
    /// resubmitted.
    SendShared,
    /// [`crate::Waker::wake`] was called, once or more since the last. No operation behind it.
    Wake,
}

impl Kind {
    /// Whether this kind can be multishot.
    pub fn can_repeat(&self) -> bool {
        matches!(
            self,
            Kind::Recv | Kind::RecvFrom | Kind::Accept | Kind::RecvMsg
        )
    }

    /// Whether the readiness backend waits on the read side for this.
    pub fn is_read(&self) -> bool {
        matches!(
            self,
            Kind::Recv
                | Kind::RecvFrom
                | Kind::Accept
                | Kind::Read
                | Kind::RecvInto
                | Kind::RecvMsg
        )
    }
}

/// A submitted operation, as the driver remembers it.
#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub kind: Kind,
    /// The descriptor, or `-1` for a [`Kind::Timeout`].
    pub fd: RawFd,
    /// Index of the send buffer, or receive buffer chosen up front, see
    /// [`crate::BufId::index`]. `None` with a provided buffer ring.
    pub buf: Option<u16>,
    /// Bytes of `buf` already sent.
    pub sent: u32,
    /// Total bytes to send.
    pub len: u32,
    /// File offset for [`Kind::Read`] and [`Kind::Write`]. `u64::MAX` on a write
    /// means the descriptor's own position. Start in the held memory for
    /// [`Kind::SendShared`].
    pub offset: u64,
    /// Per-kind argument. Region for [`Kind::ReadMem`] and [`Kind::WriteMem`],
    /// source fd for [`Kind::Splice`], bytes so far for [`Kind::RecvInto`] on
    /// readiness, write side for [`Kind::Ready`], name room for [`Kind::RecvMsg`].
    pub aux: u32,
    pub multishot: bool,
    /// Cancel requested, not yet confirmed. The slot stays held until the completion arrives.
    pub cancelling: bool,
    /// Completion queued but not taken. The slot stays held until
    /// [`crate::Io::next_completion`] so it is not reused before the caller sees it.
    pub done: bool,
}

impl Op {
    pub fn new(kind: Kind, fd: RawFd) -> Op {
        Op {
            kind,
            fd,
            buf: None,
            sent: 0,
            len: 0,
            offset: 0,
            aux: 0,
            multishot: false,
            cancelling: false,
            done: false,
        }
    }

    pub fn with_buf(mut self, buf: u16) -> Op {
        self.buf = Some(buf);
        self
    }
}

/// One finished operation.
#[derive(Debug)]
pub struct Completion {
    pub op: OpId,
    pub kind: Kind,
    /// The descriptor, or `-1` for a timeout.
    pub fd: RawFd,
    /// Bytes transferred. `Ok(0)` from a [`Kind::Recv`] is end of stream.
    pub result: Result<u32, rustix::io::Errno>,
    /// The filled buffer, for a receive. The caller must release it.
    pub buf: Option<Buf>,
    /// Peer address, for [`Kind::RecvFrom`] and [`Kind::Accept`].
    pub addr: Option<SocketAddr>,
    /// Still armed and the id still valid. False means finished, re-arm to continue.
    pub more: bool,
}

impl Completion {
    /// End of stream, for a receive or a file read.
    pub fn is_eof(&self) -> bool {
        matches!(self.kind, Kind::Recv | Kind::Read) && matches!(self.result, Ok(0))
    }

    /// The operation ended and its slot is gone.
    pub fn is_final(&self) -> bool {
        !self.more
    }

    /// The posted value of a [`Kind::Msg`]. Its `op` is not a real id. `None` for an
    /// `Err` one, the mailbox's own poll failing.
    pub fn value(&self) -> Option<u64> {
        (self.kind == Kind::Msg && self.result.is_ok()).then(|| self.op.to_user_data())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Connect => "connect",
            Kind::Recv => "recv",
            Kind::Send => "send",
            Kind::Close => "close",
            Kind::Timeout => "timeout",
            Kind::Open => "open",
            Kind::Read => "read",
            Kind::Stat => "stat",
            Kind::Write => "write",
            Kind::Accept => "accept",
            Kind::RecvFrom => "recvfrom",
            Kind::SendTo => "sendto",
            Kind::Fallocate => "fallocate",
            Kind::Sync => "sync",
            Kind::Truncate => "truncate",
            Kind::Rename => "rename",
            Kind::Unlink => "unlink",
            Kind::List => "list",
            Kind::ReadMem => "readmem",
            Kind::WriteMem => "writemem",
            Kind::SetSockOpt => "setsockopt",
            Kind::RecvInto => "recvinto",
            Kind::SendMsg => "sendmsg",
            Kind::RecvMsg => "recvmsg",
            Kind::Splice => "splice",
            Kind::Ready => "ready",
            Kind::Post => "post",
            Kind::Msg => "msg",
            Kind::SendShared => "sendshared",
            Kind::Wake => "wake",
        };
        f.write_str(s)
    }
}
