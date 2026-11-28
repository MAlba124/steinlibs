//! The theme file: a preset, seeds, overrides, sizes and per-app sections.
//!
//! ```text
//! $STEIN_THEME_FILE or $XDG_CONFIG_HOME/stein/theme.ini ──► Source
//! Source::theme: preset ──► seeds ──► from_seeds ──► overrides ──► sizes
//! Source::apply: an app's [section] over its palette
//! ```
//!
//! Read once at startup into a fixed buffer and parsed on the stack. A
//! missing file is the default theme and no error. A broken line is
//! skipped and reported, and every line after it still applies, so a typo
//! never costs the whole theme.

use core::fmt;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;

use stein_ini::{Entry, Ini, IniError, Span, Storage, parse};

use crate::color::{alpha, mix, parse_hex};
use crate::{Palette, Seeds, Theme};

/// Largest theme file read.
pub const MAX_FILE: usize = 32 << 10;
const MAX_ENTRIES: usize = 512;
const MAX_SECTIONS: usize = 64;
const MAX_PATH: usize = 4096;
/// Longest colour name, after `-` becomes `_`.
const MAX_NAME: usize = 32;
/// Both spellings, since neither is the wrong guess.
const COLOUR_SECTIONS: [&str; 2] = ["colours", "colors"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Read(std::io::ErrorKind),
    TooLarge,
    Syntax(IniError),
    UnknownPreset,
    UnknownName,
    BadColour,
    BadSize,
}

/// What was wrong, and on which line. Line 0 is the file as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeError {
    pub line: u32,
    pub problem: Problem,
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Problem::Syntax(e) = self.problem {
            return write!(f, "theme.ini {e}");
        }
        f.write_str("theme.ini")?;
        if self.line > 0 {
            write!(f, " line {}", self.line)?;
        }
        match self.problem {
            Problem::Read(k) => write!(f, ": cannot read it, {k}"),
            Problem::TooLarge => write!(f, ": larger than {} KiB", MAX_FILE >> 10),
            Problem::UnknownPreset => f.write_str(": no preset by that name, try dark or light"),
            Problem::UnknownName => f.write_str(": no colour or size by that name"),
            Problem::BadColour => {
                f.write_str(": not a colour, use #rrggbb, a name, mix(a, b, N%) or alpha(a, N%)")
            }
            Problem::BadSize => f.write_str(": not a size in pixels"),
            Problem::Syntax(_) => Ok(()),
        }
    }
}

impl core::error::Error for ThemeError {}

/// The shared theme and an app's palette, with the first thing wrong in
/// the file if anything was.
#[derive(Clone, Copy, Debug)]
pub struct Loaded<P> {
    pub theme: Theme,
    pub app: P,
    pub error: Option<ThemeError>,
    /// The app section read, which names the app in a report.
    pub section: &'static str,
}

impl<P> Loaded<P> {
    /// Say what was wrong with the file on stderr, where a person starting
    /// the app from a terminal sees it. The theme still loads.
    pub fn reported(self) -> Self {
        if let Some(e) = self.error {
            eprintln!("{}: {e}", self.section);
        }
        self
    }
}

/// The shared theme from the user's file. Never fails, a problem comes
/// back beside a theme that skipped it.
pub fn load() -> (Theme, Option<ThemeError>) {
    match Source::read() {
        Ok(s) => s.theme(),
        Err(e) => (Theme::default(), Some(e)),
    }
}

/// How a [`crate::palette!`] struct builds itself, taking each colour from
/// the override when it has one.
pub type Builder<P> = fn(&Theme, &mut dyn FnMut(&str, &P, usize) -> Option<u32>) -> P;

/// [`load`], then the app's palette built from the theme with its
/// `[section]` of the file.
pub fn load_app<P: Palette>(section: &'static str, build: Builder<P>) -> Loaded<P> {
    let src = Source::read();
    let (theme, mut error) = match &src {
        Ok(s) => s.theme(),
        Err(e) => (Theme::default(), Some(*e)),
    };
    let app = match &src {
        Ok(s) => {
            let (app, e) = s.build(section, &theme, build);
            error = error.or(e);
            app
        }
        Err(_) => build(&theme, &mut |_, _, _| None),
    };
    Loaded {
        theme,
        app,
        error,
        section,
    }
}

/// The file's bytes, and a preset the environment forces over it.
pub struct Source {
    buf: [u8; MAX_FILE],
    len: usize,
    preset: Option<Seeds>,
}

