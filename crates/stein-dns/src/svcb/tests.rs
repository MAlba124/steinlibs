#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::string::{String, ToString};
use std::vec::Vec;

/// SplitMix64 seeded damage: noise one time in five, else flips, cuts, inserts.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn damage(&mut self, seed: &[u8], max: usize) -> Vec<u8> {
        if self.below(5) == 0 {
            let n = self.below(max as u64 + 1) as usize;
            return (0..n).map(|_| self.next() as u8).collect();
        }
        let mut data = seed.to_vec();
        for _ in 0..1 + self.below(4) {
            let at = self.below(data.len() as u64 + 1) as usize;
            match self.below(4) {
                0 if at < data.len() => data[at] ^= 1 << self.below(8),
                1 => data.truncate(at),
                2 => data.insert(at, self.next() as u8),
                _ if at < data.len() => data[at] = self.next() as u8,
                _ => data.push(self.next() as u8),
            }
        }
        data
    }
}

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

// RDATA as `dig -t https <name> +unknownformat` printed it on 2026-10-17.
/// www.google.com, `1 . alpn="h2,h3"`.
const GOOGLE: &str = "00010000010006026832026833";
/// cloudflare.com, `1 . alpn="h3,h2" ipv4hint=104.16.132.229,104.16.133.229
/// ipv6hint=2606:4700::6810:84e5,2606:4700::6810:85e5`.
const CLOUDFLARE: &str = "0001000001000602683302683200040008681084E5681085E5000600 20260647000000000000000000681084E52606470000000000000000 00681085E5";
/// rr1.sn-ab5l6nrz.googlevideo.com, `1 . alpn="h3"`.
const GOOGLEVIDEO: &str = "00010000010003026833";
/// star-mini.c10r.facebook.com, `2 star-mini.fallback.c10r.facebook.com. alpn="h2,h3"`.
const FACEBOOK_FALLBACK: &str = "000209737461722D6D696E690866616C6C6261636B04633130720866 616365626F6F6B03636F6D0000010006026832026833";
/// netmeister.org, AliasMode `0 www.netmeister.org.`.
const NETMEISTER: &str = "0000037777770A6E65746D656973746572036F726700";
/// cloudflare-ech.com, with an `ech` param.
const ECH: &str = "000100000100060268330268320004000868120A7668120B76000500 470045FE0D00418D00200020EFC45EB45F483CCD800E674209F10798 37708B0EA9E6D245E6C8839B371F401B0004000100010012636C6F75 64666C6172652D6563682E636F6D0000000600202606470000000000 0000000068120A7626064700000000000000000068120B76";

#[test]
fn captured_records_read() {
    let g = parse(&hex(GOOGLE)).unwrap();
    assert_eq!(
        (g.priority, g.target_is_root(), g.alpn),
        (1, true, ALPN_H2 | ALPN_H3)
    );
    let c = parse(&hex(CLOUDFLARE)).unwrap();
    assert_eq!(
        (c.alpn, c.ipv4hints, c.ipv6hints, c.port),
        (ALPN_H2 | ALPN_H3, 2, 2, None)
    );
    assert_eq!(parse(&hex(GOOGLEVIDEO)).unwrap().alpn, ALPN_H3);
    let f = parse(&hex(FACEBOOK_FALLBACK)).unwrap();
    assert_eq!((f.priority, f.target_is_root(), f.target.1), (2, false, 38));
    let rdata = hex(NETMEISTER);
    let n = parse(&rdata).unwrap();
    assert_eq!((n.priority, n.alpn), (0, 0));
    let mut name = [0; 253];
    let len = dotted(n.target(&rdata), &mut name).unwrap();
    assert_eq!(&name[..len as usize], b"www.netmeister.org");
    let e = parse(&hex(ECH)).unwrap();
    assert!(e.ech && e.alpn == ALPN_H2 | ALPN_H3);
}

