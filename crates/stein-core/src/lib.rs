//! Identity and storage primitives for the stein stack.
//!
//! `no_std`, never allocates. Containers are fixed views over caller storage.
//!
//! ```
//! use stein_core::handle::Pool;
//!
//! let mut slots: [Option<u32>; 4] = [const { None }; 4];
//! let mut generations = [0u32; 4];
//! let mut free = [0u32; 4];
//! let mut pool = Pool::new(&mut slots, &mut generations, &mut free)
//!     .expect("storage lengths agree");
//!
//! let h = pool.insert(7).expect("capacity 4");
//! assert_eq!(pool.get(h), Some(&7));
//! ```

#![no_std]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

// std only for the test harness.
#[cfg(test)]
extern crate std;

pub mod buf;
pub mod handle;
pub mod keyed;
