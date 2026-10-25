//! The server role.
//!
//! The same four calls as the client, with two differences that follow from
//! what a server is for:
//!
//! ```text
//! socket --> received  --> read_plain  --> http
//! socket <-- transmit  <-- write_plain <-- http
//!                 |
//!                 +- handoff --> secrets for the kernel
//! ```
//!
//! `write_plain` encrypts straight into the caller's send buffer, so a
//! connection holds no ciphertext of its own and an idle one holds no buffer
//! at all. `transmit` carries only what the session itself has to say:
//! handshake flights, tickets, key updates and alerts.
//!
//! `received` takes whole records only. A record cut short by the socket is
//! left with the caller, untouched, which is what makes the handoff possible:
//! the kernel can only take over at a record boundary, so the bytes that were
//! not taken are exactly the ones that still have to be completed here.
//!
//! # The handshake on its own
//!
//! A [`Server`] is the handshake. It is small, it frees what the handshake
//! needed when it completes, and [`Server::handoff`] consumes it and returns
//! the traffic secrets and sequence numbers for both directions, after which
//! nothing of TLS is left in user space but an optional [`Rekey`].
//!
//! This runs on rustls's unbuffered connection. It is the only one that gives
//! up the key schedule with the secrets, and the only one that works on the
//! caller's buffers.

use std::cell::RefCell;
use std::sync::Arc;

use rustls::kernel::KernelConnection;
use rustls::server::{
    ClientHello, NoServerSessionStorage, ResolvesServerCert, ServerConnectionData,
    ServerSessionMemoryCache, UnbufferedServerConnection,
};
use rustls::sign::CertifiedKey;
use rustls::unbuffered::{
    ConnectionState, EncodeError, EncryptError, UnbufferedStatus, WriteTraffic,
};
use rustls::{CipherSuite, ConnectionTrafficSecrets, ProtocolVersion};
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::tls::{TlsError, classify};

/// A shared server configuration. Cloning is a refcount bump.
pub type ServerConfig = Arc<rustls::ServerConfig>;

/// The suites the kernel's record layer takes, all of them TLS 1.3.
pub const KTLS_SUITES: [CipherSuite; 3] = [
    CipherSuite::TLS13_AES_128_GCM_SHA256,
    CipherSuite::TLS13_AES_256_GCM_SHA384,
    CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
];

/// Record content types, for a caller that frames control records itself
/// once the kernel has the record layer.
pub const RECORD_ALERT: u8 = 21;
pub const RECORD_HANDSHAKE: u8 = 22;
pub const RECORD_DATA: u8 = 23;

/// The body of a `close_notify` alert record.
pub const CLOSE_NOTIFY: [u8; 2] = [1, 0];
/// The body of a TLS 1.3 key update that asks for nothing back.
pub const KEY_UPDATE: [u8; 5] = [24, 0, 0, 1, 0];
/// The body of a TLS 1.3 key update that asks the peer to update too.
pub const KEY_UPDATE_REQUEST: [u8; 5] = [24, 0, 0, 1, 1];

/// The largest plaintext one record carries.
pub const RECORD_PLAIN_MAX: usize = 16384;
/// The most a record adds around its plaintext, header included, in any
/// suite this negotiates.
pub const RECORD_OVERHEAD_MAX: usize = 29;

const HEADER: usize = 5;
/// The largest record body TLS 1.2 allows on the wire. Anything longer is
/// not a record, and is handed to rustls to refuse.
const BODY_MAX: usize = RECORD_PLAIN_MAX + 2048;

/// A certificate chain, leaf first.
#[derive(Clone, Copy, Debug)]
pub enum Certs<'a> {
    /// Any number of `CERTIFICATE` sections.
    Pem(&'a [u8]),
    /// One DER certificate each.
    Der(&'a [&'a [u8]]),
}

