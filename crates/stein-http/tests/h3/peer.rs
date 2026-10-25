//! Scripted HTTP/3 origin on quinn-proto's server side, sans-IO, for the engine tests.
//!
//! Control stream with SETTINGS, the two QPACK streams with their type alone, a
//! request read whole, and a [`Reply`] the test picks per request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use stein_http::h3::frame::{self, Out, Piece, Reader};
use stein_http::h3::qpack;
use stein_http::h3::quinn_proto as q;
use stein_http::h3::{Nanos, bytes, quic};

pub const CERT: &[u8] = include_bytes!("../tls/cert_c.der");
pub const KEY: &[u8] = include_bytes!("../tls/key_c.der");
pub const CA: &[u8] = include_bytes!("../tls/ca_c.der");

/// A request as the peer read it.
#[derive(Clone, Debug, Default)]
pub struct Got {
    pub stream: u64,
    pub fields: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Got {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What the peer does with a whole request.
#[derive(Clone, Debug)]
pub enum Reply {
    /// HEADERS, then `body` in `pieces` DATA frames, then the end of the stream.
    Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        pieces: usize,
    },
    /// Raw stream bytes, then the end of the stream when `fin`.
    Raw { bytes: Vec<u8>, fin: bool },
    /// RESET_STREAM with `code`.
    Reset(u64),
    /// Nothing at all.
    Silent,
}

pub fn ok(status: u16, headers: &[(&str, &str)], body: &[u8], pieces: usize) -> Reply {
    Reply::Answer {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        body: body.to_vec(),
        pieces,
    }
}

/// HEADERS frame of `lines` (status first) as stream bytes.
pub fn headers_frame(lines: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; 1 << 16];
    let mut section = Out::new(&mut buf);
    section.bytes(&[0, 0]).unwrap();
    for (n, v) in lines {
        qpack::put_field(&mut section, n, v).unwrap();
    }
    let section = section.written().to_vec();
    let mut out = frame_header(frame::HEADERS, section.len() as u64);
    out.extend_from_slice(&section);
    out
}

pub fn frame_header(ty: u64, len: u64) -> Vec<u8> {
    let mut buf = [0u8; 16];
    let mut out = Out::new(&mut buf);
    frame::put_header(&mut out, ty, len).unwrap();
    out.written().to_vec()
}

pub fn varint(v: u64) -> Vec<u8> {
    let mut buf = [0u8; 8];
    let mut out = Out::new(&mut buf);
    out.varint(v).unwrap();
    out.written().to_vec()
}

fn encode(reply: &Reply) -> (Vec<u8>, bool) {
    match reply {
        Reply::Answer {
            status,
            headers,
            body,
            pieces,
        } => {
            let status = status.to_string();
            let mut lines: Vec<(&[u8], &[u8])> = vec![(b":status", status.as_bytes())];
            lines.extend(headers.iter().map(|(n, v)| (n.as_bytes(), v.as_bytes())));
            let mut out = headers_frame(&lines);
            let size = body.len().div_ceil((*pieces).max(1)).max(1);
            for piece in body.chunks(size) {
                out.extend(frame_header(frame::DATA, piece.len() as u64));
                out.extend_from_slice(piece);
            }
            (out, true)
        }
        Reply::Raw { bytes, fin } => (bytes.clone(), *fin),
        Reply::Reset(_) | Reply::Silent => (Vec::new(), false),
    }
}

struct Stream {
    id: q::StreamId,
    reader: Reader,
    section: Vec<u8>,
    got: Got,
    out: Vec<u8>,
    at: usize,
    fin: bool,
    answered: bool,
}

struct Conn {
    ch: q::ConnectionHandle,
    conn: q::Connection,
    opened: bool,
    control: Option<q::StreamId>,
    streams: Vec<Stream>,
    uni: Vec<q::StreamId>,
}

pub type Answer = Box<dyn FnMut(&Got) -> Reply>;

pub struct Peer {
    ep: q::Endpoint,
    base: Instant,
    conns: Vec<Conn>,
    answer: Answer,
    buf: Vec<u8>,
    /// Requests read whole, in order.
    pub got: Vec<Got>,
    /// Control stream bytes after SETTINGS, written as soon as it opens.
    pub control_tail: Vec<u8>,
    /// SETTINGS payload in place of the default one.
    pub settings: Option<Vec<u8>>,
    /// Drops every datagram both ways.
    pub black_hole: bool,
    /// How each connection ended, as the peer saw it.
    pub lost: Vec<q::ConnectionError>,
}

