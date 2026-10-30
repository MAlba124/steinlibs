//! SASL, the line protocol before the first message.
//!
//! ```text
//! C: \0AUTH EXTERNAL <uid as hex digits>\r\n
//! S: OK <guid>\r\n
//! C: BEGIN\r\n
//! ```
//!
//! EXTERNAL only: the bus reads the peer's credentials off the socket, so
//! the uid sent is a claim it checks. Unix fd passing is not negotiated.

use core::fmt;

pub const BEGIN: &[u8] = b"BEGIN\r\n";
/// A server line longer than this is not one worth waiting for.
pub const MAX_LINE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// The bus refused EXTERNAL.
    Rejected,
    /// The bus answered ERROR.
    Error,
    /// A line this client did not expect.
    Unexpected,
    /// A line longer than [`MAX_LINE`], or the output too small.
    TooLong,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected => write!(f, "bus rejected EXTERNAL auth"),
            Self::Error => write!(f, "bus answered auth with ERROR"),
            Self::Unexpected => write!(f, "unexpected auth line from the bus"),
            Self::TooLong => write!(f, "auth line too long"),
        }
    }
}

/// What the bus said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Authenticated, with the server's guid in hex.
    Ok([u8; 32]),
    Rejected,
    Error,
    Other,
}

/// Write the opening NUL and the AUTH line into `out`.
pub fn start(uid: u32, out: &mut [u8]) -> Result<usize, AuthError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 10];
    let mut d = digits.len();
    let mut v = uid;
    loop {
        d -= 1;
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let head = b"\0AUTH EXTERNAL ";
    let len = head.len() + (digits.len() - d) * 2 + 2;
    let out = out.get_mut(..len).ok_or(AuthError::TooLong)?;
    out[..head.len()].copy_from_slice(head);
    let mut at = head.len();
    for &c in &digits[d..] {
        out[at] = HEX[(c >> 4) as usize];
        out[at + 1] = HEX[(c & 0xf) as usize];
        at += 2;
    }
    out[at..].copy_from_slice(b"\r\n");
    Ok(len)
}

/// The length of the first line in `buf`, `\r\n` included.
pub fn line(buf: &[u8]) -> Result<Option<usize>, AuthError> {
    match buf.windows(2).position(|w| w == b"\r\n") {
        Some(i) => Ok(Some(i + 2)),
        None if buf.len() > MAX_LINE => Err(AuthError::TooLong),
        None => Ok(None),
    }
}

/// Read one server line, `\r\n` included or not.
pub fn reply(line: &[u8]) -> Reply {
    let line = line.strip_suffix(b"\r\n").unwrap_or(line);
    if let Some(guid) = line.strip_prefix(b"OK ") {
        let mut g = [0u8; 32];
        if guid.len() == 32 && guid.iter().all(u8::is_ascii_hexdigit) {
            g.copy_from_slice(guid);
            return Reply::Ok(g);
        }
        return Reply::Other;
    }
    if line.starts_with(b"REJECTED") {
        return Reply::Rejected;
    }
    if line.starts_with(b"ERROR") {
        return Reply::Error;
    }
    Reply::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_auth_line_hexes_the_decimal_uid() {
        let mut out = [0u8; 64];
        let n = start(1000, &mut out).unwrap();
        assert_eq!(&out[..n], b"\0AUTH EXTERNAL 31303030\r\n");
        let n = start(0, &mut out).unwrap();
        assert_eq!(&out[..n], b"\0AUTH EXTERNAL 30\r\n");
        let n = start(u32::MAX, &mut out).unwrap();
        assert_eq!(&out[..n], b"\0AUTH EXTERNAL 34323934393637323935\r\n");
        assert_eq!(start(1000, &mut [0u8; 8]), Err(AuthError::TooLong));
    }

    #[test]
    fn replies() {
        let ok = b"OK 0123456789abcdef0123456789abcdef\r\n";
        assert_eq!(reply(ok), Reply::Ok(*b"0123456789abcdef0123456789abcdef"));
        assert_eq!(reply(b"OK short\r\n"), Reply::Other);
        assert_eq!(reply(b"REJECTED EXTERNAL\r\n"), Reply::Rejected);
        assert_eq!(reply(b"ERROR \"no\"\r\n"), Reply::Error);
        assert_eq!(reply(b"DATA\r\n"), Reply::Other);
    }

    #[test]
    fn lines_split_on_crlf() {
        assert_eq!(line(b"OK x\r\nrest"), Ok(Some(6)));
        assert_eq!(line(b"OK x\r"), Ok(None));
        assert_eq!(line(&[b'a'; 600]), Err(AuthError::TooLong));
    }
}