/// The leaf's private key.
#[derive(Clone, Copy, Debug)]
pub enum Key<'a> {
    /// The first private key section, PKCS#8, PKCS#1 or SEC1.
    Pem(&'a [u8]),
    /// PKCS#8, PKCS#1 or SEC1, told apart by their structure.
    Der(&'a [u8]),
}

/// How a client may resume.
///
/// rustls's own default is `Stored(256)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tickets {
    /// Every handshake is a full one. Nothing is sent after it.
    Off,
    /// Sessions kept in a table of this many entries behind one mutex, shared
    /// by every thread using the configuration. Two tickets after each TLS
    /// 1.3 handshake, in one record, name entries in it.
    Stored(usize),
    /// Sessions sealed into the ticket itself under a key that rotates every
    /// six hours. No table, so any core can resume what another issued. Two
    /// tickets after each TLS 1.3 handshake, in one record.
    Sealed,
}

/// Everything a server configuration is built from.
#[derive(Clone, Copy, Debug)]
pub struct ServerSetup<'a> {
    pub chain: Certs<'a>,
    pub key: Key<'a>,
    /// Protocols offered, most preferred first. Empty means no ALPN.
    pub alpn: &'a [&'a [u8]],
    /// Refuse TLS 1.2.
    pub tls13_only: bool,
    /// The suites allowed, or empty for every suite the provider has.
    pub suites: &'a [CipherSuite],
    pub tickets: Tickets,
    /// Allow [`Server::handoff`]. Off unless the secrets are wanted.
    pub extract: bool,
}

/// Records the name the client asked for on its way past, since the
/// unbuffered connection does not keep it where it can be read.
#[derive(Debug)]
struct OneCert {
    key: Arc<CertifiedKey>,
}

thread_local! {
    static SNI: RefCell<Option<Box<str>>> = const { RefCell::new(None) };
}

impl ResolvesServerCert for OneCert {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = hello.server_name() {
            let _ = SNI.try_with(|s| *s.borrow_mut() = Some(name.into()));
        }
        Some(self.key.clone())
    }
}

/// Build a server configuration.
///
/// The chain and key are parsed and checked against each other here, once.
/// 0-RTT is off and stays off.
pub fn server_config(setup: &ServerSetup<'_>) -> Result<ServerConfig, TlsError> {
    let mut provider = rustls::crypto::ring::default_provider();
    if !setup.suites.is_empty() {
        provider
            .cipher_suites
            .retain(|s| setup.suites.contains(&s.suite()));
    }
    if provider.cipher_suites.is_empty() {
        return Err(TlsError::NoSuites);
    }

    let chain = chain_of(setup.chain)?;
    let key = key_of(setup.key)?;
    let certified = CertifiedKey::from_der(chain, key, &provider).map_err(TlsError::Config)?;

    let versions: &[&rustls::SupportedProtocolVersion] = if setup.tls13_only {
        &[&rustls::version::TLS13]
    } else {
        rustls::DEFAULT_VERSIONS
    };
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(versions)
        .map_err(TlsError::Config)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(OneCert {
            key: Arc::new(certified),
        }));

    cfg.alpn_protocols = setup.alpn.iter().map(|p| p.to_vec()).collect();
    cfg.enable_secret_extraction = setup.extract;
    cfg.max_early_data_size = 0;
    match setup.tickets {
        Tickets::Off => {
            cfg.session_storage = Arc::new(NoServerSessionStorage {});
            cfg.send_tls13_tickets = 0;
        }
        Tickets::Stored(entries) => {
            cfg.session_storage = ServerSessionMemoryCache::new(entries);
        }
        Tickets::Sealed => {
            cfg.session_storage = Arc::new(NoServerSessionStorage {});
            cfg.ticketer = rustls::crypto::ring::Ticketer::new().map_err(TlsError::Config)?;
        }
    }
    Ok(Arc::new(cfg))
}

