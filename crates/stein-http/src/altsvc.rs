//! `Alt-Svc` (RFC 7838), read for whether an origin offers HTTP/3.
//!
//! Only `h3` on the origin's own host is taken. Another host has certificate rules
//! of its own (RFC 7838 2.1), and drafts such as `h3-29` are other protocols.

/// Freshness of an alternative without `ma` (RFC 7838 3.1).
pub const DEFAULT_MA: u32 = 24 * 3600;

/// What one `Alt-Svc` field value says about HTTP/3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltSvc {
    /// `clear`, every alternative of the origin is gone (section 3).
    Clear,
    /// `h3` on the origin's host at `port`, fresh for `ma` seconds.
    H3 { port: u16, ma: u32 },
    /// Nothing a client takes.
    None,
}

/// Byte cursor over a field value.
struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    /// OWS (RFC 9110 5.6.3).
    fn ows(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        self.at += usize::from(hit);
        hit
    }

    /// Token (RFC 9110 5.6.2), possibly empty.
    fn token(&mut self) -> &[u8] {
        let from = self.at;
        while self.peek().is_some_and(tchar) {
            self.at += 1;
        }
        &self.b[from..self.at]
    }

    /// Quoted-string (RFC 9110 5.6.4) unescaped into `out`. `None` if not one or too long.
    fn quoted(&mut self, out: &mut [u8]) -> Option<usize> {
        if !self.eat(b'"') {
            return None;
        }
        let mut n = 0;
        loop {
            let c = self.peek()?;
            self.at += 1;
            let c = match c {
                b'"' => return Some(n),
                b'\\' => {
                    let c = self.peek()?;
                    self.at += 1;
                    c
                }
                c => c,
            };
            *out.get_mut(n)? = c;
            n += 1;
        }
    }

    /// Past the next top level comma, skipping quoted strings.
    fn next_member(&mut self) {
        let mut quoted = false;
        while let Some(c) = self.peek() {
            self.at += 1;
            match c {
                b'\\' if quoted => self.at += 1,
                b'"' => quoted = !quoted,
                b',' if !quoted => return,
                _ => {}
            }
        }
    }

    /// `*( OWS ";" OWS parameter )`, of which `ma` matters.
    fn params(&mut self) -> u32 {
        let mut ma = DEFAULT_MA;
        loop {
            self.ows();
            if !self.eat(b';') {
                return ma;
            }
            self.ows();
            let is_ma = self.token().eq_ignore_ascii_case(b"ma");
            if !self.eat(b'=') {
                return ma;
            }
            let mut quoted = [0u8; 16];
            let v = match self.peek() {
                Some(b'"') => self.quoted(&mut quoted).map(|n| &quoted[..n]),
                _ => Some(self.token()),
            };
            if let (true, Some(v)) = (is_ma, v) {
                ma = delta_seconds(v).unwrap_or(ma);
            }
        }
    }
}

/// tchar (RFC 9110 5.6.2).
fn tchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

/// delta-seconds (RFC 9111 1.2.2), saturating.
fn delta_seconds(v: &[u8]) -> Option<u32> {
    if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = v.iter().fold(0u64, |n, d| {
        n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
    });
    Some(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Port of an `h3` authority `[ uri-host ] ":" port` on `host`, the host empty for the origin.
fn h3_port(auth: &[u8], host: &str) -> Option<u16> {
    let colon = auth.iter().rposition(|b| *b == b':')?;
    let (alt_host, port) = (&auth[..colon], &auth[colon + 1..]);
    let alt_host = alt_host
        .strip_prefix(b"[")
        .and_then(|h| h.strip_suffix(b"]"))
        .unwrap_or(alt_host);
    let same = alt_host.is_empty() || alt_host.eq_ignore_ascii_case(host.as_bytes());
    let port = core::str::from_utf8(port)
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p > 0);
    port.filter(|_| same)
}

/// Read one `Alt-Svc` value for an `h3` alternative on `host` (RFC 7838 3).
/// Alternatives are in order of preference, so the first wins.
pub fn parse(value: &[u8], host: &str) -> AltSvc {
    let mut c = Cursor { b: value, at: 0 };
    c.ows();
    // `clear` is the whole value, case-sensitive.
    if c.b[c.at..].trim_ascii_end() == b"clear" {
        return AltSvc::Clear;
    }
    while c.peek().is_some() {
        c.ows();
        let h3 = c.token() == b"h3";
        let mut authority = [0u8; 300];
        let len = if c.eat(b'=') {
            c.quoted(&mut authority)
        } else {
            None
        };
        let found = len
            .filter(|_| h3)
            .and_then(|len| h3_port(&authority[..len], host));
        let ma = c.params();
        if let Some(port) = found {
            return AltSvc::H3 { port, ma };
        }
        c.next_member();
    }
    AltSvc::None
}

#[cfg(test)]
mod tests;
