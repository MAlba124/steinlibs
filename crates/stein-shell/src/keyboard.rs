//! Turning evdev scancodes into keysyms and text.
//!
//! `wl_keyboard` hands back a scancode and a keymap file descriptor and nothing
//! else. A scancode says which switch moved; what it means depends on the
//! user's layout, the modifiers held, and whether a dead key is pending. That
//! resolution is what xkbcommon exists for, and a shortcut table built on raw
//! scancodes is wrong on every keyboard that is not US QWERTY.
//!
//! Dead keys are a second step on top of the keymap. The keymap only says
//! that a key is `dead_tilde`; what it makes with the key after it is in the
//! locale's compose table, and a layout that types `~` through a dead key
//! cannot type one at all without it.

use std::ffi::OsString;
use std::os::fd::OwnedFd;

use stein_ui::Mods;
use xkbcommon::xkb;
use xkbcommon::xkb::compose;

#[derive(Debug)]
pub enum KeyboardError {
    /// The compositor sent a keymap xkbcommon would not accept.
    BadKeymap,
    Mmap(std::io::Error),
}

impl std::fmt::Display for KeyboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyboardError::BadKeymap => write!(f, "compositor sent an unusable keymap"),
            KeyboardError::Mmap(e) => write!(f, "mapping the keymap: {e}"),
        }
    }
}
impl std::error::Error for KeyboardError {}

pub struct Keyboard {
    state: xkb::State,
    /// Dead key and compose sequences. `None` when the locale has no table,
    /// and then every key stands for itself.
    compose: Option<compose::State>,
}

/// What a press came to once the compose table had its say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compose {
    /// No sequence in flight, the key stands for itself.
    Plain,
    /// The key went into a sequence that is not finished.
    Pending,
    /// The key finished a sequence, see [`Keyboard::take_composed`].
    Done,
    /// The key broke a sequence, which is dropped along with it.
    Dropped,
}

/// The locale the compose table is looked up by, in libc's order.
fn locale() -> OsString {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("C"))
}

/// A fresh compose state for the locale, falling back to the one table
/// every install has.
fn compose_state(ctx: &xkb::Context) -> Option<compose::State> {
    let table = compose::Table::new_from_locale(ctx, &locale(), compose::COMPILE_NO_FLAGS)
        .or_else(|()| {
            let fallback = OsString::from("en_US.UTF-8");
            compose::Table::new_from_locale(ctx, &fallback, compose::COMPILE_NO_FLAGS)
        })
        .ok()?;
    Some(compose::State::new(&table, compose::STATE_NO_FLAGS))
}

/// Copy `s` into `buf` as text to insert, refusing control characters and
/// anything that does not fit.
fn text_into<'a>(s: &str, buf: &'a mut [u8; 8]) -> Option<&'a str> {
    if s.is_empty() || s.len() > buf.len() || s.chars().any(|c| c.is_control()) {
        return None;
    }
    buf[..s.len()].copy_from_slice(s.as_bytes());
    core::str::from_utf8(&buf[..s.len()]).ok()
}

impl Keyboard {
    /// Build from the compositor's keymap fd.
    pub fn from_fd(fd: OwnedFd, size: usize) -> Result<Self, KeyboardError> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // SAFETY: the compositor promises a readable, NUL-terminated XKB v1
        // keymap of `size` bytes at this fd, which is the protocol's contract.
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &ctx,
                fd,
                size,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        }
        .map_err(KeyboardError::Mmap)?
        .ok_or(KeyboardError::BadKeymap)?;
        Ok(Keyboard {
            state: xkb::State::new(&keymap),
            compose: compose_state(&ctx),
        })
    }

    /// A US layout with no compositor, for tests of the key bookkeeping.
    #[cfg(test)]
    pub(crate) fn us() -> Option<Self> {
        Keyboard::named("us")
    }

    /// Any layout by name with no compositor.
    #[cfg(test)]
    pub(crate) fn named(layout: &str) -> Option<Self> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        Some(Keyboard {
            state: xkb::State::new(&keymap),
            compose: compose_state(&ctx),
        })
    }

    /// Whether dead keys can be resolved at all.
    #[cfg(test)]
    pub(crate) fn composes(&self) -> bool {
        self.compose.is_some()
    }

    /// The mask bit of a named modifier, as `wl_keyboard.modifiers` sends it.
    #[cfg(test)]
    pub(crate) fn mask(&self, name: &str) -> u32 {
        1 << self.state.get_keymap().mod_get_index(name)
    }

    /// Apply a `wl_keyboard.modifiers` event.
    pub fn update(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.state
            .update_mask(depressed, latched, locked, 0, 0, group);
    }

    /// The keysym for an xkb keycode, which is the evdev code plus 8.
    pub fn sym(&self, keycode: u32) -> u32 {
        self.state.key_get_one_sym(keycode.into()).raw()
    }

    /// The text a keycode produces, or `None` when it produces none.
    ///
    /// Writes into the caller's buffer so a keystroke costs no allocation.
    /// Control characters are rejected: Return and Backspace are keys to act
    /// on, not text to insert, and inserting their C0 bytes into a field is a
    /// classic way to corrupt one.
    pub fn text<'a>(&self, keycode: u32, buf: &'a mut [u8; 8]) -> Option<&'a str> {
        text_into(&self.state.key_get_utf8(keycode.into()), buf)
    }

    /// Run a press through the compose table. Only presses are fed, a
    /// release or a repeat would count as a second key of the sequence.
    pub fn feed(&mut self, keycode: u32) -> Compose {
        let sym = self.state.key_get_one_sym(keycode.into());
        let Some(c) = self.compose.as_mut() else {
            return Compose::Plain;
        };
        // A key the table ignores, a modifier, leaves a sequence as it was.
        c.feed(sym);
        match c.status() {
            compose::Status::Nothing => Compose::Plain,
            compose::Status::Composing => Compose::Pending,
            compose::Status::Composed => Compose::Done,
            compose::Status::Cancelled => {
                c.reset();
                Compose::Dropped
            }
        }
    }

    /// The keysym and text of the sequence that just finished, taken once.
    pub fn take_composed<'a>(&mut self, buf: &'a mut [u8; 8]) -> (Option<u32>, Option<&'a str>) {
        let Some(c) = self.compose.as_mut() else {
            return (None, None);
        };
        if c.status() != compose::Status::Composed {
            return (None, None);
        }
        let sym = c.keysym().map(|k| k.raw());
        let text = c.utf8();
        c.reset();
        (sym, text.and_then(|t| text_into(&t, buf)))
    }

    /// Forget a sequence in flight, which is what losing focus does to it.
    pub fn reset_compose(&mut self) {
        if let Some(c) = self.compose.as_mut() {
            c.reset();
        }
    }

    /// Whether holding this key should repeat.
    ///
    /// The keymap decides, not the application. Modifiers, locks and function
    /// keys are marked as not repeating, and a client that repeats everything
    /// turns a held Shift into a stream of events.
    pub fn repeats(&self, keycode: u32) -> bool {
        self.state.get_keymap().key_repeats(keycode.into())
    }

    /// Modifiers currently in effect, either held or latched.
    pub fn mods(&self) -> Mods {
        let active = |name: &str| {
            self.state
                .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
        };
        Mods {
            ctrl: active(xkb::MOD_NAME_CTRL),
            shift: active(xkb::MOD_NAME_SHIFT),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
        }
    }
}
