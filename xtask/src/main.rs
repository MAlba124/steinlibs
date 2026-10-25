//! `cargo xtask check` runs what a change must pass, cheapest first: comments in plain
//! ASCII, rustfmt, clippy and rustdoc with warnings as errors, the tests (nextest when it is
//! installed, the doctests through cargo test since nextest runs none) and a build of every
//! fuzz target when cargo-fuzz is installed. Every feature is on, so the h3 client and the
//! json oracle are checked too.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Debug)]
enum XtaskError {
    Usage,
    Spawn(String, std::io::Error),
    Failed(String),
    Read(PathBuf, std::io::Error),
    NonAscii(Vec<String>),
}

impl fmt::Display for XtaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XtaskError::Usage => write!(f, "usage: cargo xtask check"),
            XtaskError::Spawn(step, e) => write!(f, "{step} did not start, {e}"),
            XtaskError::Failed(step) => write!(f, "{step} failed"),
            XtaskError::Read(path, e) => write!(f, "{} {e}", path.display()),
            XtaskError::NonAscii(at) => {
                write!(f, "comments with non-ASCII characters:")?;
                at.iter().try_for_each(|a| write!(f, "\n  {a}"))
            }
        }
    }
}

impl std::error::Error for XtaskError {}

fn main() -> ExitCode {
    let r = match std::env::args().nth(1).as_deref() {
        Some("check") => check(),
        _ => Err(XtaskError::Usage),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check() -> Result<(), XtaskError> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut bad = Vec::new();
    ascii(&root, &root, &mut bad)?;
    if !bad.is_empty() {
        return Err(XtaskError::NonAscii(bad));
    }
    let all = ["--workspace", "--all-features"];
    cargo(&root, "rustfmt", &["fmt", "--all", "--check"], None)?;
    let clippy = [
        &["clippy", "--all-targets"][..],
        &all,
        &["--", "-D", "warnings"],
    ]
    .concat();
    cargo(&root, "clippy", &clippy, None)?;
    let doc = [&["doc", "--no-deps"][..], &all].concat();
    cargo(
        &root,
        "rustdoc",
        &doc,
        Some(("RUSTDOCFLAGS", "-D warnings")),
    )?;
    if installed(&root, "nextest") {
        cargo(
            &root,
            "tests",
            &[&["nextest", "run"][..], &all].concat(),
            None,
        )?;
        cargo(
            &root,
            "doctests",
            &[&["test", "--doc"][..], &all].concat(),
            None,
        )?;
    } else {
        cargo(&root, "tests", &[&["test"][..], &all].concat(), None)?;
    }
    if installed(&root, "fuzz") {
        cargo(&root, "fuzz build", &["fuzz", "build"], None)?;
    } else {
        eprintln!("xtask: fuzz build skipped, cargo-fuzz is not installed");
    }
    eprintln!("xtask: all checks passed");
    Ok(())
}

/// The cargo that runs this, so a toolchain override holds for every step.
fn cargo_bin() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn cargo(
    root: &Path,
    step: &str,
    args: &[&str],
    env: Option<(&str, &str)>,
) -> Result<(), XtaskError> {
    eprintln!("xtask: {step}");
    let mut c = Command::new(cargo_bin());
    c.args(args).current_dir(root);
    if let Some((k, v)) = env {
        c.env(k, v);
    }
    let status = c.status().map_err(|e| XtaskError::Spawn(step.into(), e))?;
    match status.success() {
        true => Ok(()),
        false => Err(XtaskError::Failed(step.into())),
    }
}

/// Whether `cargo <sub>` is installed.
fn installed(root: &Path, sub: &str) -> bool {
    let mut c = Command::new(cargo_bin());
    c.args([sub, "--version"]).current_dir(root);
    c.output().is_ok_and(|o| o.status.success())
}

/// Comment lines with a non-ASCII character in the sources and manifests under `dir`, as
/// `path:line`. Test data in strings may be any UTF-8, a comment may not.
fn ascii(root: &Path, dir: &Path, bad: &mut Vec<String>) -> Result<(), XtaskError> {
    let entries = fs::read_dir(dir).map_err(|e| XtaskError::Read(dir.into(), e))?;
    for entry in entries {
        let path = entry.map_err(|e| XtaskError::Read(dir.into(), e))?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            let skip = name.starts_with('.')
                || ["target", "corpus", "artifacts", "coverage"].contains(&name);
            if !skip {
                ascii(root, &path, bad)?;
            }
            continue;
        }
        let mark = match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => "//",
            Some("toml" | "nix") => "#",
            _ => continue,
        };
        let text = fs::read_to_string(&path).map_err(|e| XtaskError::Read(path.clone(), e))?;
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with(mark) && !line.is_ascii() {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                bad.push(format!("{}:{}", rel.display(), n + 1));
            }
        }
    }
    Ok(())
}
