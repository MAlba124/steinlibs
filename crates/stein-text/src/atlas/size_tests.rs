//! Several pixel sizes in one atlas.

use super::*;
use crate::test_font;

fn atlas(font: &[u8]) -> MonoAtlas<'_> {
    let config = AtlasConfig {
        px_per_em: 14.0,
        width: 1024,
        height: 1024,
        ..AtlasConfig::default()
    };
    MonoAtlas::new(font, config).expect("atlas")
}

#[test]
fn a_bigger_size_has_a_proportionally_bigger_cell() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    let big = a.add_size(28.0).expect("size");
    assert_eq!(big, 1);
    let (s0, s1) = (
        a.size_metrics(0).expect("0"),
        a.size_metrics(big).expect("1"),
    );
    assert_eq!(s0.px_per_em, 14.0);
    assert_eq!(s1.px_per_em, 28.0);
    let ratio = s1.cell_advance / s0.cell_advance;
    assert!((1.8..=2.2).contains(&ratio), "advance ratio {ratio}");
    assert!(s1.line_height > s0.line_height && s1.baseline > s0.baseline);
    assert_eq!(a.size_metrics(2), None);
}

#[test]
fn size_zero_is_the_config_and_unchanged() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    let before = (a.cell_advance(), a.line_height(), a.baseline());
    a.add_size(30.0).expect("size");
    let s0 = a.size_metrics(0).expect("0");
    assert_eq!((s0.cell_advance, s0.line_height, s0.baseline), before);
    assert_eq!(before, (a.cell_advance(), a.line_height(), a.baseline()));
    assert_eq!(
        a.glyph('M').expect("glyph"),
        a.glyph_sized(0, 'M').expect("glyph")
    );
}

#[test]
fn both_sizes_live_in_the_cache_at_once() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    let big = a.add_size(32.0).expect("size");
    let small_m = a.glyph('M').expect("glyph");
    let big_m = a.glyph_sized(big, 'M').expect("glyph");
    assert!(
        big_m.h > small_m.h && big_m.w > small_m.w,
        "{small_m:?} {big_m:?}"
    );
    assert_ne!((small_m.x, small_m.y), (big_m.x, big_m.y), "two pictures");
    let misses = a.stats().misses;
    assert_eq!(a.glyph_sized(big, 'M').expect("glyph"), big_m);
    assert_eq!(a.glyph('M').expect("glyph"), small_m);
    assert_eq!(a.stats().misses, misses, "both came from the cache");
    assert!(a.stats().hits >= 2);
}

#[test]
fn adding_a_size_twice_gives_the_same_id() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    assert_eq!(a.add_size(14.0).expect("size"), 0, "the config's own");
    let id = a.add_size(20.0).expect("size");
    assert_eq!(a.add_size(20.0).expect("size"), id);
    assert_eq!(
        a.add_size(20.000_001).expect("size"),
        id,
        "a float's last bit"
    );
}

#[test]
fn bad_sizes_and_too_many_are_refused() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    for px in [0.0, -3.0, f32::NAN, f32::INFINITY] {
        assert!(a.add_size(px).is_err(), "{px}");
    }
    for i in 1..MAX_SIZES {
        assert_eq!(a.add_size(20.0 + i as f32).expect("size"), i as u8);
    }
    assert!(matches!(a.add_size(99.0), Err(TextError::AtlasFull { .. })));
    assert!(a.glyph_sized(MAX_SIZES as u8, 'a').is_err(), "no such size");
}

#[test]
fn a_fallback_added_after_a_size_draws_at_it() {
    let Some(font) = test_font::load() else {
        return;
    };
    let Some(sans) = test_font::load_proportional() else {
        return;
    };
    let mut a = atlas(&font);
    let big = a.add_size(30.0).expect("size");
    a.add_fallback(&sans).expect("fallback");
    let face = a.faces() - 1;
    let small = a
        .glyph_of_sized(0, face, 'W')
        .expect("mapped")
        .expect("glyph");
    let large = a
        .glyph_of_sized(big, face, 'W')
        .expect("mapped")
        .expect("glyph");
    assert!(large.h > small.h, "{small:?} {large:?}");
    assert!(
        a.glyph_of_sized(big, face, '\u{10ffff}').is_none(),
        "unmapped"
    );
}