impl Source {
    /// The user's file, empty when there is none. `STEIN_THEME` names a
    /// preset that wins over the file's.
    pub fn read() -> Result<Source, ThemeError> {
        let mut s = Source {
            buf: [0; MAX_FILE],
            len: 0,
            preset: None,
        };
        if let Some(v) = std::env::var_os("STEIN_THEME") {
            let name = v.to_str().unwrap_or("");
            s.preset = Some(Seeds::preset(name).ok_or(whole(Problem::UnknownPreset))?);
        }
        let mut path = [0u8; MAX_PATH];
        let Some(n) = path_into(&mut path) else {
            return Ok(s);
        };
        let p = std::ffi::OsStr::from_bytes(&path[..n]);
        let mut f = match std::fs::File::open(p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(s),
            Err(e) => return Err(whole(Problem::Read(e.kind()))),
        };
        s.len = fill(&mut f, &mut s.buf)?;
        Ok(s)
    }

    /// Text as if it were the file, for tests and callers with their own.
    pub fn from_bytes(b: &[u8]) -> Result<Source, ThemeError> {
        if b.len() > MAX_FILE {
            return Err(whole(Problem::TooLarge));
        }
        let mut s = Source {
            buf: [0; MAX_FILE],
            len: b.len(),
            preset: None,
        };
        s.buf[..b.len()].copy_from_slice(b);
        Ok(s)
    }

    /// Force a preset over the file's, as `STEIN_THEME` does.
    pub fn with_preset(mut self, seeds: Seeds) -> Source {
        self.preset = Some(seeds);
        self
    }

