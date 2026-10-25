//! A real handshake, driven by two byte buffers.
//!
//! No sockets and no network. A rustls server runs in the same process and the
//! test moves ciphertext between the two by hand, which is exactly the calling
//! convention an application uses and is why this layer is sans-io at all.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use rustls::server::ServerConnection;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

const NAME: &str = "stein.test";

/// A certificate for `NAME` and a client config that trusts it.
struct Pair {
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
}

fn pair() -> Pair {
    pair_for(NAME, NAME)
}

/// A certificate issued for `cert_name`, trusted by a client that will be
/// told it is talking to `trust_name`.
fn pair_for(cert_name: &str, _trust_name: &str) -> Pair {
    let issued = rcgen::generate_simple_self_signed(std::vec![cert_name.to_string()])
        .expect("a certificate");
    let cert = CertificateDer::from(issued.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(issued.signing_key.serialize_der()).expect("a key");

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(std::vec![cert.clone()], key)
        .expect("a server config");

    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("the root is parseable");
    let client = config_with(store).expect("a client config");

    Pair {
        client,
        server: Arc::new(server),
    }
}

/// Move everything each side wants to send to the other until both are quiet.
///
/// Returns how many round trips it took, so a test can assert the handshake
/// actually happened rather than silently doing nothing.
fn pump(c: &mut Tls, s: &mut ServerConnection) -> usize {
    let mut buf = std::vec![0u8; 32 * 1024];
    for turn in 1..64 {
        let mut moved = false;
        // Client to server.
        let n = c.transmit(&mut buf).expect("client transmit");
        if n > 0 {
            let mut cursor = &buf[..n];
            s.read_tls(&mut cursor).expect("server read");
            s.process_new_packets().expect("server packets");
            moved = true;
        }
        // Server to client.
        let mut out = Vec::new();
        while s.wants_write() {
            s.write_tls(&mut out).expect("server write");
        }
        if !out.is_empty() {
            give(c, &out);
            moved = true;
        }
        if !moved {
            return turn;
        }
    }
    panic!("the handshake never settled");
}

/// Hand every byte over, in as many calls as it takes.
fn give(c: &mut Tls, out: &[u8]) {
    let mut at = 0;
    while at < out.len() {
        let took = c.received(&out[at..]).expect("client received");
        if took == 0 {
            return;
        }
        at += took;
    }
}

fn server_of(p: &Pair) -> ServerConnection {
    ServerConnection::new(p.server.clone()).expect("a server connection")
}

#[test]
fn a_handshake_completes_over_two_byte_buffers() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    assert!(c.is_handshaking(), "it was done before it started");

    pump(&mut c, &mut s);
    assert!(!c.is_handshaking(), "the handshake did not finish");
    assert!(!s.is_handshaking());
    assert_eq!(c.version(), Some(rustls::ProtocolVersion::TLSv1_3));
}

#[test]
fn application_data_crosses_in_both_directions() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    pump(&mut c, &mut s);

    // Client to server.
    let sent = b"GET /feed.xml HTTP/1.1\r\nHost: stein.test\r\n\r\n";
    assert_eq!(c.write_plain(sent).unwrap(), sent.len());
    pump(&mut c, &mut s);
    let mut got = std::vec![0u8; 256];
    let n = std::io::Read::read(&mut s.reader(), &mut got).expect("server reads");
    assert_eq!(&got[..n], sent);

    // Server to client.
    let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
    std::io::Write::write_all(&mut s.writer(), reply).expect("server writes");
    pump(&mut c, &mut s);
    let mut got = std::vec![0u8; 256];
    let n = c.read_plain(&mut got).expect("client reads");
    assert_eq!(&got[..n], reply);
}

/// Data written before the handshake finishes is buffered, not lost. A caller
/// that sends a request immediately must get the same result as one that waits.
#[test]
fn plaintext_written_during_the_handshake_still_arrives() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    assert!(c.is_handshaking());
    let early = b"sent before the handshake finished";
    c.write_plain(early).expect("it buffers");

    pump(&mut c, &mut s);
    let mut got = std::vec![0u8; 256];
    let n = std::io::Read::read(&mut s.reader(), &mut got).expect("server reads");
    assert_eq!(&got[..n], early);
}

