//! The parser and the lookup.

use core::fmt;

/// A range of the source.
///
/// `u32` because a config file needing more than four gigabytes of addressing
/// is not a config file, and halving the size of [`Entry`] matters more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    pub const EMPTY: Span = Span { at: 0, len: 0 };

    /// The bytes. An out of range span reads empty rather than panicking,
    /// because a `Span` from one file applied to another is a caller mistake
    /// that should not take the process down.
    #[inline]
    pub fn of<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        let at = self.at as usize;
        src.get(at..at + self.len as usize).unwrap_or(&[])
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn end(&self) -> u32 {
        self.at + self.len
    }
}

/// One `key = value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Index into the section table, or [`Entry::ROOT`] for a pair written
    /// above the first `[section]`.
    pub section: u16,
    pub key: Span,
    /// The value with its quotes removed and its escapes still in place.
    /// [`Entry::ESCAPED`] says whether decoding it would change anything.
    pub value: Span,
    /// 1-based. The only number in here a person will ever read.
    pub line: u32,
    pub flags: u8,
}

impl Entry {
    /// The section of a pair written before any header.
    pub const ROOT: u16 = u16::MAX;
    /// The value was written in quotes, so its whitespace is deliberate.
    pub const QUOTED: u8 = 1 << 0;
    /// The value contains a backslash escape. Rare, and worth a flag so the
    /// overwhelming majority of values can be used straight out of the source.
    pub const ESCAPED: u8 = 1 << 1;

    /// Whether reading this value requires [`unescape`] rather than the span.
    #[inline]
    pub fn needs_unescape(&self) -> bool {
        self.flags & Entry::ESCAPED != 0
    }
}

/// Which conventions the file follows.
///
/// One format, two dialects. Freedesktop desktop entries are INI-shaped but
/// take values verbatim: `Exec="/opt/My App/run" --flag` opens with a quote it
/// never closes, and under the config dialect that is an error. A dialect is a
/// parse-time argument rather than a second parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// `"` wraps a value and escapes decode inside one.
    pub quotes: bool,
    /// A `;` line is a comment, as well as a `#` line.
    pub semicolon_comments: bool,
}

impl Dialect {
    /// The dialect documented in the crate root. What [`parse`] uses.
    pub const INI: Dialect = Dialect {
        quotes: true,
        semicolon_comments: true,
    };
    /// Freedesktop desktop entries: `#` comments only, values verbatim.
    pub const DESKTOP: Dialect = Dialect {
        quotes: false,
        semicolon_comments: false,
    };

    #[inline]
    fn is_comment(&self, first: u8) -> bool {
        first == b'#' || (self.semicolon_comments && first == b';')
    }
}

/// How much room a file needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub entries: usize,
    pub sections: usize,
}

/// The arrays a parse fills. The caller owns them, so a config can be read on
/// a stack buffer with no allocator in the picture.
pub struct Storage<'a> {
    pub entries: &'a mut [Entry],
    pub sections: &'a mut [Span],
}

/// A parsed file: the bytes, plus where everything is in them.
pub struct Ini<'a> {
    src: &'a [u8],
    entries: &'a [Entry],
    sections: &'a [Span],
}

/// What went wrong, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IniError {
    /// A `[` with no `]` at the end of the line.
    UnclosedSection { line: u32 },
    /// `[]`, or a header of nothing but whitespace.
    EmptySection { line: u32 },
    /// A line that is not blank, a comment, a header, or a pair.
    NotAPair { line: u32 },
    /// `= value` with no name in front of it.
    EmptyKey { line: u32 },
    /// A value that opens with `"` and does not close.
    UnclosedQuote { line: u32 },
    /// A `\` followed by something that is not an escape.
    BadEscape { line: u32 },
    /// A control byte, which is never meant and usually means the file is not
    /// text at all.
    BadByte { line: u32 },
    /// More pairs than [`Storage::entries`] holds.
    TooManyEntries { line: u32 },
    /// More sections than [`Storage::sections`] holds.
    TooManySections { line: u32 },
    /// Over four gigabytes.
    TooLarge,
}

