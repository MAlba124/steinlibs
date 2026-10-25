//! The crate's two halves against each other, in memory.
//!
//! The client role on one side, the server role on the other, and a test
//! that carries bytes between them in whatever pieces it likes. The handoff
//! is checked without a kernel. A record layer written here with ring picks
//! up from the secrets and sequence numbers and has to interoperate with
//! the client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::Tls;
use ring::aead;
use rustls::client::ClientConnection;
use rustls_pki_types::ServerName;

const NAME: &str = "stein.test";
const H2: &[u8] = b"h2";
const H1: &[u8] = b"http/1.1";

/// A self signed certificate and its key, in both encodings.
struct Ident {
    cert: Vec<u8>,
    key: Vec<u8>,
    cert_pem: String,
    key_pem: String,
}

fn ident(name: &str) -> Ident {
    let made = rcgen::generate_simple_self_signed(vec![name.to_string()]).expect("a certificate");
    Ident {
        cert: made.cert.der().to_vec(),
        key: made.signing_key.serialize_der(),
        cert_pem: made.cert.pem(),
        key_pem: made.signing_key.serialize_pem(),
    }
}

/// The plainest setup. Tests change the field they are about.
fn setup<'a>(id: &'a Ident, chain: &'a [&'a [u8]]) -> ServerSetup<'a> {
    ServerSetup {
        chain: Certs::Der(chain),
        key: Key::Der(&id.key),
        alpn: &[],
        tls13_only: false,
        suites: &[],
        tickets: Tickets::Off,
        extract: false,
    }
}

fn server_with(id: &Ident, change: impl FnOnce(&mut ServerSetup<'_>)) -> ServerConfig {
    let chain = [&id.cert[..]];
    let mut s = setup(id, &chain);
    change(&mut s);
    server_config(&s).expect("a server config")
}

/// A client configuration trusting `id`, shaped by `change`.
fn client_with(
    id: &Ident,
    versions: &[&'static rustls::SupportedProtocolVersion],
    suites: &[CipherSuite],
    change: impl FnOnce(&mut rustls::ClientConfig),
) -> crate::Config {
    let mut store = rustls::RootCertStore::empty();
    store
        .add(CertificateDer::from(id.cert.clone()))
        .expect("the root parses");
    let mut provider = rustls::crypto::ring::default_provider();
    if !suites.is_empty() {
        provider
            .cipher_suites
            .retain(|s| suites.contains(&s.suite()));
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(versions)
        .expect("versions")
        .with_root_certificates(store)
        .with_no_client_auth();
    change(&mut cfg);
    Arc::new(cfg)
}

fn client(id: &Ident) -> crate::Config {
    client_with(id, rustls::DEFAULT_VERSIONS, &[], |_| {})
}

fn client_alpn(id: &Ident, alpn: &[&[u8]]) -> crate::Config {
    client_with(id, rustls::DEFAULT_VERSIONS, &[], |c| {
        c.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    })
}

/// xorshift, so a failing seed can be run again.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// 1 to `max`, leaning small so cuts land inside headers too.
    fn cut(&mut self, max: usize) -> usize {
        let r = self.next();
        let cap = if r & 3 == 0 { 7 } else { max };
        1 + (self.next() as usize) % cap.max(1)
    }
}

/// Both ends and the bytes between them.
struct Link {
    c: Tls,
    s: Server,
    /// What the server's socket gave that the server has not taken.
    inbox: Vec<u8>,
    got_s: Vec<u8>,
    got_c: Vec<u8>,
    /// Everything the server put on the wire.
    wire_s: Vec<u8>,
    /// How much of it there was when the server's handshake completed.
    shook: Option<usize>,
    /// Cuts both directions into pieces when set.
    rng: Option<Rng>,
}

impl Link {
    fn new(ccfg: &crate::Config, scfg: &ServerConfig, name: &str) -> Link {
        Link {
            c: Tls::connect(ccfg, name).expect("a client"),
            s: Server::accept(scfg).expect("a server"),
            inbox: Vec::new(),
            got_s: Vec::new(),
            got_c: Vec::new(),
            wire_s: Vec::new(),
            shook: None,
            rng: None,
        }
    }

    fn seeded(mut self, seed: u64) -> Link {
        self.rng = Some(Rng(seed | 1));
        self
    }

    fn piece(&mut self, left: usize) -> usize {
        match &mut self.rng {
            Some(r) => r.cut(4000).min(left),
            None => left,
        }
    }

    /// Let the server take what it will from its inbox and read it out.
    fn serve(&mut self) -> Result<(), TlsError> {
        let mut plain = [0u8; 3001];
        loop {
            let took = self.s.received(&mut self.inbox)?;
            self.inbox.drain(..took);
            let mut read = 0;
            loop {
                let n = self.s.read_plain(&mut plain)?;
                if n == 0 {
                    break;
                }
                self.got_s.extend_from_slice(&plain[..n]);
                read += n;
            }
            if took == 0 && read == 0 {
                return Ok(());
            }
        }
    }

    fn give_server(&mut self, wire: &[u8]) -> Result<(), TlsError> {
        let mut at = 0;
        while at < wire.len() {
            let n = self.piece(wire.len() - at);
            self.inbox.extend_from_slice(&wire[at..at + n]);
            at += n;
            self.serve()?;
        }
        Ok(())
    }

    fn give_client(&mut self, wire: &[u8]) -> Result<(), TlsError> {
        self.wire_s.extend_from_slice(wire);
        let mut plain = [0u8; 2999];
        let mut at = 0;
        while at < wire.len() {
            let n = self.piece(wire.len() - at);
            let mut given = 0;
            while given < n {
                given += self.c.received(&wire[at + given..at + n])?;
                loop {
                    let r = self.c.read_plain(&mut plain)?;
                    if r == 0 {
                        break;
                    }
                    self.got_c.extend_from_slice(&plain[..r]);
                }
            }
            at += n;
        }
        Ok(())
    }

    /// Carry bytes both ways until neither side has any. The error is the
    /// first one either side reports, with which side it was.
    fn pump(&mut self) -> Result<(), (char, TlsError)> {
        let mut buf = vec![0u8; 20_000];
        for _ in 0..10_000 {
            let mut moved = false;
            let n = self.c.transmit(&mut buf).map_err(|e| ('c', e))?;
            if n > 0 {
                moved = true;
                self.give_server(&buf[..n]).map_err(|e| ('s', e))?;
            }
            if self.shook.is_none() && !self.s.is_handshaking() {
                self.shook = Some(self.wire_s.len());
            }
            let n = self.s.transmit(&mut buf).map_err(|e| ('s', e))?;
            if n > 0 {
                moved = true;
                self.give_client(&buf[..n]).map_err(|e| ('c', e))?;
            }
            if !moved {
                return Ok(());
            }
        }
        panic!("the two sides never settled");
    }

    fn shake(mut self) -> Link {
        self.pump().expect("the handshake");
        assert!(!self.c.is_handshaking(), "the client is still handshaking");
        assert!(!self.s.is_handshaking(), "the server is still handshaking");
        self
    }

    /// Send from the server through an output buffer of `room` bytes.
    fn s_send(&mut self, data: &[u8], room: usize) {
        let mut out = vec![0u8; room];
        let mut at = 0;
        while at < data.len() {
            let w = self.s.write_plain(&data[at..], &mut out).expect("a write");
            assert!(w.took > 0 || w.wrote > 0, "a write made no progress");
            at += w.took;
            self.give_client(&out[..w.wrote])
                .expect("the client takes it");
        }
    }

    fn c_send(&mut self, data: &[u8]) {
        let mut at = 0;
        while at < data.len() {
            at += self.c.write_plain(&data[at..]).expect("a write");
            self.pump().expect("the pump");
        }
    }
}

/// The one suite, as a slice that outlives any setup.
fn only(suite: CipherSuite) -> &'static [CipherSuite] {
    const ALL: &[CipherSuite; 3] = &KTLS_SUITES;
    let i = ALL.iter().position(|s| *s == suite).unwrap();
    &ALL[i..=i]
}

fn body(len: usize, salt: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(salt) % 251) as u8)
        .collect()
}

