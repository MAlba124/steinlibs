//! Text pipeline baselines: measure feeds layout every frame, warm layout is
//! per frame per string, rasterizing happens only on first sight.
//!
//! Red flags: measure not being trivially cheap, warm layout allocating or
//! approaching rasterize cost, or a single glyph miss costing enough that a
//! screenful of new text would blow a frame budget.
//!
//! Needs a real font. Resolution order: STEIN_TEST_FONT, then fc-match. With
//! neither the whole binary skips rather than panics, since a fontless
//! machine is a property of the machine.

// Bench setup failures are bugs in the bench, not conditions to handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;
use criterion::{Criterion, Throughput};
use std::hint::black_box;
use stein_text::{AtlasConfig, MonoAtlas, PositionedGlyph, ShapedGlyph, Shaper, classify, flow};

const SHORT: &str = "hello world";

fn long_text() -> String {
    let line = "fn solve(&mut self, viewport: [f32; 2]) -> Result<(), LayoutError> { 42..=99 }";
    let mut s = String::with_capacity(25 * (line.len() + 1));
    for _ in 0..25 {
        s.push_str(line);
        s.push('\n');
    }
    s
}

fn font_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("STEIN_TEST_FONT") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let out = std::process::Command::new("fc-match")
        .args(["--format=%{file}", "monospace"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = std::path::PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    p.is_file().then_some(p)
}

fn bench_measure(c: &mut Criterion, bytes: &[u8]) {
    let atlas = MonoAtlas::new(bytes, AtlasConfig::default()).expect("atlas");
    let long = long_text();
    let mut g = c.benchmark_group("measure");
    g.throughput(Throughput::Elements(SHORT.len() as u64));
    g.bench_function("short_11ch", |b| {
        b.iter(|| black_box(atlas.measure(black_box(SHORT))));
    });
    g.throughput(Throughput::Elements(long.len() as u64));
    g.bench_function("long_2000ch", |b| {
        b.iter(|| black_box(atlas.measure(black_box(&long))));
    });
    g.finish();
}

fn bench_layout_warm(c: &mut Criterion, bytes: &[u8]) {
    let mut atlas = MonoAtlas::new(bytes, AtlasConfig::default()).expect("atlas");
    let long = long_text();
    atlas.prepare(SHORT).expect("prepare");
    atlas.prepare(&long).expect("prepare");
    let mut out: Vec<PositionedGlyph> = Vec::with_capacity(4096);
    let mut g = c.benchmark_group("layout_warm");
    g.throughput(Throughput::Elements(SHORT.len() as u64));
    g.bench_function("short_11ch", |b| {
        b.iter(|| {
            atlas
                .layout(black_box(SHORT), black_box([2.0, 3.0]), &mut out)
                .expect("warm");
            black_box(out.len());
        });
    });
    g.throughput(Throughput::Elements(long.len() as u64));
    g.bench_function("long_2000ch", |b| {
        b.iter(|| {
            atlas
                .layout(black_box(&long), black_box([2.0, 3.0]), &mut out)
                .expect("warm");
            black_box(out.len());
        });
    });
    g.finish();
}

/// Cold path: every distinct glyph in the text rasterized from scratch.
/// Includes the reset, which the reset_cache number lets you subtract.
fn bench_cold(c: &mut Criterion, bytes: &[u8]) {
    let mut atlas = MonoAtlas::new(bytes, AtlasConfig::default()).expect("atlas");
    let long = long_text();
    let mut out: Vec<PositionedGlyph> = Vec::with_capacity(4096);
    let mut g = c.benchmark_group("layout_cold");
    g.bench_function("reset_cache_512", |b| {
        b.iter(|| {
            atlas.reset_cache();
            black_box(atlas.resident());
        });
    });
    g.bench_function("long_2000ch_all_misses", |b| {
        b.iter(|| {
            atlas.reset_cache();
            atlas
                .layout(black_box(&long), black_box([0.0, 0.0]), &mut out)
                .expect("cold");
            black_box(out.len());
        });
    });
    g.finish();
}

/// One glyph, always a miss. Small atlas so the per-iteration reset is noise
/// against the rasterize.
fn bench_glyph_miss(c: &mut Criterion, bytes: &[u8]) {
    let cfg = AtlasConfig {
        width: 64,
        height: 64,
        ..AtlasConfig::default()
    };
    let mut atlas = MonoAtlas::new(bytes, cfg).expect("atlas");
    let mut g = c.benchmark_group("glyph");
    g.bench_function("reset_cache_64", |b| {
        b.iter(|| {
            atlas.reset_cache();
            black_box(atlas.resident());
        });
    });
    g.bench_function("miss_rasterize_M_16px", |b| {
        b.iter(|| {
            atlas.reset_cache();
            black_box(atlas.glyph(black_box('M')).expect("glyph"));
        });
    });
    g.bench_function("hit_M_16px", |b| {
        b.iter(|| black_box(atlas.glyph(black_box('M')).expect("glyph")));
    });
    g.finish();
}

/// A 4 KB paragraph, shaped once, broken at a reader's column. Classifying
/// and breaking should each cost a small fraction of shaping it.
fn bench_flow(c: &mut Criterion, bytes: &[u8]) {
    let words = "the reader breaks each paragraph on its shaped advances, ";
    let text: String = words.repeat(4096 / words.len());
    let mut shaper = Shaper::new(bytes, 16.0).expect("shaper");
    let mut run: Vec<ShapedGlyph> = Vec::with_capacity(text.len());
    shaper.shape(&text, &mut run);
    let mut flags = Vec::with_capacity(run.len());
    classify(&text, &run, &mut flags);
    let mut g = c.benchmark_group("flow");
    g.throughput(Throughput::Elements(text.len() as u64));
    g.bench_function("paragraph_4k_480px", |b| {
        b.iter(|| black_box(flow(black_box(&run), black_box(&flags), 480.0).count()));
    });
    g.bench_function("classify_4k", |b| {
        b.iter(|| classify(black_box(&text), black_box(&run), &mut flags));
    });
    g.bench_function("shape_4k", |b| {
        b.iter(|| shaper.shape(black_box(&text), &mut run));
    });
    g.finish();
}

fn main() {
    let Some(path) = font_path() else {
        eprintln!("stein-text benches skipped, no monospace font found");
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!(
            "stein-text benches skipped, could not read {}",
            path.display()
        );
        return;
    };
    if MonoAtlas::new(&bytes, AtlasConfig::default()).is_err() {
        eprintln!(
            "stein-text benches skipped, {} did not parse",
            path.display()
        );
        return;
    }
    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .configure_from_args();
    bench_measure(&mut c, &bytes);
    bench_layout_warm(&mut c, &bytes);
    bench_cold(&mut c, &bytes);
    bench_glyph_miss(&mut c, &bytes);
    bench_flow(&mut c, &bytes);
    c.final_summary();
}