impl IniError {
    /// The line to point a person at, if there is one.
    pub fn line(&self) -> Option<u32> {
        match *self {
            Self::UnclosedSection { line }
            | Self::EmptySection { line }
            | Self::NotAPair { line }
            | Self::EmptyKey { line }
            | Self::UnclosedQuote { line }
            | Self::BadEscape { line }
            | Self::BadByte { line }
            | Self::TooManyEntries { line }
            | Self::TooManySections { line } => Some(line),
            Self::TooLarge => None,
        }
    }
}

impl fmt::Display for IniError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnclosedSection { line } => write!(f, "line {line}: a section header needs a ]"),
            Self::EmptySection { line } => write!(f, "line {line}: a section needs a name"),
            Self::NotAPair { line } => write!(f, "line {line}: expected key = value"),
            Self::EmptyKey { line } => write!(f, "line {line}: a value needs a key"),
            Self::UnclosedQuote { line } => write!(f, "line {line}: a quoted value needs a \""),
            Self::BadEscape { line } => write!(f, "line {line}: not an escape"),
            Self::BadByte { line } => write!(f, "line {line}: a control byte"),
            Self::TooManyEntries { line } => write!(f, "line {line}: too many settings"),
            Self::TooManySections { line } => write!(f, "line {line}: too many sections"),
            Self::TooLarge => write!(f, "the file is too large"),
        }
    }
}

impl core::error::Error for IniError {}

/// An upper bound on the room a file needs, without validating it.
///
/// Cheap enough to run before every parse. Sections are counted per header
/// rather than per distinct name, so a file repeating `[colours]` is counted
/// twice and the answer stays an upper bound.
pub fn count(src: &[u8]) -> Counts {
    count_with(src, Dialect::INI)
}

/// [`count`], under a chosen dialect. The dialects disagree on what a comment
/// is, so the count must use the same one as the parse or it stops being an
/// upper bound.
pub fn count_with(src: &[u8], dialect: Dialect) -> Counts {
    let mut c = Counts::default();
    let mut at = 0usize;
    while at < src.len() {
        let (line, next) = line_at(src, at);
        at = next;
        tally(src, trim(src, line), dialect, &mut c);
    }
    c
}

fn tally(src: &[u8], body: Span, dialect: Dialect, c: &mut Counts) {
    // Through the span, not the offset. A blank line trims to length zero at
    // an offset that still addresses a byte.
    let Some(&first) = body.of(src).first() else {
        return;
    };
    if dialect.is_comment(first) {
        return;
    }
    match first {
        b'[' => c.sections += 1,
        _ => c.entries += 1,
    }
}

/// Parse `src` into `store`.
pub fn parse<'a>(src: &'a [u8], store: Storage<'a>) -> Result<Ini<'a>, IniError> {
    parse_with(src, store, Dialect::INI)
}

/// [`parse`], under a chosen dialect.
pub fn parse_with<'a>(
    src: &'a [u8],
    store: Storage<'a>,
    dialect: Dialect,
) -> Result<Ini<'a>, IniError> {
    if src.len() > u32::MAX as usize {
        return Err(IniError::TooLarge);
    }
    let mut f = Fill {
        entries: store.entries,
        sections: store.sections,
        n_entries: 0,
        n_sections: 0,
        section: Entry::ROOT,
        dialect,
    };
    let mut at = 0usize;
    let mut line = 0u32;
    while at < src.len() {
        line += 1;
        let (span, next) = line_at(src, at);
        at = next;
        feed(src, trim(src, span), line, &mut f)?;
    }
    Ok(Ini {
        src,
        entries: &f.entries[..f.n_entries],
        sections: &f.sections[..f.n_sections],
    })
}