/// The certificate is for one name and the client asked for another, which is
/// the failure that matters most and the one a lax implementation misses.
#[test]
fn a_certificate_for_the_wrong_name_is_rejected() {
    let p = pair_for("elsewhere.test", NAME);
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);

    let mut buf = std::vec![0u8; 32 * 1024];
    let n = c.transmit(&mut buf).expect("client hello");
    let mut cursor = &buf[..n];
    s.read_tls(&mut cursor).expect("server read");
    s.process_new_packets().expect("server packets");
    let mut out = Vec::new();
    while s.wants_write() {
        s.write_tls(&mut out).expect("server write");
    }

    let err = c.received(&out).expect_err("the wrong name was accepted");
    assert!(
        matches!(err, TlsError::BadCertificate(_)),
        "wrong error: {err}"
    );
}

/// An untrusted issuer must fail too, even when the name is right.
#[test]
fn a_certificate_from_an_untrusted_issuer_is_rejected() {
    let trusted = pair();
    let other = pair();
    // A client that trusts one certificate, a server presenting a different
    // one for the same name.
    let mut c = Tls::connect(&trusted.client, NAME).expect("a client");
    let mut s = server_of(&other);

    let mut buf = std::vec![0u8; 32 * 1024];
    let n = c.transmit(&mut buf).expect("client hello");
    let mut cursor = &buf[..n];
    s.read_tls(&mut cursor).expect("server read");
    s.process_new_packets().expect("server packets");
    let mut out = Vec::new();
    while s.wants_write() {
        s.write_tls(&mut out).expect("server write");
    }

    let err = c
        .received(&out)
        .expect_err("an untrusted issuer was accepted");
    assert!(
        matches!(err, TlsError::BadCertificate(_)),
        "wrong error: {err}"
    );
}

/// Garbage is a protocol error, not a panic and not a hang.
#[test]
fn garbage_from_the_peer_is_a_protocol_error() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut buf = std::vec![0u8; 4096];
    c.transmit(&mut buf).expect("client hello");
    let err = c.received(&[0xff; 64]).expect_err("garbage was accepted");
    assert!(
        matches!(err, TlsError::Handshake(_) | TlsError::Protocol(_)),
        "wrong error: {err}"
    );
}

/// Every truncation of the server's first flight must error or ask for more,
/// never panic and never claim the handshake is done.
#[test]
fn a_truncated_flight_never_completes_the_handshake() {
    let p = pair();
    let mut buf = std::vec![0u8; 32 * 1024];
    let mut c0 = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    let n = c0.transmit(&mut buf).expect("client hello");
    let mut cursor = &buf[..n];
    s.read_tls(&mut cursor).expect("server read");
    s.process_new_packets().expect("server packets");
    let mut flight = Vec::new();
    while s.wants_write() {
        s.write_tls(&mut flight).expect("server write");
    }
    assert!(flight.len() > 64, "the flight is too small to truncate");

    for cut in 1..flight.len().min(600) {
        let mut c = Tls::connect(&p.client, NAME).expect("a client");
        let mut scratch = std::vec![0u8; 32 * 1024];
        c.transmit(&mut scratch).expect("client hello");
        let _ = c.received(&flight[..cut]);
        assert!(
            c.is_handshaking(),
            "a {cut} byte flight completed the handshake"
        );
    }
}

/// A close notification is the only honest end of stream, and it has to be
/// distinguishable from a socket that merely stopped.
#[test]
fn a_clean_close_is_reported_as_one() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    pump(&mut c, &mut s);
    assert!(!c.peer_closed(), "closed before anything happened");

    s.send_close_notify();
    pump(&mut c, &mut s);
    assert!(c.peer_closed(), "a close notification was not noticed");
}

/// Stopping without a notification must not look like a clean close, because
/// that is exactly the truncation an attacker wants to be invisible.
#[test]
fn a_socket_that_simply_stops_is_not_a_clean_close() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    pump(&mut c, &mut s);
    // Nothing more arrives, which is what a cut connection looks like.
    assert!(!c.peer_closed());
    let mut out = std::vec![0u8; 16];
    assert_eq!(c.read_plain(&mut out).unwrap(), 0);
    assert!(!c.peer_closed(), "silence was read as a close");
}

