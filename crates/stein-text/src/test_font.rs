//! Finding a real monospace font for the tests.
//!
//! Glyph tests need real outlines, and no font is checked into this repo. Every
//! test that needs one returns early when none is found. A machine without fonts
//! is a property of the machine, not a failure of this crate, and a test suite
//! that goes red on a bare CI container teaches nothing.
//!
//! Search order: `STEIN_TEST_FONT` (`STEIN_TEST_FONT_SANS` for the proportional
//! one), then `fc-match`, then a shallow scan of the usual font directories.
//! Nothing here hardcodes a store path.

use std::path::PathBuf;
use std::process::Command;

const FONT_DIRS: [&str; 3] = [
    "/run/current-system/sw/share/fonts",
    "/usr/share/fonts",
    "/usr/local/share/fonts",
];

/// Names worth preferring when scanning, most specific first.
const PREFERRED: [&str; 4] = ["DejaVuSansMono", "LiberationMono", "NotoSansMono", "Mono"];

/// Proportional fonts with kerning and mark positioning, exact file names
/// because `DejaVuSans` is also a prefix of the mono and bold files.
const PREFERRED_SANS: [&str; 3] = [
    "DejaVuSans.ttf",
    "LiberationSans-Regular.ttf",
    "NotoSans-Regular.ttf",
];

const MAX_DEPTH: u32 = 6;

/// Reads a monospace font's bytes, or `None` if the machine has none.
pub fn load() -> Option<Vec<u8>> {
    std::fs::read(find()?).ok()
}

/// Locates a monospace font file.
pub fn find() -> Option<PathBuf> {
    from_env("STEIN_TEST_FONT")
        .or_else(|| from_fontconfig("monospace"))
        .or_else(|| from_scan(&PREFERRED, false))
}

/// Reads a proportional font's bytes, or `None` if the machine has none.
pub fn load_proportional() -> Option<Vec<u8>> {
    std::fs::read(find_proportional()?).ok()
}

/// Locates a proportional font file.
pub fn find_proportional() -> Option<PathBuf> {
    from_env("STEIN_TEST_FONT_SANS")
        .or_else(|| from_fontconfig("sans-serif"))
        .or_else(|| from_scan(&PREFERRED_SANS, true))
}

/// A font fontconfig really has for `family`, as bytes and collection
/// index, or `None` when it answers with some other family.
pub fn load_family(family: &str) -> Option<(Vec<u8>, u32)> {
    let out = Command::new("fc-match")
        .args(["--format=%{family}\t%{file}\t%{index}", family])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.trim().split('\t');
    let got = parts.next()?;
    let path = parts.next()?;
    // The high half names a variable instance, the low half the face.
    let index = parts.next()?.parse::<u32>().ok()? & 0xffff;
    if !got.to_ascii_lowercase().contains(&family.to_ascii_lowercase()) {
        return None;
    }
    Some((std::fs::read(path).ok()?, index))
}

fn from_env(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var)?);
    p.is_file().then_some(p)
}

fn from_fontconfig(pattern: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["--format=%{file}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (path.is_file() && is_font(&path)).then_some(path)
}

/// Scans for a file whose name contains, or with `exact` equals, one of
/// `preferred`, most preferred first.
fn from_scan(preferred: &[&str], exact: bool) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots: Vec<PathBuf> = FONT_DIRS.iter().map(PathBuf::from).collect();
    if let Some(h) = home {
        roots.push(h.join(".local/share/fonts"));
        roots.push(h.join(".fonts"));
    }
    for root in roots {
        collect(&root, 0, &mut found);
    }
    for want in preferred {
        if let Some(p) = found.iter().find(|p| matches(file_name(p), want, exact)) {
            return Some(p.clone());
        }
    }
    None
}

fn collect(dir: &std::path::Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || out.len() > 4096 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => collect(&p, depth + 1, out),
            Ok(t) if t.is_file() && is_font(&p) => out.push(p),
            _ => {}
        }
    }
}

fn matches(name: &str, want: &str, exact: bool) -> bool {
    if exact {
        name == want
    } else {
        name.contains(want)
    }
}

fn is_font(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("ttf" | "otf" | "ttc")
    )
}

fn file_name(p: &std::path::Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_missing_font_is_not_a_failure() {
        // Documents the contract: `load` returns None instead of panicking.
        let _ = super::load();
        let _ = super::load_proportional();
    }
}