/// How many records `wire` is.
fn records(wire: &[u8]) -> usize {
    let mut at = 0;
    let mut n = 0;
    while let Some(len) = record_len(&wire[at..]) {
        at += len;
        n += 1;
    }
    assert_eq!(at, wire.len(), "the wire ends inside a record");
    n
}

#[test]
fn the_two_roles_complete_a_handshake() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME);
    assert!(l.s.is_handshaking());
    assert!(!l.s.wants_write(), "a server speaks second");
    l = l.shake();
    assert_eq!(l.s.version(), Some(ProtocolVersion::TLSv1_3));
    assert_eq!(l.c.version(), Some(ProtocolVersion::TLSv1_3));
    assert!(l.s.suite().is_some());
    assert!(!l.s.resumed());
    assert!(!l.s.peer_closed());
}

#[test]
fn alpn_is_the_servers_first_choice_the_client_offered() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.alpn = &[H2, H1]);
    type Case<'a> = (&'a [&'a [u8]], Option<&'a [u8]>);
    let cases: [Case<'_>; 5] = [
        (&[H2], Some(H2)),
        (&[H1], Some(H1)),
        (&[H1, H2], Some(H2)),
        (&[b"spdy/3", H1], Some(H1)),
        (&[], None),
    ];
    for (offer, want) in cases {
        let l = Link::new(&client_alpn(&id, offer), &scfg, NAME).shake();
        assert_eq!(l.s.alpn(), want, "offered {offer:?}");
        assert_eq!(l.c.alpn(), want, "offered {offer:?}");
    }
    // The other way round, a server with one protocol and a client with two.
    let scfg = server_with(&id, |s| s.alpn = &[H1]);
    let l = Link::new(&client_alpn(&id, &[H2, H1]), &scfg, NAME).shake();
    assert_eq!(l.s.alpn(), Some(H1));
    // A server offering none ignores what the client asks for.
    let scfg = server_with(&id, |_| {});
    let l = Link::new(&client_alpn(&id, &[H2]), &scfg, NAME).shake();
    assert_eq!(l.s.alpn(), None);
}

#[test]
fn alpn_with_nothing_in_common_fails_both_sides() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.alpn = &[H2, H1]);
    let mut l = Link::new(&client_alpn(&id, &[b"spdy/3"]), &scfg, NAME);
    let (side, err) = l.pump().expect_err("a protocol was agreed");
    assert_eq!(side, 's');
    assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
    // The server's alert, behind the hello it had already written, reaches
    // the client as its own failure.
    let mut alert = [0u8; 4096];
    let n = l.s.transmit(&mut alert).unwrap();
    assert!(n > 0, "no alert was queued");
    let err =
        l.c.received(&alert[..n])
            .expect_err("the alert was ignored");
    assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
}