fn chain_of(certs: Certs<'_>) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let chain = match certs {
        Certs::Pem(pem) => CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(TlsError::BadCertPem)?,
        Certs::Der(ders) => ders
            .iter()
            .map(|d| CertificateDer::from(d.to_vec()))
            .collect(),
    };
    if chain.is_empty() {
        return Err(TlsError::NoCertificate);
    }
    Ok(chain)
}

fn key_of(key: Key<'_>) -> Result<PrivateKeyDer<'static>, TlsError> {
    match key {
        Key::Pem(pem) => PrivateKeyDer::from_pem_slice(pem).map_err(TlsError::BadKeyPem),
        Key::Der(der) => PrivateKeyDer::try_from(der.to_vec()).map_err(TlsError::BadKeyDer),
    }
}

/// The length of the record that starts with `head`, header included.
///
/// `None` until the five byte header is there. With it a caller holding the
/// front of a record knows how many more bytes complete it.
pub fn record_len(head: &[u8]) -> Option<usize> {
    let h = head.get(..HEADER)?;
    Some(HEADER + usize::from(u16::from_be_bytes([h[3], h[4]])))
}

/// Where the whole records starting at `from` end. With `one_batch` the
/// walk stops behind the first record that can carry application data, so a
/// buffer of many records is walked once and not once per record taken.
///
/// Something that cannot be a record header ends the walk with everything
/// counted, so that rustls sees it and refuses it now instead of this
/// waiting for a body that is never coming.
fn records_end(src: &[u8], from: usize, one_batch: bool) -> usize {
    let mut at = from;
    while let Some(len) = record_len(&src[at..]) {
        let kind = src[at];
        if !(20..=23).contains(&kind) || src[at + 1] != 3 || len - HEADER > BODY_MAX {
            return src.len();
        }
        if at + len > src.len() {
            break;
        }
        at += len;
        if one_batch && kind == RECORD_DATA {
            break;
        }
    }
    at
}

/// Why a handoff cannot happen yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// The configuration was built with `extract` off.
    ExtractOff,
    /// The handshake is not complete.
    Handshaking,
    /// Bytes are waiting in [`Server::transmit`]. They were encrypted under
    /// sequence numbers the kernel would use again.
    Unsent,
    /// Plaintext is waiting in [`Server::read_plain`].
    Unread,
    /// The peer is part way through a handshake message spread over records.
    MidMessage,
    /// The suite is not one this knows how to describe.
    Suite,
}

/// Which AEAD a direction uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aead {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
}

/// One direction's record protection.
///
/// The key material stays inside rustls's own types, which wipe themselves
/// when dropped.
pub struct Secret {
    /// The sequence number of the next record in this direction.
    pub seq: u64,
    pub aead: Aead,
    secret: ConnectionTrafficSecrets,
}

impl Secret {
    fn new(seq: u64, secret: ConnectionTrafficSecrets) -> Result<Secret, TlsError> {
        let aead = match &secret {
            ConnectionTrafficSecrets::Aes128Gcm { .. } => Aead::Aes128Gcm,
            ConnectionTrafficSecrets::Aes256Gcm { .. } => Aead::Aes256Gcm,
            ConnectionTrafficSecrets::Chacha20Poly1305 { .. } => Aead::Chacha20Poly1305,
            _ => return Err(TlsError::Handoff(Blocked::Suite)),
        };
        Ok(Secret { seq, aead, secret })
    }

    /// 16 bytes for AES-128-GCM, 32 for the other two.
    pub fn key(&self) -> &[u8] {
        match &self.secret {
            ConnectionTrafficSecrets::Aes128Gcm { key, .. }
            | ConnectionTrafficSecrets::Aes256Gcm { key, .. }
            | ConnectionTrafficSecrets::Chacha20Poly1305 { key, .. } => key.as_ref(),
            _ => &[],
        }
    }

