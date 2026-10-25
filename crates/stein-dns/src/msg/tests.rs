#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::vec::Vec;

/// A message under construction. Every test builds its bytes by hand, because
/// a test that encodes with the same code it decodes with proves nothing about
/// what is on the wire.
struct Msg {
    bytes: Vec<u8>,
}

impl Msg {
    /// A header with the four counts written up front, so the sections that
    /// follow have to match what was claimed.
    fn new(id: u16, flags: u16, counts: [u16; 4]) -> Msg {
        let mut m = Msg { bytes: Vec::new() };
        m.u16(id);
        m.u16(flags);
        for c in counts {
            m.u16(c);
        }
        m
    }

    /// A reply header with one question and `an` answers.
    fn reply(an: u16) -> Msg {
        Msg::new(0x1234, Flags::QR | Flags::RD | Flags::RA, [1, an, 0, 0])
    }

    fn at(&self) -> u16 {
        self.bytes.len() as u16
    }

    fn u8(&mut self, v: u8) -> &mut Msg {
        self.bytes.push(v);
        self
    }

    fn u16(&mut self, v: u16) -> &mut Msg {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn u32(&mut self, v: u32) -> &mut Msg {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn raw(&mut self, v: &[u8]) -> &mut Msg {
        self.bytes.extend_from_slice(v);
        self
    }

    /// A dotted name as length prefixed labels plus the root byte.
    fn name(&mut self, name: &str) -> &mut Msg {
        for label in name.split('.').filter(|l| !l.is_empty()) {
            self.u8(label.len() as u8);
            self.raw(label.as_bytes());
        }
        self.u8(0)
    }

    /// A compression pointer to `target`.
    fn ptr(&mut self, target: u16) -> &mut Msg {
        self.u16(0xC000 | target)
    }

    fn question(&mut self, name: &str, kind: Type) -> &mut Msg {
        self.name(name);
        self.u16(kind.0);
        self.u16(Class::IN.0)
    }

    /// A record with a hand written owner name.
    fn record(&mut self, name: &str, kind: Type, ttl: u32, rdata: &[u8]) -> &mut Msg {
        self.name(name);
        self.head(kind, ttl, rdata.len() as u16);
        self.raw(rdata)
    }

    /// Type, class, ttl and rdlength, for when the owner name or the rdlength
    /// has to be written by hand.
    fn head(&mut self, kind: Type, ttl: u32, rdlength: u16) -> &mut Msg {
        self.u16(kind.0);
        self.u16(Class::IN.0);
        self.u32(ttl);
        self.u16(rdlength)
    }
}

/// Tables big enough for anything here unless a test says otherwise.
struct Tables {
    records: Vec<Record>,
    names: Vec<u8>,
}

impl Tables {
    fn new(records: usize, names: usize) -> Tables {
        Tables {
            records: std::vec![Record::default(); records],
            names: std::vec![0u8; names],
        }
    }

    fn big() -> Tables {
        Tables::new(16, 4096)
    }

    fn storage(&mut self) -> Storage<'_> {
        Storage::new(&mut self.records, &mut self.names)
    }
}

fn kind_of(e: DnsError) -> ErrorKind {
    e.kind
}

// Encoding.

#[test]
fn encoding_a_query_for_a_single_label_name_writes_the_header_and_one_label() {
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        udp_payload: None,
        ..Query::new("localhost", Type::A, 0xBEEF)
    };
    let n = encode(&q, &mut out).unwrap();
    let want = {
        let mut m = Msg::new(0xBEEF, Flags::RD, [1, 0, 0, 0]);
        m.question("localhost", Type::A);
        m.bytes
    };
    assert_eq!(&out[..n], &want[..]);
}

#[test]
fn encoding_a_query_for_a_name_with_several_labels_writes_every_label() {
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        udp_payload: None,
        ..Query::new("feeds.a.example.com", Type::AAAA, 1)
    };
    let n = encode(&q, &mut out).unwrap();
    let want = {
        let mut m = Msg::new(1, Flags::RD, [1, 0, 0, 0]);
        m.question("feeds.a.example.com", Type::AAAA);
        m.bytes
    };
    assert_eq!(&out[..n], &want[..]);
}

