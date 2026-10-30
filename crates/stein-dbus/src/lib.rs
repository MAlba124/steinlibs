#![no_std]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

#[cfg(test)]
extern crate std;

pub mod addr;
pub mod auth;
pub mod bus;
pub mod calls;
pub mod conn;
pub mod error;
pub mod msg;
pub mod names;
pub mod props;
pub mod read;
pub mod sig;
pub mod str;
pub mod value;
pub mod write;

pub use addr::{AddrError, Socket};
pub use auth::AuthError;
pub use calls::{Calls, Pending};
pub use conn::{Call, Conn, DbusError, Name, Outbox, ReplyTo, Signal, State, Storage};
pub use error::WireError;
pub use msg::{Head, Kind, Message};
pub use read::{Item, Reader};
pub use sig::SigError;
pub use str::Str;
pub use value::{Dict, Strs, Value};
pub use write::Writer;