    /// The 12 byte IV. For the GCM suites the kernel calls the first four
    /// bytes the salt and the last eight the IV.
    pub fn iv(&self) -> &[u8] {
        match &self.secret {
            ConnectionTrafficSecrets::Aes128Gcm { iv, .. }
            | ConnectionTrafficSecrets::Aes256Gcm { iv, .. }
            | ConnectionTrafficSecrets::Chacha20Poly1305 { iv, .. } => iv.as_ref(),
            _ => &[],
        }
    }
}

/// What is left of the session after a handoff, the key schedule alone.
///
/// Needed only to follow a TLS 1.3 key update. A caller that closes on one
/// instead can drop it and hold nothing.
pub struct Rekey {
    conn: KernelConnection<ServerConnectionData>,
}

impl Rekey {
    /// The next secret for records this side sends. The caller sends a
    /// [`KEY_UPDATE`] record under the old one first.
    pub fn tx_next(&mut self) -> Result<Secret, TlsError> {
        let (seq, secret) = self.conn.update_tx_secret().map_err(TlsError::Protocol)?;
        Secret::new(seq, secret)
    }

    /// The next secret for records the peer sends, for when its key update
    /// arrives.
    pub fn rx_next(&mut self) -> Result<Secret, TlsError> {
        let (seq, secret) = self.conn.update_rx_secret().map_err(TlsError::Protocol)?;
        Secret::new(seq, secret)
    }
}

/// What a completed handshake hands to another record layer.
pub struct Handoff {
    pub version: ProtocolVersion,
    pub suite: CipherSuite,
    /// Records this side sends.
    pub tx: Secret,
    /// Records the peer sends.
    pub rx: Secret,
    pub rekey: Rekey,
}

/// What a [`Server::write_plain`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wrote {
    /// Plaintext bytes consumed.
    pub took: usize,
    /// Bytes put in the output.
    pub wrote: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Handshaking or established, rustls knows which.
    Live,
    /// A fatal error was reported. Only `transmit` still works, for the alert.
    Dead,
}

/// What one turn of rustls came to, with its borrows let go.
enum Turn {
    /// Something was queued or acknowledged, go round again.
    Again,
    /// Plaintext is waiting to be taken.
    Plain,
    /// The peer sent its close notification.
    PeerClosed,
    /// Nothing more can be done with what was given.
    Rest,
}

/// One TLS session, server side.
pub struct Server {
    conn: UnbufferedServerConnection,
    phase: Phase,
    /// The peer sent its close notification.
    closed: bool,
    /// rustls holds decrypted records that have not been read.
    plain: bool,
    /// This side sent its close notification. rustls would still seal data
    /// after it, which no peer may accept.
    shut: bool,
    extract: bool,
    /// What the session itself has to send, waiting for `transmit`.
    out: Vec<u8>,
    out_at: usize,
    /// Records rustls has processed and still refers to, which happens only
    /// when a handshake message is spread over several of them.
    carry: Vec<u8>,
    /// Plaintext taken off rustls with nowhere to put it.
    spill: Vec<u8>,
    spill_at: usize,
    sni: Option<Box<str>>,
}

/// Run rustls once over `buf` and do what needs no caller.
fn turn(
    conn: &mut UnbufferedServerConnection,
    buf: &mut [u8],
    out: &mut Vec<u8>,
) -> (usize, Result<Turn, rustls::Error>) {
    let UnbufferedStatus { discard, state } = conn.process_tls_records(buf);
    let turn = match state {
        Err(e) => return (discard, Err(e)),
        Ok(ConnectionState::EncodeTlsData(mut enc)) => {
            // An empty buffer is how the size is asked for.
            let need = match enc.encode(&mut []) {
                Err(EncodeError::InsufficientSize(e)) => e.required_size,
                _ => 0,
            };
            let at = out.len();
            out.resize(at + need, 0);
            if need > 0 && enc.encode(&mut out[at..]).is_err() {
                out.truncate(at);
            }
            Turn::Again
        }
        Ok(ConnectionState::TransmitTlsData(t)) => {
            // `out` is the transport as far as rustls is concerned.
            t.done();
            Turn::Again
        }
        Ok(ConnectionState::ReadTraffic(_)) => Turn::Plain,
        Ok(ConnectionState::ReadEarlyData(mut early)) => {
            // 0-RTT is off, so there is never any. Dropped if there were.
            while early.next_record().is_some() {}
            Turn::Again
        }
        Ok(ConnectionState::PeerClosed) => Turn::PeerClosed,
        Ok(_) => Turn::Rest,
    };
    (discard, Ok(turn))
}

