use std::vec::Vec;

use super::*;
use crate::shape::advance_of;
use crate::test_font;

/// Face 0 has ASCII, face 1 has CJK and ASCII, face 2 has everything.
fn fake(face: usize, ch: char) -> bool {
    match face {
        0 => ch.is_ascii(),
        1 => ch.is_ascii() || ('\u{3000}'..='\u{9fff}').contains(&ch),
        _ => true,
    }
}

fn runs(text: &str) -> Vec<(u16, &str)> {
    let mut out = Vec::new();
    face_runs(text, 3, fake, |r| {
        out.push((r.face, &text[r.start as usize..r.end as usize]));
    });
    out
}

#[test]
fn empty_text_has_no_runs() {
    assert!(runs("").is_empty());
}

#[test]
fn one_font_is_one_run() {
    assert_eq!(runs("readme.txt"), [(0, "readme.txt")]);
}

#[test]
fn latin_after_cjk_goes_back_to_the_primary() {
    assert_eq!(
        runs("ab\u{65e5}\u{672c}cd"),
        [(0, "ab"), (1, "\u{65e5}\u{672c}"), (0, "cd")]
    );
}

#[test]
fn spaces_and_punctuation_stay_in_their_run() {
    assert_eq!(
        runs("\u{65e5} \u{672c}.x"),
        [(1, "\u{65e5} \u{672c}."), (0, "x")]
    );
}

#[test]
fn a_mark_stays_with_its_base() {
    // U+0301 is not ASCII, so face 0 lacks it, yet it must not split.
    assert_eq!(runs("e\u{301}f"), [(0, "e\u{301}f")]);
}

#[test]
fn a_leading_mark_starts_nothing() {
    assert_eq!(runs("\u{301}ab"), [(0, "\u{301}ab")]);
}

#[test]
fn unmapped_characters_stay_in_the_current_run() {
    let mut out = Vec::new();
    face_runs("a\u{1f600}b", 2, fake, |r| out.push(r.face));
    assert_eq!(out, [0]);
}

#[test]
fn runs_tile_the_text() {
    let text = "x\u{65e5}y \u{1f600}\u{65e5}\u{301}.z";
    let mut end = 0u32;
    face_runs(text, 3, fake, |r| {
        assert_eq!(r.start, end);
        assert!(r.end > r.start);
        end = r.end;
    });
    assert_eq!(end as usize, text.len());
}

#[test]
fn chain_tags_glyphs_with_their_font() {
    let Some(sans) = test_font::load_proportional() else {
        return;
    };
    let Some((cjk, index)) = test_font::load_family("Noto Sans CJK SC") else {
        return;
    };
    let config = crate::AtlasConfig::default();
    let mut atlas = MonoAtlas::new(&sans, config).expect("atlas");
    atlas.add_fallback_at(&cjk, index).expect("fallback");
    if atlas.covers(0, '\u{65e5}') {
        return;
    }
    let mut chain = ShaperChain::new(&[(&sans, 0), (&cjk, index)], 16.0).expect("chain");
    let mut out = Vec::new();
    chain.shape(&atlas, "a\u{65e5}b", &mut out);
    let faces: Vec<u16> = out.iter().map(|g| g.face).collect();
    let clusters: Vec<u32> = out.iter().map(|g| g.cluster).collect();
    assert_eq!(faces, [0, 1, 0]);
    assert_eq!(clusters, [0, 1, 4]);
    assert!(
        out.iter().all(|g| g.glyph_id != 0),
        "a notdef slipped through"
    );
    let mut placed = Vec::new();
    atlas.place(&out, [0.0, 0.0], &mut placed).expect("place");
    assert_eq!(placed.len(), 3);
}

#[test]
fn a_mapped_chain_tags_the_atlas_faces() {
    let (Some(mono), Some(sans)) = (test_font::load(), test_font::load_proportional()) else {
        return;
    };
    // A grid font first, prose after it, prose falling back to the grid.
    let mut atlas = MonoAtlas::new(&mono, crate::AtlasConfig::default()).expect("atlas");
    atlas.add_fallback(&sans).expect("fallback");
    let mut chain =
        ShaperChain::for_faces(&[(&sans, 0), (&mono, 0)], &[1, 0], 16.0).expect("chain");
    let mut out = Vec::new();
    chain.shape(&atlas, "prose", &mut out);
    assert!(
        out.iter().all(|g| g.face == 1),
        "prose is shaped with face 1"
    );
    let mut grid = Vec::new();
    ShaperChain::new(&[(&mono, 0)], 16.0)
        .expect("chain")
        .shape(&atlas, "prose", &mut grid);
    assert_ne!(
        advance_of(&out),
        advance_of(&grid),
        "shaped with the wrong font"
    );
    let mut placed = Vec::new();
    atlas.place(&out, [0.0, 0.0], &mut placed).expect("place");
    assert_eq!(placed.len(), 5);
}

#[test]
fn a_map_must_name_every_font() {
    let Some(sans) = test_font::load_proportional() else {
        return;
    };
    let got = ShaperChain::for_faces(&[(&sans, 0)], &[0, 1], 16.0);
    assert!(matches!(got.map(|_| ()), Err(TextError::InvalidFont)));
}
