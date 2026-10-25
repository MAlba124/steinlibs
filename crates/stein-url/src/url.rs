//! Parse http and https URLs and resolve references against them.
//! Zero-copy except [`resolve`] and [`percent_decode`], which write into caller buffers.

/// Offset and length into the input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    pub const EMPTY: Span = Span { at: 0, len: 0 };

    #[inline]
    pub fn of(src: &[u8], at: usize, end: usize) -> Span {
        let at = at.min(src.len());
        let end = end.min(src.len()).max(at);
        Span {
            at: at as u32,
            len: (end - at) as u32,
        }
    }

    /// The bytes, or empty when the span does not fit `src`.
    #[inline]
    pub fn bytes<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        let at = self.at as usize;
        src.get(at..at + self.len as usize).unwrap_or(&[])
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Supported schemes. Anything else is a parse error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    #[inline]
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    #[inline]
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            Scheme::Http => b"http",
            Scheme::Https => b"https",
        }
    }
}

/// Offsets of each URL piece. Presence lives in [`Parts::flags`] since an empty span is ambiguous.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Parts {
    pub scheme: Span,
    /// Between `//` and the path, userinfo and port included.
    pub authority: Span,
    /// Before the last `@` of the authority.
    pub userinfo: Span,
    /// Brackets removed.
    pub host: Span,
    /// Digits only.
    pub port: Span,
    /// To the `?`, the `#`, or the end.
    pub path: Span,
    /// Before any `#`.
    pub query: Span,
    pub fragment: Span,
    /// Surrounding whitespace removed.
    pub whole: Span,
    /// Presence bits plus [`Parts::IPV6`].
    pub flags: u8,
}

impl Parts {
    pub const SCHEME: u8 = 1 << 0;
    pub const AUTHORITY: u8 = 1 << 1;
    pub const USERINFO: u8 = 1 << 2;
    pub const PORT: u8 = 1 << 3;
    pub const QUERY: u8 = 1 << 4;
    pub const FRAGMENT: u8 = 1 << 5;
    /// Host was bracketed.
    pub const IPV6: u8 = 1 << 6;

    #[inline]
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// Absolute http or https URL borrowed from its input.
#[derive(Clone, Copy, Debug)]
pub struct Url<'a> {
    src: &'a [u8],
    parts: Parts,
    scheme: Scheme,
    port: u16,
}

impl<'a> Url<'a> {
    #[inline]
    pub fn parts(&self) -> Parts {
        self.parts
    }

    #[inline]
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Brackets removed, case as written. Compare with [`Url::host_eq`].
    #[inline]
    pub fn host(&self) -> &'a [u8] {
        self.parts.host.bytes(self.src)
    }

    /// Bracketed IPv6 literal. Rebuilders must re-add the brackets.
    #[inline]
    pub fn host_is_ipv6(&self) -> bool {
        self.parts.has(Parts::IPV6)
    }

    /// Case-insensitive host comparison.
    #[inline]
    pub fn host_eq(&self, other: &[u8]) -> bool {
        self.host().eq_ignore_ascii_case(other)
    }

    /// Scheme default applied.
    #[inline]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Only when the URL named one.
    #[inline]
    pub fn explicit_port(&self) -> Option<u16> {
        if self.parts.has(Parts::PORT) {
            Some(self.port)
        } else {
            None
        }
    }

    /// Userinfo, host and port as written.
    #[inline]
    pub fn authority(&self) -> &'a [u8] {
        self.parts.authority.bytes(self.src)
    }

    /// Before the last `@`, still percent encoded.
    #[inline]
    pub fn userinfo(&self) -> &'a [u8] {
        self.parts.userinfo.bytes(self.src)
    }

    /// `https://evil.com@good.com/` connects to good.com. Callers should drop such URLs.
    #[inline]
    pub fn has_userinfo(&self) -> bool {
        self.parts.has(Parts::USERINFO)
    }

    /// Empty for `http://host`. A request line needs `/` then.
    #[inline]
    pub fn path(&self) -> &'a [u8] {
        self.parts.path.bytes(self.src)
    }

    /// Without the `?`, still percent encoded.
    #[inline]
    pub fn query(&self) -> &'a [u8] {
        self.parts.query.bytes(self.src)
    }

    #[inline]
    pub fn has_query(&self) -> bool {
        self.parts.has(Parts::QUERY)
    }

    /// Without the `#`. Never sent to a server.
    #[inline]
    pub fn fragment(&self) -> &'a [u8] {
        self.parts.fragment.bytes(self.src)
    }

    #[inline]
    pub fn has_fragment(&self) -> bool {
        self.parts.has(Parts::FRAGMENT)
    }

    /// Surrounding whitespace removed.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.parts.whole.bytes(self.src)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlError {
    NoScheme,
    BadScheme,
    /// No `//authority`, or an empty host.
    NoHost,
    /// Disallowed host byte, unclosed bracket, or bracketed non-address.
    BadHost,
    /// Empty, non-digit, or over 65535.
    BadPort,
    /// Control byte, space or DEL. Blocks `\r\n` request smuggling.
    BadByte,
    /// Output buffer too small, or input over `u32::MAX`.
    TooLong,
}

