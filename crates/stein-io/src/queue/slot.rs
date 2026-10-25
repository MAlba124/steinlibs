//! Queue storage. One [`Slot`] and one [`Side`] per operation in flight.
//!
//! Slots are the queue's own bookkeeping. Sides hold what the kernel reads or writes by
//! address, so they live apart, behind a raw pointer, and must not move while the queue lives.

use std::time::Instant;

use io_uring::types::Timespec;
use rustix::net::addr::SocketAddrStorage;

use super::{NONE, Op, RingPage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Free,
    /// With the kernel, or waiting on a descriptor or deadline on `epoll`.
    Flying,
    /// `epoll` multishot with a completion on the done list, still armed.
    Queued,
    /// `epoll` finished, last completion on the done list.
    Done,
}

/// A ring entry waiting for a [`super::Queue::link`] chain to close, see
/// [`super::Queue::push_apart`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Behind {
    None,
    /// The op itself has not gone out.
    Op,
    /// The op went out, its cancel has not.
    Cancel,
}

/// Structures the kernel reads by address.
#[repr(C)]
pub(super) struct Side {
    pub(super) ts: Timespec,
    pub(super) hdr: libc::msghdr,
    pub(super) iov: libc::iovec,
    pub(super) name: SocketAddrStorage,
    pub(super) namelen: libc::socklen_t,
    pub(super) control: ControlRoom,
}

impl Side {
    fn new() -> Side {
        Side {
            ts: Timespec::new(),
            // SAFETY: integers and pointers, all zeros is valid.
            hdr: unsafe { core::mem::zeroed() },
            iov: libc::iovec {
                iov_base: core::ptr::null_mut(),
                iov_len: 0,
            },
            name: SocketAddrStorage::zeroed(),
            namelen: 0,
            control: ControlRoom([0; super::CONTROL_ROOM]),
        }
    }
}

/// Control message room, aligned for `cmsghdr`.
#[repr(C, align(8))]
pub(super) struct ControlRoom(pub(super) [u8; super::CONTROL_ROOM]);

pub(super) struct Slot {
    pub(super) op: Op,
    pub(super) user: u64,
    pub(super) generation: u32,
    pub(super) state: State,
    /// Cancel requested, op not yet ended.
    pub(super) cancel: bool,
    /// On the retry list, `epoll` only.
    pub(super) on_retry: bool,
    /// Free list, or done list on `epoll`.
    pub(super) next: u32,
    /// Retry list, `epoll` only. Apart from `next` since a slot may be on both.
    pub(super) retry_next: u32,
    /// Per-descriptor wait list, `epoll` only.
    pub(super) wait_next: u32,
    /// `epoll` completion result and flags.
    pub(super) res: i32,
    pub(super) flags: u32,
    /// `epoll` timeout deadline.
    pub(super) at: Option<Instant>,
    /// Bytes an `epoll` [`Op::RecvAll`] has so far.
    pub(super) progress: u32,
    /// A close behind a ring chain, pushed as a nop. Its fd closes when the nop lands.
    pub(super) after_chain: bool,
    /// Waits for an open chain to close, ring only.
    pub(super) behind: Behind,
    /// Cancelled before it went out, so a nop goes in its place and it ends with `ECANCELED`.
    pub(super) void: bool,
}

impl Slot {
    pub(super) fn new() -> Slot {
        Slot {
            op: Op::Nop,
            user: 0,
            generation: 0,
            state: State::Free,
            cancel: false,
            on_retry: false,
            next: NONE,
            retry_next: NONE,
            wait_next: NONE,
            res: 0,
            flags: 0,
            at: None,
            progress: 0,
            after_chain: false,
            behind: Behind::None,
            void: false,
        }
    }
}

/// Datagram source or accepted peer, from the side of a final completion.
pub(super) fn peer(op: &Op, side: &Side, res: i32) -> Option<std::net::SocketAddr> {
    let len = match op {
        Op::RecvFrom { .. } if res >= 0 => side.hdr.msg_namelen,
        Op::Accept { multi: false, .. } if res >= 0 => side.namelen,
        _ => return None,
    };
    if len < 2 || len as usize > size_of::<SocketAddrStorage>() {
        return None;
    }
    // SAFETY: the storage is plain bytes and at least `len` long.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&side.name as *const SocketAddrStorage).cast::<u8>(),
            len as usize,
        )
    };
    super::addr_of(bytes)
}

/// Intrusive FIFO of slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct List {
    pub(super) head: u32,
    pub(super) tail: u32,
}

impl List {
    pub(super) const EMPTY: List = List {
        head: NONE,
        tail: NONE,
    };

    pub(super) fn is_empty(&self) -> bool {
        self.head == NONE
    }
}

/// Whether a descriptor's file description blocks, asked once per watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Blocks {
    Unknown,
    Yes,
    No,
}

/// `epoll` state for one descriptor, indexed by fd number. Unused on the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Watch {
    /// Registered with epoll, edge triggered, both directions.
    pub(super) added: bool,
    /// Lacks `O_NONBLOCK`, so an accept on it waits for readable first.
    pub(super) blocks: Blocks,
    pub(super) read: List,
    pub(super) write: List,
}

impl Watch {
    pub(super) const EMPTY: Watch = Watch {
        added: false,
        blocks: Blocks::Unknown,
        read: List::EMPTY,
        write: List::EMPTY,
    };
}

/// Storage a queue owns, made once here. Returned by [`super::Queue::into_store`].
pub struct Owned {
    pub(super) slots: Box<[Slot]>,
    pub(super) sides: Box<[Side]>,
    pub(super) watches: Box<[Watch]>,
    pub(super) rings: Box<[RingPage]>,
}

impl Owned {
    /// `ops` operations in flight. `epoll` watches descriptors below `fds` without
    /// growing its table, higher ones grow it. Zero `fds` waits on no descriptor, socket
    /// and pipe ops on `epoll` end with `EMFILE`.
    pub fn new(ops: usize, fds: usize) -> Owned {
        Owned::with_rings(ops, fds, 0)
    }

    /// With `pages` of ring memory, see [`super::ring_pages`].
    pub fn with_rings(ops: usize, fds: usize, pages: usize) -> Owned {
        Owned {
            slots: (0..ops).map(|_| Slot::new()).collect(),
            sides: (0..ops).map(|_| Side::new()).collect(),
            watches: vec![Watch::EMPTY; fds].into_boxed_slice(),
            rings: (0..pages).map(|_| RingPage::ZERO).collect(),
        }
    }

    /// Operations it holds.
    pub fn ops(&self) -> usize {
        self.slots.len()
    }

    /// Ring pages it holds.
    pub fn ring_pages(&self) -> usize {
        self.rings.len()
    }
}
