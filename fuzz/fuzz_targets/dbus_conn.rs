//! A stein-dbus connection fed whatever a bus might send: auth lines, the Hello reply and
//! messages, in chunks of any size, with polls and transmits between them. Never a panic,
//! an error leaves the connection closed, and no message comes out before auth or after it.
//!
//! Input: ops, an op byte and its fields. 0 a chunk of up to 255 raw bytes, 1 the server's
//! OK line, 2 polls until nothing more, 3 a transmit, 4 a restart.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stein_dbus::{Conn, State, Storage};

const OK: &[u8] = b"OK 0123456789abcdef0123456789abcdef\r\n";

fuzz_target!(|data: &[u8]| {
    let (mut rx, mut tx) = (vec![0u8; 4096], vec![0u8; 4096]);
    let Ok(mut c) = Conn::new(
        Storage {
            rx: &mut rx,
            tx: &mut tx,
        },
        1000,
    ) else {
        return;
    };
    let mut out = [0u8; 4096];
    let mut at = 0;
    while let Some(&op) = data.get(at) {
        at += 1;
        match op % 5 {
            0 => {
                let n = usize::from(data.get(at).copied().unwrap_or(0));
                let chunk = data.get(at + 1..).map_or(&[][..], |r| &r[..n.min(r.len())]);
                at += 1 + chunk.len();
                let took = c.received(chunk);
                assert!(took <= chunk.len());
            }
            1 => {
                c.received(OK);
            }
            2 => poll(&mut c),
            3 => {
                let before = c.pending();
                let n = c.transmit(&mut out);
                assert!(n <= out.len() && n <= before);
                assert_eq!(c.pending(), before - n, "transmit counts as sent");
            }
            _ => {
                assert!(c.restart(1000).is_ok(), "a restart starts over");
                assert_eq!(c.state(), State::Auth);
            }
        }
    }
});

/// Polls until nothing is complete, or the connection fails and closes.
fn poll(c: &mut Conn<'_>) {
    // a bounded number of messages fit the receive buffer
    for _ in 0..4096 {
        match c.poll() {
            Ok(Some(m)) => {
                assert_ne!(m.serial(), 0);
                // what the bus sends ahead of the Hello reply comes out during Hello
                assert!(
                    matches!(c.state(), State::Hello | State::Ready),
                    "a message before auth or after close"
                );
            }
            Ok(None) => return,
            Err(_) => {
                assert_eq!(c.state(), State::Closed, "an error closes the connection");
                return;
            }
        }
    }
}
