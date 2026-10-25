//! HPACK differential fuzz of `hpack` vs `hpack::fixed` over a persistent table, plus int, Huffman and encode round trips.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::header;
use stein_http::hpack::{self, DynamicTable, Error as AllocError, fixed};

/// Small ceilings so eviction and compaction happen often.
const CEILINGS: [usize; 14] = [
    0, 31, 32, 33, 64, 65, 100, 128, 255, 256, 300, 512, 1024, 4096,
];

/// Max bytes per decoded field, so the fixed side never runs out on an accepted block.
const OUT_CAP: usize = 1 << 18;

const STATICS: u64 = hpack::STATIC_TABLE.len() as u64;

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

fuzz_target!(|data: &[u8]| {
    differential(data);
    ints(data);
    huffman(data);
    round_trip(data);
});

fn ceiling(b: u8) -> usize {
    if b < 0x80 {
        return CEILINGS[b as usize % CEILINGS.len()];
    }
    (b & 0x7f) as usize * 7
}

/// One length byte per block, or two when the top bit is set.
fn next_block<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (&b, rest) = data.split_first()?;
    let (len, rest) = if b & 0x80 == 0 {
        (b as usize, rest)
    } else {
        let (&lo, rest) = rest.split_first()?;
        (((b & 0x7f) as usize) << 8 | lo as usize, rest)
    };
    let len = len.min(rest.len());
    let (block, rest) = rest.split_at(len);
    *data = rest;
    Some(block)
}

/// Errors both decoders share, in allocating terms. Others are fixed-only limits.
fn shared(e: fixed::Error) -> Option<AllocError> {
    match e {
        fixed::Error::Truncated => Some(AllocError::NeedMore),
        fixed::Error::InvalidIndex => Some(AllocError::InvalidIndex),
        fixed::Error::InvalidHuffman => Some(AllocError::InvalidHuffman),
        fixed::Error::IntegerOverflow => Some(AllocError::IntegerOverflow),
        fixed::Error::TableSizeExceeded
        | fixed::Error::NameTooLong
        | fixed::Error::OutFull
        | fixed::Error::BadStorage => None,
    }
}

fn differential(data: &[u8]) {
    let Some((&first, mut rest)) = data.split_first() else {
        return;
    };
    let ceil = ceiling(first);
    let need = fixed::table_bytes(ceil);
    // Garbage fill so nothing relies on zeroed storage, plus a guard tail.
    let mut mem = vec![0xa5; need + 16];
    let mut table = fixed::Table::new(ceil);
    let mut dynamic = DynamicTable::new(ceil);
    let mut out = Vec::new();
    let mut fields = Vec::new();

    while let Some(block) = next_block(&mut rest) {
        let room = block.len() * (3 + ceil.max(64)) + 3;
        out.clear();
        out.resize(room.min(OUT_CAP), 0);
        fields.clear();
        let got = table.decode_block(&mut mem, block, &mut out);
        let want = hpack::decode_block(block, &mut dynamic, usize::MAX, &mut fields);
        assert!(
            mem[need..].iter().all(|b| *b == 0xa5),
            "table wrote past its storage"
        );
        let n = match got {
            Ok(n) => n,
            Err(e) => {
                // Shared errors must match. Fixed-only limits desync the tables, so stop.
                if let Some(e) = shared(e) {
                    assert_eq!(want, Err(e), "decoders disagree on {block:02x?}");
                }
                return;
            }
        };
        assert_eq!(want, Ok(()), "only the fixed decoder accepted {block:02x?}");
        assert!(n <= out.len());
        let packed = out.get(..n).unwrap_or(&[]);
        let a: Fields = header::iter(packed)
            .map(|(n, v)| (n.to_vec(), v.to_vec()))
            .collect();
        let b: Fields = fields
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect();
        assert_eq!(a, b, "fields differ for {block:02x?}");
        tables(&table, &mem, &dynamic, ceil);
    }
}

/// Same entries in the same order, and consistent size accounting.
fn tables(t: &fixed::Table, mem: &[u8], d: &DynamicTable, ceil: usize) {
    assert_eq!(t.len(), d.len(), "entry count");
    assert_eq!(t.size(), d.size(), "table size");
    assert!(t.size() <= ceil, "size past the ceiling");
    assert!(t.len() <= ceil / 32, "more entries than ring slots");
    assert!(t.len() * 32 <= t.size(), "size undercounts entries");
    let mut sum = 0;
    for i in 0..=t.len() as u64 + 1 {
        let index = STATICS + 1 + i;
        let e = t.get(mem, index);
        assert_eq!(e, d.get(index), "entry {index}");
        if let Some((n, v)) = e {
            sum += n.len() + v.len() + 32;
        }
    }
    assert_eq!(sum, t.size(), "size is not the sum of its entries");
    assert_eq!(t.get(mem, 0), None);
    assert!(t.get(mem, 1).is_some() && t.get(mem, STATICS).is_some());
}

fn ints(data: &[u8]) {
    for prefix in 1..=8u8 {
        let Ok((v, n)) = hpack::decode_int(data, prefix) else {
            continue;
        };
        assert!(
            (1..=data.len()).contains(&n),
            "consumed {n} of {}",
            data.len()
        );
        let mask = ((1u16 << prefix) - 1) as u8;
        let high = data.first().map_or(0, |b| b & !mask);
        let mut enc = Vec::new();
        hpack::encode_int(v, prefix, high, &mut enc);
        // Input may pad with zero continuations, the encoder never does.
        assert!(enc.len() <= n, "canonical form is longer");
        assert_eq!(
            enc.first().map(|b| b & !mask),
            Some(high),
            "prefix bits lost"
        );
        assert_eq!(
            hpack::decode_int(&enc, prefix),
            Ok((v, enc.len())),
            "int round trip"
        );
    }
}

