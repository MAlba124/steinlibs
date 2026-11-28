//! Rasterization quality, not just rasterization.
//!
//! The shape tests in the crate check that a glyph has the right stems and holes.
//! They would all still pass if coverage were quantised to 0 and 255, which looks
//! terrible and is an easy regression to introduce while tuning a rasterizer.

use stein_text::{AtlasConfig, MonoAtlas};

fn font() -> Option<Vec<u8>> {
    if let Ok(p) = std::env::var("STEIN_TEST_FONT") {
        if let Ok(b) = std::fs::read(&p) {
            return Some(b);
        }
    }
    let out = std::process::Command::new("fc-match")
        .args(["--format=%{file}", "monospace"])
        .output()
        .ok()?;
    std::fs::read(String::from_utf8_lossy(&out.stdout).trim()).ok()
}

/// Counts of empty, partial and full coverage over one glyph's box.
fn coverage(a: &mut MonoAtlas<'_>, ch: char) -> (u32, u32, u32) {
    let e = a.glyph(ch).expect("glyph");
    let [aw, _] = a.atlas_size();
    let px = a.pixels();
    let (mut zero, mut partial, mut full) = (0, 0, 0);
    for row in 0..e.h {
        for col in 0..e.w {
            match px[((e.y + row) * aw + (e.x + col)) as usize] {
                0 => zero += 1,
                255 => full += 1,
                _ => partial += 1,
            }
        }
    }
    (zero, partial, full)
}

#[test]
fn curved_glyphs_have_substantial_partial_coverage() {
    let Some(f) = font() else {
        eprintln!("no font, skipping");
        return;
    };
    for px in [13.0f32, 15.0, 20.0, 32.0] {
        let mut a = MonoAtlas::new(
            &f,
            AtlasConfig {
                px_per_em: px,
                ..Default::default()
            },
        )
        .expect("atlas");
        for ch in ['e', 'o', 'S', 'a'] {
            let (_, partial, full) = coverage(&mut a, ch);
            assert!(
                partial > 0,
                "{ch} at {px}px has no partial coverage at all, antialiasing is gone"
            );
            // A curve at UI sizes is mostly edge. Far more full than partial
            // would mean the coverage is being rounded to black and white.
            assert!(
                partial * 2 > full,
                "{ch} at {px}px: {partial} partial vs {full} full, coverage looks quantised"
            );
        }
    }
}

#[test]
fn straight_stems_still_reach_full_coverage() {
    let Some(f) = font() else { return };
    let mut a = MonoAtlas::new(
        &f,
        AtlasConfig {
            px_per_em: 20.0,
            ..Default::default()
        },
    )
    .expect("atlas");
    // A vertical stem should have solid interior, or the glyph is a grey smear.
    let (_, _, full) = coverage(&mut a, 'H');
    assert!(full > 0, "'H' has no fully covered pixel");
}

#[test]
fn the_ellipsis_glyph_is_real() {
    let Some(f) = font() else { return };
    let mut a = MonoAtlas::new(
        &f,
        AtlasConfig {
            px_per_em: 15.0,
            ..Default::default()
        },
    )
    .expect("atlas");
    // An ellipsis that rasterizes blank is worse than no ellipsis: it silently
    // eats a character of budget and shows nothing.
    let e = a.glyph('\u{2026}').expect("ellipsis glyph");
    let [aw, _] = a.atlas_size();
    let px = a.pixels();
    let ink: u32 = (0..e.h)
        .flat_map(|r| (0..e.w).map(move |c| (r, c)))
        .map(|(r, c)| px[((e.y + r) * aw + (e.x + c)) as usize] as u32)
        .sum();
    println!("ellipsis {}x{} advance {} ink {}", e.w, e.h, e.advance, ink);
    assert!(ink > 0, "the ellipsis rasterized blank");
}