#[test]
fn a_trailing_root_dot_encodes_the_same_as_no_trailing_dot() {
    let mut with = [0u8; QUERY_MAX];
    let mut without = [0u8; QUERY_MAX];
    let a = encode_query("example.com.", Type::A, 7, &mut with).unwrap();
    let b = encode_query("example.com", Type::A, 7, &mut without).unwrap();
    assert_eq!(&with[..a], &without[..b]);
}

#[test]
fn a_name_longer_than_the_format_allows_is_refused() {
    // Four 63 byte labels plus separators is 259 bytes on the wire.
    let label = "a".repeat(63);
    let name = std::format!("{label}.{label}.{label}.{label}");
    let mut out = [0u8; 512];
    let err = encode_query(&name, Type::A, 1, &mut out).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::BadName);
}

#[test]
fn a_name_that_just_fits_is_accepted() {
    // Three 63 byte labels and one 61 byte label is exactly 255 on the wire.
    let long = "a".repeat(63);
    let last = "b".repeat(61);
    let name = std::format!("{long}.{long}.{long}.{last}");
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        udp_payload: None,
        ..Query::new(&name, Type::A, 1)
    };
    let n = encode(&q, &mut out).unwrap();
    assert_eq!(n, HEADER + MAX_NAME + 4);
}

#[test]
fn a_label_longer_than_sixty_three_bytes_is_refused() {
    let name = std::format!("{}.com", "a".repeat(64));
    let mut out = [0u8; 512];
    let err = encode_query(&name, Type::A, 1, &mut out).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::BadName);
}

#[test]
fn an_empty_label_in_the_middle_of_a_name_is_refused() {
    let mut out = [0u8; 512];
    let err = encode_query("a..com", Type::A, 1, &mut out).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::BadName);
}

#[test]
fn a_label_with_a_space_in_it_is_refused() {
    let mut out = [0u8; 512];
    let err = encode_query("ex ample.com", Type::A, 1, &mut out).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::BadName);
}

#[test]
fn an_output_buffer_one_byte_short_reports_output_full() {
    let mut out = [0u8; QUERY_MAX];
    let n = encode_query("example.com", Type::A, 1, &mut out).unwrap();
    let err = encode_query("example.com", Type::A, 1, &mut out[..n - 1]).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::OutputFull);
}

#[test]
fn every_output_buffer_shorter_than_the_query_reports_output_full() {
    let mut out = [0u8; QUERY_MAX];
    let n = encode_query("feeds.example.com", Type::A, 1, &mut out).unwrap();
    for short in 0..n {
        let err = encode_query("feeds.example.com", Type::A, 1, &mut out[..short]).unwrap_err();
        assert_eq!(kind_of(err), ErrorKind::OutputFull, "at length {short}");
    }
}

#[test]
fn a_query_carries_an_edns_opt_record_by_default() {
    let mut out = [0u8; QUERY_MAX];
    let n = encode_query("example.com", Type::A, 0x0102, &mut out).unwrap();
    let want = {
        let mut m = Msg::new(0x0102, Flags::RD, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        // Root owner name, OPT, receive size in the class field, zero ttl.
        m.u8(0);
        m.u16(Type::OPT.0);
        m.u16(DEFAULT_UDP_PAYLOAD);
        m.u32(0);
        m.u16(0);
        m.bytes
    };
    assert_eq!(&out[..n], &want[..]);
    assert_eq!(n, HEADER + 13 + 4 + OPT_LEN);
}

#[test]
fn a_query_without_edns_claims_no_additional_records() {
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        udp_payload: None,
        ..Query::new("example.com", Type::A, 1)
    };
    let n = encode(&q, &mut out).unwrap();
    let head = decode_header(&out[..n]).unwrap();
    assert_eq!(head.counts, [1, 0, 0, 0]);
    assert!(!head.flags.is_response());
    assert!(head.flags.recursion_desired());
}

#[test]
fn a_query_can_ask_the_server_not_to_recurse() {
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        recursion_desired: false,
        ..Query::new("example.com", Type::A, 1)
    };
    let n = encode(&q, &mut out).unwrap();
    assert!(!decode_header(&out[..n]).unwrap().flags.recursion_desired());
}

// Decoding answers.