    fn parse<R>(&self, f: impl FnOnce(&Ini<'_>) -> R) -> Result<R, ThemeError> {
        const BLANK: Entry = Entry {
            section: 0,
            key: Span::EMPTY,
            value: Span::EMPTY,
            line: 0,
            flags: 0,
        };
        let mut entries = [BLANK; MAX_ENTRIES];
        let mut sections = [Span::EMPTY; MAX_SECTIONS];
        let store = Storage {
            entries: &mut entries,
            sections: &mut sections,
        };
        match parse(&self.buf[..self.len], store) {
            Ok(ini) => Ok(f(&ini)),
            Err(e) => Err(ThemeError {
                line: e.line().unwrap_or(0),
                problem: Problem::Syntax(e),
            }),
        }
    }

    /// The theme the file describes, and the first line it had to skip.
    pub fn theme(&self) -> (Theme, Option<ThemeError>) {
        let mut first = None;
        let mut note = |line: u32, problem: Problem| {
            first = first.or(Some(ThemeError { line, problem }));
        };
        let r = self.parse(|ini| theme_of(ini, self.preset, &mut note));
        match r {
            Ok(t) => (t, first),
            Err(e) => (self.fallback(), Some(e)),
        }
    }

    /// The forced preset alone, for a file that would not parse.
    fn fallback(&self) -> Theme {
        Theme::from_seeds(&self.preset.unwrap_or(Seeds::DARK))
    }

    /// An app's palette with its `[section]` of the file. An override is
    /// in place before the colours after it derive, and its value may name
    /// the app's colours above it and any of the shared theme's.
    pub fn build<P: Palette>(
        &self,
        section: &str,
        base: &Theme,
        build: Builder<P>,
    ) -> (P, Option<ThemeError>) {
        let mut first = None;
        let r = self.parse(|ini| app_of(ini, section, base, build, &mut first));
        match r {
            Ok(app) => (app, first),
            Err(e) => (build(base, &mut |_, _, _| None), Some(e)),
        }
    }
}

/// Preset, seeds, derivation, overrides and sizes, in that order.
fn theme_of(ini: &Ini<'_>, preset: Option<Seeds>, note: &mut impl FnMut(u32, Problem)) -> Theme {
    let mut seeds = preset.unwrap_or(Seeds::DARK);
    if preset.is_none() {
        file_preset(ini, &mut seeds, note);
    }
    seed_pass(ini, &mut seeds, note);
    let mut theme = Theme::from_seeds(&seeds);
    override_pass(ini, &seeds, &mut theme, note);
    sizes(ini, &mut theme, note);
    theme
}

/// An app's palette built with `section`'s overrides, noting the first
/// line that could not apply.
fn app_of<P: Palette>(
    ini: &Ini<'_>,
    section: &str,
    base: &Theme,
    build: Builder<P>,
    first: &mut Option<ThemeError>,
) -> P {
    let mut over = |field: &str, own: &P, done: usize| {
        let (v, bad) = app_override(ini, section, field, own, done, base);
        *first = first.or(bad);
        v
    };
    let app = build(base, &mut over);
    *first = first.or(unknown_in(ini, section, &app));
    app
}

/// The file into `buf`, refusing one that does not fit.
fn fill(f: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, ThemeError> {
    let mut len = 0;
    while len < buf.len() {
        match f.read(&mut buf[len..]) {
            Ok(0) => return Ok(len),
            Ok(n) => len += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(e) => return Err(whole(Problem::Read(e.kind()))),
        }
    }
    // Full, so one more byte means too large.
    match f.read(&mut [0u8; 1]) {
        Ok(0) => Ok(len),
        Ok(_) => Err(whole(Problem::TooLarge)),
        Err(e) => Err(whole(Problem::Read(e.kind()))),
    }
}

fn file_preset(ini: &Ini<'_>, seeds: &mut Seeds, note: &mut impl FnMut(u32, Problem)) {
    let Some(e) = ini.entry("", "preset") else {
        return;
    };
    let mut b = [0u8; MAX_NAME];
    match name(ini.bytes(e.value), &mut b).and_then(Seeds::preset) {
        Some(s) => *seeds = s,
        None => note(e.line, Problem::UnknownPreset),
    }
}

/// Seeds first, so everything derived follows them.
fn seed_pass(ini: &Ini<'_>, seeds: &mut Seeds, note: &mut impl FnMut(u32, Problem)) {
    for e in colour_entries(ini) {
        let mut b = [0u8; MAX_NAME];
        let Some(key) = name(ini.bytes(e.key), &mut b).filter(|k| seeds.get(k).is_some()) else {
            continue;
        };
        let v = resolve(ini.bytes(e.value), &|n| seeds.get(n));
        set(seeds, key, v, e.line, note);
    }
}

/// Every other colour, over what the seeds derived.
fn override_pass(
    ini: &Ini<'_>,
    seeds: &Seeds,
    theme: &mut Theme,
    note: &mut impl FnMut(u32, Problem),
) {
    for e in colour_entries(ini) {
        let mut b = [0u8; MAX_NAME];
        let key = name(ini.bytes(e.key), &mut b);
        match key {
            Some(k) if seeds.get(k).is_some() => (),
            Some(k) if theme.get(k).is_some() => {
                let v = resolve(ini.bytes(e.value), &|n| theme.get(n));
                set(theme, k, v, e.line, note);
            }
            _ => note(e.line, Problem::UnknownName),
        }
    }
}

/// The last value `section` gives `field`, resolved against the app's
/// colours built so far and then the shared theme.
fn app_override<P: Palette>(
    ini: &Ini<'_>,
    section: &str,
    field: &str,
    own: &P,
    done: usize,
    base: &Theme,
) -> (Option<u32>, Option<ThemeError>) {
    let Some(e) = ini
        .in_section(section)
        .filter(|e| is_key(ini, e, field))
        .last()
    else {
        return (None, None);
    };
    let names = own.names();
    let look = |n: &str| {
        let built = names.iter().position(|x| *x == n).is_some_and(|i| i < done);
        built.then(|| own.get(n)).flatten().or_else(|| base.get(n))
    };
    let v = resolve(ini.bytes(e.value), &look);
    let bad = v.is_none().then_some(ThemeError {
        line: e.line,
        problem: Problem::BadColour,
    });
    (v, bad)
}

/// The first line of `section` naming no colour the app has.
fn unknown_in<P: Palette>(ini: &Ini<'_>, section: &str, app: &P) -> Option<ThemeError> {
    let e = ini.in_section(section).find(|e| {
        let mut b = [0u8; MAX_NAME];
        name(ini.bytes(e.key), &mut b)
            .and_then(|k| app.get(k))
            .is_none()
    })?;
    Some(ThemeError {
        line: e.line,
        problem: Problem::UnknownName,
    })
}

fn is_key(ini: &Ini<'_>, e: &Entry, field: &str) -> bool {
    let mut b = [0u8; MAX_NAME];
    name(ini.bytes(e.key), &mut b) == Some(field)
}

fn whole(problem: Problem) -> ThemeError {
    ThemeError { line: 0, problem }
}

fn set<P: Palette>(
    p: &mut P,
    key: &str,
    v: Option<u32>,
    line: u32,
    note: &mut impl FnMut(u32, Problem),
) {
    match (v, p.slot(key)) {
        (Some(c), Some(slot)) => *slot = c,
        _ => note(line, Problem::BadColour),
    }
}

fn colour_entries<'a>(ini: &'a Ini<'a>) -> impl Iterator<Item = &'a Entry> + 'a {
    COLOUR_SECTIONS.into_iter().flat_map(|s| ini.in_section(s))
}

fn sizes(ini: &Ini<'_>, theme: &mut Theme, note: &mut impl FnMut(u32, Problem)) {
    for e in ini.in_section("sizes") {
        let mut b = [0u8; MAX_NAME];
        let Some(slot) = name(ini.bytes(e.key), &mut b).and_then(|k| theme.size.slot(k)) else {
            note(e.line, Problem::UnknownName);
            continue;
        };
        let v = core::str::from_utf8(ini.bytes(e.value))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| (0.0..=512.0).contains(v));
        match v {
            Some(v) => *slot = v,
            None => note(e.line, Problem::BadSize),
        }
    }
}

