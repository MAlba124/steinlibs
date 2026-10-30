//! stein-dbus signatures, bus addresses and auth lines from any bytes. The signature
//! checks agree with each other and with `Types`, an address decodes into the room it is
//! given, and an auth line is found where its `\r\n` is.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_dbus::sig::{self, Types};
use stein_dbus::{addr, auth};

fuzz_target!(|data: &[u8]| {
    signature(data);
    if let Ok(s) = core::str::from_utf8(data) {
        let mut out = [0u8; 108];
        if let Ok((_, n)) = addr::unix(s, &mut out) {
            assert!(n <= out.len());
        }
    }
    match auth::line(data) {
        Ok(Some(n)) => {
            assert!(n >= 2 && n <= data.len() && &data[n - 2..n] == b"\r\n");
            assert!(
                !data[..n - 2].windows(2).any(|w| w == b"\r\n"),
                "the first line"
            );
            let _ = auth::reply(&data[..n]);
        }
        Ok(None) => assert!(!data.windows(2).any(|w| w == b"\r\n")),
        Err(_) => {}
    }
});

fn signature(s: &[u8]) {
    let whole = sig::check(s);
    let single = sig::check_single(s);
    if single.is_ok() {
        assert!(whole.is_ok(), "a single type is a signature");
        assert_eq!(sig::first_len(s), Ok(s.len()));
    }
    if whole.is_ok() {
        let mut total = 0;
        for t in Types::new(s) {
            assert!(t.is_ok(), "a checked signature splits into types");
            let Ok(t) = t else {
                return;
            };
            assert!(!t.is_empty());
            assert!(sig::check_single(t).is_ok(), "each type is single");
            total += t.len();
        }
        assert_eq!(total, s.len(), "the types cover the signature");
    }
    if let Ok(n) = sig::first_len(s) {
        assert!(n >= 1 && n <= s.len());
    }
    if let Ok(n) = sig::element_len(s) {
        assert!(n >= 1 && n <= s.len());
    }
}
