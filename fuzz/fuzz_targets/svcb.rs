//! HTTPS record RDATA: refused or read with its TargetName inside the RDATA.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_dns::svcb::{dotted, parse};

fuzz_target!(|rdata: &[u8]| {
    let Ok(s) = parse(rdata) else {
        return;
    };
    let target = s.target(rdata);
    assert_eq!(target.len(), usize::from(s.target.1));
    assert_eq!(target.last(), Some(&0), "a name ends in the root label");
    assert_eq!(s.target_is_root(), target == [0]);
    let mut name = [0u8; 253];
    match dotted(target, &mut name) {
        Some(n) => assert!(!s.target_is_root() && n > 0),
        None => assert!(s.target_is_root(), "a wire name of 255 bytes is 253 dotted"),
    }
    assert!(!s.no_default_alpn || s.alpn != 0);
});