#[test]
fn decoding_an_a_response_yields_the_address() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        m.record("example.com", Type::A, 300, &[93, 184, 216, 34]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.id, 0x1234);
    assert_eq!(reply.rcode, Rcode::NoError);
    assert!(!reply.is_truncated());
    assert_eq!(reply.answers, 1);
    let answers = reply.answers(&t.records);
    assert_eq!(answers[0].data, Data::A([93, 184, 216, 34]));
    assert_eq!(answers[0].ttl, 300);
    assert_eq!(answers[0].class, Class::IN);
    assert_eq!(answers[0].name.bytes(&t.names), b"example.com");
    assert_eq!(reply.question_name(&t.names), b"example.com");
}

#[test]
fn decoding_an_aaaa_response_yields_the_address() {
    let addr = [
        0x26, 0x06, 0x28, 0x00, 0x02, 0x20, 0x00, 0x01, 0x02, 0x48, 0x18, 0x93, 0x25, 0xc8, 0x19,
        0x46,
    ];
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::AAAA);
        m.record("example.com", Type::AAAA, 60, &addr);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.answers(&t.records)[0].data, Data::Aaaa(addr));
}

#[test]
fn several_answers_are_all_reported_in_order() {
    let msg = {
        let mut m = Msg::reply(3);
        m.question("example.com", Type::A);
        m.record("example.com", Type::A, 1, &[1, 1, 1, 1]);
        m.record("example.com", Type::A, 2, &[2, 2, 2, 2]);
        m.record("example.com", Type::A, 3, &[3, 3, 3, 3]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.answers, 3);
    let got: Vec<Data> = reply.answers(&t.records).iter().map(|r| r.data).collect();
    assert_eq!(
        got,
        std::vec![
            Data::A([1, 1, 1, 1]),
            Data::A([2, 2, 2, 2]),
            Data::A([3, 3, 3, 3]),
        ]
    );
}

#[test]
fn a_cname_followed_by_an_a_is_reported_as_both_and_not_followed() {
    let msg = {
        let mut m = Msg::reply(2);
        m.question("www.example.com", Type::A);
        let mut target = Msg { bytes: Vec::new() };
        target.name("host.example.net");
        m.record("www.example.com", Type::CNAME, 60, &target.bytes);
        m.record("host.example.net", Type::A, 60, &[10, 0, 0, 1]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let answers = reply.answers(&t.records);
    assert_eq!(answers[0].kind, Type::CNAME);
    assert_eq!(answers[0].name.bytes(&t.names), b"www.example.com");
    let Data::Name(target) = answers[0].data else {
        panic!("expected a name");
    };
    assert_eq!(target.bytes(&t.names), b"host.example.net");
    assert_eq!(answers[1].kind, Type::A);
    assert_eq!(answers[1].name.bytes(&t.names), b"host.example.net");
    assert_eq!(answers[1].data, Data::A([10, 0, 0, 1]));
}

#[test]
fn records_from_every_section_land_in_their_own_slice() {
    let msg = {
        let mut m = Msg::new(9, Flags::QR, [1, 1, 1, 1]);
        m.question("example.com", Type::A);
        m.record("example.com", Type::A, 1, &[1, 1, 1, 1]);
        let mut ns = Msg { bytes: Vec::new() };
        ns.name("ns.example.com");
        m.record("example.com", Type::NS, 2, &ns.bytes);
        m.record("ns.example.com", Type::A, 3, &[2, 2, 2, 2]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(
        (reply.answers, reply.authorities, reply.additionals),
        (1, 1, 1)
    );
    assert_eq!(reply.answers(&t.records)[0].ttl, 1);
    assert_eq!(reply.authorities(&t.records)[0].ttl, 2);
    assert_eq!(reply.additionals(&t.records)[0].ttl, 3);
}

#[test]
fn an_unknown_record_type_keeps_its_rdata_where_it_lies() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        m.record("example.com", Type::TXT, 60, b"\x05hello");
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let Data::Raw(raw) = reply.answers(&t.records)[0].data else {
        panic!("expected raw rdata");
    };
    assert_eq!(raw.bytes(&msg), b"\x05hello");
}

// Codes and flags.

#[test]
fn nxdomain_is_distinguishable_from_success() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR | Rcode::NxDomain.bits(), [1, 0, 0, 0]);
        m.question("nope.example.com", Type::A);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.rcode, Rcode::NxDomain);
    assert_eq!(reply.answers, 0);
}

#[test]
fn servfail_is_distinguishable_from_nxdomain() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR | Rcode::ServFail.bits(), [1, 0, 0, 0]);
        m.question("example.com", Type::A);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.rcode, Rcode::ServFail);
    assert_ne!(reply.rcode, Rcode::NxDomain);
}

