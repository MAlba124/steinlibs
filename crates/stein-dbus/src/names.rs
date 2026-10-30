//! The name rules. A bus drops a connection that sends a bad one, so they
//! are checked before a message is written rather than found out that way.

/// Longest bus, interface, member or error name.
pub const MAX_NAME: usize = 255;

/// `/`, or `/` separated elements of `[A-Za-z0-9_]`, no trailing slash.
pub fn is_path(p: &[u8]) -> bool {
    match p {
        [] => false,
        [b'/'] => true,
        [b'/', rest @ ..] => rest
            .split(|&c| c == b'/')
            .all(|e| !e.is_empty() && e.iter().all(|&c| word(c))),
        _ => false,
    }
}

/// Two or more dot separated elements, none starting with a digit.
pub fn is_interface(n: &[u8]) -> bool {
    n.len() <= MAX_NAME && dotted(n, |e| !e[0].is_ascii_digit() && e.iter().all(|&c| word(c)))
}

/// Error names follow the interface rules.
pub fn is_error(n: &[u8]) -> bool {
    is_interface(n)
}

/// One element of `[A-Za-z0-9_]`, not starting with a digit.
pub fn is_member(n: &[u8]) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && !n[0].is_ascii_digit() && n.iter().all(|&c| word(c))
}

/// A unique name (`:1.42`) or a well known one (`org.freedesktop.DBus`).
pub fn is_bus_name(n: &[u8]) -> bool {
    if n.len() > MAX_NAME {
        return false;
    }
    let dash = |c: u8| word(c) || c == b'-';
    match n {
        [b':', rest @ ..] => dotted(rest, |e| e.iter().all(|&c| dash(c))),
        _ => dotted(n, |e| !e[0].is_ascii_digit() && e.iter().all(|&c| dash(c))),
    }
}

fn word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// At least two non-empty elements, each passing `ok`.
fn dotted(n: &[u8], ok: impl Fn(&[u8]) -> bool) -> bool {
    let mut count = 0;
    for e in n.split(|&c| c == b'.') {
        if e.is_empty() || !ok(e) {
            return false;
        }
        count += 1;
    }
    count >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in ["/", "/org", "/org/freedesktop/DBus", "/a_b/c9"] {
            assert!(is_path(ok.as_bytes()), "{ok}");
        }
        for bad in ["", "org", "/org/", "//", "/a//b", "/a-b", "/a.b"] {
            assert!(!is_path(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn interfaces_and_members() {
        assert!(is_interface(b"org.freedesktop.DBus"));
        assert!(!is_interface(b"org"));
        assert!(!is_interface(b"org..x"));
        assert!(!is_interface(b"org.9x"));
        assert!(!is_interface(b"org.x-y"));
        assert!(is_member(b"ListNames"));
        assert!(is_member(b"_x9"));
        assert!(!is_member(b"9x"));
        assert!(!is_member(b"a.b"));
        assert!(!is_member(b""));
    }

    #[test]
    fn bus_names() {
        assert!(is_bus_name(b":1.42"));
        assert!(is_bus_name(b"org.freedesktop.Notifications"));
        assert!(is_bus_name(b"org.kde.foo-bar"));
        assert!(!is_bus_name(b":1"));
        assert!(!is_bus_name(b"org.9x"));
        assert!(!is_bus_name(b"org"));
        assert!(!is_bus_name(b""));
        let long = [b'a'; 256];
        assert!(!is_bus_name(&long));
    }
}
