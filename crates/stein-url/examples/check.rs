//! Prints how the parser handles security-relevant URLs.
fn main() {
    let cases: &[&str] = &[
        "https://evil.com@good.com/p",
        "https://a@b@good.com/p",
        "https://user:pass@good.com/",
        "https://[2001:db8::1]:8443/x",
        "https://good.com:65536/",
        "https://good.com\r\nX-Evil: 1/",
    ];
    for c in cases {
        match stein_url::parse(c.as_bytes()) {
            Ok(u) => println!(
                "{c:<42} host={:<12} port={} userinfo={}",
                String::from_utf8_lossy(u.host()),
                u.port(),
                u.has_userinfo()
            ),
            Err(e) => println!("{c:<42} REFUSED {e}"),
        }
    }
    println!("--- resolution, the redirect path ---");
    let base = b"https://good.com/a/b/c?q";
    for r in [
        &b"../../../../etc/passwd"[..],
        b"//evil.com/x",
        b"/abs",
        b"rel",
        b"?only",
        b"",
    ] {
        let mut out = [0u8; 256];
        match stein_url::resolve(base, r, &mut out) {
            Ok(n) => println!(
                "{:<26} -> {}",
                String::from_utf8_lossy(r),
                String::from_utf8_lossy(&out[..n])
            ),
            Err(e) => println!("{:<26} -> REFUSED {e}", String::from_utf8_lossy(r)),
        }
    }
}
