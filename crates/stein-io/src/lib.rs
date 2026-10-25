//! Completion-based io over `io_uring` or epoll.
//!
//! The driver owns all buffers. Operations name regions by id, so callers never hold `&mut`
//! to memory the kernel may write.
//!
//! # Features
//!
//! The [`queue`] is always built. It is the owner-agnostic slot queue over `io_uring` with its
//! epoll backend, together with [`msg`] and [`error`].
//!
//! `driver` is on by default. It adds the driver over the queue with buffers, an op table and
//! owned descriptors, and the `buf`, `op`, `file`, `dir` and `net` modules. A consumer of the
//! queue alone sets `default-features = false` and skips that code and `stein-core`.

#![forbid(clippy::disallowed_methods)]

#[cfg(feature = "driver")]
pub mod buf;
#[cfg(feature = "driver")]
pub mod dir;
#[cfg(feature = "driver")]
pub mod driver;
pub mod error;
#[cfg(feature = "driver")]
pub mod file;
pub mod msg;
#[cfg(feature = "driver")]
pub mod net;
#[cfg(feature = "driver")]
pub mod op;
mod pinned;
pub mod queue;

#[cfg(feature = "driver")]
pub use buf::{Buf, BufId, Bufs};
#[cfg(feature = "driver")]
pub use driver::{APPEND, Io, Mailbox, Peer, Region, Shared, Storage, Waker};
pub use error::{IoError, PoolFull};
#[cfg(feature = "driver")]
pub use file::{FileMeta, encode as encode_meta, meta};
#[cfg(feature = "driver")]
pub use net::UnixAddr;
#[cfg(feature = "driver")]
pub use op::{Completion, Kind, Op, OpId};
pub use queue::{Backend, CWD, RingFlags};
/// Flags and mode for [`queue::Op::Open`].
pub use rustix::fs::{Mode, OFlags};
