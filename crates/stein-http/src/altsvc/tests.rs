#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

const A: &str = "a.test";

#[test]
fn an_h3_alternative_on_the_origin_is_taken() {
    assert_eq!(
        parse(b"h3=\":443\"; ma=86400", A),
        AltSvc::H3 {
            port: 443,
            ma: 86400
        }
    );
    assert_eq!(
        parse(b"h3=\":8443\"", A),
        AltSvc::H3 {
            port: 8443,
            ma: DEFAULT_MA
        }
    );
    // googlevideo's, drafts after the RFC.
    let google = b"h3=\":443\"; ma=2592000,h3-29=\":443\"; ma=2592000";
    assert_eq!(
        parse(google, "rr1.test"),
        AltSvc::H3 {
            port: 443,
            ma: 2_592_000
        }
    );
    let drafts_first = b"h3-29=\":443\"; ma=60, h3=\":444\"; ma=30";
    assert_eq!(parse(drafts_first, A), AltSvc::H3 { port: 444, ma: 30 });
}

#[test]
fn hosts_ports_and_quoting() {
    let default = |port| AltSvc::H3 {
        port,
        ma: DEFAULT_MA,
    };
    // Same host in another case, and quoted with escapes.
    assert_eq!(parse(b"h3=\"A.Test:443\"", A), default(443));
    assert_eq!(parse(b"h3=\"a\\.test:443\"", A), default(443));
    // Another host is not followed, an IPv6 literal of the origin is.
    assert_eq!(parse(b"h3=\"b.test:443\"", A), AltSvc::None);
    assert_eq!(parse(b"h3=\"[::1]:443\"", "::1"), default(443));
    assert_eq!(
        parse(b"h3=\"b.test:443\", h3=\":443\"; ma=5", A),
        AltSvc::H3 { port: 443, ma: 5 }
    );
    // Quoted parameter, an unknown one, a persist flag.
    assert_eq!(
        parse(b"h3=\":443\"; persist=1; ma=\"60\"; v=\"x,y\"", A),
        AltSvc::H3 { port: 443, ma: 60 }
    );
    // A comma inside a quoted parameter starts no member.
    assert_eq!(
        parse(b"h2=\":443\"; v=\"h3=\\\":1\\\", z\", h3=\":9\"", A),
        default(9)
    );
}

#[test]
fn clear_and_nothing() {
    assert_eq!(parse(b"clear", A), AltSvc::Clear);
    assert_eq!(parse(b"  clear ", A), AltSvc::Clear);
    // Case-sensitive (section 3).
    assert_eq!(parse(b"CLEAR", A), AltSvc::None);
    assert_eq!(parse(b"h2=\":443\"", A), AltSvc::None);
    assert_eq!(parse(b"", A), AltSvc::None);
    let bad: [&[u8]; 8] = [
        b"h3",
        b"h3=",
        b"h3=\":",
        b"h3=\":0\"",
        b"h3=\":99999\"",
        b"h3=:443",
        b"=\":443\"",
        b"h3=\"\\",
    ];
    for b in bad {
        assert_eq!(
            parse(b, A),
            AltSvc::None,
            "{:?}",
            String::from_utf8_lossy(b)
        );
    }
    // An unreadable `ma` keeps the default, one too large saturates.
    assert_eq!(
        parse(b"h3=\":443\"; ma=x1", A),
        AltSvc::H3 {
            port: 443,
            ma: DEFAULT_MA
        }
    );
    assert_eq!(
        parse(b"h3=\":443\"; ma=99999999999", A),
        AltSvc::H3 {
            port: 443,
            ma: u32::MAX
        }
    );
}

#[test]
fn any_bytes_parse_without_a_panic() {
    let mut s = 1u64;
    for _ in 0..4000 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let n = (s >> 58) as usize;
        let alphabet = b"h3=\":;,\\ ma0129abc\t";
        let v: Vec<u8> = (0..n)
            .map(|i| alphabet[((s >> (i % 50)) as usize) % alphabet.len()])
            .collect();
        let _ = parse(&v, A);
    }
}