/// Parser state. A line is the whole unit, so there is nothing to carry
/// between them but the section in force and where the arrays have got to.
struct Fill<'a> {
    entries: &'a mut [Entry],
    sections: &'a mut [Span],
    n_entries: usize,
    n_sections: usize,
    section: u16,
    dialect: Dialect,
}

/// One line, already trimmed. This is the state machine, and it is a match on
/// the first byte because that is all the format needs.
fn feed(src: &[u8], body: Span, line: u32, f: &mut Fill) -> Result<(), IniError> {
    // Through the span, not the offset. A blank line trims to length zero at
    // an offset that still addresses a byte, so reading the byte would make
    // every empty line a pair.
    let Some(&first) = body.of(src).first() else {
        return Ok(());
    };
    if f.dialect.is_comment(first) {
        return Ok(());
    }
    match first {
        b'[' => header(src, body, line, f),
        _ => pair(src, body, line, f),
    }
}

fn header(src: &[u8], body: Span, line: u32, f: &mut Fill) -> Result<(), IniError> {
    if body.len < 2 || src.get(body.end() as usize - 1) != Some(&b']') {
        return Err(IniError::UnclosedSection { line });
    }
    let name = trim(
        src,
        Span {
            at: body.at + 1,
            len: body.len - 2,
        },
    );
    if name.is_empty() {
        return Err(IniError::EmptySection { line });
    }
    check_bytes(src, name, line)?;
    f.section = intern(src, name, line, f)?;
    Ok(())
}

/// The index of `name`, reusing the slot if the file already named it.
///
/// Reuse rather than a second slot, because a file that opens `[colours]`
/// twice means to continue it, and a lookup that stopped at the first one
/// would silently ignore every pair under the second.
fn intern(src: &[u8], name: Span, line: u32, f: &mut Fill) -> Result<u16, IniError> {
    let found = f.sections[..f.n_sections]
        .iter()
        .position(|s| s.of(src).eq_ignore_ascii_case(name.of(src)));
    if let Some(i) = found {
        return Ok(i as u16);
    }
    // ROOT is a real index, so the table must stop one short of it.
    if f.n_sections >= f.sections.len() || f.n_sections >= Entry::ROOT as usize {
        return Err(IniError::TooManySections { line });
    }
    f.sections[f.n_sections] = name;
    f.n_sections += 1;
    Ok(f.n_sections as u16 - 1)
}

fn pair(src: &[u8], body: Span, line: u32, f: &mut Fill) -> Result<(), IniError> {
    let Some(eq) = find(src, body, b'=') else {
        return Err(IniError::NotAPair { line });
    };
    let key = trim(
        src,
        Span {
            at: body.at,
            len: eq - body.at,
        },
    );
    if key.is_empty() {
        return Err(IniError::EmptyKey { line });
    }
    check_bytes(src, key, line)?;
    let rest = trim(
        src,
        Span {
            at: eq + 1,
            len: body.end() - eq - 1,
        },
    );
    let (value, flags) = read_value(src, rest, line, f.dialect)?;
    push(key, value, flags, line, f)
}

/// Strip the quotes if there are any, and check the escapes now rather than at
/// read time. A bad escape is a mistake in the file, and the line number that
/// makes it fixable only exists here.
fn read_value(src: &[u8], raw: Span, line: u32, dialect: Dialect) -> Result<(Span, u8), IniError> {
    if !dialect.quotes || raw.of(src).first() != Some(&b'"') {
        check_bytes(src, raw, line)?;
        return Ok((raw, 0));
    }
    if raw.len < 2 || src.get(raw.end() as usize - 1) != Some(&b'"') {
        return Err(IniError::UnclosedQuote { line });
    }
    let inner = Span {
        at: raw.at + 1,
        len: raw.len - 2,
    };
    check_bytes(src, inner, line)?;
    Ok((inner, Entry::QUOTED | scan_escapes(src, inner, line)?))
}