#[test]
fn the_truncated_bit_is_reported_alongside_the_partial_answer() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR | Flags::TC, [1, 1, 0, 0]);
        m.question("example.com", Type::A);
        m.record("example.com", Type::A, 60, &[1, 2, 3, 4]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(reply.is_truncated());
    assert!(reply.flags.truncated());
    assert_eq!(reply.rcode, Rcode::NoError);
    // The record that did arrive is real; the set is not complete.
    assert_eq!(reply.answers, 1);
}

#[test]
fn a_query_is_not_a_response() {
    let mut out = [0u8; QUERY_MAX];
    let n = encode_query("example.com", Type::A, 1, &mut out).unwrap();
    let mut t = Tables::big();
    let err = decode(&out[..n], &mut t.storage()).unwrap_err();
    assert_eq!(kind_of(err), ErrorKind::NotAResponse);
}

#[test]
fn the_authoritative_and_recursion_available_bits_are_reported() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR | Flags::AA | Flags::RA, [1, 0, 0, 0]);
        m.question("example.com", Type::A);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(reply.flags.authoritative());
    assert!(reply.flags.recursion_available());
    assert_eq!(reply.flags.opcode(), 0);
}

// EDNS0.

#[test]
fn an_opt_record_is_reported_as_edns_and_not_as_a_record() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        m.u8(0);
        m.u16(Type::OPT.0);
        m.u16(4096);
        m.u32(0x0000_8000);
        m.u16(0);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let opt = reply.edns.unwrap();
    assert_eq!(opt.udp_payload, 4096);
    assert_eq!(opt.version, 0);
    assert_eq!(opt.flags, 0x8000);
    assert_eq!(reply.additionals, 0);
    assert!(reply.additionals(&t.records).is_empty());
}

#[test]
fn an_opt_records_extended_rcode_extends_the_header_rcode() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        m.u8(0);
        m.u16(Type::OPT.0);
        m.u16(1232);
        // Extended rcode 1 over a header rcode of 0 is BADVERS, 16.
        m.u32(0x0100_0000);
        m.u16(0);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.edns.unwrap().ext_rcode, 1);
    assert_eq!(reply.rcode, Rcode::BadVers);
}

#[test]
fn an_opt_record_carrying_options_keeps_them_as_raw_bytes() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        m.u8(0);
        m.u16(Type::OPT.0);
        m.u16(1232);
        m.u32(0);
        m.u16(8);
        // Option code 10, four bytes of cookie.
        m.u16(10);
        m.u16(4);
        m.u32(0xDEAD_BEEF);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let opt = reply.edns.unwrap();
    assert_eq!(opt.options.len, 8);
    assert_eq!(&opt.options.bytes(&msg)[..4], &[0, 10, 0, 4]);
}

#[test]
fn an_opt_option_running_past_the_rdata_is_refused() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        m.u8(0);
        m.u16(Type::OPT.0);
        m.u16(1232);
        m.u32(0);
        m.u16(4);
        // An option claiming 99 bytes inside a 4 byte rdata.
        m.u16(10);
        m.u16(99);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadOpt
    );
}

#[test]
fn an_opt_record_on_a_name_other_than_the_root_is_refused() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 1]);
        m.question("example.com", Type::A);
        m.record("example.com", Type::OPT, 0, &[]);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadOpt
    );
}

#[test]
fn an_opt_record_in_the_answer_section_is_refused() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 1, 0, 0]);
        m.question("example.com", Type::A);
        m.u8(0);
        m.head(Type::OPT, 0, 0);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadOpt
    );
}

#[test]
fn two_opt_records_are_refused() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 2]);
        m.question("example.com", Type::A);
        for _ in 0..2 {
            m.u8(0);
            m.head(Type::OPT, 0, 0);
        }
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadOpt
    );
}

// Compression pointers.

/// Read one name out of `src` with a fresh arena, for the pointer tests that
/// need no surrounding message.
fn name_at(src: &[u8], at: usize) -> Result<(Vec<u8>, usize), DnsError> {
    let mut buf = std::vec![0u8; 512];
    let mut arena = Arena::new(&mut buf);
    let (name, end) = read_name(src, at, &mut arena)?;
    Ok((name.bytes(&buf).to_vec(), end))
}

