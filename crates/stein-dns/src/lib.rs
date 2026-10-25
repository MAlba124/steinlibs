//! DNS messages, encoded and decoded.
//!
//! Wire format only. No sockets, no resolver policy, no cache, no
//! `/etc/resolv.conf`. Build a query into a caller's buffer, decode a reply
//! into caller supplied tables.
//!
//! # Why this exists
//!
//! `getaddrinfo` blocks a thread and cannot be driven from an event loop, so
//! resolving a few dozen feed hostnames at once means either a thread pool or
//! speaking DNS directly. This is the wire half of speaking it directly.
//!
//! # What the caller owns
//!
//! Decoding writes into a [`Storage`] the caller supplies: a table of records
//! and a byte arena for names. Capacity is a hard ceiling, so a response that
//! needs more is refused rather than grown. Nothing here allocates.
//!
//! # Hostile input
//!
//! A response arrives over UDP from whoever answered first, which is not
//! necessarily the server that was asked. Every field is bounds checked,
//! compression pointers are bounded three separate ways, and [`matches_query`]
//! exists so a driver can refuse a reply that does not answer the question it
//! asked.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod msg;
pub mod svcb;

pub use msg::{
    Class, Data, Flags, Header, Name, Opt, Query, Question, Raw, Rcode, Record, Reply, Storage,
    Type, decode, decode_header, encode, encode_query, matches_query, name_eq,
};

/// What went wrong, and where.
///
/// The offset is a byte offset into the message being read, or into the output
/// buffer being written. A malformed response without a position is not a
/// report, and this is the kind of bug that only ever shows up against one
/// particular resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsError {
    pub kind: ErrorKind,
    pub at: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The message ended in the middle of a field.
    Truncated,
    /// Longer than 65535 bytes, which the format cannot address.
    TooLong,
    /// A label or a whole name that breaks the length limits, or a reserved
    /// label type.
    BadName,
    /// A compression pointer that does not point strictly backwards, points
    /// into the header, or starts a chain longer than the jump cap.
    BadPointer,
    /// RDLENGTH disagrees with what the record type must contain.
    BadRdLength,
    /// A question section that is not the single question a query asks.
    BadQuestion,
    /// A malformed, duplicated, or misplaced EDNS0 OPT record.
    BadOpt,
    /// More records than the table the caller supplied.
    TooManyRecords,
    /// More name bytes than the arena the caller supplied.
    NamesFull,
    /// The output buffer filled before the query was written.
    OutputFull,
    /// The QR bit is clear, so this is a query and not a reply to one.
    NotAResponse,
}

impl DnsError {
    #[inline]
    pub fn new(kind: ErrorKind, at: usize) -> DnsError {
        DnsError {
            kind,
            at: at.min(u16::MAX as usize) as u16,
        }
    }
}

impl core::fmt::Display for DnsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self.kind {
            ErrorKind::Truncated => "the message ended mid field",
            ErrorKind::TooLong => "longer than the format can address",
            ErrorKind::BadName => "malformed name",
            ErrorKind::BadPointer => "malformed compression pointer",
            ErrorKind::BadRdLength => "rdlength does not match the record type",
            ErrorKind::BadQuestion => "not a single question",
            ErrorKind::BadOpt => "malformed OPT record",
            ErrorKind::TooManyRecords => "more records than the table allows",
            ErrorKind::NamesFull => "more name bytes than the arena allows",
            ErrorKind::OutputFull => "the output buffer filled",
            ErrorKind::NotAResponse => "not a response",
        };
        write!(f, "{what} at byte {}", self.at)
    }
}