#[test]
fn the_name_the_client_asked_for_is_reported() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let l = Link::new(&client(&id), &scfg, NAME);
    assert_eq!(l.s.sni(), None, "a name before any hello");
    let l = l.shake();
    assert_eq!(l.s.sni(), Some(NAME));

    // An address is not a name and is not sent as one.
    let ip = ident("127.0.0.1");
    let scfg = server_with(&ip, |_| {});
    let l = Link::new(&client(&ip), &scfg, "127.0.0.1").shake();
    assert_eq!(l.s.sni(), None);

    // One connection's name does not leak into the next on the thread.
    let scfg = server_with(&id, |_| {});
    let a = Link::new(&client(&id), &scfg, NAME).shake();
    let b = Link::new(&client(&ip), &server_with(&ip, |_| {}), "127.0.0.1").shake();
    assert_eq!(a.s.sni(), Some(NAME));
    assert_eq!(b.s.sni(), None);
}

#[test]
fn a_chain_the_client_does_not_trust_fails_both_sides() {
    let served = ident(NAME);
    let trusted = ident(NAME);
    let scfg = server_with(&served, |_| {});
    let mut l = Link::new(&client(&trusted), &scfg, NAME);
    let (side, err) = l.pump().expect_err("an untrusted chain was accepted");
    assert_eq!(side, 'c');
    assert!(
        matches!(err, TlsError::BadCertificate(_)),
        "wrong error: {err}"
    );
    // The client's alert is the server's failure.
    let (side, err) = l.pump().expect_err("the server carried on");
    assert_eq!(side, 's');
    assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
    assert!(l.s.is_handshaking());
    assert!(matches!(
        l.s.received(&mut [22, 3, 3, 0, 0]),
        Err(TlsError::Dead)
    ));
}

#[test]
fn a_chain_for_another_name_fails_the_client() {
    let id = ident("elsewhere.test");
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME);
    let (side, err) = l.pump().expect_err("the wrong name was accepted");
    assert_eq!(side, 'c');
    assert!(
        matches!(err, TlsError::BadCertificate(_)),
        "wrong error: {err}"
    );
}

#[test]
fn tls12_is_spoken_unless_refused() {
    let id = ident(NAME);
    let old = client_with(&id, &[&rustls::version::TLS12], &[], |_| {});

    let mut l = Link::new(&old, &server_with(&id, |_| {}), NAME).shake();
    assert_eq!(l.s.version(), Some(ProtocolVersion::TLSv1_2));
    l.c_send(b"over 1.2");
    l.s_send(b"and back", 4096);
    assert_eq!(l.got_s, b"over 1.2");
    assert_eq!(l.got_c, b"and back");

    let scfg = server_with(&id, |s| s.tls13_only = true);
    let mut l = Link::new(&old, &scfg, NAME);
    let (side, err) = l.pump().expect_err("1.2 was accepted");
    assert_eq!(side, 's');
    assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
}

#[test]
fn each_kernel_suite_can_be_the_only_one() {
    let id = ident(NAME);
    for suite in KTLS_SUITES {
        let scfg = server_with(&id, |s| {
            s.tls13_only = true;
            s.suites = only(suite);
        });
        let mut l = Link::new(&client(&id), &scfg, NAME).shake();
        assert_eq!(l.s.suite(), Some(suite));
        l.c_send(b"ping");
        l.s_send(b"pong", 4096);
        assert_eq!((&l.got_s[..], &l.got_c[..]), (&b"ping"[..], &b"pong"[..]));

        // A client that has only some other suite gets nowhere.
        let other = KTLS_SUITES.iter().copied().find(|s| *s != suite).unwrap();
        let ccfg = client_with(&id, rustls::DEFAULT_VERSIONS, &[other], |_| {});
        let mut l = Link::new(&ccfg, &scfg, NAME);
        let (side, err) = l.pump().expect_err("a suite outside the set was agreed");
        assert_eq!(side, 's');
        assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
    }
}

#[test]
fn the_kernel_suites_together_leave_only_tls13_suites() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.suites = &KTLS_SUITES);
    let old = client_with(&id, &[&rustls::version::TLS12], &[], |_| {});
    let mut l = Link::new(&old, &scfg, NAME);
    assert!(l.pump().is_err(), "1.2 was agreed with no 1.2 suite");
}

#[test]
fn a_configuration_says_what_is_wrong_with_it() {
    let id = ident(NAME);
    let other = ident(NAME);
    let chain = [&id.cert[..]];
    let good = setup(&id, &chain);

    let no_suites = [CipherSuite::TLS_RSA_WITH_AES_128_CBC_SHA];
    let e = server_config(&ServerSetup {
        suites: &no_suites,
        ..good
    });
    assert!(matches!(e, Err(TlsError::NoSuites)));

    let e = server_config(&ServerSetup {
        chain: Certs::Der(&[]),
        ..good
    });
    assert!(matches!(e, Err(TlsError::NoCertificate)));

    let e = server_config(&ServerSetup {
        chain: Certs::Pem(b"no sections here"),
        ..good
    });
    assert!(matches!(e, Err(TlsError::NoCertificate)));

    let e = server_config(&ServerSetup {
        key: Key::Pem(id.cert_pem.as_bytes()),
        ..good
    });
    assert!(matches!(e, Err(TlsError::BadKeyPem(_))));

    let e = server_config(&ServerSetup {
        key: Key::Der(b"not a key"),
        ..good
    });
    assert!(matches!(e, Err(TlsError::BadKeyDer(_))));

    let e = server_config(&ServerSetup {
        key: Key::Der(&other.key),
        ..good
    });
    assert!(
        matches!(e, Err(TlsError::Config(_))),
        "a key for another certificate"
    );

    let e = server_config(&ServerSetup {
        chain: Certs::Der(&[b"not a certificate"]),
        ..good
    });
    assert!(matches!(e, Err(TlsError::Config(_))));
}

