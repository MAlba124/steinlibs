#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn from(text: &str) -> (Theme, Option<ThemeError>) {
    Source::from_bytes(text.as_bytes()).unwrap().theme()
}

#[test]
fn mixing_lands_on_the_ends() {
    assert_eq!(mix(0xff00_0000, 0xffff_ffff, 0), 0xff00_0000);
    assert_eq!(mix(0xff00_0000, 0xffff_ffff, 255), 0xffff_ffff);
    assert_eq!(mix(0xff00_0000, 0xffff_ffff, 128), 0xff80_8080);
    // Alpha mixes like any channel.
    assert_eq!(mix(0x0000_0000, 0xff00_0000, 255), 0xff00_0000);
}

#[test]
fn deepen_moves_away_from_the_middle() {
    assert!(luma(deepen(0xff30_3030, 64)) < 0x30);
    assert!(luma(deepen(0xffd0_d0d0, 64)) > 0xd0);
    assert_eq!(deepen(0x8030_3030, 64) >> 24, 0x80);
}

#[test]
fn hex_forms() {
    assert_eq!(parse_hex(b"#abc"), Some(0xffaa_bbcc));
    assert_eq!(parse_hex(b"#112233"), Some(0xff11_2233));
    assert_eq!(parse_hex(b"#80112233"), Some(0x8011_2233));
    assert_eq!(parse_hex(b"0x112233"), Some(0xff11_2233));
    assert_eq!(parse_hex(b"#12345"), None);
    assert_eq!(parse_hex(b"#gg0000"), None);
    assert_eq!(parse_hex(b"112233"), None);
}

#[test]
fn the_default_is_the_dark_preset_derived() {
    let t = Theme::default();
    assert_eq!(t, Theme::from_seeds(&Seeds::DARK));
    assert_eq!(t.cursor, t.accent);
    assert_eq!(t.drop >> 24, 0x50);
    // Surfaces step from bg toward fg in order.
    let steps = [t.bg, t.pane, t.header, t.menu, t.border];
    assert!(
        steps.windows(2).all(|w| luma(w[0]) <= luma(w[1])),
        "{steps:x?}"
    );
    assert!(luma(t.status) < luma(t.bg));
}

#[test]
fn a_light_theme_steps_the_other_way() {
    let t = Theme::from_seeds(&Seeds::LIGHT);
    assert!(is_light(t.bg));
    assert!(luma(t.header) < luma(t.bg));
    assert!(luma(t.status) > luma(t.bg) || t.bg == 0xffff_ffff);
    assert!(luma(t.dim) > luma(t.fg));
}

#[test]
fn every_name_reads_and_writes() {
    let mut t = Theme::default();
    for n in Theme::NAMES {
        assert!(t.get(n).is_some(), "{n}");
        *t.slot(n).unwrap() = 0x1234_5678;
        assert_eq!(t.get(n), Some(0x1234_5678), "{n}");
    }
    assert_eq!(t.get("ansi15"), Some(Seeds::DARK.ansi[15]));
    assert_eq!(t.get("ansi16"), None);
    assert_eq!(t.get("nope"), None);
}

#[test]
fn an_empty_file_is_the_default() {
    assert_eq!(from(""), (Theme::default(), None));
}

#[test]
fn a_seed_moves_what_derives_from_it() {
    let (t, e) = from("[colours]\naccent = #ff0000\n");
    assert_eq!(e, None);
    assert_eq!(t.accent, 0xffff_0000);
    assert_eq!(t.cursor, 0xffff_0000);
    assert_eq!(t.selection, mix(t.bg, 0xffff_0000, 110));
    assert_eq!(t.drop, 0x50ff_0000);
}

#[test]
fn an_override_after_derivation_stays() {
    let (t, e) = from("[colours]\nselection = #010203\naccent = blue\n");
    assert_eq!(e, None);
    assert_eq!(t.selection, 0xff01_0203);
    assert_eq!(t.accent, Seeds::DARK.blue);
}

#[test]
fn values_can_mix_and_fade() {
    let (t, e) = from(
        "[colors]\nhover = mix(bg, #ffffff, 50%)\nshroud = alpha(bg, 25)\nmenu = MIX ( fg , bg , 0 )\n",
    );
    assert_eq!(e, None);
    assert_eq!(t.hover, mix(t.bg, 0xffff_ffff, 128));
    assert_eq!(t.shroud, alpha(t.bg, 64));
    assert_eq!(t.menu, t.fg);
}

#[test]
fn names_ignore_case_and_dashes() {
    let (t, e) = from("[Colours]\nSelection-Idle = #000001\n");
    assert_eq!(e, None);
    assert_eq!(t.selection_idle, 0xff00_0001);
}

#[test]
fn a_preset_by_name() {
    let (t, e) = from("preset = light\n");
    assert_eq!(e, None);
    assert_eq!(t, Theme::from_seeds(&Seeds::LIGHT));
    let forced = Source::from_bytes(b"preset = light\n")
        .unwrap()
        .with_preset(Seeds::DARK);
    assert_eq!(forced.theme().0, Theme::default());
}