/// Validate every escape, and report whether there were any.
fn scan_escapes(src: &[u8], span: Span, line: u32) -> Result<u8, IniError> {
    let bytes = span.of(src);
    let mut i = 0usize;
    let mut flags = 0u8;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let ok = matches!(
            bytes.get(i + 1),
            Some(b'\\' | b'"' | b'n' | b't' | b'r' | b'0')
        );
        if !ok {
            return Err(IniError::BadEscape { line });
        }
        flags = Entry::ESCAPED;
        i += 2;
    }
    Ok(flags)
}

fn push(key: Span, value: Span, flags: u8, line: u32, f: &mut Fill) -> Result<(), IniError> {
    if f.n_entries >= f.entries.len() {
        return Err(IniError::TooManyEntries { line });
    }
    f.entries[f.n_entries] = Entry {
        section: f.section,
        key,
        value,
        line,
        flags,
    };
    f.n_entries += 1;
    Ok(())
}

/// The line starting at `at`, without its terminator, and where the next one
/// begins. A `\r` is left on the end for [`trim`] to take, which is what makes
/// a CRLF file work without a separate pass.
fn line_at(src: &[u8], at: usize) -> (Span, usize) {
    let rest = &src[at..];
    let len = memchr(rest, b'\n').unwrap_or(rest.len());
    (
        Span {
            at: at as u32,
            len: len as u32,
        },
        at + len + 1,
    )
}

fn memchr(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn find(src: &[u8], span: Span, needle: u8) -> Option<u32> {
    memchr(span.of(src), needle).map(|i| span.at + i as u32)
}

#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c)
}

fn trim(src: &[u8], span: Span) -> Span {
    let bytes = span.of(src);
    let start = bytes.iter().position(|&b| !is_space(b));
    let Some(start) = start else {
        return Span {
            at: span.at,
            len: 0,
        };
    };
    let end = bytes
        .iter()
        .rposition(|&b| !is_space(b))
        .map_or(start, |i| i + 1);
    Span {
        at: span.at + start as u32,
        len: (end - start) as u32,
    }
}

/// Reject control bytes. Tab survives because a value may legitimately contain
/// one; everything below space and DEL is a sign the file is not text.
fn check_bytes(src: &[u8], span: Span, line: u32) -> Result<(), IniError> {
    let bad = span
        .of(src)
        .iter()
        .any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f);
    if bad {
        return Err(IniError::BadByte { line });
    }
    Ok(())
}

/// How many bytes [`unescape`] would write.
pub fn unescape_len(raw: &[u8]) -> usize {
    let mut i = 0usize;
    let mut n = 0usize;
    while i < raw.len() {
        let jump = usize::from(raw[i] == b'\\' && i + 1 < raw.len());
        i += 1 + jump;
        n += 1;
    }
    n
}

/// Decode escapes into `out`, returning how many bytes were written.
///
/// `None` if `out` is too short, with nothing written, so a short buffer is
/// never mistaken for a short value. Only worth calling when
/// [`Entry::needs_unescape`] is true.
pub fn unescape(raw: &[u8], out: &mut [u8]) -> Option<usize> {
    if unescape_len(raw) > out.len() {
        return None;
    }
    let mut i = 0usize;
    let mut n = 0usize;
    while i < raw.len() {
        let (byte, used) = decode_one(&raw[i..]);
        out[n] = byte;
        n += 1;
        i += used;
    }
    Some(n)
}

/// One byte of a value, and how much of the input it consumed. A trailing lone
/// backslash is kept as itself, which a parsed file can never contain but a
/// caller passing its own bytes can.
fn decode_one(raw: &[u8]) -> (u8, usize) {
    if raw[0] != b'\\' {
        return (raw[0], 1);
    }
    match raw.get(1) {
        Some(b'n') => (b'\n', 2),
        Some(b't') => (b'\t', 2),
        Some(b'r') => (b'\r', 2),
        Some(b'0') => (0, 2),
        Some(&b) => (b, 2),
        None => (b'\\', 1),
    }
}