#[test]
fn a_chain_and_key_in_pem_work() {
    let id = ident(NAME);
    let scfg = server_config(&ServerSetup {
        chain: Certs::Pem(id.cert_pem.as_bytes()),
        key: Key::Pem(id.key_pem.as_bytes()),
        ..setup(&id, &[])
    })
    .expect("pem is accepted");
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    l.c_send(b"hello");
    assert_eq!(l.got_s, b"hello");
}

/// Every seed cuts both directions differently, inside headers, inside
/// bodies, several records to a piece and one record over many.
#[test]
fn data_crosses_both_ways_however_the_bytes_are_cut() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.tickets = Tickets::Stored(8));
    let ccfg = client(&id);
    for seed in 1..=120u64 {
        let mut l = Link::new(&ccfg, &scfg, NAME)
            .seeded(seed * 0x9e37_79b9)
            .shake();
        let mut rng = Rng(seed);
        let (mut up, mut down) = (Vec::new(), Vec::new());
        for round in 0..12 {
            let n = rng.cut(40_000);
            let data = body(n, round);
            if rng.next() & 1 == 0 {
                l.c_send(&data);
                up.extend_from_slice(&data);
            } else {
                let room = 64 + rng.cut(40_000);
                l.s_send(&data, room);
                down.extend_from_slice(&data);
            }
        }
        l.pump().expect("the pump");
        assert_eq!(l.got_s, up, "seed {seed}, client to server");
        assert_eq!(l.got_c, down, "seed {seed}, server to client");
        assert!(l.inbox.is_empty(), "seed {seed}, bytes left over");
    }
}

#[test]
fn a_large_transfer_crosses_each_way() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).seeded(7).shake();
    let up = body(3_000_000, 1);
    let down = body(5_000_000, 2);
    l.c_send(&up);
    l.s_send(&down, 70_000);
    assert!(l.got_s == up, "client to server differs");
    assert!(l.got_c == down, "server to client differs");
}

/// A partial record is never taken, whatever the cut, so what the caller
/// still holds is always the untouched front of one record.
#[test]
fn only_whole_records_are_taken() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    l.c.write_plain(b"one").unwrap();
    l.c.write_plain(b"two").unwrap();
    let mut wire = vec![0u8; 4096];
    let n = l.c.transmit(&mut wire).unwrap();
    wire.truncate(n);
    let first = record_len(&wire).unwrap();
    assert!(first < n, "the two writes were one record");

    let mut inbox = Vec::new();
    let mut got = Vec::new();
    let mut plain = [0u8; 16];
    for (i, b) in wire.iter().enumerate() {
        inbox.push(*b);
        let before = inbox.clone();
        let took = l.s.received(&mut inbox).unwrap();
        let whole = i + 1 == first || i + 1 == n;
        assert_eq!(took > 0, whole, "byte {i}");
        if took == 0 {
            assert_eq!(inbox, before, "untaken bytes were written to");
            assert_eq!(record_len(&inbox).is_some(), inbox.len() >= 5);
        }
        inbox.drain(..took);
        let r = l.s.read_plain(&mut plain).unwrap();
        got.extend_from_slice(&plain[..r]);
    }
    assert_eq!(got, b"onetwo");
    assert_eq!(l.s.received(&mut []).unwrap(), 0);
}

#[test]
fn record_len_reads_the_header() {
    assert_eq!(record_len(&[23, 3, 3, 0]), None);
    assert_eq!(record_len(&[23, 3, 3, 0, 0]), Some(5));
    assert_eq!(record_len(&[23, 3, 3, 1, 2, 9, 9]), Some(5 + 258));
}

/// A reader smaller than a record gets it in pieces and loses nothing.
#[test]
fn a_small_read_buffer_gets_a_record_in_pieces() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    let data = body(9000, 3);
    l.c.write_plain(&data).unwrap();
    let mut wire = vec![0u8; 20_000];
    let n = l.c.transmit(&mut wire).unwrap();
    assert_eq!(l.s.received(&mut wire[..n]).unwrap(), n);
    assert_eq!(l.s.handoff_ready(), Err(Blocked::ExtractOff));

    let mut got = Vec::new();
    let mut small = [0u8; 13];
    loop {
        let n = l.s.read_plain(&mut small).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&small[..n]);
    }
    assert_eq!(got, data);
}

#[test]
fn a_write_is_cut_to_the_room_there_is() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    let data = body(1000, 4);

    // No room for even an empty record takes nothing and writes nothing.
    let mut none = [0u8; 20];
    let w = l.s.write_plain(&data, &mut none).unwrap();
    assert_eq!(w, Wrote { took: 0, wrote: 0 });

    // Room for part of it takes exactly what fits.
    let mut out = [0u8; 300];
    let w = l.s.write_plain(&data, &mut out).unwrap();
    assert_eq!(w.wrote, out.len(), "the room was not filled");
    assert!(w.took < out.len() && w.took + RECORD_OVERHEAD_MAX >= out.len());
    assert_eq!(records(&out[..w.wrote]), 1);
    l.give_client(&out[..w.wrote]).unwrap();
    assert_eq!(l.got_c, &data[..w.took]);

    // And a large write is cut into full records.
    let big = body(RECORD_PLAIN_MAX * 2 + 5, 5);
    let mut out = vec![0u8; 3 * (RECORD_PLAIN_MAX + RECORD_OVERHEAD_MAX)];
    let w = l.s.write_plain(&big, &mut out).unwrap();
    assert_eq!(w.took, big.len());
    assert_eq!(records(&out[..w.wrote]), 3);
    assert!(w.wrote <= big.len() + 3 * RECORD_OVERHEAD_MAX);
}