impl core::fmt::Display for UrlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match *self {
            UrlError::NoScheme => "no scheme",
            UrlError::BadScheme => "not an http or https scheme",
            UrlError::NoHost => "no host",
            UrlError::BadHost => "malformed host",
            UrlError::BadPort => "malformed port",
            UrlError::BadByte => "a byte no URL may contain",
            UrlError::TooLong => "does not fit",
        };
        f.write_str(what)
    }
}

impl core::error::Error for UrlError {}

/// Parse an absolute http or https URL. Surrounding whitespace is trimmed, inner whitespace is an error.
pub fn parse(src: &[u8]) -> Result<Url<'_>, UrlError> {
    let parts = parse_ref(src)?;
    if !parts.has(Parts::SCHEME) {
        return Err(UrlError::NoScheme);
    }
    // Scheme first so `mailto:someone@x` reports the scheme, not the missing host.
    let scheme = scheme_of(parts.scheme.bytes(src))?;
    if !parts.has(Parts::AUTHORITY) {
        return Err(UrlError::NoHost);
    }
    let port = if parts.has(Parts::PORT) {
        parse_port(parts.port.bytes(src))?
    } else {
        scheme.default_port()
    };
    Ok(Url {
        src,
        parts,
        scheme,
        port,
    })
}

/// Split a reference into parts. Validates any authority so relative refs cannot smuggle a bad host into [`resolve`].
fn parse_ref(src: &[u8]) -> Result<Parts, UrlError> {
    if src.len() > u32::MAX as usize {
        return Err(UrlError::TooLong);
    }
    let (mut lo, hi) = trim(src);
    check_bytes(src, lo, hi)?;
    let mut p = Parts {
        whole: Span::of(src, lo, hi),
        ..Parts::default()
    };
    // Fragment first, a `?` after `#` belongs to it.
    let mut end = hi;
    if let Some(i) = find(src, lo, end, b'#') {
        p.fragment = Span::of(src, i + 1, end);
        p.flags |= Parts::FRAGMENT;
        end = i;
    }
    if let Some(i) = find(src, lo, end, b'?') {
        p.query = Span::of(src, i + 1, end);
        p.flags |= Parts::QUERY;
        end = i;
    }
    if let Some(i) = scheme_end(src, lo, end) {
        p.scheme = Span::of(src, lo, i);
        p.flags |= Parts::SCHEME;
        lo = i + 1;
    }
    if slice(src, lo, end).starts_with(b"//") {
        let stop = find(src, lo + 2, end, b'/').unwrap_or(end);
        p.authority = Span::of(src, lo + 2, stop);
        p.flags |= Parts::AUTHORITY;
        split_authority(src, &mut p)?;
        lo = stop;
    }
    p.path = Span::of(src, lo, end);
    Ok(p)
}

fn trim(src: &[u8]) -> (usize, usize) {
    let lo = src.iter().position(|b| !is_space(*b)).unwrap_or(src.len());
    let hi = src
        .iter()
        .rposition(|b| !is_space(*b))
        .map_or(lo, |i| i + 1);
    (lo, hi)
}

#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0C)
}

/// Reject controls, space and DEL. Non-ASCII passes through.
fn check_bytes(src: &[u8], lo: usize, hi: usize) -> Result<(), UrlError> {
    if slice(src, lo, hi).iter().any(|b| *b <= b' ' || *b == 0x7F) {
        return Err(UrlError::BadByte);
    }
    Ok(())
}

#[inline]
fn slice(src: &[u8], lo: usize, hi: usize) -> &[u8] {
    src.get(lo..hi).unwrap_or(&[])
}

fn find(src: &[u8], lo: usize, hi: usize, what: u8) -> Option<usize> {
    slice(src, lo, hi)
        .iter()
        .position(|b| *b == what)
        .map(|i| i + lo)
}

fn rfind(src: &[u8], lo: usize, hi: usize, what: u8) -> Option<usize> {
    slice(src, lo, hi)
        .iter()
        .rposition(|b| *b == what)
        .map(|i| i + lo)
}

