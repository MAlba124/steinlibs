//! Bus addresses, `unix:path=/run/user/1000/bus;...`.
//!
//! Only what a client connects to: the first `unix` entry's path or
//! abstract name, decoded. Reading `DBUS_SESSION_BUS_ADDRESS` is the
//! caller's, this crate touches no environment.

use core::fmt;

/// Where the system bus is when nothing says otherwise.
pub const SYSTEM_BUS: &str = "unix:path=/var/run/dbus/system_bus_socket";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Socket {
    /// A path in the filesystem.
    Path,
    /// A name in the abstract namespace, without the leading NUL.
    Abstract,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrError {
    /// No `unix` entry with a path or abstract name.
    NoUnix,
    /// A key without `=`.
    Malformed,
    /// A `%` not followed by two hex digits.
    Escape,
    /// The decoded name does not fit the output.
    TooLong,
}

impl fmt::Display for AddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUnix => write!(f, "bus address has no unix socket"),
            Self::Malformed => write!(f, "bus address has a key without a value"),
            Self::Escape => write!(f, "bus address has a bad % escape"),
            Self::TooLong => write!(f, "bus address path too long"),
        }
    }
}

/// The first unix socket in `addr`, its name decoded into `out`. Returns
/// which kind and how many bytes.
pub fn unix(addr: &str, out: &mut [u8]) -> Result<(Socket, usize), AddrError> {
    for entry in addr.split(';') {
        let Some(rest) = entry.strip_prefix("unix:") else {
            continue;
        };
        for kv in rest.split(',').filter(|kv| !kv.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or(AddrError::Malformed)?;
            let kind = match k {
                "path" => Socket::Path,
                "abstract" => Socket::Abstract,
                _ => continue,
            };
            return Ok((kind, unescape(v.as_bytes(), out)?));
        }
    }
    Err(AddrError::NoUnix)
}

fn unescape(v: &[u8], out: &mut [u8]) -> Result<usize, AddrError> {
    let mut n = 0;
    let mut i = 0;
    while i < v.len() {
        let b = if v[i] == b'%' {
            let hex = v.get(i + 1..i + 3).ok_or(AddrError::Escape)?;
            i += 3;
            (nibble(hex[0])? << 4) | nibble(hex[1])?
        } else {
            i += 1;
            v[i - 1]
        };
        *out.get_mut(n).ok_or(AddrError::TooLong)? = b;
        n += 1;
    }
    Ok(n)
}

fn nibble(c: u8) -> Result<u8, AddrError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(AddrError::Escape),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(a: &str) -> Result<(Socket, std::string::String), AddrError> {
        let mut out = [0u8; 108];
        let (k, n) = unix(a, &mut out)?;
        Ok((k, std::string::String::from_utf8_lossy(&out[..n]).into()))
    }

    #[test]
    fn a_session_address() {
        assert_eq!(
            get("unix:path=/run/user/1000/bus"),
            Ok((Socket::Path, "/run/user/1000/bus".into()))
        );
    }

    #[test]
    fn the_first_unix_entry_wins_and_others_are_skipped() {
        assert_eq!(
            get("tcp:host=x,port=1;unix:guid=abc,abstract=/tmp/dbus-X;unix:path=/b"),
            Ok((Socket::Abstract, "/tmp/dbus-X".into()))
        );
    }

    #[test]
    fn escapes_decode() {
        assert_eq!(
            get("unix:path=/tmp/a%20b%2c"),
            Ok((Socket::Path, "/tmp/a b,".into()))
        );
        assert_eq!(get("unix:path=/x%2"), Err(AddrError::Escape));
        assert_eq!(get("unix:path=/x%zz"), Err(AddrError::Escape));
    }

    #[test]
    fn failures() {
        assert_eq!(get("tcp:host=x"), Err(AddrError::NoUnix));
        assert_eq!(get("unix:runtime=yes"), Err(AddrError::NoUnix));
        assert_eq!(get("unix:path"), Err(AddrError::Malformed));
        let long = std::format!("unix:path=/{}", "a".repeat(200));
        assert_eq!(get(&long), Err(AddrError::TooLong));
        assert_eq!(get(SYSTEM_BUS).map(|(k, _)| k), Ok(Socket::Path));
    }
}
