#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::h3::prng::Rng;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn fields(block: &[u8]) -> Result<Vec<(String, String)>, H3Error> {
    let mut out = Vec::new();
    let mut line = [0u8; 4096];
    decode(block, 65536, &mut line, |n, v| {
        out.push((
            String::from_utf8_lossy(n).into_owned(),
            String::from_utf8_lossy(v).into_owned(),
        ));
        Ok(())
    })?;
    Ok(out)
}

fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn section(lines: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let mut out = Out::new(&mut buf);
    out.bytes(&[0, 0]).unwrap();
    for (n, v) in lines {
        put_field(&mut out, n, v).unwrap();
    }
    out.written().to_vec()
}

fn request(method: &str, authority: &str, path: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let mut out = Out::new(&mut buf);
    put_request(&mut out, method, authority, path, extra.iter().copied()).unwrap();
    out.written().to_vec()
}

#[test]
fn the_static_table_is_appendix_a() {
    assert_eq!(STATIC.len(), 99);
    assert_eq!(STATIC[0], (":authority", ""));
    assert_eq!(STATIC[17], (":method", "GET"));
    assert_eq!(STATIC[20], (":method", "POST"));
    assert_eq!(STATIC[23], (":scheme", "https"));
    assert_eq!(STATIC[25], (":status", "200"));
    assert_eq!(STATIC[55], ("range", "bytes=0-"));
    assert_eq!(STATIC[65], (":status", "206"));
    assert_eq!(STATIC[83], ("alt-svc", "clear"));
    assert_eq!(STATIC[98], ("x-frame-options", "sameorigin"));
    assert_eq!(find(b"range", b"bytes=7-"), (None, Some(55)));
}

#[test]
fn rfc_9204_appendix_b1_literal_with_name_reference() {
    // B.1, `:path=/index.html`.
    let block = hex("0000 510b 2f69 6e64 6578 2e68 746d 6c");
    assert_eq!(fields(&block), Ok(pairs(&[(":path", "/index.html")])));
    // B.2 on reference the dynamic table and are refused.
    assert_eq!(fields(&hex("0381 10 11")), Err(dynamic(0)));
    let at2 = |r: Result<_, H3Error>| matches!(r, Err(H3Error::Decompression { at: 2, .. }));
    assert!(at2(fields(&hex("0000 80"))), "indexed, T clear");
    assert!(at2(fields(&hex("0000 10"))), "post-base index");
    assert!(
        at2(fields(&hex("0000 4103 616263"))),
        "name reference, T clear"
    );
    assert!(at2(fields(&hex("0000 00"))), "post-base name reference");
}