/// A name as the tables spell it: lower case, `-` and spaces as `_`.
fn name<'b>(raw: &[u8], buf: &'b mut [u8; MAX_NAME]) -> Option<&'b str> {
    let raw = raw.trim_ascii();
    if raw.is_empty() || raw.len() > MAX_NAME {
        return None;
    }
    for (o, &c) in buf.iter_mut().zip(raw) {
        *o = match c {
            b'-' | b' ' => b'_',
            c => c.to_ascii_lowercase(),
        };
    }
    core::str::from_utf8(&buf[..raw.len()]).ok()
}

/// A value: a colour, a name, `mix(a, b, N%)` or `alpha(a, N%)`.
pub(crate) fn resolve(v: &[u8], look: &dyn Fn(&str) -> Option<u32>) -> Option<u32> {
    let v = v.trim_ascii();
    if let Some(args) = call(v, b"mix") {
        let mut it = args.split(|c| *c == b',');
        let a = atom(it.next()?, look)?;
        let b = atom(it.next()?, look)?;
        let w = weight(it.next()?)?;
        return it.next().is_none().then(|| mix(a, b, w));
    }
    if let Some(args) = call(v, b"alpha") {
        let mut it = args.split(|c| *c == b',');
        let a = atom(it.next()?, look)?;
        let w = weight(it.next()?)?;
        return it.next().is_none().then(|| alpha(a, w));
    }
    atom(v, look)
}

fn call<'v>(v: &'v [u8], f: &[u8]) -> Option<&'v [u8]> {
    let head = v.get(..f.len())?;
    if !head.eq_ignore_ascii_case(f) {
        return None;
    }
    v[f.len()..]
        .trim_ascii_start()
        .strip_prefix(b"(")?
        .strip_suffix(b")")
}

fn atom(v: &[u8], look: &dyn Fn(&str) -> Option<u32>) -> Option<u32> {
    let v = v.trim_ascii();
    if v.first() == Some(&b'#') || v.starts_with(b"0x") || v.starts_with(b"0X") {
        return parse_hex(v);
    }
    let mut b = [0u8; MAX_NAME];
    look(name(v, &mut b)?)
}

/// `40%` or `40` as a weight out of 255.
fn weight(v: &[u8]) -> Option<u8> {
    let v = v.trim_ascii();
    let v = v.strip_suffix(b"%").unwrap_or(v).trim_ascii();
    let p: f32 = core::str::from_utf8(v).ok()?.parse().ok()?;
    (0.0..=100.0).contains(&p).then(|| (p * 2.55).round() as u8)
}

/// Where the theme file is read from, for a caller watching it change.
pub fn path() -> Option<std::path::PathBuf> {
    let mut buf = [0u8; MAX_PATH];
    let n = path_into(&mut buf)?;
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n])))
}

/// `$STEIN_THEME_FILE`, else the XDG config location, into `out`.
fn path_into(out: &mut [u8; MAX_PATH]) -> Option<usize> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let mut n = 0usize;
    let mut push = |b: &[u8]| -> Option<()> {
        out.get_mut(n..n + b.len())?.copy_from_slice(b);
        n += b.len();
        Some(())
    };
    if let Some(p) = var("STEIN_THEME_FILE") {
        push(p.as_bytes())?;
    } else if let Some(x) = var("XDG_CONFIG_HOME") {
        push(x.as_bytes())?;
        push(b"/stein/theme.ini")?;
    } else {
        push(var("HOME")?.as_bytes())?;
        push(b"/.config/stein/theme.ini")?;
    }
    Some(n)
}
