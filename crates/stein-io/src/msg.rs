//! Layout of the driver's `RecvMsg` buffer and of one control message.
//!
//! ```text
//! 0        16             80
//! | header | control (64) | payload
//! ```
//!
//! With the sender address, as `recv_msg_from` asks for it, name room sits between.
//!
//! ```text
//! 0        16                144            208
//! | header | name (128)      | control (64) | payload
//! ```
//!
//! Header is `io_uring_recvmsg_out`, four `u32`. Name len, control len, payload len, flags.
//! The readiness backend writes the same layout.

use std::net::SocketAddr;

/// Control room in every receive buffer. One header and 48 bytes of data.
pub const CONTROL_ROOM: usize = 64;

/// `io_uring_recvmsg_out` length.
pub const OUT_LEN: usize = 16;

/// Payload offset in a receive buffer.
pub const HEAD: usize = OUT_LEN + CONTROL_ROOM;

/// Name room in a receive buffer with the sender address, a `sockaddr_storage`.
pub const NAME_ROOM: usize = crate::queue::ADDR_LEN;

/// Payload offset in a receive buffer with the sender address.
pub const HEAD_NAMED: usize = OUT_LEN + NAME_ROOM + CONTROL_ROOM;

/// `struct cmsghdr` size. `usize` length, then level and type.
pub const CMSG_HDR: usize = size_of::<usize>() + 8;

/// Max data [`encode`] takes.
pub const CMSG_MAX: usize = CONTROL_ROOM - CMSG_HDR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control<'a> {
    pub level: i32,
    pub kind: i32,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msg<'a> {
    pub payload: &'a [u8],
    /// The first control message.
    pub control: Option<Control<'a>>,
    /// Every control message, in kernel order.
    pub controls: Controls<'a>,
    /// Sender address, for a buffer with name room.
    pub from: Option<SocketAddr>,
    /// `msg_flags` from the kernel.
    pub flags: u32,
}

/// Walks the control messages of a control buffer. Stops at the first malformed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls<'a> {
    room: &'a [u8],
}

impl<'a> Iterator for Controls<'a> {
    type Item = Control<'a>;

    fn next(&mut self) -> Option<Control<'a>> {
        let Some(c) = first(self.room) else {
            self.room = &[];
            return None;
        };
        let step = (CMSG_HDR + c.data.len()).next_multiple_of(size_of::<usize>());
        self.room = self.room.get(step..).unwrap_or(&[]);
        Some(c)
    }
}

/// Every control message of a control buffer.
pub fn controls(room: &[u8]) -> Controls<'_> {
    Controls { room }
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Stated payload length, clamped to the buffer.
pub fn payload_len(bytes: &[u8]) -> Option<u32> {
    payload_len_at(bytes, HEAD)
}

/// Stated payload length, clamped to a buffer whose payload starts at `head`.
pub fn payload_len_at(bytes: &[u8], head: usize) -> Option<u32> {
    let stated = word(bytes, 8)? as usize;
    Some(stated.min(bytes.len().checked_sub(head)?) as u32)
}

/// Parse a receive buffer. `None` if shorter than its header says.
pub fn parse(bytes: &[u8]) -> Option<Msg<'_>> {
    parse_with(bytes, 0)
}

/// Parse a receive buffer with name room, see [`HEAD_NAMED`].
pub fn parse_named(bytes: &[u8]) -> Option<Msg<'_>> {
    parse_with(bytes, NAME_ROOM)
}

fn parse_with(bytes: &[u8], name_room: usize) -> Option<Msg<'_>> {
    let name_len = (word(bytes, 0)? as usize).min(name_room);
    let control_len = (word(bytes, 4)? as usize).min(CONTROL_ROOM);
    let head = OUT_LEN + name_room + CONTROL_ROOM;
    let n = payload_len_at(bytes, head)? as usize;
    let at = OUT_LEN + name_room;
    let room = bytes.get(at..at + control_len)?;
    let from = match name_len {
        0 => None,
        _ => crate::queue::addr_of(bytes.get(OUT_LEN..OUT_LEN + name_len)?),
    };
    Some(Msg {
        payload: bytes.get(head..head + n)?,
        control: first(room),
        controls: controls(room),
        from,
        flags: word(bytes, 12)?,
    })
}

/// The first control message of a control buffer.
pub fn first(room: &[u8]) -> Option<Control<'_>> {
    let len = usize::from_ne_bytes(room.get(..size_of::<usize>())?.try_into().ok()?);
    let at = size_of::<usize>();
    if len < CMSG_HDR {
        return None;
    }
    Some(Control {
        level: word(room, at)? as i32,
        kind: word(room, at + 4)? as i32,
        data: room.get(CMSG_HDR..len)?,
    })
}

/// Write one control message into `room`. Returns the word-padded `msg_controllen`,
/// `None` if it does not fit.
pub fn encode(room: &mut [u8], c: Control<'_>) -> Option<usize> {
    let len = CMSG_HDR + c.data.len();
    let padded = len.next_multiple_of(size_of::<usize>());
    let at = size_of::<usize>();
    room.get_mut(..padded)?.fill(0);
    room[..at].copy_from_slice(&len.to_ne_bytes());
    room[at..at + 4].copy_from_slice(&c.level.to_ne_bytes());
    room[at + 4..at + 8].copy_from_slice(&c.kind.to_ne_bytes());
    room[CMSG_HDR..len].copy_from_slice(c.data);
    Some(padded)
}

/// Write a header with no control, for the readiness backend.
pub fn write_plain(bytes: &mut [u8], payload: u32) {
    let Some(head) = bytes.get_mut(..OUT_LEN) else {
        return;
    };
    head.fill(0);
    head[8..12].copy_from_slice(&payload.to_ne_bytes());
}

#[cfg(test)]
mod tests;
