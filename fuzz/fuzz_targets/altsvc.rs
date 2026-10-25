//! Alt-Svc for HTTP/3: any value reads to an answer that holds, and an `h3` on the
//! origin put first wins whatever follows it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_http::altsvc::{AltSvc, DEFAULT_MA, parse};

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let (host, value) = rest.split_at(usize::from(knob % 32).min(rest.len()));
    let Ok(host) = core::str::from_utf8(host) else {
        return;
    };
    match parse(value, host) {
        AltSvc::H3 { port, .. } => assert!(port > 0),
        AltSvc::Clear => {
            let from = value
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t'))
                .unwrap_or(value.len());
            assert_eq!(value[from..].trim_ascii_end(), b"clear");
        }
        AltSvc::None => {}
    }
    let mut first = b"h3=\":443\",".to_vec();
    first.extend_from_slice(value);
    assert_eq!(
        parse(&first, host),
        AltSvc::H3 {
            port: 443,
            ma: DEFAULT_MA
        }
    );
});