/// Move every decrypted record rustls holds into `spill`.
fn spill_plain(conn: &mut UnbufferedServerConnection, spill: &mut Vec<u8>) {
    // Plaintext is reported before the buffer is looked at, so an empty one
    // is safe here whatever rustls has kept positions into.
    if let Ok(ConnectionState::ReadTraffic(mut rt)) = conn.process_tls_records(&mut []).state {
        while let Some(Ok(rec)) = rt.next_record() {
            spill.extend_from_slice(rec.payload);
        }
    }
}

/// What a turn left to do once its borrow of the connection is over.
enum Act {
    Fail(rustls::Error),
    Again,
    Spill,
    PeerClosed,
}

/// Seal the front of `piece` as one record, a shorter one when the room
/// asks for it. Gives the plaintext taken and the bytes written.
fn seal_one(
    w: &mut WriteTraffic<'_, ServerConnectionData>,
    piece: &[u8],
    room: &mut [u8],
) -> Result<(usize, usize), EncryptError> {
    let len = piece.len().min(RECORD_PLAIN_MAX);
    match w.encrypt(&piece[..len], room) {
        Ok(n) => Ok((len, n)),
        Err(EncryptError::InsufficientSize(e)) => {
            // The overhead is fixed, so a record shorter by what is missing
            // fits exactly.
            let len = len.saturating_sub(e.required_size - room.len());
            if len == 0 {
                return Err(EncryptError::InsufficientSize(e));
            }
            w.encrypt(&piece[..len], room).map(|n| (len, n))
        }
        Err(e) => Err(e),
    }
}

/// Encrypt as much of `src` as `out` has room for, one record at a time.
fn seal(
    w: &mut WriteTraffic<'_, ServerConnectionData>,
    src: &[u8],
    out: &mut [u8],
    done: &mut Wrote,
) -> Result<(), TlsError> {
    while done.took < src.len() {
        match seal_one(w, &src[done.took..], &mut out[done.wrote..]) {
            Ok((took, wrote)) => {
                done.took += took;
                done.wrote += wrote;
            }
            Err(EncryptError::InsufficientSize(_)) => break,
            Err(EncryptError::EncryptExhausted) => return Err(TlsError::Exhausted),
        }
    }
    Ok(())
}

impl Server {
    /// Start a session. Nothing is sent until the client's hello arrives.
    pub fn accept(config: &ServerConfig) -> Result<Server, TlsError> {
        let conn = UnbufferedServerConnection::new(config.clone()).map_err(TlsError::Config)?;
        Ok(Server {
            conn,
            phase: Phase::Live,
            closed: false,
            plain: false,
            shut: false,
            extract: config.enable_secret_extraction,
            out: Vec::new(),
            out_at: 0,
            carry: Vec::new(),
            spill: Vec::new(),
            spill_at: 0,
            sni: None,
        })
    }