/// Twelve bytes of filler so offsets in these tests look like real ones, since
/// a pointer into the header is refused.
fn header_filler() -> Vec<u8> {
    std::vec![0u8; HEADER]
}

#[test]
fn a_compression_pointer_resolves_to_the_name_it_points_at() {
    let mut src = header_filler();
    // "example.com" at 12, then "www" plus a pointer back to it.
    src.extend_from_slice(b"\x07example\x03com\x00");
    let www = src.len();
    src.extend_from_slice(b"\x03www\xc0\x0c");
    let (name, end) = name_at(&src, www).unwrap();
    assert_eq!(name, b"www.example.com");
    assert_eq!(end, src.len());
}

#[test]
fn a_pointer_straight_to_another_name_resolves_without_a_prefix() {
    let mut src = header_filler();
    src.extend_from_slice(b"\x07example\x03com\x00");
    let at = src.len();
    src.extend_from_slice(b"\xc0\x0c");
    let (name, end) = name_at(&src, at).unwrap();
    assert_eq!(name, b"example.com");
    // A name that is only a pointer occupies exactly two bytes.
    assert_eq!(end, at + 2);
}

#[test]
fn a_pointer_to_itself_is_an_error_rather_than_a_hang() {
    let mut src = header_filler();
    let at = src.len();
    src.extend_from_slice(&[0xC0, at as u8]);
    assert_eq!(
        kind_of(name_at(&src, at).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_two_pointer_cycle_is_an_error_rather_than_a_hang() {
    let mut src = header_filler();
    let first = src.len();
    let second = first + 2;
    // 12 points forward at 14, 14 points back at 12.
    src.extend_from_slice(&[0xC0, second as u8, 0xC0, first as u8]);
    assert_eq!(
        kind_of(name_at(&src, second).unwrap_err()),
        ErrorKind::BadPointer
    );
    assert_eq!(
        kind_of(name_at(&src, first).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_forward_pointer_is_an_error() {
    let mut src = header_filler();
    let at = src.len();
    // Points at the name two bytes further on, which is forwards.
    src.extend_from_slice(&[0xC0, (at + 2) as u8]);
    src.extend_from_slice(b"\x07example\x03com\x00");
    assert_eq!(
        kind_of(name_at(&src, at).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_pointer_that_is_backwards_from_itself_but_forwards_from_the_chain_is_an_error() {
    // The trap the naive rule falls into. Every pointer here points below its
    // own offset, yet following them loops forever: the labels at 12 run up to
    // the pointer at 22, which lands back on 12.
    let mut src = header_filler();
    src.extend_from_slice(b"\x08aaaaaaaa");
    let loop_ptr = src.len();
    assert_eq!(loop_ptr, 21);
    src.extend_from_slice(&[0xC0, HEADER as u8]);
    let entry = src.len();
    src.extend_from_slice(&[0xC0, loop_ptr as u8]);
    assert_eq!(
        kind_of(name_at(&src, entry).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_pointer_into_the_header_is_an_error() {
    let mut src = header_filler();
    let at = src.len();
    src.extend_from_slice(&[0xC0, 0x02]);
    assert_eq!(
        kind_of(name_at(&src, at).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_pointer_past_the_end_of_the_message_cannot_get_past_the_backwards_rule() {
    // Anything beyond the message is also beyond the pointer, so the ordering
    // rule refuses it before a bounds check ever has to.
    let mut src = header_filler();
    let at = src.len();
    src.extend_from_slice(&[0xC0, 0xFF]);
    assert_eq!(
        kind_of(name_at(&src, at).unwrap_err()),
        ErrorKind::BadPointer
    );
}

/// A ladder of pointers, each one two bytes below the last, so following it
/// from the top takes `steps` jumps. Every jump points strictly backwards, so
/// only the jump cap can stop it.
fn pointer_ladder(steps: usize) -> (Vec<u8>, usize) {
    let mut src = header_filler();
    // The foot of the ladder is a root byte, so a short chain terminates.
    src.push(0);
    let mut target = HEADER;
    for _ in 0..steps {
        let here = src.len();
        src.extend_from_slice(&[0xC0, target as u8]);
        target = here;
    }
    let top = src.len() - 2;
    (src, top)
}

#[test]
fn a_pointer_chain_within_the_jump_cap_resolves() {
    let (src, top) = pointer_ladder(MAX_JUMPS as usize);
    let (name, end) = name_at(&src, top).unwrap();
    assert!(name.is_empty());
    assert_eq!(end, top + 2);
}

#[test]
fn a_pointer_chain_longer_than_the_jump_cap_is_an_error() {
    let (src, top) = pointer_ladder(MAX_JUMPS as usize + 1);
    assert_eq!(
        kind_of(name_at(&src, top).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn a_reserved_label_type_is_an_error() {
    for high in [0x40u8, 0x80u8] {
        let mut src = header_filler();
        let at = src.len();
        src.extend_from_slice(&[high | 3, b'a', b'b', b'c', 0]);
        assert_eq!(kind_of(name_at(&src, at).unwrap_err()), ErrorKind::BadName);
    }
}

#[test]
fn a_name_that_never_terminates_is_truncated_rather_than_read_past_the_end() {
    let mut src = header_filler();
    let at = src.len();
    src.extend_from_slice(b"\x03www\x07example");
    assert_eq!(
        kind_of(name_at(&src, at).unwrap_err()),
        ErrorKind::Truncated
    );
}

#[test]
fn a_name_assembled_past_the_length_limit_is_refused() {
    // Each hop adds a 63 byte label and jumps back one hop, so five of them
    // pass 255 bytes without any single label or pointer being illegal.
    let mut src = header_filler();
    src.push(0);
    let mut target = HEADER;
    for _ in 0..5 {
        let here = src.len();
        src.push(63);
        src.extend_from_slice(&[b'a'; 63]);
        src.extend_from_slice(&[0xC0, target as u8]);
        target = here;
    }
    let top = src.len() - 65;
    assert_eq!(kind_of(name_at(&src, top).unwrap_err()), ErrorKind::BadName);
}

#[test]
fn a_compressed_answer_name_decodes_through_the_whole_message() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("www.example.com", Type::A);
        // The answer's owner name is a pointer back to the question's.
        m.ptr(HEADER as u16);
        m.head(Type::A, 60, 4);
        m.raw(&[1, 2, 3, 4]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let answers = reply.answers(&t.records);
    assert_eq!(answers[0].name.bytes(&t.names), b"www.example.com");
    assert_eq!(answers[0].data, Data::A([1, 2, 3, 4]));
}

/// What a real server sends: the CNAME's target shares a suffix with the
/// question, so its rdata is a label and a pointer, and the rdlength counts
/// the pointer's two bytes and not the bytes it resolves to.
#[test]
fn a_cname_whose_target_is_compressed_resolves_and_still_fills_its_rdata() {
    let msg = {
        let mut m = Msg::reply(2);
        m.question("www.example.com", Type::A);
        // "example.com" starts four bytes into the question name.
        let suffix = HEADER as u16 + 4;
        m.ptr(HEADER as u16);
        m.head(Type::CNAME, 60, 8);
        m.u8(5);
        m.raw(b"cdn42");
        m.ptr(suffix);
        m.record("cdn42.example.com", Type::A, 60, &[198, 51, 100, 7]);
        m.bytes
    };
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let answers = reply.answers(&t.records);
    let Data::Name(target) = answers[0].data else {
        panic!("expected a name");
    };
    assert_eq!(target.bytes(&t.names), b"cdn42.example.com");
    assert_eq!(answers[1].name.bytes(&t.names), b"cdn42.example.com");
}

#[test]
fn a_self_pointing_answer_name_is_an_error_rather_than_a_hang() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        let here = m.at();
        m.ptr(here);
        m.head(Type::A, 60, 4);
        m.raw(&[1, 2, 3, 4]);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadPointer
    );
}

// Lengths.

#[test]
fn rdlength_running_past_the_message_is_an_error() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        m.name("example.com");
        m.head(Type::A, 60, 64);
        m.raw(&[1, 2, 3, 4]);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::Truncated
    );
}

#[test]
fn an_a_record_whose_rdlength_is_not_four_is_an_error() {
    for len in [0u16, 3, 5, 16] {
        let msg = {
            let mut m = Msg::reply(1);
            m.question("example.com", Type::A);
            m.name("example.com");
            m.head(Type::A, 60, len);
            m.raw(&std::vec![7u8; len as usize]);
            m.bytes
        };
        let mut t = Tables::big();
        assert_eq!(
            kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
            ErrorKind::BadRdLength,
            "rdlength {len}"
        );
    }
}

#[test]
fn an_aaaa_record_whose_rdlength_is_not_sixteen_is_an_error() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::AAAA);
        m.record("example.com", Type::AAAA, 60, &[1, 2, 3, 4]);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadRdLength
    );
}

#[test]
fn a_cname_whose_name_does_not_fill_its_rdata_is_an_error() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("www.example.com", Type::A);
        m.name("www.example.com");
        // The name is 10 bytes, the record claims 16 and pads the rest.
        m.head(Type::CNAME, 60, 16);
        m.name("host.net");
        m.raw(&[0; 6]);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadRdLength
    );
}

#[test]
fn a_record_cut_off_before_its_rdlength_is_truncated() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        m.name("example.com");
        m.u16(Type::A.0);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::Truncated
    );
}

#[test]
fn a_header_shorter_than_twelve_bytes_is_truncated() {
    for n in 0..HEADER {
        let mut t = Tables::big();
        let src = std::vec![0x80u8; n];
        assert_eq!(
            kind_of(decode(&src, &mut t.storage()).unwrap_err()),
            ErrorKind::Truncated,
            "at length {n}"
        );
    }
}

/// The whole point: no prefix of a valid reply may panic, hang, or decode as
/// if it were complete.
#[test]
fn truncating_a_valid_response_at_every_offset_is_an_error_not_a_panic() {
    let full = {
        let mut m = Msg::new(0x4242, Flags::QR | Flags::RD | Flags::RA, [1, 2, 1, 1]);
        m.question("www.example.com", Type::A);
        let mut target = Msg { bytes: Vec::new() };
        target.name("host.example.net");
        m.record("www.example.com", Type::CNAME, 60, &target.bytes);
        m.ptr(HEADER as u16);
        m.head(Type::A, 30, 4);
        m.raw(&[203, 0, 113, 9]);
        let mut ns = Msg { bytes: Vec::new() };
        ns.name("ns.example.com");
        m.record("example.com", Type::NS, 90, &ns.bytes);
        m.u8(0);
        m.head(Type::OPT, 0, 0);
        m.bytes
    };
    let mut t = Tables::big();
    assert!(decode(&full, &mut t.storage()).is_ok());
    for n in 0..full.len() {
        let mut t = Tables::big();
        assert!(
            decode(&full[..n], &mut t.storage()).is_err(),
            "prefix of {n} bytes decoded as if it were whole"
        );
    }
}

// Caller supplied capacity.

#[test]
fn more_records_than_the_table_holds_is_refused() {
    let msg = {
        let mut m = Msg::reply(4);
        m.question("example.com", Type::A);
        for i in 0..4u8 {
            m.record("example.com", Type::A, 60, &[i, i, i, i]);
        }
        m.bytes
    };
    let mut t = Tables::new(3, 4096);
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::TooManyRecords
    );
    // Three fit, so the refusal is the fourth and not the first.
    let mut t = Tables::new(4, 4096);
    assert_eq!(decode(&msg, &mut t.storage()).unwrap().answers, 4);
}

#[test]
fn a_name_arena_too_small_for_the_reply_is_refused() {
    let msg = {
        let mut m = Msg::reply(1);
        m.question("example.com", Type::A);
        m.record("example.com", Type::A, 60, &[1, 2, 3, 4]);
        m.bytes
    };
    // Room for one "example.com" but not for the answer's copy of it.
    let mut t = Tables::new(4, 16);
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::NamesFull
    );
    let mut t = Tables::new(4, 22);
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.names_len, 22);
}

#[test]
fn a_reply_with_no_records_needs_no_record_table() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR | Rcode::NxDomain.bits(), [1, 0, 0, 0]);
        m.question("example.com", Type::A);
        m.bytes
    };
    let mut t = Tables::new(0, 64);
    assert_eq!(
        decode(&msg, &mut t.storage()).unwrap().rcode,
        Rcode::NxDomain
    );
}

// Matching a reply to its query.

fn reply_for(name: &str, kind: Type, id: u16) -> Vec<u8> {
    let mut m = Msg::new(id, Flags::QR, [1, 1, 0, 0]);
    m.question(name, kind);
    m.record(name, Type::A, 60, &[1, 2, 3, 4]);
    m.bytes
}

#[test]
fn a_reply_that_echoes_the_question_matches_its_query() {
    let q = Query::new("example.com", Type::A, 0x9999);
    let msg = reply_for("example.com", Type::A, 0x9999);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(matches_query(&reply, &t.names, &q));
}

#[test]
fn a_reply_whose_question_names_something_else_does_not_match() {
    let q = Query::new("example.com", Type::A, 0x9999);
    let msg = reply_for("attacker.example", Type::A, 0x9999);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(!matches_query(&reply, &t.names, &q));
}

#[test]
fn a_reply_whose_id_differs_does_not_match() {
    let q = Query::new("example.com", Type::A, 0x9999);
    let msg = reply_for("example.com", Type::A, 0x9998);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.id, 0x9998);
    assert!(!matches_query(&reply, &t.names, &q));
}

