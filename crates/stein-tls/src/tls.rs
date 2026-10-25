//! The connection itself.
//!
//! Four calls make a session. Two move ciphertext to and from the socket, two
//! move plaintext to and from whatever is layered on top:
//!
//! ```text
//! socket --> received  --> read_plain  --> http
//! socket <-- transmit  <-- write_plain <-- http
//! ```
//!
//! None of them touch a descriptor. The application owns the io, which is what
//! lets the whole thing be driven from a test with two byte buffers.

use std::fmt;
use std::io::{Read as _, Write as _};
use std::sync::Arc;

use rustls::client::ClientConnection;
use rustls_pki_types::ServerName;

use crate::roots::root_store;

/// What went wrong.
///
/// Separated from rustls's own error rather than passed through, because most
/// of what a caller does about a TLS failure depends only on which of these it
/// is, and `rustls::Error` has fifty variants describing the ones it cannot.
#[derive(Debug)]
pub enum TlsError {
    /// The name is not one a certificate could be issued for.
    BadName(String),
    /// The peer's certificate did not validate. The single most common real
    /// failure, and the one worth never papering over.
    BadCertificate(rustls::Error),
    /// The handshake failed for some other reason, such as no shared cipher.
    Handshake(rustls::Error),
    /// The peer sent something that is not valid TLS.
    Protocol(rustls::Error),
    /// The peer closed the connection in the middle of the handshake.
    Closed,
    /// Setting up a configuration failed.
    Config(rustls::Error),
    /// The certificate chain is not valid PEM.
    BadCertPem(rustls_pki_types::pem::Error),
    /// The private key is not valid PEM, or there is none in it.
    BadKeyPem(rustls_pki_types::pem::Error),
    /// The private key is not PKCS#8, PKCS#1 or SEC1.
    BadKeyDer(&'static str),
    /// The certificate chain is empty.
    NoCertificate,
    /// None of the suites asked for is one the provider has.
    NoSuites,
    /// The session was used after it reported a fatal error.
    Dead,
    /// The keys have sealed as many records as they safely can and could
    /// not be renewed.
    Exhausted,
    /// The session cannot be handed off as it stands.
    Handoff(crate::server::Blocked),
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName(n) => write!(f, "{n} is not a valid server name"),
            Self::BadCertificate(e) => write!(f, "the server's certificate was rejected: {e}"),
            Self::Handshake(e) => write!(f, "the handshake failed: {e}"),
            Self::Protocol(e) => write!(f, "the peer sent invalid TLS: {e}"),
            Self::Closed => write!(f, "the peer closed during the handshake"),
            Self::Config(e) => write!(f, "the tls configuration is not usable: {e}"),
            Self::BadCertPem(e) => write!(f, "the certificate chain is not valid pem: {e:?}"),
            Self::BadKeyPem(e) => write!(f, "the private key is not valid pem: {e:?}"),
            Self::BadKeyDer(e) => write!(f, "the private key is not usable: {e}"),
            Self::NoCertificate => write!(f, "the certificate chain is empty"),
            Self::NoSuites => write!(f, "no cipher suite is left to offer"),
            Self::Dead => write!(f, "the session already failed"),
            Self::Exhausted => write!(f, "the traffic keys are used up"),
            Self::Handoff(b) => write!(f, "the session cannot be handed off: {b:?}"),
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadCertificate(e) | Self::Handshake(e) | Self::Protocol(e) | Self::Config(e) => {
                Some(e)
            }
            _ => None,
        }
    }
}

/// Sort a rustls error into something a caller can act on.
pub(crate) fn classify(e: rustls::Error, handshaking: bool) -> TlsError {
    match &e {
        rustls::Error::InvalidCertificate(_) => TlsError::BadCertificate(e),
        _ if handshaking => TlsError::Handshake(e),
        _ => TlsError::Protocol(e),
    }
}

/// A client configuration with the platform's roots.
///
/// Built once and shared: parsing every root certificate is not something to
/// do per connection.
pub fn config() -> Result<Arc<rustls::ClientConfig>, TlsError> {
    let (store, _) = root_store();
    config_with(store)
}