fn server_config() -> q::ServerConfig {
    let certs = vec![CertificateDer::from(CERT.to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    tls.max_early_data_size = 0;
    let crypto = q::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls)).unwrap();
    let mut server = q::ServerConfig::with_crypto(Arc::new(crypto));
    server.migration(false);
    let mut transport = q::TransportConfig::default();
    transport
        .max_concurrent_uni_streams(8u32.into())
        .max_concurrent_bidi_streams(100u32.into());
    server.transport_config(Arc::new(transport));
    server
}

/// The client side TLS config trusting [`CA`].
pub fn client_crypto() -> Arc<dyn q::crypto::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(CA.to_vec())).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    Arc::new(q::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).unwrap())
}

impl Peer {
    pub fn new(seed: u64, answer: Answer) -> Peer {
        let config = quic::endpoint_config(Some(seed ^ 0x5E4)).unwrap();
        let ep = q::Endpoint::new(
            Arc::new(config),
            Some(Arc::new(server_config())),
            true,
            None,
        );
        Peer {
            ep,
            base: Instant::now(),
            conns: Vec::new(),
            answer,
            buf: Vec::new(),
            got: Vec::new(),
            control_tail: Vec::new(),
            settings: None,
            black_hole: false,
            lost: Vec::new(),
        }
    }

    fn at(&self, now: Nanos) -> Instant {
        self.base + Duration::from_nanos(now.0)
    }

    /// Earliest timer of any connection.
    pub fn timeout(&mut self) -> Option<Nanos> {
        let base = self.base;
        self.conns
            .iter_mut()
            .filter_map(|c| c.conn.poll_timeout())
            .min()
            .map(|t| Nanos(t.saturating_duration_since(base).as_nanos() as u64))
    }

    /// Write GOAWAY with `id` on every control stream.
    pub fn goaway(&mut self, id: u64) {
        let mut bytes = frame_header(frame::GOAWAY, varint(id).len() as u64);
        bytes.extend(varint(id));
        for c in &mut self.conns {
            if let Some(ctl) = c.control {
                c.conn.send_stream(ctl).write(&bytes).unwrap();
            }
        }
    }

    /// Close every connection with application `code`.
    pub fn close(&mut self, now: Nanos, code: u64) {
        let at = self.at(now);
        for c in &mut self.conns {
            c.conn
                .close(at, q::VarInt::from_u64(code).unwrap(), bytes::Bytes::new());
        }
    }

    /// A datagram from `from`, or a timer when `None`. Replies go to `out`.
    pub fn input(
        &mut self,
        now: Nanos,
        input: Option<(SocketAddr, &[u8])>,
        out: &mut Vec<Vec<u8>>,
    ) {
        let at = self.at(now);
        match input {
            Some(_) if self.black_hole => {}
            Some((from, data)) => self.datagram(at, from, data, out),
            None => {
                for c in &mut self.conns {
                    if c.conn.poll_timeout().is_some_and(|t| t <= at) {
                        c.conn.handle_timeout(at);
                    }
                }
            }
        }
        self.drive(at, out);
        if self.black_hole {
            out.clear();
        }
    }

    fn datagram(&mut self, at: Instant, from: SocketAddr, data: &[u8], out: &mut Vec<Vec<u8>>) {
        self.buf.clear();
        let data = bytes::BytesMut::from(data);
        match self.ep.handle(at, from, None, None, data, &mut self.buf) {
            Some(q::DatagramEvent::NewConnection(incoming)) => {
                self.buf.clear();
                match self.ep.accept(incoming, at, &mut self.buf, None) {
                    Ok((ch, conn)) => self.conns.push(Conn {
                        ch,
                        conn,
                        opened: false,
                        control: None,
                        streams: Vec::new(),
                        uni: Vec::new(),
                    }),
                    Err(e) => {
                        if let Some(t) = e.response {
                            out.push(self.buf[..t.size].to_vec());
                        }
                    }
                }
            }
            Some(q::DatagramEvent::ConnectionEvent(ch, ev)) => {
                if let Some(c) = self.conns.iter_mut().find(|c| c.ch == ch) {
                    c.conn.handle_event(ev);
                }
            }
            Some(q::DatagramEvent::Response(t)) => out.push(self.buf[..t.size].to_vec()),
            None => {}
        }
    }