#[test]
fn huffman_strings_decode_with_rfc_7541_appendix_c_vectors() {
    // C.6.1 Huffman values under static name references.
    let block = hex("0000 5f09 826402 5f15 85aec3771a4b \
         56 96d07abe941054d444a8200595040b8166e082a62d1bff \
         5c 919d29ad171863c78f0b97c8e9ae82ae43d3");
    let want = [
        (":status", "302"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("location", "https://www.example.com"),
    ];
    assert_eq!(fields(&block), Ok(pairs(&want)));
    // C.4.3 `custom-key: custom-value`, both Huffman, as a literal name.
    let block = hex("0000 2f01 25a849e95ba97d7f 89 25a849e95bb8e8b4bf");
    assert_eq!(fields(&block), Ok(pairs(&[("custom-key", "custom-value")])));
    // C.4.1 `www.example.com`, and a code not ending in EOS bits.
    let block = hex("0000 508c f1e3c2e5f23a6ba0ab90f4ff");
    assert_eq!(
        fields(&block),
        Ok(pairs(&[(":authority", "www.example.com")]))
    );
    assert!(matches!(
        fields(&hex("0000 5081 00")),
        Err(H3Error::Decompression { .. })
    ));
}

#[test]
fn a_request_encodes_and_decodes_back() {
    let extra = [
        ("Range", "bytes=10-"),
        ("User-Agent", "profluens"),
        ("Accept-Encoding", "identity"),
        ("X-Thing", "a b"),
    ];
    let out = request("GET", "a.test:8443", "/x?y=1", &extra);
    // GET and https are single static bytes.
    assert_eq!(&out[..4], &[0x00, 0x00, 0xd1, 0xd7]);
    let want = [
        (":method", "GET"),
        (":scheme", "https"),
        (":authority", "a.test:8443"),
        (":path", "/x?y=1"),
        ("range", "bytes=10-"),
        ("user-agent", "profluens"),
        ("accept-encoding", "identity"),
        ("x-thing", "a b"),
    ];
    assert_eq!(fields(&out), Ok(pairs(&want)));
    let out = request("PATCH", "h", "/", &[]);
    let want = [
        (":method", "PATCH"),
        (":scheme", "https"),
        (":authority", "h"),
        (":path", "/"),
    ];
    assert_eq!(fields(&out), Ok(pairs(&want)));
    // A name past the 64 byte stack copy is lowercased in place.
    let long = "X-".to_string() + &"A".repeat(70);
    let out = request("GET", "h", "/", &[(long.as_str(), "v")]);
    let got = fields(&out).unwrap();
    assert_eq!(got[4].0, long.to_ascii_lowercase());
}

#[test]
fn a_request_too_big_for_the_buffer_is_full() {
    let mut buf = [0u8; 16];
    let mut out = Out::new(&mut buf);
    let r = put_request(
        &mut out,
        "GET",
        "a.long.host.test",
        "/",
        core::iter::empty(),
    );
    assert_eq!(r, Err(Full));
}

fn response(section: &[u8], max: usize, out: &mut [u8]) -> Result<Head, H3Error> {
    let mut line = [0u8; 1024];
    decode_response(section, max, &mut line, out)
}

#[test]
fn a_response_head_reads_as_packed_records() {
    let s = section(&[
        (b":status", b"206"),
        (b"content-range", b"bytes 0-9/100"),
        (b"alt-svc", b"h3=\":443\""),
    ]);
    let mut out = [0u8; 256];
    let head = response(&s, 65536, &mut out).unwrap();
    assert_eq!((head.status, head.count), (206, 2));
    let packed = &out[..head.len];
    assert_eq!(
        header::find(packed, "Content-Range"),
        Some(&b"bytes 0-9/100"[..])
    );
    let all: Vec<_> = header::iter(packed).collect();
    assert_eq!(all[1], (&b"alt-svc"[..], &b"h3=\":443\""[..]));
    let mut bad = |lines: &[(&[u8], &[u8])]| response(&section(lines), 65536, &mut out);
    let message = |r: Result<Head, H3Error>| matches!(r, Err(H3Error::Message { .. }));
    assert!(message(bad(&[(b"x", b"y")])), "a field before :status");
    assert_eq!(bad(&[]), Err(H3Error::Message { what: "no :status" }));
    assert!(message(bad(&[(b":status", b"2000")])));
    assert!(message(bad(&[(b":status", b"200"), (b":path", b"/")])));
    assert!(
        message(bad(&[(b":status", b"200"), (b"X", b"y")])),
        "uppercase"
    );
    assert!(
        message(bad(&[(b":status", b"200"), (b"x", b"a\r\nb: c")])),
        "a CRLF smuggled in"
    );
    // Over the advertised limit, over the line, and over the header region.
    let big = section(&[(b":status", b"200"), (b"x", &[b'a'; 200])]);
    let over = |r| matches!(r, Err(H3Error::ExcessiveLoad { .. }));
    assert!(over(response(&big, 100, &mut out)));
    let huge = section(&[(b":status", b"200"), (b"x", &[b'a'; 2000])]);
    assert!(over(response(&huge, 65536, &mut out)));
    let mut tiny = [0u8; 8];
    assert!(over(response(&big, 65536, &mut tiny)));
}

/// Sections come off the network. Damage is an error, never a panic.
#[test]
fn decoding_survives_corrupt_sections() {
    let seed = section(&[
        (b":status", b"200"),
        (b"content-length", b"5000"),
        (b"x-custom", b"value"),
    ]);
    let huff =
        hex("0000 5f09 826402 5f15 85aec3771a4b 2f01 25a849e95ba97d7f 89 25a849e95bb8e8b4bf");
    let mut rng = Rng::new(11);
    let (mut line, mut out) = ([0u8; 512], [0u8; 512]);
    for i in 0..4000 {
        let base = if i % 3 == 0 { &huff } else { &seed };
        let data = rng.damage(base, 120);
        let _ = decode_response(&data, 1024, &mut line, &mut out);
    }
}