#[test]
fn a_bad_line_is_reported_and_the_rest_applies() {
    let (t, e) = from("preset = sepia\n[colours]\nfg = #zzz\nbogus = #fff\nbg = #000000\n");
    assert_eq!(
        e,
        Some(ThemeError {
            line: 1,
            problem: Problem::UnknownPreset
        })
    );
    assert_eq!(t.bg, 0xff00_0000);
    assert_eq!(t.fg, Seeds::DARK.fg);
    let (_, e) = from("[colours]\nbogus = #fff\n");
    assert_eq!(
        e.map(|e| (e.line, e.problem)),
        Some((2, Problem::UnknownName))
    );
    let (_, e) = from("[colours]\nfg = mix(bg, fg)\n");
    assert_eq!(e.map(|e| e.problem), Some(Problem::BadColour));
    let (_, e) = from("[colours]\naccent = selection\n");
    assert_eq!(
        e.map(|e| e.problem),
        Some(Problem::BadColour),
        "a seed cannot name a derived token"
    );
}

#[test]
fn a_file_that_will_not_parse_falls_back() {
    let (t, e) = from("[colours\n");
    assert_eq!(t, Theme::default());
    assert!(matches!(e.map(|e| e.problem), Some(Problem::Syntax(_))));
    assert!(e.unwrap().to_string().contains("line 1"));
}

#[test]
fn sizes_parse_and_refuse_nonsense() {
    let (t, e) = from("[sizes]\nscrollbar = 7\nmin-thumb = 30.5\n");
    assert_eq!(e, None);
    assert_eq!(t.size.scrollbar, 7.0);
    assert_eq!(t.size.min_thumb, 30.5);
    let (t, e) = from("[sizes]\ndivider = -3\n");
    assert_eq!(e.map(|e| e.problem), Some(Problem::BadSize));
    assert_eq!(t.size.divider, Sizes::DEFAULT.divider);
}

#[test]
fn ansi_is_a_seed() {
    let (t, e) = from("[colours]\nansi1 = #ff0000\nansi9 = ansi1\n");
    assert_eq!(e, None);
    assert_eq!(t.ansi[1], 0xffff_0000);
    assert_eq!(t.ansi[9], 0xffff_0000);
}

palette! {
    /// A test app's colours.
    pub struct App(t, own) {
        bg: t.bg,
        folder: mix(t.orange, t.yellow, 128),
        /// Follows folder.
        folder_front: mix(own.folder, 0xffff_ffff, 64),
    }
}

palette! {
    pub struct Plain(t) {
        fg: t.fg,
    }
}

fn app(file: &[u8]) -> (Theme, App, Option<ThemeError>) {
    let src = Source::from_bytes(file).unwrap();
    let (theme, e) = src.theme();
    assert_eq!(e, None);
    let (app, e) = src.build("app", &theme, App::build);
    (theme, app, e)
}

#[test]
fn an_app_palette_derives_from_the_theme_and_itself() {
    let (theme, a, e) = app(b"[colours]\nyellow = #ffff00\n");
    assert_eq!(e, None);
    assert_eq!(a, App::new(&theme));
    assert_eq!(a.folder, mix(theme.orange, 0xffff_ff00, 128));
    assert_eq!(a.folder_front, mix(a.folder, 0xffff_ffff, 64));
    assert_eq!(App::NAMES, &["bg", "folder", "folder_front"]);
    assert_eq!(Plain::default().fg, Theme::default().fg);
    let _ = (App::load, Plain::load);
}

#[test]
fn an_override_carries_to_what_derives_from_it() {
    let (theme, a, e) = app(b"[app]\nfolder = green\n");
    assert_eq!(e, None);
    assert_eq!(a.folder, theme.green);
    assert_eq!(a.folder_front, mix(theme.green, 0xffff_ffff, 64));
}

#[test]
fn an_app_value_names_its_own_earlier_colours() {
    let (theme, a, e) = app(b"[app]\nbg = #010101\nfolder = mix(bg, fg, 50%)\n");
    assert_eq!(e, None);
    assert_eq!(a.folder, mix(0xff01_0101, theme.fg, 128));
    // A later colour is not built yet, so the name falls to the shared theme.
    let (_, a, e) = app(b"[app]\nfolder = bg\nbg = #020202\n");
    assert_eq!(e, None);
    assert_eq!(a.folder, 0xff02_0202, "bg comes first in the struct");
    let (_, _, e) = app(b"[app]\nbg = folder\n");
    assert_eq!(e.map(|e| e.problem), Some(Problem::BadColour));
}

#[test]
fn an_app_section_reports_unknown_names() {
    let (_, a, e) = app(b"[app]\nnope = #fff\nfolder = #000\n");
    assert_eq!(
        e.map(|e| (e.line, e.problem)),
        Some((2, Problem::UnknownName))
    );
    assert_eq!(a.folder, 0xff00_0000, "the rest still applies");
}

#[test]
fn an_oversized_file_is_refused() {
    let big = vec![b'#'; file::MAX_FILE + 1];
    assert_eq!(
        Source::from_bytes(&big).err().map(|e| e.problem),
        Some(Problem::TooLarge)
    );
}