    /// Hand over ciphertext read from the socket.
    ///
    /// Returns how much was taken, which is always a whole number of
    /// records. The rest is untouched and is passed again with what the
    /// socket gives next. A short take with plaintext waiting means
    /// [`Server::read_plain`] has to drain it before more is taken.
    ///
    /// The bytes that were taken are scratch afterwards. Records are
    /// decrypted where they lie.
    pub fn received(&mut self, src: &mut [u8]) -> Result<usize, TlsError> {
        if self.phase == Phase::Dead {
            return Err(TlsError::Dead);
        }
        if src.is_empty() {
            return Ok(0);
        }
        if self.closed {
            // Nothing after a close notification means anything.
            return Ok(src.len());
        }
        let handshaking = self.conn.is_handshaking();
        if handshaking {
            let _ = SNI.try_with(|s| s.borrow_mut().take());
        }
        let took = if self.carry.is_empty() {
            self.take_in_place(src, handshaking)
        } else {
            let whole = records_end(src, 0, false);
            self.carry.extend_from_slice(&src[..whole]);
            self.take_carried(handshaking).map(|()| whole)
        };
        if handshaking && self.sni.is_none() {
            self.sni = SNI.try_with(|s| s.borrow_mut().take()).ok().flatten();
        }
        if self.out.is_empty() && self.out.capacity() > 0 {
            self.out = Vec::new();
        }
        took
    }

    /// The common path. rustls works on the caller's bytes.
    fn take_in_place(&mut self, src: &mut [u8], handshaking: bool) -> Result<usize, TlsError> {
        let mut at = 0;
        // rustls is shown the buffer a batch at a time. What it has seen
        // only ever grows at the end, which is what its positions need.
        let mut end = records_end(src, 0, true);
        loop {
            let (discard, turn) = turn(&mut self.conn, &mut src[at..end], &mut self.out);
            at += discard;
            match turn {
                Err(e) => return Err(self.fail(e, handshaking)),
                Ok(Turn::Again) => {}
                Ok(Turn::Plain) => {
                    self.plain = true;
                    return Ok(at);
                }
                Ok(Turn::PeerClosed) => self.closed = true,
                Ok(Turn::Rest) => {
                    let more = records_end(src, end, true);
                    if more == end {
                        break;
                    }
                    end = more;
                }
            }
        }
        if self.closed {
            return Ok(src.len());
        }
        // Whole records rustls did not let go of are a handshake message
        // still arriving. It refers to them by position, so they move to a
        // buffer that stays put.
        self.carry.extend_from_slice(&src[at..end]);
        Ok(end)
    }

    /// The rare path, a handshake message over several records. Everything
    /// is copied, and plaintext goes to the spill since it cannot stay put.
    fn take_carried(&mut self, handshaking: bool) -> Result<(), TlsError> {
        let mut at = 0;
        loop {
            let (discard, turn) = turn(&mut self.conn, &mut self.carry[at..], &mut self.out);
            at += discard;
            match turn {
                Err(e) => {
                    self.carry = Vec::new();
                    return Err(self.fail(e, handshaking));
                }
                Ok(Turn::Again) => {}
                Ok(Turn::Plain) => spill_plain(&mut self.conn, &mut self.spill),
                Ok(Turn::PeerClosed) => self.closed = true,
                Ok(Turn::Rest) => break,
            }
        }
        if self.closed || at == self.carry.len() {
            self.carry = Vec::new();
        } else {
            self.carry.drain(..at);
        }
        Ok(())
    }

    /// Record a fatal error, and collect the alert rustls wants sent for it.
    fn fail(&mut self, e: rustls::Error, handshaking: bool) -> TlsError {
        self.phase = Phase::Dead;
        // Only while something is queued. With output waiting rustls hands
        // it over before it looks at its input, and it must not look again:
        // the bytes that failed would fail a second time.
        for _ in 0..64 {
            if !self.conn.wants_write() {
                break;
            }
            match turn(&mut self.conn, &mut [], &mut self.out).1 {
                Ok(Turn::Again) => {}
                Ok(Turn::Plain) => spill_plain(&mut self.conn, &mut self.spill),
                _ => break,
            }
        }
        classify(e, handshaking)
    }

