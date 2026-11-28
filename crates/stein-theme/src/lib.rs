//! One palette for every stein app.
//!
//! ```text
//! Seeds (bg, fg, accent, eight hues) ──► Theme::from_seeds ──► every token
//! theme.ini: preset, then seeds, then any token ──► Theme
//! Theme ──► TableStyle::from(&theme), DockStyle::from(&theme), app palettes
//! ```
//!
//! A theme is a flat struct of ARGB colours. Eleven of them are seeds and
//! the rest are derived by mixing, so a whole theme restyles from three
//! lines of a file:
//!
//! ```ini
//! # ~/.config/stein/theme.ini
//! preset = dark
//!
//! [colours]
//! accent = #e5c07b
//! selection = mix(bg, accent, 30%)
//! menu = #202020
//!
//! [sizes]
//! scrollbar = 8
//!
//! [steinfiles]
//! folder = orange
//! ```
//!
//! Values are `#rgb`, `#rrggbb`, `#aarrggbb`, another token's name,
//! `mix(a, b, N%)` or `alpha(a, N%)`. A seed set in the file is applied
//! before derivation, so `accent = ...` moves the selection, the cursor
//! and the drop highlight with it. Any other token is applied after, as a
//! plain override. An app keeps its own colours in a [`palette!`] struct
//! and reads its own `[section]` of the same file.
//!
//! `STEIN_THEME=light` picks a preset for one run, and
//! `STEIN_THEME_FILE=path` reads another file.

pub mod color;
pub mod file;

pub use color::{alpha, argb, deepen, is_light, luma, mix, on, parse_hex};
pub use file::{Builder, Loaded, Problem, Source, ThemeError, load, load_app, path};

/// Colours looked up and written by name, which is how a file reaches
/// them. Implemented by [`Theme`] and by every [`palette!`] struct.
pub trait Palette {
    fn get(&self, name: &str) -> Option<u32>;
    fn slot(&mut self, name: &str) -> Option<&mut u32>;
    /// The colours in the order they are built.
    fn names(&self) -> &'static [&'static str];
}

/// `get`, `slot` and `NAMES` for a struct of `u32` colour fields.
#[doc(hidden)]
#[macro_export]
macro_rules! __by_name {
    ($ty:ident { $($f:ident),* $(,)? }) => {
        impl $ty {
            /// Every colour's name, as a file spells it.
            pub const NAMES: &'static [&'static str] = &[$(stringify!($f)),*];

            #[doc(hidden)]
            pub fn __get(&self, name: &str) -> Option<u32> {
                match name {
                    $(stringify!($f) => Some(self.$f),)*
                    _ => None,
                }
            }

            #[doc(hidden)]
            pub fn __slot(&mut self, name: &str) -> Option<&mut u32> {
                match name {
                    $(stringify!($f) => Some(&mut self.$f),)*
                    _ => None,
                }
            }
        }
    };
}