impl<'a> Ini<'a> {
    /// The bytes this was parsed from.
    pub fn source(&self) -> &'a [u8] {
        self.src
    }

    /// Every pair, in the order written.
    pub fn entries(&self) -> &'a [Entry] {
        self.entries
    }

    /// The bytes of a span, for a caller holding an [`Entry`].
    pub fn bytes(&self, span: Span) -> &'a [u8] {
        span.of(self.src)
    }

    /// The index of `name`, or `None`. `""` is the root section, which always
    /// exists and is never in the table.
    pub fn section_of(&self, name: &str) -> Option<u16> {
        if name.is_empty() {
            return Some(Entry::ROOT);
        }
        let found = self
            .sections
            .iter()
            .position(|s| s.of(self.src).eq_ignore_ascii_case(name.as_bytes()));
        found.map(|i| i as u16)
    }

    /// Whether the file has this section. The root counts only if something is
    /// actually in it, since every file has one otherwise.
    pub fn has_section(&self, name: &str) -> bool {
        match self.section_of(name) {
            Some(Entry::ROOT) => self.entries.iter().any(|e| e.section == Entry::ROOT),
            Some(_) => true,
            None => false,
        }
    }

    /// Every section name, in the order first written. The root is not
    /// included, because it has no name to return.
    pub fn sections(&self) -> Sections<'a> {
        Sections {
            src: self.src,
            spans: self.sections,
            at: 0,
        }
    }

    /// The last entry for `key`, which is the one a repeated key means.
    pub fn entry(&self, section: &str, key: &str) -> Option<&'a Entry> {
        let want = self.section_of(section)?;
        let k = key.as_bytes();
        self.entries
            .iter()
            .rfind(|e| e.section == want && e.key.of(self.src).eq_ignore_ascii_case(k))
    }

    /// The raw value, escapes and all. Prefer [`Ini::text`] unless the value is
    /// known not to be quoted.
    pub fn get(&self, section: &str, key: &str) -> Option<&'a [u8]> {
        self.entry(section, key).map(|e| e.value.of(self.src))
    }

    /// Whether the key is present at all, which a flag-shaped setting needs to
    /// tell apart from one set to false.
    pub fn has(&self, section: &str, key: &str) -> bool {
        self.entry(section, key).is_some()
    }

    /// Every value for `key`, in order. What a repeated setting means, such as
    /// a list of bindings.
    pub fn all<'k>(&self, section: &str, key: &'k str) -> All<'a, 'k> {
        All {
            src: self.src,
            entries: self.entries,
            at: 0,
            section: self.section_of(section),
            key: key.as_bytes(),
        }
    }

    /// Every pair in a section, for settings whose names are not known ahead
    /// of time.
    pub fn in_section(&self, section: &str) -> InSection<'a> {
        InSection {
            entries: self.entries,
            at: 0,
            section: self.section_of(section),
        }
    }

    /// The value as UTF-8, or `None` if it is not.
    ///
    /// Escapes are left in place, since decoding needs somewhere to write.
    /// [`Entry::needs_unescape`] says when that matters, and it almost never
    /// does.
    pub fn text(&self, section: &str, key: &str) -> Option<&'a str> {
        core::str::from_utf8(self.get(section, key)?).ok()
    }

    /// The value as a whole number. Accepts a leading sign, `0x`/`0b`/`0o`, and
    /// `_` as a group separator.
    pub fn int(&self, section: &str, key: &str) -> Option<i64> {
        parse_int(self.get(section, key)?)
    }

    /// The value as a count. A negative number is `None` rather than a wrap,
    /// because a negative scrollback is a mistake worth reporting.
    pub fn uint(&self, section: &str, key: &str) -> Option<u64> {
        let v = parse_int(self.get(section, key)?)?;
        u64::try_from(v).ok()
    }

    /// The value as a number. Written out in full rather than in exponent
    /// notation, which nobody puts in a config file.
    pub fn float(&self, section: &str, key: &str) -> Option<f32> {
        core::str::from_utf8(self.get(section, key)?)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The value as a flag. `true`/`yes`/`on`/`1` and `false`/`no`/`off`/`0`,
    /// case insensitively. Anything else is `None`, not false, so a typo is
    /// distinguishable from a decision.
    pub fn bool(&self, section: &str, key: &str) -> Option<bool> {
        parse_bool(self.get(section, key)?)
    }
}