#[test]
fn nothing_is_written_before_the_handshake_is_done() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME);
    let mut out = [0u8; 4096];
    assert_eq!(
        l.s.write_plain(b"too early", &mut out).unwrap(),
        Wrote::default()
    );
    l.s.close();
    assert!(
        !l.s.wants_write(),
        "a close before the handshake said something"
    );
    let mut l = l.shake();
    l.s_send(b"now", 4096);
    assert_eq!(l.got_c, b"now");
}

/// What the session queued goes out ahead of what is written after it,
/// since it was sealed first.
#[test]
fn a_write_flushes_what_was_queued_first() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.tickets = Tickets::Stored(8));
    let mut l = Link::new(&client(&id), &scfg, NAME);
    // Run the handshake by hand so the tickets stay queued in the server.
    let mut buf = vec![0u8; 20_000];
    while l.s.is_handshaking() {
        let n = l.c.transmit(&mut buf).unwrap();
        l.give_server(&buf[..n]).unwrap();
        if l.s.is_handshaking() {
            let n = l.s.transmit(&mut buf).unwrap();
            l.give_client(&buf[..n]).unwrap();
        }
    }
    assert!(l.s.wants_write(), "no tickets were queued");
    let w = l.s.write_plain(b"after the tickets", &mut buf).unwrap();
    assert_eq!(w.took, 17);
    assert!(!l.s.wants_write());
    assert_eq!(records(&buf[..w.wrote]), 2, "the tickets and the data");
    l.give_client(&buf[..w.wrote]).unwrap();
    assert_eq!(l.got_c, b"after the tickets");
}

#[test]
fn a_close_from_the_client_is_reported() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    l.c_send(b"last words");
    assert!(!l.s.peer_closed());
    l.c.close();
    l.pump().unwrap();
    assert!(l.s.peer_closed(), "the close went unnoticed");
    assert_eq!(l.got_s, b"last words");
    // The server may still finish what it was saying, then close too.
    l.s_send(b"goodbye", 4096);
    l.s.close();
    l.pump().unwrap();
    assert_eq!(l.got_c, b"goodbye");
    assert!(l.c.peer_closed());
    // Anything after the close is taken and ignored.
    assert_eq!(l.s.received(&mut [23, 3, 3, 0, 1, 0]).unwrap(), 6);
}

#[test]
fn a_close_from_the_server_is_reported() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    assert!(!l.c.peer_closed());
    l.s.close();
    assert!(l.s.wants_write(), "no alert was queued");
    l.pump().unwrap();
    assert!(l.c.peer_closed(), "the close went unnoticed");
    assert!(!l.s.peer_closed());
    // Nothing more can be written.
    let mut out = [0u8; 256];
    assert_eq!(
        l.s.write_plain(b"more", &mut out).unwrap(),
        Wrote::default()
    );
}

#[test]
fn silence_is_not_a_close() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    let mut out = [0u8; 16];
    assert_eq!(l.s.read_plain(&mut out).unwrap(), 0);
    assert!(!l.s.peer_closed());
}

#[test]
fn garbage_is_an_error_an_alert_and_then_nothing() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    for junk in [
        &[0xffu8; 64][..],
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
        &[22, 3, 3, 0, 4, 2, 0, 0, 0],
    ] {
        let mut s = Server::accept(&scfg).unwrap();
        let mut junk = junk.to_vec();
        let err = s.received(&mut junk).expect_err("garbage was accepted");
        assert!(matches!(err, TlsError::Handshake(_)), "wrong error: {err}");
        let mut alert = [0u8; 64];
        let n = s.transmit(&mut alert).unwrap();
        assert_eq!(n, 7, "an alert record is seven bytes");
        assert_eq!(alert[0], RECORD_ALERT);
        assert!(matches!(s.received(&mut junk), Err(TlsError::Dead)));
        assert!(matches!(
            s.write_plain(b"x", &mut alert),
            Err(TlsError::Dead)
        ));
        assert!(s.is_handshaking());
    }
}

#[test]
fn garbage_after_the_handshake_is_a_protocol_error() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    // A well formed header over a body that will not decrypt.
    let mut junk = vec![23, 3, 3, 0, 40];
    junk.extend_from_slice(&[0x5a; 40]);
    let err =
        l.s.received(&mut junk)
            .expect_err("a forged record was accepted");
    assert!(matches!(err, TlsError::Protocol(_)), "wrong error: {err}");
    assert!(l.s.wants_write(), "no alert for a bad record");
}