pub fn config_with(store: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    // The provider is named rather than taken from the process default,
    // because the default is set by whichever crate installed one first and a
    // library should not depend on that.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(TlsError::Config)?
        .with_root_certificates(store)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// One TLS session.
pub struct Tls {
    conn: ClientConnection,
    /// Set once the peer has closed. Reported rather than treated as an error,
    /// since a clean close is how a connection normally ends.
    closed: bool,
}

impl Tls {
    /// Start a session for `server`, which must be a DNS name or an IP.
    ///
    /// Nothing is sent yet. The first flight is produced by [`Tls::transmit`],
    /// which the caller writes to the socket it has just opened.
    pub fn connect(config: &Arc<rustls::ClientConfig>, server: &str) -> Result<Tls, TlsError> {
        let name = ServerName::try_from(server)
            .map_err(|_| TlsError::BadName(server.to_string()))?
            .to_owned();
        let conn = ClientConnection::new(config.clone(), name).map_err(TlsError::Handshake)?;
        Ok(Tls {
            conn,
            closed: false,
        })
    }

    /// Hand over ciphertext read from the socket.
    ///
    /// Returns how much was taken. A short take is normal and means the
    /// internal buffer is full; call [`Tls::read_plain`] to drain it and pass
    /// the rest on the next turn.
    pub fn received(&mut self, src: &[u8]) -> Result<usize, TlsError> {
        if src.is_empty() {
            return Ok(0);
        }
        let mut cursor = src;
        // `read_tls` only fails when the buffer is full, which is not an error
        // but a signal to drain first.
        let took = self.conn.read_tls(&mut cursor).unwrap_or(0);
        let handshaking = self.conn.is_handshaking();
        let state = self
            .conn
            .process_new_packets()
            .map_err(|e| classify(e, handshaking))?;
        self.closed |= state.peer_has_closed();
        Ok(took)
    }

    /// Take decrypted application data.
    ///
    /// Zero means nothing is available yet, not end of stream. Use
    /// [`Tls::peer_closed`] for that, because a TLS close is a message rather
    /// than a socket event and conflating the two is how a truncation attack
    /// goes unnoticed.
    pub fn read_plain(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        if out.is_empty() {
            return Ok(0);
        }
        match self.conn.reader().read(out) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            // The reader surfaces a protocol failure the packet processing
            // already reported, so there is nothing new to classify.
            Err(_) => Ok(0),
        }
    }

    /// Queue plaintext to be encrypted and sent.
    ///
    /// The bytes are buffered, not sent. [`Tls::transmit`] produces the
    /// ciphertext for the socket.
    pub fn write_plain(&mut self, src: &[u8]) -> Result<usize, TlsError> {
        if src.is_empty() {
            return Ok(0);
        }
        self.conn.writer().write(src).map_err(|_| TlsError::Closed)
    }

    /// Take ciphertext to write to the socket.
    ///
    /// Zero means there is nothing to send. Call this after every `received`
    /// and every `write_plain`, because both can produce output: a handshake
    /// message, a key update, an alert.
    pub fn transmit(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        if out.is_empty() || !self.conn.wants_write() {
            return Ok(0);
        }
        let mut cursor = out;
        // Writing into a slice cannot fail for any reason worth reporting: it
        // stops when the slice is full, which is what a short return means.
        Ok(self.conn.write_tls(&mut cursor).unwrap_or(0))
    }

    /// Whether the handshake is still in progress. Application data written
    /// before this goes false is buffered, not lost.
    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// Whether there is ciphertext waiting for the socket.
    pub fn wants_write(&self) -> bool {
        self.conn.wants_write()
    }

    /// Whether the session needs more bytes from the socket to make progress.
    pub fn wants_read(&self) -> bool {
        self.conn.wants_read()
    }

    /// Whether the peer sent a close notification.
    ///
    /// This is the only honest end of stream. A socket that simply stopped
    /// could have been cut by anyone, and treating that as a clean end is
    /// exactly the truncation an attacker wants.
    pub fn peer_closed(&self) -> bool {
        self.closed
    }

    /// Begin a clean shutdown. The alert comes out of [`Tls::transmit`].
    pub fn close(&mut self) {
        self.conn.send_close_notify();
    }

    /// The protocol agreed by ALPN, if any.
    pub fn alpn(&self) -> Option<&[u8]> {
        self.conn.alpn_protocol()
    }

    /// The version in use, once the handshake is done.
    pub fn version(&self) -> Option<rustls::ProtocolVersion> {
        self.conn.protocol_version()
    }
}

#[cfg(test)]
mod tests;