fn huffman(data: &[u8]) {
    let mut all = Vec::new();
    let full = hpack::huffman_decode(data, &mut all, usize::MAX);
    // Shortest code is five bits.
    assert!(all.len() <= data.len() * 8 / 5, "huffman grew too much");
    let limit = data.first().map_or(0, |b| *b as usize);
    let mut capped = Vec::new();
    let got = hpack::huffman_decode(data, &mut capped, limit);
    assert!(capped.len() <= limit, "limit ignored");
    assert!(all.starts_with(&capped), "capped output is not a prefix");
    match full {
        Ok(()) if all.len() <= limit => assert_eq!(got, Ok(()), "capped decode failed"),
        Ok(()) => assert_eq!(got, Err(AllocError::StringTooLong)),
        Err(e) => {
            assert_eq!(e, AllocError::InvalidHuffman);
            assert!(got.is_err(), "capped decode passed bad huffman");
        }
    }
    // Same string as a value literal, to hit the fixed decoder's Huffman loop.
    let mut block = vec![0x01];
    hpack::encode_int(data.len() as u64, 7, 0x80, &mut block);
    block.extend_from_slice(data);
    let mut mem: [u8; 0] = [];
    let mut out = vec![0; 3 + 10 + all.len().max(1)];
    let got = fixed::Table::new(0).decode_block(&mut mem, &block, &mut out);
    match full {
        Ok(()) if all.len() <= u16::MAX as usize => {
            let n = got.unwrap_or(0);
            let packed = out.get(..n).unwrap_or(&[]);
            let f: Vec<_> = header::iter(packed).collect();
            assert_eq!(f, [(&b":authority"[..], &all[..])], "fixed huffman differs");
        }
        Ok(()) => assert_eq!(got, Err(fixed::Error::OutFull)),
        Err(_) => assert_eq!(got, Err(fixed::Error::InvalidHuffman)),
    }
}

/// Fields from the input, split on zero bytes.
fn pairs(data: &[u8]) -> Fields {
    let mut parts = data.split(|b| *b == 0);
    let mut out = Vec::new();
    while let (Some(n), Some(v)) = (parts.next(), parts.next()) {
        out.push((n.to_vec(), v.to_vec()));
    }
    out
}

fn round_trip(data: &[u8]) {
    let fields = pairs(data);
    let mut buf = vec![0u8; data.len() * 2 + 64];
    let mut at = 0;
    for (n, v) in &fields {
        let next = fixed::encode_field(&mut buf, at, n, v);
        assert!(next.is_some(), "encode_field ran out of room");
        at = next.unwrap_or(at);
    }
    let status = data.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mut want = fields.clone();
    if let Some(s) = status {
        let next = fixed::encode_status(&mut buf, at, s);
        assert_eq!(next.is_some(), (100..=999).contains(&s), "status {s}");
        if let Some(next) = next {
            at = next;
            want.push((b":status".to_vec(), s.to_string().into_bytes()));
        }
    }
    let block = buf.get(..at).unwrap_or(&[]);

    let mut d = DynamicTable::new(0);
    let mut got = Vec::new();
    assert_eq!(
        hpack::decode_block(block, &mut d, usize::MAX, &mut got),
        Ok(())
    );
    let got: Fields = got.into_iter().map(|h| (h.name, h.value)).collect();
    assert_eq!(got, want, "allocating round trip");

    let fits = want
        .iter()
        .all(|(n, v)| n.len() <= 255 && v.len() <= u16::MAX as usize);
    // Exactly the packed size, so running out of room is a bug.
    let packed_len: usize = want.iter().map(|(n, v)| 3 + n.len() + v.len()).sum();
    let mut out = vec![0u8; packed_len];
    let mut mem: [u8; 0] = [];
    let r = fixed::Table::new(0).decode_block(&mut mem, block, &mut out);
    if !fits {
        assert!(r.is_err(), "fixed accepted an oversized field");
        return;
    }
    assert_eq!(r, Ok(packed_len), "fixed round trip failed");
    let n = r.unwrap_or(0);
    let packed = out.get(..n).unwrap_or(&[]);
    let f: Fields = header::iter(packed)
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect();
    assert_eq!(f, want, "fixed round trip");
    alloc_encoder(&fields);
}

/// Allocating encoder takes `&str`, so only UTF-8 fields.
fn alloc_encoder(fields: &Fields) {
    let mut block = Vec::new();
    let mut want = Vec::new();
    for (n, v) in fields {
        let (Ok(ns), Ok(vs)) = (core::str::from_utf8(n), core::str::from_utf8(v)) else {
            continue;
        };
        hpack::encode_header(ns, vs, &mut block);
        want.push((n.clone(), v.clone()));
    }
    let mut d = DynamicTable::new(0);
    let mut got = Vec::new();
    assert_eq!(
        hpack::decode_block(&block, &mut d, usize::MAX, &mut got),
        Ok(())
    );
    let got: Fields = got.into_iter().map(|h| (h.name, h.value)).collect();
    assert_eq!(got, want, "encode_header round trip");
}