#[test]
fn records_after_rfc_9460_appendix_d_read() {
    // D.1, AliasMode `example.com. HTTPS 0 foo.example.com.`.
    let alias = hex("0000 03666f6f076578616d706c6503636f6d00");
    assert_eq!(parse(&alias).map(|s| (s.priority, s.target.1)), Ok((0, 17)));
    // As D.2, `1 . port=53`.
    let port = hex("0001 00 0003 0002 0035");
    assert_eq!(parse(&port).map(|s| s.port), Ok(Some(53)));
    // As D.2, unknown `key667=hello` is skipped.
    let unknown = hex("0001 03666f6f076578616d706c6503636f6d00 029b 0005 68656c6c6f");
    assert_eq!(parse(&unknown).map(|s| s.alpn), Ok(0));
    // As D.2, `alpn=h2,h3-19 mandatory=ipv4hint,alpn ipv4hint=192.0.2.1`.
    let mandatory = hex(
        "0010 03666f6f076578616d706c6503636f6d00 0000 0004 00010004 0001 0009 02683205 6833 2d3139 0004 0004 c0000201",
    );
    let s = parse(&mandatory).unwrap();
    assert_eq!((s.alpn, s.ipv4hints), (ALPN_H2 | ALPN_OTHER, 1));
}

#[test]
fn malformed_records_are_refused() {
    let p = |s: &str| parse(&hex(s));
    // As RFC 9460 D.3, keys out of order, unknown mandatory key, a key twice.
    assert_eq!(
        p("0001 00 0003 0002 0035 0001 0003 026832"),
        Err(SvcbError::KeyOrder { key: 1 })
    );
    assert_eq!(
        p("0001 00 0000 0002 029b"),
        Err(SvcbError::Mandatory { key: 667 })
    );
    assert_eq!(
        p("0001 00 0001 0003 026832 0001 0003 026833"),
        Err(SvcbError::KeyOrder { key: 1 })
    );
    // `mandatory` naming itself.
    assert_eq!(
        p("0001 00 0000 0002 0000"),
        Err(SvcbError::BadValue { key: 0 })
    );
    // `no-default-alpn` alone, a port of the wrong length.
    assert_eq!(p("0001 00 0002 0000"), Err(SvcbError::BadValue { key: 2 }));
    assert_eq!(
        p("0001 00 0003 0001 35"),
        Err(SvcbError::BadValue { key: 3 })
    );
    // Compressed TargetName, a cut one, a value past the end.
    assert_eq!(p("0001 c00c"), Err(SvcbError::BadTarget { at: 2 }));
    assert_eq!(p("0001 03666f"), Err(SvcbError::Truncated { at: 6 }));
    assert_eq!(
        p("0001 00 0001 0009 026833"),
        Err(SvcbError::Truncated { at: 7 })
    );
    assert_eq!(p("00"), Err(SvcbError::Truncated { at: 0 }));
    // Empty ALPN id.
    assert_eq!(
        p("0001 00 0001 0001 00"),
        Err(SvcbError::BadValue { key: 1 })
    );
    // Every error renders on one line.
    let e = SvcbError::Mandatory { key: 667 }.to_string();
    assert!(e.contains("667") && !e.contains('\n'));
}

#[test]
fn dotted_names_refuse_the_root_and_overruns() {
    let mut name = [0; 253];
    assert_eq!(dotted(&[0], &mut name), None);
    assert_eq!(dotted(&[3, b'a'], &mut name), None, "label past the wire");
    let mut long = Vec::new();
    for _ in 0..5 {
        long.push(63);
        long.extend_from_slice(&[b'x'; 63]);
    }
    long.push(0);
    assert_eq!(dotted(&long, &mut name), None, "over 253 bytes");
}

/// Records are untrusted. Damage is an error, never a panic.
#[test]
fn parsing_survives_corrupt_records() {
    let seeds = [
        hex(GOOGLE),
        hex(CLOUDFLARE),
        hex(FACEBOOK_FALLBACK),
        hex(NETMEISTER),
        hex(ECH),
    ];
    let mut rng = Rng(3);
    let mut name = [0; 253];
    for i in 0..4000 {
        let rdata = rng.damage(&seeds[i % seeds.len()], 300);
        if let Ok(s) = parse(&rdata) {
            let _ = dotted(s.target(&rdata), &mut name);
        }
    }
}