#[test]
fn a_reply_answering_a_different_type_does_not_match() {
    let q = Query::new("example.com", Type::A, 1);
    let msg = reply_for("example.com", Type::AAAA, 1);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(!matches_query(&reply, &t.names, &q));
}

#[test]
fn a_reply_with_no_question_at_all_does_not_match() {
    let q = Query::new("example.com", Type::A, 1);
    let msg = Msg::new(1, Flags::QR | Rcode::FormErr.bits(), [0, 0, 0, 0]).bytes;
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.rcode, Rcode::FormErr);
    assert!(reply.question.is_none());
    assert!(!matches_query(&reply, &t.names, &q));
}

#[test]
fn a_question_echoed_in_a_different_case_still_matches() {
    let q = Query::new("Example.COM", Type::A, 1);
    let msg = reply_for("eXAMPLE.com", Type::A, 1);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert!(matches_query(&reply, &t.names, &q));
}

#[test]
fn the_question_name_is_kept_byte_for_byte_for_case_randomised_queries() {
    let msg = reply_for("eXaMpLe.CoM", Type::A, 1);
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    let question = reply.question.unwrap();
    assert_eq!(question.name.bytes(&t.names), b"eXaMpLe.CoM");
    assert_eq!(question.raw.bytes(&msg), b"\x07eXaMpLe\x03CoM\x00");
}

