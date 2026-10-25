#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn every_error_says_its_code_on_one_line() {
    let all = [
        (
            H3Error::FrameUnexpected {
                frame: 0,
                stream: 2,
            },
            0x105,
            "H3_FRAME_UNEXPECTED",
        ),
        (H3Error::Frame { frame: 4, len: 3 }, 0x106, "of 3 bytes"),
        (H3Error::StreamCreation { kind: 1 }, 0x103, "type 0x1"),
        (
            H3Error::ClosedCriticalStream { kind: 0 },
            0x104,
            "CLOSED_CRITICAL",
        ),
        (H3Error::MissingSettings { frame: 7 }, 0x10a, "0x7"),
        (H3Error::Settings { id: 6 }, 0x109, "setting 0x6"),
        (H3Error::Id { id: 8 }, 0x108, "GOAWAY id 8"),
        (
            H3Error::ExcessiveLoad {
                len: 70000,
                max: 65536,
            },
            0x107,
            "70000 bytes",
        ),
        (H3Error::Message { what: "no :status" }, 0x10e, "no :status"),
        (
            H3Error::Decompression {
                at: 3,
                what: "dynamic reference",
            },
            0x200,
            "byte 3",
        ),
        (H3Error::EncoderStream { byte: 0x3f }, 0x201, "0x3f"),
        (H3Error::RequestRejected, 0x10b, "REJECTED"),
        (H3Error::StreamReset { code: 0x10c }, 0x10c, "0x10c"),
        (H3Error::Connect { what: "no address" }, 0x102, "no address"),
        (H3Error::HandshakeTimedOut { ms: 3000 }, 0x102, "3000 ms"),
        (
            H3Error::Lost {
                loss: QuicLoss::Application { code: 0x100 },
            },
            0x100,
            "HTTP/3 error 0x100",
        ),
        (
            H3Error::Lost {
                loss: QuicLoss::Transport { code: 0xa },
            },
            0x100,
            "transport error 0xa",
        ),
        (
            H3Error::Lost {
                loss: QuicLoss::TimedOut,
            },
            0x100,
            "timed out",
        ),
    ];
    for (e, code, needle) in all {
        let s = e.to_string();
        assert_eq!(e.code(), code, "{s}");
        assert!(
            s.contains(needle) && !s.contains('\n') && !s.contains('\u{2014}'),
            "{s}"
        );
    }
    assert!(core::mem::size_of::<H3Error>() <= 32);
}