fn parse_bool(v: &[u8]) -> Option<bool> {
    const TRUE: [&[u8]; 4] = [b"true", b"yes", b"on", b"1"];
    const FALSE: [&[u8]; 4] = [b"false", b"no", b"off", b"0"];
    if TRUE.iter().any(|t| v.eq_ignore_ascii_case(t)) {
        return Some(true);
    }
    if FALSE.iter().any(|t| v.eq_ignore_ascii_case(t)) {
        return Some(false);
    }
    None
}

fn parse_int(v: &[u8]) -> Option<i64> {
    let (neg, rest) = match v.first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    let (radix, digits) = radix_of(rest);
    let n = digits_to_i64(digits, radix)?;
    if neg { Some(-n) } else { Some(n) }
}

fn radix_of(v: &[u8]) -> (u32, &[u8]) {
    let two = v.get(..2).unwrap_or_default();
    match two {
        b"0x" | b"0X" => (16, &v[2..]),
        b"0b" | b"0B" => (2, &v[2..]),
        b"0o" | b"0O" => (8, &v[2..]),
        _ => (10, v),
    }
}

/// Fold digits, refusing an empty run so a bare `0x` is not zero. `_` is
/// skipped, which makes a large scrollback readable.
fn digits_to_i64(digits: &[u8], radix: u32) -> Option<i64> {
    let mut n: i64 = 0;
    let mut any = false;
    for &b in digits {
        if b == b'_' {
            continue;
        }
        let d = (b as char).to_digit(radix)?;
        n = n.checked_mul(radix as i64)?.checked_add(d as i64)?;
        any = true;
    }
    any.then_some(n)
}

/// Every section name.
pub struct Sections<'a> {
    src: &'a [u8],
    spans: &'a [Span],
    at: usize,
}

impl<'a> Iterator for Sections<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let s = self.spans.get(self.at)?;
        self.at += 1;
        Some(s.of(self.src))
    }
}

/// Every value for one key.
///
/// The key is borrowed separately from the source, so a lookup can be written
/// with a literal without tying it to the file's lifetime.
pub struct All<'a, 'k> {
    src: &'a [u8],
    entries: &'a [Entry],
    at: usize,
    section: Option<u16>,
    key: &'k [u8],
}

impl<'a> Iterator for All<'a, '_> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let want = self.section?;
        let rest = self.entries.get(self.at..)?;
        let hit = rest
            .iter()
            .position(|e| e.section == want && e.key.of(self.src).eq_ignore_ascii_case(self.key))?;
        self.at += hit + 1;
        Some(rest[hit].value.of(self.src))
    }
}

/// Every pair in one section.
pub struct InSection<'a> {
    entries: &'a [Entry],
    at: usize,
    section: Option<u16>,
}

impl<'a> Iterator for InSection<'a> {
    type Item = &'a Entry;

    fn next(&mut self) -> Option<&'a Entry> {
        let want = self.section?;
        let rest = self.entries.get(self.at..)?;
        let hit = rest.iter().position(|e| e.section == want)?;
        self.at += hit + 1;
        Some(&rest[hit])
    }
}

#[cfg(test)]
mod tests;
