#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Fixed storage HPACK tests. RFC 7541 Appendix C, plus differential tests against the
//! allocating decoder.

use super::*;
use crate::hpack::{self, DynamicTable};

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

fn hex(s: &str) -> Vec<u8> {
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    clean
        .chunks(2)
        .map(|c| u8::from_str_radix(core::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Fields {
    list.iter()
        .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

fn unpack(packed: &[u8]) -> Fields {
    header::iter(packed)
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect()
}

struct Dec {
    table: Table,
    mem: Vec<u8>,
    out: Vec<u8>,
}

fn dec(ceiling: usize) -> Dec {
    Dec {
        table: Table::new(ceiling),
        mem: vec![0xaa; table_bytes(ceiling)],
        out: vec![0; 70_000],
    }
}

impl Dec {
    fn decode(&mut self, src: &[u8]) -> Result<Fields, Error> {
        let n = self.table.decode_block(&mut self.mem, src, &mut self.out)?;
        Ok(unpack(&self.out[..n]))
    }

    /// Newest first.
    fn entries(&self) -> Fields {
        (0..self.table.len() as u64)
            .map(|i| {
                let (n, v) = self.table.get(&self.mem, 62 + i).expect("live entry");
                (n.to_vec(), v.to_vec())
            })
            .collect()
    }

    fn expect_table(&self, entries: &[(&str, &str)], size: usize) {
        assert_eq!(self.entries(), pairs(entries));
        assert_eq!(self.table.len(), entries.len());
        assert_eq!(self.table.size(), size);
        assert_eq!(self.table.is_empty(), entries.is_empty());
        let past = 62 + entries.len() as u64;
        assert!(self.table.get(&self.mem, past).is_none());
    }
}

// RFC 7541 section C.2

#[test]
fn rfc_c21_literal_with_indexing() {
    let mut d = dec(4096);
    let got = d
        .decode(&hex("400a637573746f6d2d6b65790d637573746f6d2d686561646572"))
        .unwrap();
    assert_eq!(got, pairs(&[("custom-key", "custom-header")]));
    d.expect_table(&[("custom-key", "custom-header")], 55);
}

#[test]
fn rfc_c22_literal_without_indexing() {
    let mut d = dec(4096);
    let got = d.decode(&hex("040c2f73616d706c652f70617468")).unwrap();
    assert_eq!(got, pairs(&[(":path", "/sample/path")]));
    d.expect_table(&[], 0);
}

#[test]
fn rfc_c23_literal_never_indexed() {
    let mut d = dec(4096);
    let got = d
        .decode(&hex("100870617373776f726406736563726574"))
        .unwrap();
    assert_eq!(got, pairs(&[("password", "secret")]));
    d.expect_table(&[], 0);
}

#[test]
fn rfc_c24_indexed() {
    let mut d = dec(4096);
    assert_eq!(d.decode(&[0x82]).unwrap(), pairs(&[(":method", "GET")]));
    d.expect_table(&[], 0);
}

// RFC 7541 section C.3, section C.4

const AUTHORITY: (&str, &str) = (":authority", "www.example.com");
const NO_CACHE: (&str, &str) = ("cache-control", "no-cache");
const CUSTOM: (&str, &str) = ("custom-key", "custom-value");

fn requests(blocks: [&str; 3]) {
    let mut d = dec(4096);
    let base = [(":method", "GET"), (":scheme", "http"), (":path", "/")];

    let got = d.decode(&hex(blocks[0])).unwrap();
    let mut want = base.to_vec();
    want.push(AUTHORITY);
    assert_eq!(got, pairs(&want));
    d.expect_table(&[AUTHORITY], 57);

    let got = d.decode(&hex(blocks[1])).unwrap();
    want.push(NO_CACHE);
    assert_eq!(got, pairs(&want));
    d.expect_table(&[NO_CACHE, AUTHORITY], 110);

    let got = d.decode(&hex(blocks[2])).unwrap();
    let want = [
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/index.html"),
        AUTHORITY,
        CUSTOM,
    ];
    assert_eq!(got, pairs(&want));
    d.expect_table(&[CUSTOM, NO_CACHE, AUTHORITY], 164);
}

const C3: [&str; 3] = [
    "828684410f7777772e6578616d706c652e636f6d",
    "828684be58086e6f2d6361636865",
    "828785bf400a637573746f6d2d6b65790c637573746f6d2d76616c7565",
];

const C4: [&str; 3] = [
    "828684418cf1e3c2e5f23a6ba0ab90f4ff",
    "828684be5886a8eb10649cbf",
    "828785bf408825a849e95ba97d7f8925a849e95bb8e8b4bf",
];

#[test]
fn rfc_c3_requests_without_huffman() {
    requests(C3);
}

#[test]
fn rfc_c4_requests_with_huffman() {
    requests(C4);
}

// RFC 7541 section C.5, section C.6

const DATE1: (&str, &str) = ("date", "Mon, 21 Oct 2013 20:13:21 GMT");
const DATE2: (&str, &str) = ("date", "Mon, 21 Oct 2013 20:13:22 GMT");
const LOCATION: (&str, &str) = ("location", "https://www.example.com");
const PRIVATE: (&str, &str) = ("cache-control", "private");
const COOKIE: (&str, &str) = (
    "set-cookie",
    "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
);
const GZIP: (&str, &str) = ("content-encoding", "gzip");

fn responses(blocks: [&str; 3]) {
    let mut d = dec(256);
    let s302 = (":status", "302");
    let s307 = (":status", "307");

    let got = d.decode(&hex(blocks[0])).unwrap();
    assert_eq!(got, pairs(&[s302, PRIVATE, DATE1, LOCATION]));
    d.expect_table(&[LOCATION, DATE1, PRIVATE, s302], 222);

    let got = d.decode(&hex(blocks[1])).unwrap();
    assert_eq!(got, pairs(&[s307, PRIVATE, DATE1, LOCATION]));
    d.expect_table(&[s307, LOCATION, DATE1, PRIVATE], 222);

    // several evictions inside one block
    let got = d.decode(&hex(blocks[2])).unwrap();
    let want = [(":status", "200"), PRIVATE, DATE2, LOCATION, GZIP, COOKIE];
    assert_eq!(got, pairs(&want));
    d.expect_table(&[COOKIE, GZIP, DATE2], 215);
}

const C5: [&str; 3] = [
    "4803333032580770726976617465611d4d6f6e2c203231204f637420323031332032303a\
     31333a323120474d546e1768747470733a2f2f7777772e6578616d706c652e636f6d",
    "4803333037c1c0bf",
    "88c1611d4d6f6e2c203231204f637420323031332032303a31333a323220474d54c05a04\
     677a697077 38666f6f3d4153444a4b48514b425a584f5157454f50495541585157454f49\
     553b206d61782d6167653d333630303b2076657273696f6e3d31",
];

const C6: [&str; 3] = [
    "488264025885aec3771a4b6196d07abe941054d444a8200595040b8166e082a62d1bff6e\
     919d29ad171863c78f0b97c8e9ae82ae43d3",
    "4883640effc1c0bf",
    "88c16196d07abe941054d444a8200595040b8166e084a62d1bffc05a839bd9ab77ad94e7\
     821dd7f2e6c7b335dfdfcd5b3960d5af27087f3672c1ab270fb5291f9587316065c003ed\
     4ee5b1063d5007",
];

#[test]
fn rfc_c5_responses_without_huffman() {
    responses(C5);
}

#[test]
fn rfc_c6_responses_with_huffman() {
    responses(C6);
}

fn indexed_literal(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut b = vec![0x40];
    hpack::encode_string(name, &mut b);
    hpack::encode_string(value, &mut b);
    b
}

fn size_update(max: u64) -> Vec<u8> {
    let mut b = Vec::new();
    hpack::encode_int(max, 5, 0x20, &mut b);
    b
}

#[test]
fn eviction_is_oldest_first() {
    // 34 bytes per entry, 100 holds two
    let mut d = dec(100);
    for (i, name) in [b"a", b"b", b"c", b"d"].iter().enumerate() {
        d.decode(&indexed_literal(*name, b"1")).unwrap();
        assert_eq!(d.table.len(), (i + 1).min(2));
    }
    d.expect_table(&[("d", "1"), ("c", "1")], 68);
}

#[test]
fn size_update_lowers_then_raises() {
    let mut d = dec(200);
    for name in [b"a", b"b", b"c", b"d"] {
        d.decode(&indexed_literal(name, b"1")).unwrap();
    }
    assert_eq!(d.table.len(), 4);

    d.decode(&size_update(68)).unwrap();
    d.expect_table(&[("d", "1"), ("c", "1")], 68);
    d.decode(&indexed_literal(b"e", b"1")).unwrap();
    d.expect_table(&[("e", "1"), ("d", "1")], 68);

    // two updates in one block, to zero then back to the ceiling
    let mut block = size_update(0);
    block.extend(size_update(200));
    block.extend(indexed_literal(b"f", b"1"));
    d.decode(&block).unwrap();
    d.expect_table(&[("f", "1")], 34);
    for name in [b"g", b"h", b"i", b"j"] {
        d.decode(&indexed_literal(name, b"1")).unwrap();
    }
    assert_eq!(d.table.len(), 5, "raised back to the ceiling");
}

#[test]
fn size_update_above_the_ceiling_is_an_error() {
    let mut d = dec(200);
    assert_eq!(d.decode(&size_update(200)), Ok(Vec::new()));
    assert_eq!(d.decode(&size_update(201)), Err(Error::TableSizeExceeded));
}

#[test]
fn size_update_after_a_field_is_an_error() {
    let mut d = dec(200);
    let mut block = vec![0x82];
    block.extend(size_update(100));
    assert_eq!(d.decode(&block), Err(Error::TableSizeExceeded));
}

#[test]
fn oversized_entry_empties_the_table() {
    let mut d = dec(100);
    d.decode(&indexed_literal(b"a", b"1")).unwrap();
    d.decode(&indexed_literal(b"b", b"1")).unwrap();
    assert_eq!(d.table.len(), 2);

    // 1 + 68 + 32 = 101
    let big = vec![b'x'; 68];
    let got = d.decode(&indexed_literal(b"c", &big)).unwrap();
    assert_eq!(
        got,
        vec![(b"c".to_vec(), big)],
        "the field is still decoded"
    );
    d.expect_table(&[], 0);

    d.decode(&indexed_literal(b"d", &[b'y'; 67])).unwrap();
    assert_eq!(d.table.len(), 1);
    assert_eq!(d.table.size(), 100);
}

#[test]
fn a_name_outlives_the_entry_it_came_from() {
    // RFC 7541 section 4.4, the name's source entry is evicted by the insert using it
    let mut d = dec(64);
    d.decode(&indexed_literal(b"evicted-name", b"first-value"))
        .unwrap();
    d.expect_table(&[("evicted-name", "first-value")], 55);

    let mut block = vec![0x40 | 62];
    hpack::encode_string(b"second-value", &mut block);
    let got = d.decode(&block).unwrap();
    assert_eq!(got, pairs(&[("evicted-name", "second-value")]));
    d.expect_table(&[("evicted-name", "second-value")], 56);

    // entry too large to insert
    let mut block = vec![0x40 | 62];
    hpack::encode_string(&[b'z'; 40], &mut block);
    let got = d.decode(&block).unwrap();
    assert_eq!(got[0].0, b"evicted-name");
    assert_eq!(got[0].1, vec![b'z'; 40]);
    d.expect_table(&[], 0);
}

#[test]
fn an_indexed_field_reads_a_dynamic_entry() {
    let mut d = dec(4096);
    d.decode(&indexed_literal(b"x-one", b"1")).unwrap();
    d.decode(&indexed_literal(b"x-two", b"2")).unwrap();
    let got = d.decode(&[0x80 | 63, 0x80 | 62]).unwrap();
    assert_eq!(got, pairs(&[("x-one", "1"), ("x-two", "2")]));
    assert_eq!(d.decode(&[0x80 | 64]), Err(Error::InvalidIndex));
    assert_eq!(d.decode(&[0x80]), Err(Error::InvalidIndex));
}

#[test]
fn a_long_run_of_inserts_matches_a_model() {
    // wraps the ring and compacts many times
    let mut d = dec(128);
    let mut model: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for i in 0..2000u32 {
        let name = format!("n{}", i % 7).into_bytes();
        let value = vec![b'a' + (i % 26) as u8; (i as usize * 7) % 45];
        d.decode(&indexed_literal(&name, &value)).unwrap();
        model.insert(0, (name, value));
        let mut size: usize = model.iter().map(|(n, v)| n.len() + v.len() + 32).sum();
        while size > 128 {
            let (n, v) = model.pop().unwrap();
            size -= n.len() + v.len() + 32;
        }
        assert_eq!(d.entries(), model, "after insert {i}");
        assert_eq!(d.table.size(), size);
    }
}

fn after_two_requests() -> Dec {
    let mut d = dec(4096);
    d.decode(&hex(C4[0])).unwrap();
    d.decode(&hex(C4[1])).unwrap();
    d
}

#[test]
fn out_too_small_is_an_error_at_every_size() {
    let block = hex(C4[2]);
    let mut d = after_two_requests();
    let n = d
        .table
        .decode_block(&mut d.mem, &block, &mut d.out)
        .unwrap();
    assert!(n > 0);

    for room in 0..n {
        let mut out = vec![0u8; room];
        let mut d = after_two_requests();
        let got = d.table.decode_block(&mut d.mem, &block, &mut out);
        assert_eq!(got, Err(Error::OutFull), "room {room}");
    }
    let mut out = vec![0u8; n];
    let mut d = after_two_requests();
    assert_eq!(d.table.decode_block(&mut d.mem, &block, &mut out), Ok(n));
}

#[test]
fn a_name_past_255_bytes_is_refused() {
    let mut d = dec(4096);
    let name = vec![b'n'; 256];
    let mut block = vec![0x00];
    hpack::encode_string(&name, &mut block);
    hpack::encode_string(b"v", &mut block);
    assert_eq!(d.decode(&block), Err(Error::NameTooLong));

    let mut block = vec![0x00];
    hpack::encode_string(&name[..255], &mut block);
    hpack::encode_string(b"v", &mut block);
    assert_eq!(d.decode(&block).unwrap()[0].0.len(), 255);

    // via Huffman, length only known after decoding
    let mut block = vec![0x00];
    huff_string(&name, &mut block);
    hpack::encode_string(b"v", &mut block);
    assert_eq!(d.decode(&block), Err(Error::NameTooLong));
}

#[test]
fn a_value_past_65535_bytes_is_refused() {
    let mut d = dec(4096);
    let value = vec![b'v'; 65_536];
    let mut block = vec![0x00];
    hpack::encode_string(b"n", &mut block);
    hpack::encode_string(&value, &mut block);
    assert_eq!(d.decode(&block), Err(Error::OutFull));

    let mut block = vec![0x00];
    hpack::encode_string(b"n", &mut block);
    hpack::encode_string(&value[..65_535], &mut block);
    assert_eq!(d.decode(&block).unwrap()[0].1.len(), 65_535);
}

#[test]
fn short_storage_is_refused() {
    let mut t = Table::new(4096);
    let mut mem = vec![0u8; table_bytes(4096) - 1];
    let mut out = [0u8; 64];
    assert_eq!(
        t.decode_block(&mut mem, &[0x82], &mut out),
        Err(Error::BadStorage)
    );
    mem.push(0);
    assert!(t.decode_block(&mut mem, &[0x82], &mut out).is_ok());
}

#[test]
fn table_bytes_covers_entries_and_offsets() {
    assert_eq!(table_bytes(4096), 4096 + 128 * 4);
    assert_eq!(table_bytes(0), 0);
    assert_eq!(table_bytes(31), 31);
    let mut d = dec(31);
    d.decode(&indexed_literal(b"", b"")).unwrap();
    d.expect_table(&[], 0);
    let mut d = dec(32);
    d.decode(&indexed_literal(b"", b"")).unwrap();
    d.expect_table(&[("", "")], 32);
}

#[test]
fn bad_huffman_is_an_error() {
    let mut d = dec(4096);
    // padding of zeros, not ones
    assert_eq!(
        d.decode(&[0x00, 0x81, 0x00, 0x00]),
        Err(Error::InvalidHuffman)
    );
}

#[test]
fn an_integer_that_overflows_is_an_error() {
    let mut d = dec(4096);
    let mut block = vec![0xff];
    block.extend([0xff; 12]);
    block.push(0x01);
    assert_eq!(d.decode(&block), Err(Error::IntegerOverflow));
}

#[test]
fn every_prefix_of_a_block_is_an_error_or_fewer_fields() {
    let blocks = [C3, C4, C5, C6];
    for set in blocks {
        let mut done = dec(256);
        for block in set {
            let block = hex(block);
            let mut whole = Dec {
                table: done.table,
                mem: done.mem.clone(),
                out: vec![0; 4096],
            };
            let full = whole.decode(&block).unwrap();
            for cut in 0..block.len() {
                let mut d = Dec {
                    table: done.table,
                    mem: done.mem.clone(),
                    out: vec![0; 4096],
                };
                match d.decode(&block[..cut]) {
                    Ok(fields) => {
                        assert!(fields.len() < full.len() || cut == block.len());
                        assert_eq!(fields[..], full[..fields.len()], "cut {cut}");
                    }
                    Err(e) => assert_eq!(e, Error::Truncated, "cut {cut}"),
                }
            }
            done = whole;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn text(&mut self, max: u64) -> Vec<u8> {
        let len = self.below(max + 1);
        (0..len).map(|_| b'a' + self.below(26) as u8).collect()
    }
}

fn huff_string(s: &[u8], dst: &mut Vec<u8>) {
    let mut bits = 0u64;
    let mut have = 0usize;
    let mut coded = Vec::new();
    for b in s {
        let (n, code) = hpack::table::ENCODE_TABLE[*b as usize];
        bits = (bits << n) | code;
        have += n;
        while have >= 8 {
            have -= 8;
            coded.push((bits >> have) as u8);
        }
    }
    if have > 0 {
        let pad = 8 - have;
        coded.push(((bits << pad) | ((1 << pad) - 1)) as u8);
    }
    hpack::encode_int(coded.len() as u64, 7, 0x80, dst);
    dst.extend(coded);
}

fn any_string(rng: &mut Rng, s: &[u8], dst: &mut Vec<u8>) {
    if rng.below(2) == 0 {
        huff_string(s, dst);
    } else {
        hpack::encode_string(s, dst);
    }
}

const NAMES: [&str; 8] = [
    "cookie",
    "accept",
    ":path",
    "x-request-id",
    "user-agent",
    "x-a",
    "content-type",
    ":status",
];

fn any_name(rng: &mut Rng) -> Vec<u8> {
    if rng.below(3) == 0 {
        let mut n = rng.text(20);
        n.push(b'x');
        return n;
    }
    NAMES[rng.below(NAMES.len() as u64) as usize]
        .as_bytes()
        .to_vec()
}

fn any_value(rng: &mut Rng) -> Vec<u8> {
    match rng.below(10) {
        0 => rng.text(300),
        1 => Vec::new(),
        _ => rng.text(40),
    }
}

/// `model` mirrors the table so chosen indexes are valid.
fn any_block(rng: &mut Rng, model: &mut DynamicTable, ceiling: usize) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..rng.below(3).saturating_sub(1) {
        let max = rng.below(ceiling as u64 + 1);
        hpack::encode_int(max, 5, 0x20, &mut b);
        model.set_max_size(max as usize);
    }
    for _ in 0..rng.below(12) {
        let index = 1 + rng.below(61 + model.len() as u64);
        let value = any_value(rng);
        match rng.below(6) {
            0 => {
                let name = any_name(rng);
                let (n, v) = (String::from_utf8(name), String::from_utf8(value));
                hpack::encode_header(&n.unwrap(), &v.unwrap(), &mut b);
            }
            1 => {
                let name = any_name(rng);
                b.push(0x40);
                any_string(rng, &name, &mut b);
                any_string(rng, &value, &mut b);
                model.insert(name, value);
            }
            2 => {
                hpack::encode_int(index, 6, 0x40, &mut b);
                any_string(rng, &value, &mut b);
                let name = model.get(index).unwrap().0.to_vec();
                model.insert(name, value);
            }
            3 => hpack::encode_int(index, 7, 0x80, &mut b),
            4 => {
                hpack::encode_int(index, 4, 0x10, &mut b);
                any_string(rng, &value, &mut b);
            }
            _ => {
                let name = any_name(rng);
                b.push(0x00);
                any_string(rng, &name, &mut b);
                any_string(rng, &value, &mut b);
            }
        }
    }
    b
}

#[test]
fn differential_against_the_allocating_decoder() {
    for seed in 0..300u64 {
        let mut rng = Rng(seed);
        let ceiling = [64usize, 128, 256, 1024, 4096][rng.below(5) as usize];
        let mut model = DynamicTable::new(ceiling);
        let mut old = DynamicTable::new(ceiling);
        let mut new = dec(ceiling);
        for round in 0..20 {
            let block = any_block(&mut rng, &mut model, ceiling);
            let mut want = Vec::new();
            hpack::decode_block(&block, &mut old, 70_000, &mut want).unwrap();
            let want: Fields = want.into_iter().map(|h| (h.name, h.value)).collect();
            let got = new.decode(&block).unwrap();
            assert_eq!(got, want, "seed {seed} round {round}");

            assert_eq!(new.table.len(), old.len(), "seed {seed} round {round}");
            assert_eq!(new.table.size(), old.size(), "seed {seed} round {round}");
            for i in 0..old.len() as u64 + 2 {
                let a = old.get(62 + i);
                let b = new.table.get(&new.mem, 62 + i);
                assert_eq!(a, b, "seed {seed} round {round} entry {i}");
            }
        }
    }
}

#[test]
fn random_bytes_never_panic() {
    for seed in 0..300u64 {
        let mut rng = Rng(seed ^ 0xdead_beef);
        let mut d = dec(256);
        d.out = vec![0; 512];
        for _ in 0..20 {
            let len = rng.below(64);
            let block: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if d.decode(&block).is_err() {
                break;
            }
            assert!(d.table.size() <= 256);
        }
    }
}

fn encoded(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = [0u8; 512];
    let n = encode_field(&mut out, 0, name, value).unwrap();
    out[..n].to_vec()
}

#[test]
fn static_hits_are_exact_bytes() {
    let mut out = [0u8; 16];
    assert_eq!(encode_status(&mut out, 0, 200), Some(1));
    assert_eq!(out[0], 0x88);
    let codes = [(204, 0x89), (206, 0x8a), (304, 0x8b), (400, 0x8c)];
    for (code, byte) in codes {
        assert_eq!(encode_status(&mut out, 0, code), Some(1));
        assert_eq!(out[0], byte);
    }
    assert_eq!(encoded(b":method", b"GET"), [0x82]);
    assert_eq!(encoded(b":method", b"POST"), [0x83]);
    assert_eq!(encoded(b"accept-encoding", b"gzip, deflate"), [0x90]);
    assert_eq!(encoded(b"www-authenticate", b""), [0x80 | 61]);
}

#[test]
fn name_hits_and_misses_are_exact_bytes() {
    // :status name index 8, 4 bit prefix
    let mut out = [0u8; 16];
    assert_eq!(encode_status(&mut out, 0, 201), Some(5));
    assert_eq!(&out[..5], b"\x08\x03201");
    assert_eq!(encode_status(&mut out, 0, 99), None);
    assert_eq!(encode_status(&mut out, 0, 1000), None);

    // content-type index 31 overflows the prefix, 15 then 16
    assert_eq!(encoded(b"content-type", b"a/b"), b"\x0f\x10\x03a/b");
    assert_eq!(encoded(b":method", b"PUT"), b"\x02\x03PUT");
    assert_eq!(encoded(b"x-a", b"1"), b"\x00\x03x-a\x011");
}

#[test]
fn encoding_stops_cleanly_when_out_is_short() {
    let cases: [(&[u8], &[u8]); 4] = [
        (b":method", b"GET"),
        (b"content-type", b"text/plain"),
        (b"x-custom", b"value"),
        (b"x-long", &[b'v'; 300]),
    ];
    for (name, value) in cases {
        let full = encoded(name, value);
        for room in 0..full.len() {
            let mut out = vec![0u8; room];
            assert_eq!(encode_field(&mut out, 0, name, value), None);
        }
        let mut out = vec![0u8; full.len() + 3];
        assert_eq!(
            encode_field(&mut out, 3, name, value),
            Some(full.len() + 3),
            "at is honoured"
        );
        assert_eq!(out[3..], full[..]);
    }
}

#[test]
fn encoder_output_decodes_through_both_decoders() {
    for seed in 0..300u64 {
        let mut rng = Rng(seed);
        let mut out = vec![0u8; 8192];
        let mut at = 0;
        let mut want: Fields = Vec::new();
        let status = [200u16, 204, 301, 404, 500, 503, 100, 999][rng.below(8) as usize];
        at = encode_status(&mut out, at, status).unwrap();
        want.push((b":status".to_vec(), status.to_string().into_bytes()));
        for _ in 0..rng.below(12) {
            let (name, value) = match rng.below(4) {
                0 => {
                    let (n, v) = hpack::STATIC_TABLE[rng.below(61) as usize];
                    (n.as_bytes().to_vec(), v.as_bytes().to_vec())
                }
                _ => (any_name(&mut rng), any_value(&mut rng)),
            };
            at = encode_field(&mut out, at, &name, &value).unwrap();
            want.push((name, value));
        }

        let mut d = dec(4096);
        assert_eq!(d.decode(&out[..at]).unwrap(), want, "seed {seed}");
        d.expect_table(&[], 0);

        let mut old = DynamicTable::new(4096);
        let mut got = Vec::new();
        hpack::decode_block(&out[..at], &mut old, 70_000, &mut got).unwrap();
        let got: Fields = got.into_iter().map(|h| (h.name, h.value)).collect();
        assert_eq!(got, want, "seed {seed}");
        assert!(old.is_empty());
    }
}
