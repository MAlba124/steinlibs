//! Driver errors, separate from operation results.

use core::fmt;

/// A failure of the driver itself, not of an operation.
#[derive(Debug)]
pub enum IoError {
    /// The operation table is full.
    OpsFull,
    /// Every arena buffer is on loan.
    BuffersFull,
    /// A buffer the caller does not hold, one free or with the kernel.
    NotHeld(u16),
    /// The arena cannot be divided into buffers of the requested size.
    BadArena { bytes: usize, buffer: usize },
    /// More slots than a handle can address, or more buffers than an id can.
    TooManySlots(usize),
    /// A path does not fit in one buffer. Usually the buffer size is too small.
    PathTooLong { len: usize, max: usize },
    /// Neither backend could be started.
    NoBackend {
        uring: rustix::io::Errno,
        epoll: rustix::io::Errno,
    },
    /// The kernel refused a call the driver made on its own behalf.
    Kernel(rustix::io::Errno),
    /// Every region slot is taken. Regions are never freed.
    RegionsFull,
    /// A range outside its region, or an unknown region.
    OutOfRegion { at: usize, len: usize, size: usize },
    /// A control message larger than the header's control room.
    ControlTooLong { len: usize, max: usize },
    /// The operation needs a provided buffer ring and there is none.
    NoBufRing,
    /// Not an open, or one that already has a stat waiting.
    NotOpening,
    /// The backend in use cannot do this.
    Unsupported,
    /// A [`crate::queue::Mem`] of a region its queue no longer has.
    StaleMem,
    /// A region range an operation in flight names.
    RegionBusy { at: usize, len: usize },
    /// A buffer on loan, so the arena cannot be laid out again, or a provided buffer id a
    /// group still holds.
    Loaned(u16),
    /// A read or write longer than one call moves, see [`crate::queue::MAX_RW_COUNT`].
    TooLong { len: usize, max: usize },
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpsFull => write!(f, "no free operation slots"),
            Self::BuffersFull => write!(f, "no free buffers"),
            Self::NotHeld(id) => write!(f, "buffer {id} is not the caller's"),
            Self::BadArena { bytes, buffer } => {
                write!(f, "an arena of {bytes} bytes holds no buffers of {buffer}")
            }
            Self::TooManySlots(n) => write!(f, "{n} slots is more than a handle can address"),
            Self::PathTooLong { len, max } => {
                write!(f, "a path of {len} bytes does not fit a buffer of {max}")
            }
            Self::NoBackend { uring, epoll } => write!(
                f,
                "no usable backend: io_uring failed with {uring}, epoll with {epoll}"
            ),
            Self::Kernel(e) => write!(f, "the kernel refused a driver call: {e}"),
            Self::RegionsFull => write!(f, "no free region slots"),
            Self::OutOfRegion { at, len, size } => {
                write!(f, "{len} bytes at {at} do not lie in a region of {size}")
            }
            Self::ControlTooLong { len, max } => {
                write!(f, "a control message of {len} bytes where {max} fit")
            }
            Self::NoBufRing => write!(f, "no provided buffer ring to receive from"),
            Self::NotOpening => write!(f, "not an open a stat can wait for"),
            Self::Unsupported => write!(f, "not something this backend does"),
            Self::StaleMem => write!(f, "memory of a region the queue no longer has"),
            Self::RegionBusy { at, len } => {
                write!(f, "{len} bytes at {at} overlap an operation in flight")
            }
            Self::Loaned(id) => write!(f, "buffer {id} is on loan"),
            Self::TooLong { len, max } => {
                write!(
                    f,
                    "a transfer of {len} bytes where at most {max} move at once"
                )
            }
        }
    }
}

impl core::error::Error for IoError {}

/// A pool that had no room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolFull;

impl fmt::Display for PoolFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the pool is full")
    }
}

impl core::error::Error for PoolFull {}
