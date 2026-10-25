//! HTTP/3 (RFC 9114) and QPACK (RFC 9204), client side.
//!
//! [`frame`] reads and writes frames and varints, [`qpack`] encodes a request's
//! field section and decodes a response's with the static table alone. With the
//! `h3` feature `Client` runs them over quinn-proto's QUIC. The HTTPS record
//! that says an origin speaks h3 is read by `stein_dns::svcb`.

pub mod frame;
pub mod qpack;

#[cfg(test)]
mod prng;

#[cfg(feature = "h3")]
mod client;
#[cfg(feature = "h3")]
mod conn;
#[cfg(feature = "h3")]
pub mod quic;

#[cfg(feature = "h3")]
pub use client::*;
#[cfg(feature = "h3")]
pub use {bytes, quinn_proto};

use core::fmt;

/// Error codes (RFC 9114 8.1, RFC 9204 6).
pub const H3_NO_ERROR: u64 = 0x100;
pub const H3_GENERAL_PROTOCOL_ERROR: u64 = 0x101;
pub const H3_INTERNAL_ERROR: u64 = 0x102;
pub const H3_STREAM_CREATION_ERROR: u64 = 0x103;
pub const H3_CLOSED_CRITICAL_STREAM: u64 = 0x104;
pub const H3_FRAME_UNEXPECTED: u64 = 0x105;
pub const H3_FRAME_ERROR: u64 = 0x106;
pub const H3_EXCESSIVE_LOAD: u64 = 0x107;
pub const H3_ID_ERROR: u64 = 0x108;
pub const H3_SETTINGS_ERROR: u64 = 0x109;
pub const H3_MISSING_SETTINGS: u64 = 0x10a;
pub const H3_REQUEST_REJECTED: u64 = 0x10b;
pub const H3_REQUEST_CANCELLED: u64 = 0x10c;
pub const H3_MESSAGE_ERROR: u64 = 0x10e;
pub const QPACK_DECOMPRESSION_FAILED: u64 = 0x200;
pub const QPACK_ENCODER_STREAM_ERROR: u64 = 0x201;

/// How a QUIC connection ended, as plain data so errors carry no quinn type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuicLoss {
    /// No packet for the idle timeout, or the handshake deadline passed.
    TimedOut,
    /// Stateless reset from the peer (RFC 9000 10.3).
    Reset,
    /// Peer closed with this transport error code (RFC 9000 20.1).
    Transport { code: u64 },
    /// Peer closed with this application error code (RFC 9114 8.1).
    Application { code: u64 },
    /// No QUIC version in common.
    VersionMismatch,
    /// This side closed it.
    Local,
    /// The endpoint ran out of connection ids.
    CidsExhausted,
}