/// A glyph wider than the cell, a proportional `W` here, fits the cell
/// once `fit_cell` is on, and a glyph that already fits is left alone.
#[test]
fn fit_cell_squeezes_only_what_is_too_wide() {
    let (Some(mono), Some(sans)) = (test_font::load(), test_font::load_proportional()) else {
        return;
    };
    let make = |fit_cell| {
        let config = AtlasConfig {
            px_per_em: 28.0,
            width: 1024,
            height: 1024,
            fit_cell,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&mono, config).expect("atlas");
        a.add_fallback(&sans).expect("fallback");
        a
    };
    let (mut plain, mut fit) = (make(false), make(true));
    let cell = fit.cell_advance();
    let wide = |a: &mut MonoAtlas<'_>| a.glyph_of(1, 'W').expect("W").expect("drawn");
    let (w0, w1) = (wide(&mut plain), wide(&mut fit));
    assert!(w0.advance > cell * 1.1, "the test needs a wide W: {}", w0.advance);
    assert!(w1.w < w0.w, "squeezed {} from {}", w1.w, w0.w);
    assert!(w1.left as f32 + w1.w as f32 <= cell + 1.0, "{w1:?} in a {cell} cell");
    assert!((w1.advance - cell).abs() <= 1.0, "advance {} in a {cell} cell", w1.advance);
    let narrow = |a: &mut MonoAtlas<'_>| a.glyph('i').expect("i");
    let (n0, n1) = (narrow(&mut plain), narrow(&mut fit));
    assert_eq!((n0.w, n0.h, n0.left, n0.top), (n1.w, n1.h, n1.left, n1.top));
}

/// A face kept at its own width, icons say, is not squeezed.
#[test]
fn a_face_kept_at_its_width_is_not_squeezed() {
    let (Some(mono), Some(sans)) = (test_font::load(), test_font::load_proportional()) else {
        return;
    };
    let make = |fit_cell| {
        let config = AtlasConfig {
            px_per_em: 28.0,
            width: 1024,
            height: 1024,
            fit_cell,
            ..AtlasConfig::default()
        };
        let mut a = MonoAtlas::new(&mono, config).expect("atlas");
        a.add_fallback(&sans).expect("fallback");
        a
    };
    let mut plain = make(false);
    let mut kept = make(true);
    kept.keep_width(1);
    let wide = |a: &mut MonoAtlas<'_>| a.glyph_of(1, 'W').expect("W").expect("drawn");
    let (w0, w1) = (wide(&mut plain), wide(&mut kept));
    assert_eq!((w0.w, w0.h, w0.left), (w1.w, w1.h, w1.left));
}

#[test]
fn a_run_shaped_at_a_size_places_at_that_size() {
    let Some(font) = test_font::load() else {
        return;
    };
    let mut a = atlas(&font);
    let big = a.add_size(28.0).expect("size");
    let mut run = Vec::new();
    crate::Shaper::new(&font, 14.0).expect("shaper").shape("Hm", &mut run);
    let mut small = Vec::new();
    a.place(&run, [0.0, 0.0], &mut small).expect("place");
    let mut same = Vec::new();
    a.place_sized(0, &run, [0.0, 0.0], &mut same).expect("place");
    assert_eq!(small, same, "size 0 is what place draws");

    crate::Shaper::new(&font, 28.0).expect("shaper").shape("Hm", &mut run);
    let mut large = Vec::new();
    a.place_sized(big, &run, [0.0, 0.0], &mut large).expect("place");
    let height = |g: &[PositionedGlyph]| g[0].dst[3];
    let ratio = height(&large) / height(&small);
    assert!((1.8..=2.2).contains(&ratio), "glyph height ratio {ratio}");
    let baseline = |g: &[PositionedGlyph]| g[0].dst[1] + g[0].dst[3];
    assert!(baseline(&large) > baseline(&small), "the bigger size sits lower");
    assert!(a.place_sized(9, &run, [0.0, 0.0], &mut large).is_err(), "unknown size");
}