/// A large body has to cross in whatever chunks the socket happens to give,
/// which is the case a buffer size bug shows up in. Neither side is allowed a
/// convenient chunk boundary.
#[test]
fn a_large_body_crosses_in_arbitrary_chunks() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    pump(&mut c, &mut s);

    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut got = Vec::new();
    let mut plain = std::vec![0u8; 1777];
    let mut wire = Vec::new();

    // The server's own buffer is bounded, so it has to be drained as it goes
    // rather than handed the whole body at once.
    for chunk in body.chunks(4096) {
        std::io::Write::write_all(&mut s.writer(), chunk).expect("server writes");
        wire.clear();
        while s.wants_write() {
            s.write_tls(&mut wire).expect("server write");
        }
        feed(&mut c, &wire, &mut plain, &mut got);
    }
    assert_eq!(got.len(), body.len(), "the body did not arrive whole");
    assert_eq!(got, body);
}

/// Hand ciphertext to the client in deliberately awkward pieces, draining the
/// plaintext as it appears.
fn feed(c: &mut Tls, wire: &[u8], plain: &mut [u8], got: &mut Vec<u8>) {
    let mut at = 0;
    while at < wire.len() {
        let end = (at + 997).min(wire.len());
        let took = c.received(&wire[at..end]).expect("client received");
        at += took.max(1);
        drain(c, plain, got);
    }
    drain(c, plain, got);
}

fn drain(c: &mut Tls, plain: &mut [u8], got: &mut Vec<u8>) {
    loop {
        let n = c.read_plain(plain).expect("client reads");
        if n == 0 {
            return;
        }
        got.extend_from_slice(&plain[..n]);
    }
}

#[test]
fn an_empty_buffer_is_accepted_everywhere() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    assert_eq!(c.received(&[]).unwrap(), 0);
    assert_eq!(c.write_plain(&[]).unwrap(), 0);
    assert_eq!(c.read_plain(&mut []).unwrap(), 0);
    assert_eq!(c.transmit(&mut []).unwrap(), 0);
}

/// A name that cannot be on a certificate is refused before any io happens.
#[test]
fn a_name_that_is_not_a_server_name_is_refused() {
    let p = pair();
    for bad in ["", "not a host name", "http://example.com/", "exa mple.com"] {
        match Tls::connect(&p.client, bad) {
            Err(TlsError::BadName(_)) => {}
            Err(e) => panic!("wrong error for {bad:?}: {e}"),
            Ok(_) => panic!("{bad:?} was accepted as a server name"),
        }
    }
}

#[test]
fn an_ip_address_is_a_valid_server_name() {
    let p = pair();
    assert!(Tls::connect(&p.client, "127.0.0.1").is_ok());
    assert!(Tls::connect(&p.client, "::1").is_ok());
}

/// The first thing out of a fresh connection is the client hello, and there
/// must be one before anything is received.
#[test]
fn a_fresh_connection_has_something_to_send() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    assert!(c.wants_write(), "nothing to send");
    let mut buf = std::vec![0u8; 4096];
    let n = c.transmit(&mut buf).expect("client hello");
    assert!(n > 32, "the hello is {n} bytes");
    // A TLS record starts with the content type and the legacy version.
    assert_eq!(buf[0], 0x16, "not a handshake record");
}

/// Transmitting into a buffer too small for the whole flight has to be safe
/// and resumable, since a socket write can be short for any reason.
#[test]
fn a_small_transmit_buffer_is_resumable() {
    let p = pair();
    let mut c = Tls::connect(&p.client, NAME).expect("a client");
    let mut s = server_of(&p);
    let mut all = Vec::new();
    let mut small = [0u8; 17];
    while c.wants_write() {
        let n = c.transmit(&mut small).expect("transmit");
        if n == 0 {
            break;
        }
        all.extend_from_slice(&small[..n]);
    }
    assert!(all.len() > 32, "only {} bytes came out", all.len());
    // And what came out in pieces is still a valid hello.
    let mut cursor = &all[..];
    s.read_tls(&mut cursor).expect("server read");
    s.process_new_packets()
        .expect("the reassembled hello parses");
}