/// What went wrong on a connection or one of its requests. [`H3Error::code`] is the RFC code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H3Error {
    /// Frame of type `frame` where none is allowed (0x105).
    FrameUnexpected { frame: u64, stream: u64 },
    /// Frame payload of type `frame` does not read (0x106).
    Frame { frame: u64, len: u64 },
    /// Second control stream, or a type a client may not get (0x103).
    StreamCreation { kind: u64 },
    /// Peer ended a critical stream (0x104).
    ClosedCriticalStream { kind: u64 },
    /// First control frame is not SETTINGS (0x10a).
    MissingSettings { frame: u64 },
    /// SETTINGS does not read, or names a setting twice (0x109).
    Settings { id: u64 },
    /// GOAWAY id above an earlier one, or a push stream (0x108).
    Id { id: u64 },
    /// Field section over the `max` this side takes (0x107).
    ExcessiveLoad { len: u64, max: u64 },
    /// Malformed response (0x10e).
    Message { what: &'static str },
    /// Field section does not decode at byte `at` (0x200).
    Decompression { at: usize, what: &'static str },
    /// Encoder stream instruction a zero capacity table forbids (0x201).
    EncoderStream { byte: u8 },
    /// Peer refused the request before acting on it (0x10b).
    RequestRejected,
    /// Peer reset the request stream with `code`.
    StreamReset { code: u64 },
    /// The connection could not be started.
    Connect { what: &'static str },
    /// The QUIC handshake did not finish in `ms` milliseconds.
    HandshakeTimedOut { ms: u64 },
    /// The QUIC connection ended.
    Lost { loss: QuicLoss },
}

impl H3Error {
    /// The code a connection or stream error closes with (RFC 9114 8.1).
    pub fn code(&self) -> u64 {
        match self {
            H3Error::FrameUnexpected { .. } => H3_FRAME_UNEXPECTED,
            H3Error::Frame { .. } => H3_FRAME_ERROR,
            H3Error::StreamCreation { .. } => H3_STREAM_CREATION_ERROR,
            H3Error::ClosedCriticalStream { .. } => H3_CLOSED_CRITICAL_STREAM,
            H3Error::MissingSettings { .. } => H3_MISSING_SETTINGS,
            H3Error::Settings { .. } => H3_SETTINGS_ERROR,
            H3Error::Id { .. } => H3_ID_ERROR,
            H3Error::ExcessiveLoad { .. } => H3_EXCESSIVE_LOAD,
            H3Error::Message { .. } => H3_MESSAGE_ERROR,
            H3Error::Decompression { .. } => QPACK_DECOMPRESSION_FAILED,
            H3Error::EncoderStream { .. } => QPACK_ENCODER_STREAM_ERROR,
            H3Error::RequestRejected => H3_REQUEST_REJECTED,
            H3Error::StreamReset { code } => *code,
            H3Error::Connect { .. } | H3Error::HandshakeTimedOut { .. } => H3_INTERNAL_ERROR,
            H3Error::Lost { .. } => H3_NO_ERROR,
        }
    }
}

impl fmt::Display for QuicLoss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuicLoss::TimedOut => f.write_str("timed out"),
            QuicLoss::Reset => f.write_str("reset by the peer"),
            QuicLoss::Transport { code } => {
                write!(f, "closed by the peer with transport error {code:#x}")
            }
            QuicLoss::Application { code } => {
                write!(f, "closed by the peer with HTTP/3 error {code:#x}")
            }
            QuicLoss::VersionMismatch => f.write_str("no QUIC version in common"),
            QuicLoss::Local => f.write_str("closed by this side"),
            QuicLoss::CidsExhausted => f.write_str("out of connection ids"),
        }
    }
}

impl fmt::Display for H3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            H3Error::FrameUnexpected { frame, stream } => write!(
                f,
                "frame type {frame:#x} unexpected on stream {stream} (H3_FRAME_UNEXPECTED)"
            ),
            H3Error::Frame { frame, len } => write!(
                f,
                "frame type {frame:#x} of {len} bytes does not read (H3_FRAME_ERROR)"
            ),
            H3Error::StreamCreation { kind } => write!(
                f,
                "peer opened a stream of type {kind:#x} it may not (H3_STREAM_CREATION_ERROR)"
            ),
            H3Error::ClosedCriticalStream { kind } => write!(
                f,
                "peer closed its stream of type {kind:#x} (H3_CLOSED_CRITICAL_STREAM)"
            ),
            H3Error::MissingSettings { frame } => write!(
                f,
                "control stream starts with frame type {frame:#x}, not SETTINGS (H3_MISSING_SETTINGS)"
            ),
            H3Error::Settings { id } => {
                write!(
                    f,
                    "SETTINGS malformed at setting {id:#x} (H3_SETTINGS_ERROR)"
                )
            }
            H3Error::Id { id } => write!(f, "GOAWAY id {id} above the one before (H3_ID_ERROR)"),
            H3Error::ExcessiveLoad { len, max } => write!(
                f,
                "field section of {len} bytes over the {max} allowed (H3_EXCESSIVE_LOAD)"
            ),
            H3Error::Message { what } => write!(f, "malformed response, {what} (H3_MESSAGE_ERROR)"),
            H3Error::Decompression { at, what } => write!(
                f,
                "field section does not decode at byte {at}, {what} (QPACK_DECOMPRESSION_FAILED)"
            ),
            H3Error::EncoderStream { byte } => write!(
                f,
                "encoder stream instruction {byte:#04x} on a table of capacity zero (QPACK_ENCODER_STREAM_ERROR)"
            ),
            H3Error::RequestRejected => {
                f.write_str("request rejected before it was acted on (H3_REQUEST_REJECTED)")
            }
            H3Error::StreamReset { code } => write!(f, "request stream reset with code {code:#x}"),
            H3Error::Connect { what } => write!(f, "QUIC connection not started, {what}"),
            H3Error::HandshakeTimedOut { ms } => write!(f, "QUIC handshake not done in {ms} ms"),
            H3Error::Lost { loss } => write!(f, "QUIC connection {loss}"),
        }
    }
}

impl std::error::Error for H3Error {}

#[cfg(test)]
mod tests;