/// An app's own colours, each derived from the shared [`Theme`] and the
/// colours above it, and each overridable from the app's `[section]` of the
/// theme file.
///
/// ```
/// stein_theme::palette! {
///     /// What the window paints with.
///     pub struct Colours(t, own) {
///         bg: t.bg,
///         /// Folders in the listing.
///         folder: stein_theme::mix(t.orange, t.yellow, 128),
///         /// Follows `folder`, also when a file sets it.
///         folder_front: stein_theme::mix(own.folder, t.fg, 40),
///     }
/// }
/// let c = Colours::default();
/// assert_eq!(c.bg, stein_theme::Theme::default().bg);
/// ```
///
/// Fields are built top to bottom. `own` names the ones already built,
/// and a file's override of one is in place before the next is derived.
/// `(t)` alone works when no field needs another. Generates the struct,
/// `new(&Theme)`, `build`, `load(section)` which reads the user's file,
/// `Default` from the built-in theme, and [`Palette`].
#[macro_export]
macro_rules! palette {
    (
        $(#[$sm:meta])*
        $vis:vis struct $name:ident ($t:ident) { $($body:tt)* }
    ) => {
        $crate::palette! { $(#[$sm])* $vis struct $name ($t, _own) { $($body)* } }
    };
    (
        $(#[$sm:meta])*
        $vis:vis struct $name:ident ($t:ident, $own:ident) {
            $( $(#[$fm:meta])* $field:ident : $init:expr ),* $(,)?
        }
    ) => {
        $(#[$sm])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        $vis struct $name {
            $( $(#[$fm])* pub $field: u32, )*
        }

        impl $name {
            /// Every colour, derived from the shared theme.
            pub fn new($t: &$crate::Theme) -> $name {
                $name::build($t, &mut |_, _, _| None)
            }

            /// Every colour top to bottom, from `over` when it has one for
            /// the name, else derived. `over` sees the colours built so far
            /// and how many there are.
            #[allow(unused_assignments)]
            pub fn build(
                $t: &$crate::Theme,
                over: &mut dyn FnMut(&str, &$name, usize) -> Option<u32>,
            ) -> $name {
                let mut out = $name { $( $field: 0, )* };
                let mut done = 0usize;
                $(
                    out.$field = match over(stringify!($field), &out, done) {
                        Some(v) => v,
                        None => {
                            #[allow(unused_variables)]
                            let $own = &out;
                            $init
                        }
                    };
                    done += 1;
                )*
                out
            }

            /// The shared theme from the user's file, then this app's own
            /// `[section]` of it.
            pub fn load(section: &'static str) -> $crate::Loaded<$name> {
                $crate::load_app(section, $name::build)
            }
        }

        $crate::__by_name!($name { $($field),* });

        impl $crate::Palette for $name {
            fn get(&self, name: &str) -> Option<u32> {
                self.__get(name)
            }
            fn slot(&mut self, name: &str) -> Option<&mut u32> {
                self.__slot(name)
            }
            fn names(&self) -> &'static [&'static str] {
                $name::NAMES
            }
        }

        impl Default for $name {
            fn default() -> $name {
                $name::new(&$crate::Theme::default())
            }
        }
    };
}

/// What a theme is grown from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seeds {
    pub bg: u32,
    pub fg: u32,
    pub accent: u32,
    pub red: u32,
    pub orange: u32,
    pub yellow: u32,
    pub green: u32,
    pub cyan: u32,
    pub blue: u32,
    pub purple: u32,
    pub pink: u32,
    /// The terminal's sixteen, kept as given since programs print against
    /// what each index conventionally means.
    pub ansi: [u32; 16],
}

__by_name!(Seeds {
    bg,
    fg,
    accent,
    red,
    orange,
    yellow,
    green,
    cyan,
    blue,
    purple,
    pink
});

impl Palette for Seeds {
    fn names(&self) -> &'static [&'static str] {
        Seeds::NAMES
    }
    fn get(&self, name: &str) -> Option<u32> {
        self.__get(name)
            .or_else(|| ansi_index(name).map(|i| self.ansi[i]))
    }
    fn slot(&mut self, name: &str) -> Option<&mut u32> {
        match ansi_index(name) {
            Some(i) => Some(&mut self.ansi[i]),
            None => self.__slot(name),
        }
    }
}

/// Tango, what gnome-terminal ships and most output was eyeballed against.
const TANGO: [u32; 16] = [
    0xff2e_3436,
    0xffcc_0000,
    0xff4e_9a06,
    0xffc4_a000,
    0xff34_65a4,
    0xff75_507b,
    0xff06_989a,
    0xffd3_d7cf,
    0xff55_5753,
    0xffef_2929,
    0xff8a_e234,
    0xfffc_e94f,
    0xff72_9fcf,
    0xffad_7fa8,
    0xff34_e2e2,
    0xffee_eeec,
];

/// Tango's hues darkened to read on a light background.
const TANGO_LIGHT: [u32; 16] = [
    0xff2e_3436,
    0xffa4_0000,
    0xff3a_7404,
    0xff8f_7500,
    0xff20_4a87,
    0xff5c_3566,
    0xff05_7a7b,
    0xffba_bdb6,
    0xff55_5753,
    0xffcc_0000,
    0xff4e_9a06,
    0xffc4_a000,
    0xff34_65a4,
    0xff75_507b,
    0xff06_989a,
    0xff88_8a85,
];

impl Seeds {
    /// Blue grey with soft hues. The default.
    pub const DARK: Seeds = Seeds {
        bg: 0xff17_191e,
        fg: 0xffdc_e0e8,
        accent: 0xff5b_9cf0,
        red: 0xffe0_6c75,
        orange: 0xffd1_9a66,
        yellow: 0xffe5_c07b,
        green: 0xff98_c379,
        cyan: 0xff56_b6c2,
        blue: 0xff61_afef,
        purple: 0xffc6_78dd,
        pink: 0xffe0_8fbf,
        ansi: TANGO,
    };

    /// Near white paper with the same hues deepened.
    pub const LIGHT: Seeds = Seeds {
        bg: 0xfff8_f8fa,
        fg: 0xff2a_2d34,
        accent: 0xff2f_6fd6,
        red: 0xffc8_3b46,
        orange: 0xffb0_6a1d,
        yellow: 0xff9a_7a00,
        green: 0xff3c_8a2e,
        cyan: 0xff0f_8290,
        blue: 0xff2f_6fd6,
        purple: 0xff9a_3fb8,
        pink: 0xffc0_3f86,
        ansi: TANGO_LIGHT,
    };

    /// A preset by the name a file or `STEIN_THEME` gives.
    pub fn preset(name: &str) -> Option<Seeds> {
        match name {
            "dark" => Some(Seeds::DARK),
            "light" => Some(Seeds::LIGHT),
            _ => None,
        }
    }
}

/// Sizes the shared widgets take, before any scale factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    /// Grab width of a split divider.
    pub divider: f32,
    /// Thickness of a scroll bar.
    pub scrollbar: f32,
    /// Floor on a scroll thumb, so a long document keeps one worth grabbing.
    pub min_thumb: f32,
}

impl Sizes {
    pub const DEFAULT: Sizes = Sizes {
        divider: 5.0,
        scrollbar: 10.0,
        min_thumb: 24.0,
    };

    pub fn slot(&mut self, name: &str) -> Option<&mut f32> {
        match name {
            "divider" => Some(&mut self.divider),
            "scrollbar" => Some(&mut self.scrollbar),
            "min_thumb" => Some(&mut self.min_thumb),
            _ => None,
        }
    }
}

/// Every shared colour, ARGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// The window behind everything, and between panes.
    pub bg: u32,
    /// Body text.
    pub fg: u32,
    /// Focus, the active tab's edge, links to act on.
    pub accent: u32,
    pub red: u32,
    pub orange: u32,
    pub yellow: u32,
    pub green: u32,
    pub cyan: u32,
    pub blue: u32,
    pub purple: u32,
    pub pink: u32,
    /// Content: lists, tables, text views, the active tab.
    pub pane: u32,
    pub sidebar: u32,
    /// Toolbars over a pane.
    pub header: u32,
    /// Menus, popups, tooltips, transients.
    pub menu: u32,
    /// The status line.
    pub status: u32,
    /// Behind charts.
    pub plot: u32,
    pub hover: u32,
    /// Secondary text.
    pub dim: u32,
    /// Hints, disabled text, idle scroll thumbs.
    pub faint: u32,
    pub selection: u32,
    /// Selection where the keyboard is not.
    pub selection_idle: u32,
    pub cursor: u32,
    /// The edge of anything floating.
    pub border: u32,
    /// Lines between rows and sections.
    pub divider: u32,
    /// Search and fuzzy hits.
    pub matched: u32,
    pub good: u32,
    pub bad: u32,
    pub warn: u32,
    pub info: u32,
    pub link: u32,
    /// The fill of a button that destroys something.
    pub danger: u32,
    /// Text on an accent or danger fill.
    pub on_accent: u32,
    pub shadow: u32,
    /// Over the window behind a modal question.
    pub shroud: u32,
    /// Where a drag would land. Translucent.
    pub drop: u32,
    pub ansi: [u32; 16],
    pub size: Sizes,
}

__by_name!(Theme {
    bg,
    fg,
    accent,
    red,
    orange,
    yellow,
    green,
    cyan,
    blue,
    purple,
    pink,
    pane,
    sidebar,
    header,
    menu,
    status,
    plot,
    hover,
    dim,
    faint,
    selection,
    selection_idle,
    cursor,
    border,
    divider,
    matched,
    good,
    bad,
    warn,
    info,
    link,
    danger,
    on_accent,
    shadow,
    shroud,
    drop,
});

impl Palette for Theme {
    fn names(&self) -> &'static [&'static str] {
        Theme::NAMES
    }
    fn get(&self, name: &str) -> Option<u32> {
        self.__get(name)
            .or_else(|| ansi_index(name).map(|i| self.ansi[i]))
    }
    fn slot(&mut self, name: &str) -> Option<&mut u32> {
        match ansi_index(name) {
            Some(i) => Some(&mut self.ansi[i]),
            None => self.__slot(name),
        }
    }
}

impl Default for Theme {
    fn default() -> Theme {
        Theme::from_seeds(&Seeds::DARK)
    }
}

impl Theme {
    /// Every token from the seeds. Surfaces step from `bg` toward `fg`, so
    /// the same weights make a light theme from light seeds.
    pub fn from_seeds(s: &Seeds) -> Theme {
        let (bg, fg, accent) = (s.bg, s.fg, s.accent);
        Theme {
            bg,
            fg,
            accent,
            red: s.red,
            orange: s.orange,
            yellow: s.yellow,
            green: s.green,
            cyan: s.cyan,
            blue: s.blue,
            purple: s.purple,
            pink: s.pink,
            pane: mix(bg, fg, 7),
            sidebar: mix(bg, fg, 2),
            header: mix(bg, fg, 14),
            menu: mix(bg, fg, 18),
            status: deepen(bg, 26),
            plot: deepen(bg, 40),
            hover: mix(bg, fg, 19),
            dim: mix(fg, bg, 105),
            faint: mix(fg, bg, 189),
            selection: mix(bg, accent, 110),
            selection_idle: mix(bg, fg, 33),
            cursor: accent,
            border: mix(bg, fg, 46),
            divider: mix(bg, fg, 28),
            matched: s.yellow,
            good: s.green,
            bad: s.red,
            warn: s.orange,
            info: s.blue,
            link: s.cyan,
            danger: mix(s.red, bg, 40),
            on_accent: on(accent),
            shadow: 0x6000_0000,
            shroud: if is_light(bg) {
                0x4000_0000
            } else {
                0x6000_0000
            },
            drop: alpha(accent, 0x50),
            ansi: s.ansi,
            size: Sizes::DEFAULT,
        }
    }

    /// The eight hues in a fixed order, for anything that cycles colours:
    /// graph lanes, chart series.
    pub fn hues(&self) -> [u32; 8] {
        [
            self.blue,
            self.green,
            self.yellow,
            self.purple,
            self.red,
            self.cyan,
            self.pink,
            self.orange,
        ]
    }
}

/// `ansi0` to `ansi15`.
fn ansi_index(name: &str) -> Option<usize> {
    let n: usize = name.strip_prefix("ansi")?.parse().ok()?;
    (n < 16).then_some(n)
}

#[cfg(test)]
mod tests;