/// The `:` ending a scheme. `a/b:c` has none.
fn scheme_end(src: &[u8], lo: usize, hi: usize) -> Option<usize> {
    let s = slice(src, lo, hi);
    if !s.first()?.is_ascii_alphabetic() {
        return None;
    }
    let n = s.iter().position(|b| !is_scheme_byte(*b))?;
    if s.get(n) == Some(&b':') {
        Some(lo + n)
    } else {
        None
    }
}

#[inline]
fn is_scheme_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
}

fn scheme_of(raw: &[u8]) -> Result<Scheme, UrlError> {
    if raw.eq_ignore_ascii_case(b"http") {
        return Ok(Scheme::Http);
    }
    if raw.eq_ignore_ascii_case(b"https") {
        return Ok(Scheme::Https);
    }
    Err(UrlError::BadScheme)
}

/// Split authority into userinfo, host and port. Splits at the LAST `@` so `evil.com@good.com` has host good.com.
fn split_authority(src: &[u8], p: &mut Parts) -> Result<(), UrlError> {
    let at = p.authority.at as usize;
    let end = at + p.authority.len as usize;
    let bar = rfind(src, at, end, b'@');
    if let Some(i) = bar {
        p.userinfo = Span::of(src, at, i);
        p.flags |= Parts::USERINFO;
    }
    let host_at = bar.map_or(at, |i| i + 1);
    if slice(src, host_at, end).first() == Some(&b'[') {
        return bracketed(src, host_at, end, p);
    }
    // Unbracketed, the first colon starts the port.
    let colon = find(src, host_at, end, b':');
    let host_end = colon.unwrap_or(end);
    if host_end == host_at {
        return Err(UrlError::NoHost);
    }
    p.host = Span::of(src, host_at, host_end);
    if !valid_host(p.host.bytes(src)) {
        return Err(UrlError::BadHost);
    }
    if let Some(i) = colon {
        p.port = Span::of(src, i + 1, end);
        p.flags |= Parts::PORT;
        parse_port(p.port.bytes(src))?;
    }
    Ok(())
}

/// `[2001:db8::1]:8443`. Port is only looked for after `]`.
fn bracketed(src: &[u8], at: usize, end: usize, p: &mut Parts) -> Result<(), UrlError> {
    let close = find(src, at + 1, end, b']').ok_or(UrlError::BadHost)?;
    p.host = Span::of(src, at + 1, close);
    p.flags |= Parts::IPV6;
    if p.host.is_empty() {
        return Err(UrlError::NoHost);
    }
    if !valid_ipv6(p.host.bytes(src)) {
        return Err(UrlError::BadHost);
    }
    let rest = slice(src, close + 1, end);
    if rest.is_empty() {
        return Ok(());
    }
    if rest.first() != Some(&b':') {
        return Err(UrlError::BadHost);
    }
    p.port = Span::of(src, close + 2, end);
    p.flags |= Parts::PORT;
    parse_port(p.port.bytes(src))?;
    Ok(())
}

/// Digits only, max 65535. Never truncates.
fn parse_port(digits: &[u8]) -> Result<u16, UrlError> {
    if digits.is_empty() {
        return Err(UrlError::BadPort);
    }
    let mut n: u32 = 0;
    for b in digits {
        if !b.is_ascii_digit() {
            return Err(UrlError::BadPort);
        }
        n = n * 10 + u32::from(*b - b'0');
        if n > u16::MAX as u32 {
            return Err(UrlError::BadPort);
        }
    }
    Ok(n as u16)
}

/// RFC 3986 `reg-name`, ASCII only. Punycode IDNs before this.
fn valid_host(host: &[u8]) -> bool {
    !host.is_empty() && host.iter().all(|b| is_host_byte(*b))
}

#[inline]
fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'%'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

/// Not a full address parser. Requires a colon and a safe byte set.
fn valid_ipv6(host: &[u8]) -> bool {
    host.contains(&b':')
        && host
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'%' | b'-' | b'_'))
}

/// Resolve `reference` against absolute `base` per RFC 3986 5.3 into `out`, returning the length.
/// Writes nothing when it does not fit. Non-http(s) schemes are refused.
///
/// Uses the non-strict rule of 5.2.2, so `http:g` resolves against the base.
/// Dot segments are removed in every branch, including an empty reference.
pub fn resolve(base: &[u8], reference: &[u8], out: &mut [u8]) -> Result<usize, UrlError> {
    let b = parse(base)?;
    let r = parse_ref(reference)?;
    let t = target(b, reference, r)?;
    let n = emit_len(&t);
    if n > out.len() {
        return Err(UrlError::TooLong);
    }
    emit(&t, out);
    Ok(n)
}

