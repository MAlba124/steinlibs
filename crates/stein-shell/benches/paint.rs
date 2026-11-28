//! Software painter baselines: the blend this replaced, the scalar reference
//! and the SIMD kernel.
//!
//! Two shapes. `blend_row` is the kernel alone at glyph widths, where the
//! eight pixel step and the widened tail decide everything. `screen` is a
//! terminal's worth of glyphs, painted through the public entry point for
//! the two current paths and by a direct row loop for the legacy one, which
//! is what a full-screen scroll costs. Red flags: the scalar reference not
//! clearing the legacy blend by a wide margin (it is written to
//! autovectorize, the legacy one branches per texel), or the screen number
//! not tracking the row number, which would mean the per glyph setup is
//! what is being paid for.
//!
//! In the nix dev shell run with `CC=gcc` set inside the shell. Criterion's
//! `alloca` shim adds `-flto` whenever the C compiler is called clang, and
//! the BFD linker cannot read the bitcode that produces.

// Bench setup failures are bugs in the bench, not conditions to handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use stein_layout::Rect;
use stein_shell::Blend;
use stein_shell::blend::{LANES, Span, blend_span};
use stein_shell::paint::{Atlas, BG, paint};
use stein_ui::draw::Quad;

/// xorshift64, so every run paints the same frame.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn paths() -> [(&'static str, Blend); 2] {
    [("scalar", Blend::Scalar), ("simd", Blend::detect())]
}

/// The blend this kernel replaced, kept as the baseline: a signed lerp with
/// a truncating divide and a branch per texel on zero coverage.
fn blend_row_legacy(dst: &mut [u32], cov: &[u8], color: u32) {
    let (fr, fg, fb) = (
        ((color >> 16) & 0xff) as i32,
        ((color >> 8) & 0xff) as i32,
        (color & 0xff) as i32,
    );
    for (d, &a) in dst.iter_mut().zip(cov) {
        let a = a as i32;
        if a == 0 {
            continue;
        }
        let (br, bg, bb) = (
            ((*d >> 16) & 0xff) as i32,
            ((*d >> 8) & 0xff) as i32,
            (*d & 0xff) as i32,
        );
        let r8 = br + ((fr - br) * a + 127) / 255;
        let g8 = bg + ((fg - bg) * a + 127) / 255;
        let b8 = bb + ((fb - bb) * a + 127) / 255;
        *d = ((r8 as u32) << 16) | ((g8 as u32) << 8) | b8 as u32;
    }
}

fn bench_row(c: &mut Criterion) {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    let mut g = c.benchmark_group("blend_row");
    // A step of slack after each row, as a framebuffer has, so the widened
    // tail takes its normal path rather than the end of buffer fallback. The
    // span carries the width; the slices stay long.
    let slack = LANES;
    for width in [7usize, 8, 12, 64] {
        let cov: Vec<u8> = (0..width + slack).map(|_| rng.next() as u8).collect();
        let base: Vec<u32> = (0..width + slack)
            .map(|_| rng.next() as u32 & 0x00ff_ffff)
            .collect();
        let span = Span {
            dst: 0,
            dst_stride: width,
            src: 0,
            src_stride: width,
            width,
            rows: 1,
        };
        g.throughput(Throughput::Elements(width as u64));
        let mut dst = base.clone();
        g.bench_with_input(BenchmarkId::new("legacy", width), &width, |b, _| {
            b.iter(|| {
                blend_row_legacy(
                    black_box(&mut dst[..width]),
                    black_box(&cov[..width]),
                    0x00d8_d8e4,
                );
            });
        });
        for (name, blend) in paths() {
            let mut dst = base.clone();
            g.bench_with_input(BenchmarkId::new(name, width), &width, |b, _| {
                b.iter(|| {
                    blend_span(
                        black_box(blend),
                        black_box(&mut dst),
                        black_box(&cov),
                        span,
                        0x00d8_d8e4,
                    );
                });
            });
        }
    }
    g.finish();
}

/// A 200 by 50 cell terminal of `gw` by 16 glyphs, every cell holding a
/// random glyph from a 256 by 256 coverage atlas. Eight wide is the kernel's
/// step exactly; seven is a real cell width at a common font size and puts
/// every row through the tail path.
fn bench_screen(c: &mut Criterion, gw: usize) {
    let mut rng = Rng(0xfeed_face_dead_beef);
    let (cols, rows) = (200usize, 50usize);
    let gh = 16usize;
    let (w, h) = (cols * gw, rows * gh);
    let atlas_size = 256u32;
    let atlas_px: Vec<u8> = (0..atlas_size * atlas_size)
        .map(|_| rng.next() as u8)
        .collect();
    let atlas = Atlas {
        pixels: &atlas_px,
        size: [atlas_size, atlas_size],
        images: stein_shell::paint::Images::NONE,
    };
    let per_row = atlas_size as usize / gw;
    let per_col = atlas_size as usize / gh;
    let clip = Rect {
        min: [0.0, 0.0],
        max: [w as f32, h as f32],
    };
    let mut quads = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        for col in 0..cols {
            let slot = rng.next() as usize;
            let u = (slot % per_row) * gw;
            let v = ((slot / per_row) % per_col) * gh;
            let x = (col * gw) as f32;
            let y = (r * gh) as f32;
            quads.push(Quad::textured(
                Rect {
                    min: [x, y],
                    max: [x + gw as f32, y + gh as f32],
                },
                clip,
                0x00d8_d8e4,
                [
                    u as f32 / atlas_size as f32,
                    v as f32 / atlas_size as f32,
                    gw as f32 / atlas_size as f32,
                    gh as f32 / atlas_size as f32,
                ],
            ));
        }
    }

    let mut g = c.benchmark_group(format!("screen{gw}"));
    g.throughput(Throughput::Elements((cols * rows) as u64));
    g.sample_size(30);
    // The legacy path never went through a span, so drive it row by row
    // straight from the quads, which are whole pixel and unclipped here.
    let mut px = vec![BG; w * h];
    let aw = atlas_size as usize;
    g.bench_function("legacy", |b| {
        b.iter(|| {
            for q in black_box(&quads) {
                let (x, y) = (q.rect.min[0] as usize, q.rect.min[1] as usize);
                let (u, v) = ((q.uv[0] * aw as f32) as usize, (q.uv[1] * aw as f32) as usize);
                for r in 0..gh {
                    let d = (y + r) * w + x;
                    let s = (v + r) * aw + u;
                    blend_row_legacy(&mut px[d..d + gw], &atlas_px[s..s + gw], q.color);
                }
            }
        });
    });
    for (name, blend) in paths() {
        let mut px = vec![BG; w * h];
        g.bench_function(name, |b| {
            b.iter(|| {
                paint(
                    black_box(blend),
                    black_box(&mut px),
                    w,
                    black_box(&quads),
                    &atlas,
                );
            });
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_secs(2));
    targets = bench_row, bench_screen8, bench_screen7
}

fn bench_screen8(c: &mut Criterion) {
    bench_screen(c, 8);
}

fn bench_screen7(c: &mut Criterion) {
    bench_screen(c, 7);
}
criterion_main!(benches);
