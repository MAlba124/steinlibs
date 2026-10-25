//! DNS reply decode: spans stay inside their buffers, more storage never changes an
//! accepted reply, the header agrees, and a query encodes and reads back as asked.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_dns::{Query, Record, Storage, Type, decode, decode_header, encode, matches_query};

const RECORDS: usize = 64;
const NAMES: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, msg)) = data.split_first() else {
        return;
    };
    replies(knob, msg);
    query(knob, msg);
});

/// Decode with a small table picked by `knob`, then with the full one.
fn replies(knob: u8, msg: &[u8]) {
    let mut big_records = [Record::default(); RECORDS];
    let mut big_names = [0u8; NAMES];
    let mut big = Storage::new(&mut big_records, &mut big_names);
    let whole = decode(msg, &mut big);
    let head = decode_header(msg);
    if let Ok(r) = &whole {
        check(msg, r, big.records, big.names);
        let agrees = head.is_ok_and(|h| (h.id, h.flags) == (r.id, r.flags));
        assert!(agrees, "the header of an accepted reply agrees");
    }
    let (n, k) = (usize::from(knob & 0x0f), usize::from(knob >> 4) * 64);
    let mut small_records = [Record::default(); RECORDS];
    let mut small_names = [0u8; NAMES];
    let mut small = Storage::new(&mut small_records[..n], &mut small_names[..k]);
    let Ok(r) = decode(msg, &mut small) else {
        return;
    };
    // Accepted in less room means accepted the same in more.
    assert!(
        whole.as_ref().is_ok_and(|w| *w == r),
        "more storage changed what less took"
    );
    let used = usize::from(r.answers) + usize::from(r.authorities) + usize::from(r.additionals);
    assert_eq!(&small.records[..used], &big.records[..used]);
    let names = usize::from(r.names_len);
    assert_eq!(&small.names[..names], &big.names[..names]);
}

fn check(msg: &[u8], r: &stein_dns::Reply, records: &[Record], names: &[u8]) {
    let used = usize::from(r.answers) + usize::from(r.authorities) + usize::from(r.additionals);
    assert!(used <= records.len());
    let names_len = usize::from(r.names_len);
    assert!(names_len <= names.len());
    let name_ok = |n: stein_dns::Name| usize::from(n.at) + usize::from(n.len) <= names_len;
    let raw_ok = |w: stein_dns::Raw| usize::from(w.at) + usize::from(w.len) <= msg.len();
    if let Some(q) = r.question {
        assert!(name_ok(q.name) && raw_ok(q.raw));
    }
    if let Some(o) = r.edns {
        assert!(raw_ok(o.options));
    }
    for rec in &records[..used] {
        assert!(name_ok(rec.name));
        match rec.data {
            stein_dns::Data::Name(n) => assert!(name_ok(n)),
            stein_dns::Data::Raw(w) => {
                assert!(raw_ok(w));
                // RDATA of an HTTPS record goes on to its own reader.
                if rec.kind == Type(stein_dns::svcb::HTTPS) {
                    let _ = stein_dns::svcb::parse(w.bytes(msg));
                }
            }
            stein_dns::Data::A(_) | stein_dns::Data::Aaaa(_) => {}
        }
    }
}

/// A name from the input, encoded as a query, read back as its own reply.
fn query(knob: u8, msg: &[u8]) {
    let Ok(name) = core::str::from_utf8(&msg[..msg.len().min(300)]) else {
        return;
    };
    let mut q = Query::new(name, Type(u16::from(knob)), u16::from(knob) << 8 | 7);
    q.udp_payload = (knob & 1 == 1).then_some(1232);
    let mut out = [0u8; 600];
    let Ok(n) = encode(&q, &mut out) else {
        return;
    };
    // Set QR so the query reads as a reply to itself.
    out[2] |= 0x80;
    let mut records = [Record::default(); 4];
    let mut names = [0u8; 512];
    let mut st = Storage::new(&mut records, &mut names);
    let r = decode(&out[..n], &mut st);
    assert!(
        r.is_ok_and(|r| matches_query(&r, st.names, &q)),
        "{name:?} reads back as asked"
    );
}