    /// Take decrypted application data.
    ///
    /// Zero means nothing is available, not end of stream. Use
    /// [`Server::peer_closed`] for that.
    pub fn read_plain(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        let mut n = 0;
        if self.spill_at < self.spill.len() {
            let from = &self.spill[self.spill_at..];
            n = from.len().min(out.len());
            out[..n].copy_from_slice(&from[..n]);
            self.spill_at += n;
            if self.spill_at < self.spill.len() {
                return Ok(n);
            }
            self.spill = Vec::new();
            self.spill_at = 0;
        }
        if !self.plain || n == out.len() {
            return Ok(n);
        }
        // Plaintext is reported before the buffer is looked at, so an empty
        // one is safe here.
        let ConnectionState::ReadTraffic(mut rt) = self
            .conn
            .process_tls_records(&mut [])
            .state
            .map_err(TlsError::Protocol)?
        else {
            self.plain = false;
            return Ok(n);
        };
        while n < out.len() {
            let Some(Ok(rec)) = rt.next_record() else {
                self.plain = false;
                return Ok(n);
            };
            let fit = rec.payload.len().min(out.len() - n);
            out[n..n + fit].copy_from_slice(&rec.payload[..fit]);
            n += fit;
            // A record is taken whole or not at all, the rest waits here.
            self.spill.extend_from_slice(&rec.payload[fit..]);
        }
        Ok(n)
    }

    /// Encrypt plaintext into `out`, ready for the socket.
    ///
    /// Anything [`Server::transmit`] still holds goes first, since it was
    /// sealed first. Then as many records as fit, the last one cut to what
    /// the room allows. Nothing is taken while the handshake is running or
    /// when `out` has no room for a record.
    pub fn write_plain(&mut self, src: &[u8], out: &mut [u8]) -> Result<Wrote, TlsError> {
        if self.phase == Phase::Dead {
            return Err(TlsError::Dead);
        }
        let mut done = Wrote {
            took: 0,
            wrote: self.transmit(out)?,
        };
        if self.wants_write() || src.is_empty() || self.shut {
            return Ok(done);
        }
        loop {
            let act = match self.conn.process_tls_records(&mut self.carry).state {
                Ok(ConnectionState::WriteTraffic(mut w)) => {
                    return match seal(&mut w, src, out, &mut done) {
                        Ok(()) => Ok(done),
                        Err(e) => {
                            self.phase = Phase::Dead;
                            Err(e)
                        }
                    };
                }
                Ok(ConnectionState::ReadTraffic(_)) => Act::Spill,
                Ok(ConnectionState::PeerClosed) => Act::PeerClosed,
                Ok(ConnectionState::TransmitTlsData(t)) => {
                    t.done();
                    Act::Again
                }
                // Queued handshake output is flushed by the encrypt itself,
                // and anything else means the session cannot send.
                Ok(_) => return Ok(done),
                Err(e) => Act::Fail(e),
            };
            if let Act::Fail(e) = act {
                return Err(self.fail(e, false));
            }
            self.act(act);
        }
    }

    fn act(&mut self, act: Act) {
        match act {
            Act::Again | Act::Fail(_) => {}
            Act::Spill => spill_plain(&mut self.conn, &mut self.spill),
            Act::PeerClosed => self.closed = true,
        }
    }

    /// Take what the session itself has to send.
    ///
    /// Zero means there is nothing. Call this after every `received` and
    /// after `close`, because both produce output, a handshake flight,
    /// tickets, a key update, an alert.
    pub fn transmit(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        let from = &self.out[self.out_at..];
        let n = from.len().min(out.len());
        out[..n].copy_from_slice(&from[..n]);
        self.out_at += n;
        if self.out_at == self.out.len() {
            // Released, not kept. A flight is a few kilobytes and an
            // established connection should not go on holding it.
            self.out = Vec::new();
            self.out_at = 0;
        }
        Ok(n)
    }