/// Two slices treated as one path, so a merged path needs no buffer.
#[derive(Clone, Copy)]
struct Cat<'a> {
    head: &'a [u8],
    tail: &'a [u8],
}

impl<'a> Cat<'a> {
    #[inline]
    fn one(all: &'a [u8]) -> Cat<'a> {
        Cat {
            head: all,
            tail: &[],
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    #[inline]
    fn at(&self, i: usize) -> u8 {
        if i < self.head.len() {
            return self.head.get(i).copied().unwrap_or(0);
        }
        self.tail.get(i - self.head.len()).copied().unwrap_or(0)
    }

    fn seg_is(&self, s: usize, e: usize, what: &[u8]) -> bool {
        e - s == what.len() && what.iter().enumerate().all(|(i, b)| self.at(s + i) == *b)
    }
}

struct Target<'a> {
    scheme: Scheme,
    authority: &'a [u8],
    path: Cat<'a>,
    query: &'a [u8],
    fragment: &'a [u8],
    /// [`Parts::QUERY`] and [`Parts::FRAGMENT`] only.
    flags: u8,
}

/// RFC 3986 5.2.2, non-strict.
fn target<'a>(b: Url<'a>, reference: &'a [u8], r: Parts) -> Result<Target<'a>, UrlError> {
    let scheme = if r.has(Parts::SCHEME) {
        scheme_of(r.scheme.bytes(reference))?
    } else {
        b.scheme()
    };
    let flags = r.flags & Parts::FRAGMENT;
    if r.has(Parts::AUTHORITY) {
        return Ok(Target {
            scheme,
            authority: r.authority.bytes(reference),
            path: Cat::one(r.path.bytes(reference)),
            query: r.query.bytes(reference),
            fragment: r.fragment.bytes(reference),
            flags: flags | (r.flags & Parts::QUERY),
        });
    }
    // With no authority of its own the reference borrows the base's, which
    // only makes sense when it did not name some other scheme.
    if scheme != b.scheme() {
        return Err(UrlError::NoHost);
    }
    let (path, query, qbit) = relative(b, reference, r);
    Ok(Target {
        scheme,
        authority: b.authority(),
        path,
        query,
        fragment: r.fragment.bytes(reference),
        flags: flags | qbit,
    })
}

/// The 5.2.2 branches that keep the base authority.
fn relative<'a>(b: Url<'a>, reference: &'a [u8], r: Parts) -> (Cat<'a>, &'a [u8], u8) {
    let rp = r.path.bytes(reference);
    if rp.is_empty() {
        // Empty ref is the base minus fragment. Query-only ref replaces just the query.
        if r.has(Parts::QUERY) {
            return (Cat::one(b.path()), r.query.bytes(reference), Parts::QUERY);
        }
        return (
            Cat::one(b.path()),
            b.query(),
            b.parts().flags & Parts::QUERY,
        );
    }
    let path = if rp.first() == Some(&b'/') {
        Cat::one(rp)
    } else {
        merge(b, rp)
    };
    (path, r.query.bytes(reference), r.flags & Parts::QUERY)
}

/// RFC 3986 5.2.3.
fn merge<'a>(b: Url<'a>, ref_path: &'a [u8]) -> Cat<'a> {
    let p = b.path();
    if p.is_empty() {
        return Cat {
            head: b"/",
            tail: ref_path,
        };
    }
    let cut = p.iter().rposition(|x| *x == b'/').map_or(0, |i| i + 1);
    Cat {
        head: p.get(..cut).unwrap_or(&[]),
        tail: ref_path,
    }
}

fn emit_len(t: &Target<'_>) -> usize {
    let q = if t.flags & Parts::QUERY != 0 {
        1 + t.query.len()
    } else {
        0
    };
    let f = if t.flags & Parts::FRAGMENT != 0 {
        1 + t.fragment.len()
    } else {
        0
    };
    t.scheme.as_bytes().len() + 3 + t.authority.len() + path_len(t.path) + q + f
}

/// RFC 3986 5.3 recomposition. Caller checked [`emit_len`] fits.
fn emit(t: &Target<'_>, out: &mut [u8]) {
    let mut at = put(out, 0, t.scheme.as_bytes());
    at = put(out, at, b"://");
    at = put(out, at, t.authority);
    at = write_path(t.path, out, at);
    if t.flags & Parts::QUERY != 0 {
        at = put(out, at, b"?");
        at = put(out, at, t.query);
    }
    if t.flags & Parts::FRAGMENT != 0 {
        at = put(out, at, b"#");
        put(out, at, t.fragment);
    }
}

/// Copy `what` at `at`, clamped to `out`. Returns the unclamped end.
fn put(out: &mut [u8], at: usize, what: &[u8]) -> usize {
    let end = at.saturating_add(what.len()).min(out.len());
    let n = end.saturating_sub(at);
    if let (Some(dst), Some(src)) = (out.get_mut(at..end), what.get(..n)) {
        dst.copy_from_slice(src);
    }
    at + what.len()
}

/// RFC 3986 5.2.4. Calls `keep` for each surviving segment, last first, and returns the normalised length.
/// Walks backwards so `..` is a counter instead of a stack. Leftover `..` is dropped, so refs cannot climb above root.
fn walk(path: Cat<'_>, mut keep: impl FnMut(usize, usize)) -> usize {
    let mut len = 0;
    let mut skip = 0usize;
    let mut end = path.len();
    // Trailing `.` or `..` keeps a trailing slash, emitted as an empty segment.
    if trailing_dot(path) {
        len += 1;
        keep(end, end);
    }
    while end > 0 {
        let start = segment_start(path, end);
        if survives(path, start, end, &mut skip) {
            len += 1 + (end - start);
            keep(start, end);
        }
        end = start.saturating_sub(1);
    }
    len
}

/// Index after the `/` preceding the segment ending at `end`.
fn segment_start(path: Cat<'_>, end: usize) -> usize {
    let mut i = end;
    while i > 0 {
        if path.at(i - 1) == b'/' {
            return i;
        }
        i -= 1;
    }
    0
}

fn trailing_dot(path: Cat<'_>) -> bool {
    let end = path.len();
    let start = segment_start(path, end);
    path.seg_is(start, end, b".") || path.seg_is(start, end, b"..")
}

/// `.` is checked before the skip since `..` cannot cancel it.
fn survives(path: Cat<'_>, s: usize, e: usize, skip: &mut usize) -> bool {
    if path.seg_is(s, e, b".") {
        return false;
    }
    if path.seg_is(s, e, b"..") {
        *skip += 1;
        return false;
    }
    if *skip > 0 {
        *skip -= 1;
        return false;
    }
    true
}

fn path_len(path: Cat<'_>) -> usize {
    walk(path, |_, _| {})
}

fn write_path(path: Cat<'_>, out: &mut [u8], at: usize) -> usize {
    let len = path_len(path);
    let mut cursor = at + len;
    walk(path, |s, e| cursor = put_segment(path, s, e, out, cursor));
    at + len
}

/// Write `/` plus one segment ending at `cursor`, returning its start. Segments arrive last first.
fn put_segment(path: Cat<'_>, s: usize, e: usize, out: &mut [u8], cursor: usize) -> usize {
    let start = cursor.saturating_sub(1 + (e - s));
    let mut at = start;
    if let Some(slot) = out.get_mut(at) {
        *slot = b'/';
    }
    at += 1;
    for i in s..e {
        if let Some(slot) = out.get_mut(at) {
            *slot = path.at(i);
        }
        at += 1;
    }
    start
}

/// Output length of [`percent_decode`].
pub fn decoded_len(src: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < src.len() {
        i += decode_one(src, i).1;
        n += 1;
    }
    n
}

/// Percent decode `src` into `out`, returning the length. Writes nothing when it does not fit.
/// For display only, never build a request from the output.
/// Malformed `%` is copied through. `+` is left alone. Output may not be UTF-8.
pub fn percent_decode(src: &[u8], out: &mut [u8]) -> Result<usize, UrlError> {
    let n = decoded_len(src);
    if n > out.len() {
        return Err(UrlError::TooLong);
    }
    let mut wrote = 0;
    let mut i = 0;
    while i < src.len() {
        let (byte, step) = decode_one(src, i);
        if let Some(slot) = out.get_mut(wrote) {
            *slot = byte;
        }
        wrote += 1;
        i += step;
    }
    Ok(wrote)
}

/// One output byte and the input consumed.
fn decode_one(src: &[u8], i: usize) -> (u8, usize) {
    let raw = src.get(i).copied().unwrap_or(0);
    if raw != b'%' {
        return (raw, 1);
    }
    let hi = src.get(i + 1).copied().and_then(hex);
    let lo = src.get(i + 2).copied().and_then(hex);
    match (hi, lo) {
        (Some(h), Some(l)) => (h * 16 + l, 3),
        _ => (b'%', 1),
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
