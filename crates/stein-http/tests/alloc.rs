//! The chunked decoder, h2 extended CONNECT and the h3 codecs allocate nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::counting;
use stein_http::h1::{chunk, dechunk};
use stein_http::h2::server as h2;
use stein_http::h3::frame::{self as h3frame, Out};
use stein_http::h3::qpack;
use stein_http::{altsvc, hpack};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

/// Many chunks with extensions and a trailer.
fn chunked_wire() -> (Vec<u8>, usize) {
    let mut wire = Vec::new();
    let mut body = 0;
    for i in 0..200usize {
        let data = vec![i as u8; 1 + i * 37 % 3000];
        body += data.len();
        if i % 3 == 0 {
            wire.extend_from_slice(format!("{:x};ext=\"v {i}\"\r\n", data.len()).as_bytes());
            wire.extend_from_slice(&data);
            wire.extend_from_slice(b"\r\n");
        } else {
            let mut out = vec![0u8; chunk::framed_len(data.len() as u64) as usize];
            chunk::encode(&mut out, &data).unwrap();
            wire.extend_from_slice(&out);
        }
    }
    wire.extend_from_slice(b"0\r\nDigest: abc\r\n\r\n");
    (wire, body)
}

/// Decode in `piece` byte reads. Returns body length and message end.
fn dechunk_all(wire: &[u8], piece: usize) -> (usize, usize) {
    let limits = dechunk::Limits {
        max_ext: 4096,
        ..dechunk::Limits::default()
    };
    let mut dec = dechunk::Decoder::new(limits);
    let (mut body, mut pos) = (0, 0);
    for src in wire.chunks(piece) {
        let mut at = 0;
        loop {
            let s = dec.step(&src[at..]).unwrap();
            at += s.consumed;
            match s.event {
                dechunk::Event::Data { len, .. } => body += len,
                dechunk::Event::NeedMore => break,
                dechunk::Event::End => return (body, pos + at),
            }
        }
        pos += src.len();
    }
    (body, pos)
}

/// Extended CONNECT through the h2 machine.
fn connect_h2(conn: &mut h2::Conn, mem: &mut [u8], src: &[u8], ctl: &mut [u8]) {
    let mut ctl = h2::Ctl { buf: ctl, len: 0 };
    let mut pos = 0;
    let mut requests = 0;
    while pos < src.len() {
        let s = conn.step(mem, &src[pos..], &mut ctl).unwrap();
        pos += s.consumed;
        if let h2::Event::Request { fields, .. } = s.event {
            assert_eq!(h2::protocol(fields), Some(&b"websocket"[..]));
            requests += 1;
        }
    }
    assert_eq!(requests, 1);
}

fn connect_wire() -> Vec<u8> {
    let mut block = Vec::new();
    for (n, v) in [
        (":method", "CONNECT"),
        (":protocol", "websocket"),
        (":scheme", "https"),
        (":path", "/chat"),
        (":authority", "example.com"),
        ("sec-websocket-version", "13"),
    ] {
        hpack::encode_header(n, v, &mut block);
    }
    let mut wire = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    wire.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    wire.extend_from_slice(&[0, 0, block.len() as u8, 1, 4, 0, 0, 0, 1]);
    wire.extend_from_slice(&block);
    wire
}

#[test]
fn the_codecs_do_not_allocate() {
    // Allocate everything before counting starts.
    let (chunked, body) = chunked_wire();
    let mut out = vec![0u8; 4096];
    let cfg = h2::Config::default();
    let mut conn = h2::Conn::new(cfg);
    let mut mem = vec![0u8; h2::mem_bytes(&cfg)];
    let connect = connect_wire();
    let mut ctl = vec![0u8; 128];

    counting::arm();
    let before = counting::thread_count();

    assert_eq!(dechunk_all(&chunked, chunked.len()), (body, chunked.len()));
    assert_eq!(dechunk_all(&chunked, 1), (body, chunked.len()));
    assert_eq!(dechunk_all(&chunked, 977), (body, chunked.len()));
    let mut bad = dechunk::Decoder::new(dechunk::Limits::default());
    assert_eq!(bad.step(b"5\nhello"), Err(dechunk::Error::BadLineEnd));

    conn.init(&mut mem).unwrap();
    conn.enable_connect_protocol();
    assert_eq!(conn.write_settings(&mut out), Some(45));
    connect_h2(&mut conn, &mut mem, &connect, &mut ctl);
    assert_eq!(conn.trailers(&mem, 1), None);
    let status: [(&[u8], &[u8]); 1] = [(b"grpc-status", b"0")];
    assert!(h2::trailers_frame(&mut out, 1, &status).is_some());

    let after = counting::thread_count();
    assert_eq!(after - before, 0, "allocations in the codecs");
}

/// A response section as a peer encodes it, Huffman values included.
fn h3_section() -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let mut out = Out::new(&mut buf);
    out.bytes(&[0, 0]).unwrap();
    qpack::put_field(&mut out, b":status", b"206").unwrap();
    qpack::put_field(&mut out, b"content-range", b"bytes 0-9/100").unwrap();
    qpack::put_field(&mut out, b"x-custom", b"value").unwrap();
    let mut section = out.written().to_vec();
    // RFC 7541 C.6.1 `cache-control: private`, Huffman, under a static name.
    section.extend_from_slice(&[0x5f, 0x15, 0x85, 0xae, 0xc3, 0x77, 0x1a, 0x4b]);
    section
}

#[test]
fn the_h3_codecs_do_not_allocate() {
    let section = h3_section();
    let (mut line, mut packed, mut buf) = (vec![0u8; 1024], vec![0u8; 1024], vec![0u8; 1024]);

    counting::arm();
    let before = counting::thread_count();

    let head = qpack::decode_response(&section, 4096, &mut line, &mut packed).unwrap();
    assert_eq!((head.status, head.count), (206, 3));
    let mut out = Out::new(&mut buf);
    let fields = [("range", "bytes=0-"), ("User-Agent", "stein")];
    qpack::put_request(&mut out, "GET", "a.test", "/x", fields.into_iter()).unwrap();
    h3frame::put_settings(&mut out, 65536).unwrap();
    let wire = out.len();
    let mut reader = h3frame::Reader::default();
    let mut at = 2;
    while at < wire {
        let (n, _) = reader.step(&buf[at..wire]);
        at += n;
    }
    let alt = altsvc::parse(b"h3-29=\":443\", h3=\":443\"; ma=60", "a.test");
    assert_eq!(alt, altsvc::AltSvc::H3 { port: 443, ma: 60 });

    let after = counting::thread_count();
    assert_eq!(after - before, 0, "allocations in the h3 codecs");
}