    fn drive(&mut self, at: Instant, out: &mut Vec<Vec<u8>>) {
        let mut i = 0;
        while i < self.conns.len() {
            let c = &mut self.conns[i];
            loop {
                let mut any = false;
                while let Some(ev) = c.conn.poll_endpoint_events() {
                    any = true;
                    if let Some(back) = self.ep.handle_event(c.ch, ev) {
                        c.conn.handle_event(back);
                    }
                }
                while let Some(ev) = c.conn.poll() {
                    any = true;
                    match ev {
                        q::Event::Connected => {
                            open_streams(c, self.settings.as_deref(), &self.control_tail)
                        }
                        q::Event::ConnectionLost { reason } => self.lost.push(reason),
                        _ => {}
                    }
                }
                serve(c, &mut self.answer, &mut self.got);
                if !any {
                    break;
                }
            }
            loop {
                self.buf.clear();
                let Some(t) = c.conn.poll_transmit(at, 1, &mut self.buf) else {
                    break;
                };
                out.push(self.buf[..t.size].to_vec());
            }
            if c.conn.is_drained() {
                self.conns.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

/// Control stream with SETTINGS and the QPACK streams (RFC 9114 6.2).
fn open_streams(c: &mut Conn, settings: Option<&[u8]>, tail: &[u8]) {
    if c.opened {
        return;
    }
    c.opened = true;
    for kind in [
        frame::STREAM_CONTROL,
        frame::STREAM_ENCODER,
        frame::STREAM_DECODER,
    ] {
        let id = c.conn.streams().open(q::Dir::Uni).unwrap();
        let mut bytes = varint(kind);
        if kind == frame::STREAM_CONTROL {
            match settings {
                Some(p) => {
                    bytes.extend(frame_header(frame::SETTINGS, p.len() as u64));
                    bytes.extend_from_slice(p);
                }
                None => {
                    let mut buf = [0u8; 64];
                    let mut o = Out::new(&mut buf);
                    frame::put_settings(&mut o, 64 * 1024).unwrap();
                    bytes.extend_from_slice(o.written());
                }
            }
            bytes.extend_from_slice(tail);
            c.control = Some(id);
        }
        c.conn.send_stream(id).write(&bytes).unwrap();
    }
}

/// Read requests, answer whole ones, write what waits.
fn serve(c: &mut Conn, answer: &mut Answer, got: &mut Vec<Got>) {
    while let Some(id) = c.conn.streams().accept(q::Dir::Bi) {
        c.streams.push(Stream {
            id,
            reader: Reader::default(),
            section: Vec::new(),
            got: Got {
                stream: id.into(),
                ..Got::default()
            },
            out: Vec::new(),
            at: 0,
            fin: false,
            answered: false,
        });
    }
    while let Some(id) = c.conn.streams().accept(q::Dir::Uni) {
        c.uni.push(id);
    }
    for id in &c.uni {
        if let Ok(mut chunks) = c.conn.recv_stream(*id).read(true) {
            while let Ok(Some(_)) = chunks.next(usize::MAX) {}
            let _ = chunks.finalize();
        }
    }
    for s in c.streams.iter_mut().filter(|s| !s.answered) {
        if !read_request(&mut c.conn, s) {
            continue;
        }
        s.answered = true;
        let mut fields = Vec::new();
        let mut line = vec![0u8; 1 << 16];
        let decoded = qpack::decode(&s.section, 1 << 16, &mut line, |n, v| {
            fields.push((
                String::from_utf8_lossy(n).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            ));
            Ok(())
        });
        assert!(decoded.is_ok(), "the client's section decodes");
        s.got.fields = fields;
        got.push(s.got.clone());
        let reply = answer(&s.got);
        if let Reply::Reset(code) = reply {
            let _ = c
                .conn
                .send_stream(s.id)
                .reset(q::VarInt::from_u64(code).unwrap());
            continue;
        }
        (s.out, s.fin) = encode(&reply);
    }
    for s in c.streams.iter_mut().filter(|s| s.answered) {
        while s.at < s.out.len() {
            match c.conn.send_stream(s.id).write(&s.out[s.at..]) {
                Ok(n) if n > 0 => s.at += n,
                _ => break,
            }
        }
        if s.fin && s.at == s.out.len() {
            let _ = c.conn.send_stream(s.id).finish();
            s.fin = false;
        }
    }
}

/// Read what stream `s` has. True once its request is whole.
fn read_request(conn: &mut q::Connection, s: &mut Stream) -> bool {
    let mut recv = conn.recv_stream(s.id);
    let Ok(mut chunks) = recv.read(true) else {
        return false;
    };
    let mut fin = false;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                let mut data = &chunk.bytes[..];
                while !data.is_empty() {
                    let (n, piece) = s.reader.step(data);
                    data = &data[n..];
                    match piece {
                        Some(Piece::Payload {
                            ty: frame::HEADERS,
                            bytes,
                            ..
                        }) => s.section.extend_from_slice(bytes),
                        Some(Piece::Payload {
                            ty: frame::DATA,
                            bytes,
                            ..
                        }) => s.got.body.extend_from_slice(bytes),
                        _ => {}
                    }
                }
            }
            Ok(None) => {
                fin = true;
                break;
            }
            Err(_) => break,
        }
    }
    let _ = chunks.finalize();
    fin
}