/// Every cut of either client flight leaves the handshake unfinished, and
/// none of them is an error or a panic.
#[test]
fn a_truncated_handshake_never_completes() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let ccfg = client(&id);
    let mut buf = vec![0u8; 20_000];

    let mut c = Tls::connect(&ccfg, NAME).unwrap();
    let n = c.transmit(&mut buf).unwrap();
    let hello = buf[..n].to_vec();
    for cut in 0..hello.len() {
        let mut s = Server::accept(&scfg).unwrap();
        let mut part = hello[..cut].to_vec();
        assert_eq!(s.received(&mut part).unwrap(), 0, "cut {cut} was taken");
        assert!(s.is_handshaking() && !s.wants_write(), "cut {cut}");
    }

    // The second flight, the client's Finished.
    let mut l = Link::new(&ccfg, &scfg, NAME);
    let n = l.c.transmit(&mut buf).unwrap();
    l.give_server(&buf[..n]).unwrap();
    let n = l.s.transmit(&mut buf).unwrap();
    l.give_client(&buf[..n]).unwrap();
    let n = l.c.transmit(&mut buf).unwrap();
    let fin = buf[..n].to_vec();
    assert_eq!(
        record_len(&fin),
        Some(6),
        "a change cipher spec comes first"
    );
    for cut in 0..6 {
        let mut part = fin[..cut].to_vec();
        assert_eq!(l.s.received(&mut part).unwrap(), 0, "cut {cut} was taken");
    }
    let mut ccs = fin[..6].to_vec();
    assert_eq!(l.s.received(&mut ccs).unwrap(), 6);
    let rest = &fin[6..];
    for cut in 0..rest.len() {
        let mut part = rest[..cut].to_vec();
        assert_eq!(l.s.received(&mut part).unwrap(), 0, "cut {cut} was taken");
        assert!(l.s.is_handshaking(), "cut {cut} completed the handshake");
    }
    let mut whole = rest.to_vec();
    assert_eq!(l.s.received(&mut whole).unwrap(), rest.len());
    assert!(!l.s.is_handshaking());
}

/// A client hello cut into many small records is a handshake message spread
/// over records, which rustls reassembles in place and refers to by
/// position. The bytes it holds on to have to survive between calls.
#[test]
fn a_handshake_message_spread_over_records_is_carried() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| s.alpn = &[H2, H1]);
    for seed in 1..=40u64 {
        let ccfg = client_with(&id, rustls::DEFAULT_VERSIONS, &[], |c| {
            c.max_fragment_size = Some(64);
            c.alpn_protocols = vec![H1.to_vec()];
        });
        let mut l = Link::new(&ccfg, &scfg, NAME).seeded(seed);
        // The request rides behind the Finished, in small records too.
        let early = body(700, 6);
        l.c.write_plain(&early).unwrap();
        l.pump().expect("the handshake");
        assert!(!l.s.is_handshaking(), "seed {seed}");
        assert_eq!(l.s.sni(), Some(NAME));
        assert_eq!(l.s.alpn(), Some(H1));
        assert_eq!(l.got_s, early, "seed {seed}");
        let reply = body(5000, 7);
        l.s_send(&reply, 2048);
        assert_eq!(l.got_c, reply, "seed {seed}");
        assert!(l.inbox.is_empty());
    }
}

/// A rustls client driven directly, for what the client role does not
/// expose, a key update.
struct Raw {
    c: ClientConnection,
    s: Server,
    got_s: Vec<u8>,
}

impl Raw {
    fn new(ccfg: &crate::Config, scfg: &ServerConfig) -> Raw {
        let name = ServerName::try_from(NAME).unwrap();
        let mut r = Raw {
            c: ClientConnection::new(ccfg.clone(), name).unwrap(),
            s: Server::accept(scfg).unwrap(),
            got_s: Vec::new(),
        };
        r.pump();
        assert!(!r.s.is_handshaking());
        r
    }

    fn pump(&mut self) {
        let mut buf = vec![0u8; 20_000];
        let mut plain = [0u8; 4096];
        loop {
            let mut moved = false;
            let mut wire = Vec::new();
            while self.c.wants_write() {
                self.c.write_tls(&mut wire).unwrap();
            }
            while !wire.is_empty() {
                moved = true;
                let took = self.s.received(&mut wire).expect("the server takes it");
                wire.drain(..took);
                loop {
                    let n = self.s.read_plain(&mut plain).unwrap();
                    if n == 0 {
                        break;
                    }
                    self.got_s.extend_from_slice(&plain[..n]);
                }
            }
            let n = self.s.transmit(&mut buf).unwrap();
            if n > 0 {
                moved = true;
                self.c.read_tls(&mut &buf[..n]).unwrap();
                self.c.process_new_packets().expect("the client takes it");
            }
            if !moved {
                return;
            }
        }
    }
}

#[test]
fn a_key_update_from_the_client_is_followed() {
    use std::io::{Read as _, Write as _};
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut r = Raw::new(&client(&id), &scfg);
    r.c.writer().write_all(b"old keys").unwrap();
    r.pump();
    r.c.refresh_traffic_keys().expect("a key update");
    r.c.writer().write_all(b", new keys").unwrap();
    r.pump();
    assert_eq!(r.got_s, b"old keys, new keys");

    // rustls asks for an update back, and the server's next records are
    // under its own new keys. The client reading them proves both moved.
    let mut out = [0u8; 256];
    let w = r.s.write_plain(b"still here", &mut out).unwrap();
    r.c.read_tls(&mut &out[..w.wrote]).unwrap();
    r.c.process_new_packets().expect("the client follows");
    let mut got = [0u8; 32];
    let n = r.c.reader().read(&mut got).unwrap();
    assert_eq!(&got[..n], b"still here");
}

