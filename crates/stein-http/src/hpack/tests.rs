#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! HPACK tests against RFC 7541 Appendix C, since a round trip passes with a wrong table.

use super::*;

fn hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .filter_map(|c| u8::from_str_radix(core::str::from_utf8(c).ok()?, 16).ok())
        .collect()
}

// RFC 7541 section C.1

#[test]
fn rfc_c11_ten_in_five_bit_prefix() {
    assert_eq!(decode_int(&[0x0a], 5).expect("valid"), (10, 1));
}

#[test]
fn rfc_c12_1337_in_five_bit_prefix() {
    let src = [0x1f, 0x9a, 0x0a];
    assert_eq!(decode_int(&src, 5).expect("valid"), (1337, 3));
}

#[test]
fn rfc_c13_42_in_eight_bit_prefix() {
    assert_eq!(decode_int(&[0x2a], 8).expect("valid"), (42, 1));
}

#[test]
fn integer_roundtrip() {
    for v in [0u64, 1, 30, 31, 32, 127, 128, 1337, 100_000] {
        for prefix in [5u8, 6, 7] {
            let mut buf = Vec::new();
            encode_int(v, prefix, 0x00, &mut buf);
            let (got, n) = decode_int(&buf, prefix).expect("roundtrip");
            assert_eq!(got, v, "value {v} prefix {prefix}");
            assert_eq!(n, buf.len(), "consumed all bytes");
        }
    }
}

#[test]
fn truncated_integer_reports_need_more() {
    assert_eq!(decode_int(&[0x1f, 0x9a], 5), Err(Error::NeedMore));
    assert_eq!(decode_int(&[], 5), Err(Error::NeedMore));
}

#[test]
fn absurd_continuation_is_refused_not_overflowed() {
    let src = [
        0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ];
    assert_eq!(decode_int(&src, 5), Err(Error::IntegerOverflow));
}

// RFC 7541 section C.4

#[test]
fn rfc_c41_huffman_decodes_www_example_com() {
    let src = hex("f1e3c2e5f23a6ba0ab90f4ff");
    let mut out = Vec::new();
    huffman_decode(&src, &mut out, 1024).expect("valid huffman");
    assert_eq!(out, b"www.example.com");
}

#[test]
fn rfc_c42_huffman_decodes_no_cache() {
    let src = hex("a8eb10649cbf");
    let mut out = Vec::new();
    huffman_decode(&src, &mut out, 1024).expect("valid huffman");
    assert_eq!(out, b"no-cache");
}

#[test]
fn rfc_huffman_decodes_a_date_header() {
    // RFC 7541 section C.6, mixes code lengths
    let src = hex("d07abe941054d444a8200595040b8166e084a62d1bff");
    let mut out = Vec::new();
    huffman_decode(&src, &mut out, 1024).expect("valid huffman");
    assert_eq!(out, b"Mon, 21 Oct 2013 20:13:22 GMT");
}

#[test]
fn huffman_respects_the_length_limit() {
    let src = hex("f1e3c2e5f23a6ba0ab90f4ff");
    let mut out = Vec::new();
    assert_eq!(huffman_decode(&src, &mut out, 4), Err(Error::StringTooLong));
}

// RFC 7541 section C.2, section C.3

#[test]
fn rfc_c21_literal_with_indexing() {
    let src = hex("400a637573746f6d2d6b65790d637573746f6d2d686561646572");
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&src, &mut t, 4096, &mut out).expect("valid block");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].name, b"custom-key");
    assert_eq!(out[0].value, b"custom-header");
    assert_eq!(t.len(), 1, "must have been added to the dynamic table");
}

#[test]
fn rfc_c22_literal_without_indexing() {
    let src = hex("040c2f73616d706c652f70617468");
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&src, &mut t, 4096, &mut out).expect("valid block");
    assert_eq!(out[0].name, b":path");
    assert_eq!(out[0].value, b"/sample/path");
    assert!(t.is_empty(), "must NOT be added to the dynamic table");
}

