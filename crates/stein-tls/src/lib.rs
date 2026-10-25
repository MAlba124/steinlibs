//! TLS for the sans-io stack.
//!
//! Bytes in, bytes out. No sockets, no DNS, no clock, no async. This sits
//! under `stein-http` the same way `stein-http` sits under a socket: the
//! application owns the io and shuttles buffers between the two.
//!
//! ```text
//! socket --> Tls::received --> Tls::read_plain --> http
//! socket <-- Tls::transmit <-- Tls::write_plain <-- http
//! ```
//!
//! Both roles are here. [`Tls`] is the client. [`Server`] is the server,
//! with the same four calls, and one more: [`Server::handoff`] ends the
//! session in user space and gives its secrets and sequence numbers to
//! another record layer, the kernel's.
//!
//! # What this is and is not
//!
//! It is a thin shaping layer over rustls, whose connections are already
//! buffer state machines. What it adds is the calling convention the rest of
//! the stack uses, the trust store decision, and errors that say what actually
//! went wrong rather than handing back an `io::Error` that means six things.
//!
//! It is not an implementation of TLS. Writing one was considered and
//! rejected, and the reason is worth stating plainly: TLS is the one place in
//! this stack where a subtle mistake produces no symptom. A chain validated
//! loosely, a comparison that is not constant time, a nonce reused under an
//! edge case, all of them still connect, still transfer, and are still broken.
//! Nothing you could write as a test would tell you. Everything else here is
//! worth building because the failure modes are visible.
//!
//! # Allocation
//!
//! This one allocates, unlike the rest of the stack. rustls owns its buffers
//! and its certificate parsing, and there is no configuration that changes
//! that. The allocation happens per connection rather than per byte, so it
//! does not reach the hot path, but it is a real difference and not worth
//! papering over.

use std::sync::Arc;

mod roots;
mod server;
mod tls;

pub use roots::{Roots, bundled_store, root_store};
pub use server::{
    Aead, Blocked, CLOSE_NOTIFY, Certs, Handoff, KEY_UPDATE, KEY_UPDATE_REQUEST, KTLS_SUITES, Key,
    RECORD_ALERT, RECORD_DATA, RECORD_HANDSHAKE, RECORD_OVERHEAD_MAX, RECORD_PLAIN_MAX, Rekey,
    Secret, Server, ServerConfig, ServerSetup, Tickets, Wrote, record_len, server_config,
};
pub use tls::{Tls, TlsError, config, config_with};

/// A shared client configuration. The server's is [`ServerConfig`].
///
/// Building one parses every root certificate, so it is done once and shared.
/// Cloning is a refcount bump.
pub type Config = Arc<rustls::ClientConfig>;