#[test]
fn a_second_connection_resumes_when_tickets_are_on() {
    let id = ident(NAME);
    for (tickets, resumes) in [
        (Tickets::Off, false),
        (Tickets::Stored(16), true),
        (Tickets::Sealed, true),
    ] {
        let scfg = server_with(&id, |s| s.tickets = tickets);
        let ccfg = client(&id);
        let first = Link::new(&ccfg, &scfg, NAME).shake();
        assert!(!first.s.resumed(), "{tickets:?}, the first one resumed");
        // Both tickets travel in one record.
        let sent = if resumes { 1 } else { 0 };
        let after = &first.wire_s[first.shook.unwrap()..];
        assert_eq!(records(after), sent, "{tickets:?}, tickets sent");
        let mut second = Link::new(&ccfg, &scfg, NAME).shake();
        assert_eq!(second.s.resumed(), resumes, "{tickets:?}");
        second.c_send(b"resumed or not");
        assert_eq!(second.got_s, b"resumed or not");
    }
}

fn nonce(iv: &[u8], seq: u64) -> aead::Nonce {
    let mut n = [0u8; 12];
    n.copy_from_slice(iv);
    for (b, s) in n[4..].iter_mut().zip(seq.to_be_bytes()) {
        *b ^= s;
    }
    aead::Nonce::assume_unique_for_key(n)
}

fn key_of_secret(s: &Secret) -> aead::LessSafeKey {
    let alg = match s.aead {
        Aead::Aes128Gcm => &aead::AES_128_GCM,
        Aead::Aes256Gcm => &aead::AES_256_GCM,
        Aead::Chacha20Poly1305 => &aead::CHACHA20_POLY1305,
    };
    assert_eq!(s.key().len(), alg.key_len());
    assert_eq!(s.iv().len(), 12);
    aead::LessSafeKey::new(aead::UnboundKey::new(alg, s.key()).unwrap())
}

/// Open one TLS 1.3 record with a secret and a sequence number, the way
/// any record layer that took over would.
fn open(s: &Secret, seq: u64, record: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (head, body) = record.split_at(5);
    let mut body = body.to_vec();
    let plain = key_of_secret(s)
        .open_in_place(nonce(s.iv(), seq), aead::Aad::from(head), &mut body)
        .ok()?;
    let end = plain.iter().rposition(|b| *b != 0)?;
    Some((plain[end], plain[..end].to_vec()))
}

fn seal_record(s: &Secret, seq: u64, kind: u8, plain: &[u8]) -> Vec<u8> {
    let mut body = plain.to_vec();
    body.push(kind);
    let len = (body.len() + 16) as u16;
    let mut rec = vec![RECORD_DATA, 3, 3, (len >> 8) as u8, len as u8];
    key_of_secret(s)
        .seal_in_place_append_tag(nonce(s.iv(), seq), aead::Aad::from(rec.clone()), &mut body)
        .unwrap();
    rec.extend_from_slice(&body);
    rec
}

/// The whole handoff, as the kernel path will run it. The request rides in
/// with the Finished and is read here, a second record is cut by the socket
/// and stays with the caller, and a record layer that is not rustls
/// continues in both directions from the numbers the handoff gave.
#[test]
fn another_record_layer_continues_from_the_handoff() {
    let id = ident(NAME);
    for suite in KTLS_SUITES {
        for (tickets, sent) in [(Tickets::Off, 0), (Tickets::Stored(8), 1)] {
            let scfg = server_with(&id, |s| {
                s.tls13_only = true;
                s.suites = only(suite);
                s.tickets = tickets;
                s.extract = true;
            });
            let ccfg = client_with(&id, rustls::DEFAULT_VERSIONS, &[], |_| {});
            let mut l = Link::new(&ccfg, &scfg, NAME);
            assert_eq!(l.s.handoff_ready(), Err(Blocked::Handshaking));
            l.c.write_plain(b"GET / HTTP/1.1\r\n\r\n").unwrap();
            l.pump().expect("the handshake");
            assert_eq!(l.got_s, b"GET / HTTP/1.1\r\n\r\n", "the early request");

            // A second request, of which the socket has given all but 9 bytes.
            l.c.write_plain(b"second request").unwrap();
            let mut wire = vec![0u8; 4096];
            let n = l.c.transmit(&mut wire).unwrap();
            wire.truncate(n);
            let mut held = wire[..n - 9].to_vec();
            assert_eq!(l.s.received(&mut held).unwrap(), 0);
            assert_eq!(record_len(&held), Some(n));

            assert_eq!(l.s.handoff_ready(), Ok(()));
            let h = l.s.handoff().expect("a handoff");
            assert_eq!((h.version, h.suite), (ProtocolVersion::TLSv1_3, suite));
            assert_eq!(h.rx.seq, 1, "one record was read here");
            assert_eq!(h.tx.seq, sent, "{tickets:?}");

            // The held record, completed, is the next one in.
            held.extend_from_slice(&wire[n - 9..]);
            assert!(
                open(&h.rx, 0, &held).is_none(),
                "a reused sequence number opened"
            );
            let (kind, plain) = open(&h.rx, h.rx.seq, &held).expect("the record opens");
            assert_eq!((kind, &plain[..]), (RECORD_DATA, &b"second request"[..]));

            // And the client accepts what is sealed from here on.
            let reply = seal_record(&h.tx, h.tx.seq, RECORD_DATA, b"HTTP/1.1 204\r\n\r\n");
            l.c.received(&reply).expect("the client opens it");
            let mut got = [0u8; 64];
            let n = l.c.read_plain(&mut got).unwrap();
            assert_eq!(&got[..n], b"HTTP/1.1 204\r\n\r\n");
            let bye = seal_record(&h.tx, h.tx.seq + 1, RECORD_ALERT, &CLOSE_NOTIFY);
            l.c.received(&bye).expect("the client opens it");
            assert!(l.c.peer_closed(), "the close notification was not one");
        }
    }
}