    /// Begin a clean shutdown. The alert comes out of [`Server::transmit`],
    /// and nothing is written after it. Does nothing before the handshake
    /// is complete.
    pub fn close(&mut self) {
        if self.phase == Phase::Dead {
            return;
        }
        let mut alert = [0u8; 128];
        for _ in 0..8 {
            let act = match self.conn.process_tls_records(&mut self.carry).state {
                Ok(ConnectionState::WriteTraffic(mut w)) => {
                    // When it does not fit rustls keeps the alert queued and
                    // the next turn collects it.
                    let n = w.queue_close_notify(&mut alert).unwrap_or(0);
                    self.out.extend_from_slice(&alert[..n]);
                    self.shut = true;
                    break;
                }
                Ok(ConnectionState::ReadTraffic(_)) => Act::Spill,
                Ok(ConnectionState::PeerClosed) => Act::PeerClosed,
                Ok(ConnectionState::TransmitTlsData(t)) => {
                    t.done();
                    Act::Again
                }
                _ => break,
            };
            self.act(act);
        }
        // Whatever rustls queued instead of writing.
        while let (_, Ok(Turn::Again)) = turn(&mut self.conn, &mut self.carry, &mut self.out) {}
    }

    /// Whether the handshake is still in progress.
    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// Whether [`Server::transmit`] has bytes for the socket.
    pub fn wants_write(&self) -> bool {
        self.out_at < self.out.len()
    }

    /// Whether the peer sent a close notification, the only honest end of
    /// stream.
    pub fn peer_closed(&self) -> bool {
        self.closed
    }

    /// The protocol agreed by ALPN, if any.
    pub fn alpn(&self) -> Option<&[u8]> {
        self.conn.alpn_protocol()
    }

    /// The name the client asked for, once its hello has been read.
    pub fn sni(&self) -> Option<&str> {
        self.sni.as_deref()
    }

    /// The version in use, once the client's hello has been answered.
    pub fn version(&self) -> Option<ProtocolVersion> {
        self.conn.protocol_version()
    }

    /// The suite in use, once the client's hello has been answered.
    pub fn suite(&self) -> Option<CipherSuite> {
        self.conn.negotiated_cipher_suite().map(|s| s.suite())
    }

    /// Whether the handshake resumed an earlier session and so skipped the
    /// certificate and its signature.
    pub fn resumed(&self) -> bool {
        self.conn.handshake_kind() == Some(rustls::HandshakeKind::Resumed)
    }

    /// Whether [`Server::handoff`] would succeed now, and why not.
    pub fn handoff_ready(&self) -> Result<(), Blocked> {
        if !self.extract {
            Err(Blocked::ExtractOff)
        } else if self.phase == Phase::Dead || self.conn.is_handshaking() {
            Err(Blocked::Handshaking)
        } else if self.wants_write() {
            Err(Blocked::Unsent)
        } else if self.plain || self.spill_at < self.spill.len() {
            Err(Blocked::Unread)
        } else if !self.carry.is_empty() {
            Err(Blocked::MidMessage)
        } else {
            Ok(())
        }
    }

    /// End the session here and give its record protection to someone else.
    ///
    /// The sequence numbers count every record sent and received so far,
    /// tickets included, so the next record in each direction is the one
    /// they name. Check [`Server::handoff_ready`] first, on an error the
    /// session is gone.
    pub fn handoff(self) -> Result<Handoff, TlsError> {
        self.handoff_ready().map_err(TlsError::Handoff)?;
        let (secrets, conn) =
            self.conn
                .dangerous_into_kernel_connection()
                .map_err(|e| match e {
                    rustls::Error::General(_) => TlsError::Handoff(Blocked::Unsent),
                    e => TlsError::Config(e),
                })?;
        Ok(Handoff {
            version: conn.protocol_version(),
            suite: conn.negotiated_cipher_suite().suite(),
            tx: Secret::new(secrets.tx.0, secrets.tx.1)?,
            rx: Secret::new(secrets.rx.0, secrets.rx.1)?,
            rekey: Rekey { conn },
        })
    }
}

#[cfg(test)]
mod tests;