#[test]
fn two_questions_in_one_reply_are_refused() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [2, 0, 0, 0]);
        m.question("example.com", Type::A);
        m.question("example.org", Type::A);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadQuestion
    );
}

#[test]
fn a_compressed_question_name_is_refused_since_nothing_precedes_it() {
    let msg = {
        let mut m = Msg::new(1, Flags::QR, [1, 0, 0, 0]);
        m.ptr(2);
        m.u16(Type::A.0);
        m.u16(Class::IN.0);
        m.bytes
    };
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&msg, &mut t.storage()).unwrap_err()),
        ErrorKind::BadPointer
    );
}

#[test]
fn name_comparison_ignores_ascii_case_only() {
    assert!(name_eq(b"Example.COM", b"example.com"));
    assert!(!name_eq(b"example.com", b"example.net"));
    assert!(!name_eq(b"example.com", b"example.com."));
}

#[test]
fn trailing_bytes_after_the_last_record_are_ignored() {
    let mut msg = reply_for("example.com", Type::A, 1);
    msg.extend_from_slice(&[0xFF; 8]);
    let mut t = Tables::big();
    assert_eq!(decode(&msg, &mut t.storage()).unwrap().answers, 1);
}

#[test]
fn a_message_longer_than_the_format_can_address_is_refused() {
    let src = std::vec![0u8; 65536];
    let mut t = Tables::big();
    assert_eq!(
        kind_of(decode(&src, &mut t.storage()).unwrap_err()),
        ErrorKind::TooLong
    );
}

#[test]
fn a_root_question_round_trips() {
    let mut out = [0u8; QUERY_MAX];
    let q = Query {
        udp_payload: None,
        ..Query::new(".", Type::NS, 5)
    };
    let n = encode(&q, &mut out).unwrap();
    assert_eq!(n, HEADER + 1 + 4);
    // Flip QR so the same bytes decode as a reply to themselves.
    let mut msg = out[..n].to_vec();
    msg[2] |= 0x80;
    let mut t = Tables::big();
    let reply = decode(&msg, &mut t.storage()).unwrap();
    assert_eq!(reply.question.unwrap().name, Name::EMPTY);
    assert!(reply.question.unwrap().name.is_root());
}