#[test]
fn a_handoff_waits_for_what_is_unsent_and_unread() {
    let id = ident(NAME);
    let scfg = server_with(&id, |s| {
        s.tickets = Tickets::Stored(8);
        s.extract = true;
    });
    let mut l = Link::new(&client(&id), &scfg, NAME);
    let mut buf = vec![0u8; 20_000];
    l.c.write_plain(b"early").unwrap();
    while l.s.is_handshaking() {
        let n = l.c.transmit(&mut buf).unwrap();
        // Straight in, with no read behind it.
        let mut at = 0;
        while at < n {
            let took = l.s.received(&mut buf[at..n]).unwrap();
            assert!(took > 0);
            at += took;
            if !l.s.is_handshaking() {
                break;
            }
        }
        if l.s.is_handshaking() {
            let n = l.s.transmit(&mut buf).unwrap();
            l.give_client(&buf[..n]).unwrap();
        }
    }
    assert_eq!(
        l.s.handoff_ready(),
        Err(Blocked::Unsent),
        "tickets are queued"
    );
    let n = l.s.transmit(&mut buf).unwrap();
    assert_eq!(records(&buf[..n]), 1);
    assert_eq!(
        l.s.handoff_ready(),
        Err(Blocked::Unread),
        "the request is waiting"
    );
    let mut got = [0u8; 16];
    let n = l.s.read_plain(&mut got).unwrap();
    assert_eq!(&got[..n], b"early");
    assert_eq!(l.s.handoff_ready(), Ok(()));
}

#[test]
fn a_handoff_is_refused_unless_asked_for() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let l = Link::new(&client(&id), &scfg, NAME).shake();
    assert_eq!(l.s.handoff_ready(), Err(Blocked::ExtractOff));
    assert!(matches!(
        l.s.handoff(),
        Err(TlsError::Handoff(Blocked::ExtractOff))
    ));
}

/// After a handoff the key schedule alone follows a key update in either
/// direction, checked against a real client that performs one.
#[test]
fn the_key_schedule_follows_updates_after_a_handoff() {
    use std::io::Write as _;
    let id = ident(NAME);
    let scfg = server_with(&id, |s| {
        s.tls13_only = true;
        s.extract = true;
    });
    let mut r = Raw::new(&client(&id), &scfg);
    let mut h = r.s.handoff().expect("a handoff");

    // The client updates its sending keys. The update itself is the last
    // record under the old ones.
    r.c.refresh_traffic_keys().expect("a key update");
    r.c.writer().write_all(b"under new keys").unwrap();
    let mut wire = Vec::new();
    while r.c.wants_write() {
        r.c.write_tls(&mut wire).unwrap();
    }
    let first = record_len(&wire).unwrap();
    let (kind, update) = open(&h.rx, h.rx.seq, &wire[..first]).expect("the update opens");
    assert_eq!(kind, RECORD_HANDSHAKE);
    assert_eq!(update, KEY_UPDATE_REQUEST, "rustls asks for one back");
    let rx = h.rekey.rx_next().expect("the next receive secret");
    assert_eq!(rx.seq, 0);
    let (kind, plain) = open(&rx, 0, &wire[first..]).expect("the data opens");
    assert_eq!((kind, &plain[..]), (RECORD_DATA, &b"under new keys"[..]));

    // And the answer, an update under the old sending keys, then data under
    // the new ones.
    let mut back = seal_record(&h.tx, h.tx.seq, RECORD_HANDSHAKE, &KEY_UPDATE);
    let tx = h.rekey.tx_next().expect("the next send secret");
    assert_eq!(tx.seq, 0);
    assert_ne!(tx.key(), h.tx.key());
    back.extend_from_slice(&seal_record(&tx, 0, RECORD_DATA, b"so are these"));
    r.c.read_tls(&mut &back[..]).unwrap();
    r.c.process_new_packets().expect("the client follows");
    let mut got = [0u8; 32];
    let n = std::io::Read::read(&mut r.c.reader(), &mut got).unwrap();
    assert_eq!(&got[..n], b"so are these");
}

#[test]
fn an_empty_buffer_is_accepted_everywhere() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut s = Server::accept(&scfg).unwrap();
    assert_eq!(s.received(&mut []).unwrap(), 0);
    assert_eq!(s.read_plain(&mut []).unwrap(), 0);
    assert_eq!(s.transmit(&mut []).unwrap(), 0);
    assert_eq!(s.write_plain(&[], &mut []).unwrap(), Wrote::default());
    let mut l = Link::new(&client(&id), &scfg, NAME).shake();
    assert_eq!(l.s.write_plain(b"x", &mut []).unwrap(), Wrote::default());
    assert_eq!(l.s.read_plain(&mut []).unwrap(), 0);
}

/// A flight has to come out through a buffer of any size.
#[test]
fn a_small_transmit_buffer_is_resumable() {
    let id = ident(NAME);
    let scfg = server_with(&id, |_| {});
    let mut l = Link::new(&client(&id), &scfg, NAME);
    let mut buf = vec![0u8; 20_000];
    let n = l.c.transmit(&mut buf).unwrap();
    l.give_server(&buf[..n]).unwrap();
    let mut flight = Vec::new();
    let mut small = [0u8; 17];
    while l.s.wants_write() {
        let n = l.s.transmit(&mut small).unwrap();
        flight.extend_from_slice(&small[..n]);
    }
    assert!(flight.len() > 500, "only {} bytes came out", flight.len());
    l.give_client(&flight).unwrap();
    l.pump().unwrap();
    assert!(!l.s.is_handshaking());
}
