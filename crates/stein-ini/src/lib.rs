//! INI configuration, as spans into the bytes you already have.
//!
//! Nothing is copied and nothing is allocated. A parse walks the file once
//! and writes offsets into arrays the caller supplies, so reading a config is
//! one read syscall and a linear scan.
//!
//! ```text
//! bytes ──► parse ──► Entry[]  ──► get("colours", "fg") ──► &[u8]
//!                     Span[]
//! ```
//!
//! # The dialect
//!
//! INI has no standard, so this one is written down rather than implied.
//!
//! ```ini
//! # a comment. so is a ; line.
//! shell = /bin/zsh
//!
//! [colours]
//! fg = #d3d7cf
//! prompt = "  two leading spaces  "
//! ```
//!
//! - `key = value`, one per line. `=` only, and the first one splits.
//! - Whitespace around a key and a value is not part of either.
//! - **A comment is a whole line.** A `#` or `;` starts one only as the first
//!   non-blank byte of a line, never after a value. Every other dialect ends a
//!   value at an inline `#`, which silently turns the extremely common
//!   `fg = #d3d7cf` into an empty value. Losing a colour to a comment rule is
//!   worse than losing the ability to annotate a line, so this dialect gives up
//!   the trailing comment.
//! - `"quoted values"` keep their leading and trailing whitespace, and take
//!   `\\`, `\"`, `\n`, `\t`, `\r` and `\0`. An unquoted value has no escapes at
//!   all, so a Windows path needs no doubling.
//! - `[section]` until the next one. Pairs above the first are in the root
//!   section, which is named `""`.
//! - A section named twice continues the first rather than shadowing it.
//! - Section and key names are ASCII case insensitive, which is the one part
//!   of INI every dialect does agree on.
//! - A repeated key is kept, not replaced. [`Ini::get`] returns the last, which
//!   makes a later line an override, and [`Ini::all`] returns every one, which
//!   is what a list of keybindings needs.
//!
//! # Errors point at a line
//!
//! A person wrote the file and a person has to fix it, so every failure
//! carries the 1-based line it happened on and [`IniError`] prints it.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod ini;

pub use ini::{
    All, Counts, Dialect, Entry, InSection, Ini, IniError, Sections, Span, Storage, count,
    count_with, parse, parse_with, unescape, unescape_len,
};