#[test]
fn rfc_c24_indexed_header_field() {
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&[0x82], &mut t, 4096, &mut out).expect("valid");
    assert_eq!(out[0].name, b":method");
    assert_eq!(out[0].value, b"GET");
}

#[test]
fn rfc_c31_first_request_block() {
    let src = hex("828684410f7777772e6578616d706c652e636f6d");
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&src, &mut t, 4096, &mut out).expect("valid block");
    let got: Vec<(String, String)> = out
        .iter()
        .map(|h| {
            (
                String::from_utf8_lossy(&h.name).into_owned(),
                String::from_utf8_lossy(&h.value).into_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (":method".into(), "GET".into()),
            (":scheme".into(), "http".into()),
            (":path".into(), "/".into()),
            (":authority".into(), "www.example.com".into()),
        ]
    );
}

#[test]
fn dynamic_table_index_resolves_after_insert() {
    // 62 is the first dynamic index
    let src = hex("400a637573746f6d2d6b65790d637573746f6d2d686561646572");
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&src, &mut t, 4096, &mut out).expect("valid");
    out.clear();
    decode_block(&[0x80 | 62], &mut t, 4096, &mut out).expect("index 62 is live");
    assert_eq!(out[0].name, b"custom-key");
}

#[test]
fn invalid_index_is_rejected() {
    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    assert_eq!(
        decode_block(&[0x80 | 99], &mut t, 4096, &mut out),
        Err(Error::InvalidIndex)
    );
}

// RFC 7541 section 4

#[test]
fn eviction_is_fifo_and_bounded() {
    let mut t = DynamicTable::new(64 + 32 * 2); // room for ~2 small entries
    t.insert(b"a".to_vec(), b"1".to_vec());
    t.insert(b"b".to_vec(), b"2".to_vec());
    t.insert(b"c".to_vec(), b"3".to_vec());
    assert!(t.size() <= 64 + 32 * 2, "must stay within max size");
    // newest at index 62
    let (n, _) = t.get(62).expect("newest entry");
    assert_eq!(n, b"c");
}

#[test]
fn oversized_entry_clears_the_table() {
    // RFC 7541 section 4.4
    let mut t = DynamicTable::new(64);
    t.insert(b"a".to_vec(), b"1".to_vec());
    assert_eq!(t.len(), 1);
    t.insert(vec![b'x'; 200], b"v".to_vec());
    assert!(t.is_empty(), "oversized insert must clear, not append");
    assert_eq!(t.size(), 0);
}

#[test]
fn size_update_evicts() {
    let mut t = DynamicTable::new(4096);
    t.insert(b"aaaa".to_vec(), b"bbbb".to_vec());
    assert_eq!(t.len(), 1);
    t.set_max_size(0);
    assert!(t.is_empty(), "shrinking to zero must evict everything");
}

#[test]
fn static_hits_encode_as_one_byte() {
    let mut out = Vec::new();
    encode_header(":method", "GET", &mut out);
    assert_eq!(out, vec![0x82], ":method GET is static index 2");
}

#[test]
fn encoded_headers_decode_back() {
    let mut buf = Vec::new();
    encode_header(":method", "GET", &mut buf);
    encode_header(":path", "/hello", &mut buf);
    encode_header("x-custom", "value", &mut buf);

    let mut t = DynamicTable::new(4096);
    let mut out = Vec::new();
    decode_block(&buf, &mut t, 4096, &mut out).expect("our own output must decode");
    let got: Vec<(String, String)> = out
        .iter()
        .map(|h| {
            (
                String::from_utf8_lossy(&h.name).into_owned(),
                String::from_utf8_lossy(&h.value).into_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (":method".into(), "GET".into()),
            (":path".into(), "/hello".into()),
            ("x-custom".into(), "value".into()),
        ]
    );
}
