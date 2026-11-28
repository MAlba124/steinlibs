//! Solve-time baselines for the layout table.
//!
//! The solver runs every frame, so these numbers bound how many boxes a window
//! can hold at 60fps. Red flags: solve growing faster than linearly in box
//! count, or the deep tree costing much more than the wide one at equal count,
//! since every pass is supposed to be a linear scan regardless of nesting.

// Bench setup failures are bugs in the bench, not conditions to handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use stein_layout::{
    Align, Axis, BoxSpec, BoxTable, Insets, LayoutStorage, Options, Rect, SizeSpec,
};

const VP: [f32; 2] = [1280.0, 720.0];

/// Owns the column vectors the table borrows.
struct Backing {
    key: Vec<u64>,
    parent: Vec<u32>,
    subtree_end: Vec<u32>,
    size: Vec<[SizeSpec; 2]>,
    axis: Vec<Axis>,
    measured: Vec<[f32; 2]>,
    pad: Vec<Insets>,
    opts: Vec<Options>,
    computed: Vec<[f32; 2]>,
    content: Vec<[f32; 2]>,
    rect: Vec<Rect>,
    clip: Vec<Rect>,
    stack: Vec<u32>,
}

impl Backing {
    fn new(n: usize, depth: usize) -> Self {
        Backing {
            key: vec![0; n],
            parent: vec![0; n],
            subtree_end: vec![0; n],
            size: vec![[SizeSpec::px(0.0); 2]; n],
            axis: vec![Axis::X; n],
            measured: vec![[0.0; 2]; n],
            pad: vec![Insets::ZERO; n],
            opts: vec![Options::default(); n],
            computed: vec![[0.0; 2]; n],
            content: vec![[0.0; 2]; n],
            rect: vec![Rect::default(); n],
            clip: vec![Rect::default(); n],
            stack: vec![0; depth],
        }
    }

    fn storage(&mut self) -> LayoutStorage<'_> {
        LayoutStorage {
            key: &mut self.key,
            parent: &mut self.parent,
            subtree_end: &mut self.subtree_end,
            size: &mut self.size,
            axis: &mut self.axis,
            measured: &mut self.measured,
            pad: &mut self.pad,
            opts: &mut self.opts,
            computed: &mut self.computed,
            content: &mut self.content,
            rect: &mut self.rect,
            clip: &mut self.clip,
            stack: &mut self.stack,
        }
    }
}

/// A window-shaped tree: header with buttons, clipped scrolling sidebar list,
/// content panels. Mixes Px, Grow, ChildrenSum and Measured with padding and
/// gaps, because a flat list of Px leaves would skip most of the solver.
fn emit_window(t: &mut BoxTable<'_>, budget: usize) {
    // 14 fixed boxes, 3 per sidebar row, 6 per content panel.
    let rest = budget.saturating_sub(14);
    let rows = rest * 2 / 5 / 3;
    let panels = (rest - rows * 3) / 6;
    let mut k = 0u64;

    t.open(
        next(&mut k),
        BoxSpec::container(Axis::Y).fill().pad(8.0).gap(4.0),
    )
    .expect("cap");
    emit_header(t, &mut k);
    t.open(next(&mut k), BoxSpec::container(Axis::X).fill().gap(4.0))
        .expect("cap");
    emit_sidebar(t, &mut k, rows);
    emit_panels(t, &mut k, panels);
    t.close().expect("balanced");
    t.close().expect("balanced");
}

fn next(k: &mut u64) -> u64 {
    *k += 1;
    *k
}

fn emit_header(t: &mut BoxTable<'_>, k: &mut u64) {
    t.open(
        next(k),
        BoxSpec::container(Axis::X)
            .w(SizeSpec::grow())
            .h(SizeSpec::px(32.0))
            .pad_xy(6.0, 4.0)
            .gap(4.0),
    )
    .expect("cap");
    for _ in 0..8 {
        t.leaf(
            next(k),
            BoxSpec::leaf().w(SizeSpec::px(60.0)).h(SizeSpec::grow()),
        )
        .expect("cap");
    }
    t.leaf(
        next(k),
        BoxSpec::leaf().w(SizeSpec::grow()).h(SizeSpec::px(1.0)),
    )
    .expect("cap");
    t.close().expect("balanced");
}

fn emit_sidebar(t: &mut BoxTable<'_>, k: &mut u64, rows: usize) {
    t.open(
        next(k),
        BoxSpec::container(Axis::Y)
            .w(SizeSpec::px(200.0))
            .h(SizeSpec::grow())
            .pad(4.0)
            .gap(2.0)
            .clip()
            .scroll([0.0, 40.0]),
    )
    .expect("cap");
    for i in 0..rows {
        t.open(
            next(k),
            BoxSpec::container(Axis::X).w(SizeSpec::grow()).gap(4.0),
        )
        .expect("cap");
        t.leaf(
            next(k),
            BoxSpec::leaf().w(SizeSpec::px(16.0)).h(SizeSpec::px(16.0)),
        )
        .expect("cap");
        let w = 90.0 + (i % 5) as f32 * 14.0;
        t.leaf(next(k), BoxSpec::leaf().measured(w, 14.0))
            .expect("cap");
        t.close().expect("balanced");
    }
    t.close().expect("balanced");
}

fn emit_panels(t: &mut BoxTable<'_>, k: &mut u64, panels: usize) {
    t.open(
        next(k),
        BoxSpec::container(Axis::Y).fill().pad(6.0).gap(6.0),
    )
    .expect("cap");
    for i in 0..panels {
        t.open(
            next(k),
            BoxSpec::container(Axis::Y)
                .w(SizeSpec::grow())
                .pad(6.0)
                .gap(2.0),
        )
        .expect("cap");
        let w = 120.0 + (i % 7) as f32 * 8.0;
        t.leaf(next(k), BoxSpec::leaf().measured(w, 16.0))
            .expect("cap");
        t.open(
            next(k),
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .h(SizeSpec::px(22.0))
                .gap(2.0)
                .justify(Align::Center),
        )
        .expect("cap");
        for _ in 0..3 {
            t.leaf(next(k), BoxSpec::leaf().fill()).expect("cap");
        }
        t.close().expect("balanced");
        t.close().expect("balanced");
    }
    t.close().expect("balanced");
}

/// A chain of nested containers, 2 boxes per level.
fn emit_deep(t: &mut BoxTable<'_>, levels: usize) {
    let mut key = 0u64;
    for i in 0..levels {
        let axis = if i % 2 == 0 { Axis::X } else { Axis::Y };
        key += 1;
        t.open(key, BoxSpec::container(axis).gap(1.0)).expect("cap");
        key += 1;
        t.leaf(
            key,
            BoxSpec::leaf().w(SizeSpec::px(20.0)).h(SizeSpec::px(14.0)),
        )
        .expect("cap");
    }
    for _ in 0..levels {
        t.close().expect("balanced");
    }
}

/// One container over a flat run of leaves. Same box count as the deep chain.
fn emit_wide(t: &mut BoxTable<'_>, count: usize) {
    t.open(1, BoxSpec::container(Axis::Y).gap(1.0))
        .expect("cap");
    for i in 0..count - 1 {
        t.leaf(
            i as u64 + 2,
            BoxSpec::leaf().w(SizeSpec::px(20.0)).h(SizeSpec::px(14.0)),
        )
        .expect("cap");
    }
    t.close().expect("balanced");
}

fn bench_solve(c: &mut Criterion) {
    let mut g = c.benchmark_group("solve");
    for &n in &[100usize, 1_000, 10_000] {
        let mut back = Backing::new(n, 64);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        emit_window(&mut t, n);
        g.throughput(Throughput::Elements(t.len() as u64));
        g.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                t.solve(black_box(VP)).expect("balanced");
                black_box(t.rect(0));
            });
        });
    }
    g.finish();
}

fn bench_emit(c: &mut Criterion) {
    let mut g = c.benchmark_group("emit");
    for &n in &[100usize, 1_000, 10_000] {
        let mut back = Backing::new(n, 64);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                t.reset();
                emit_window(&mut t, black_box(n));
                black_box(t.len());
            });
        });
    }
    g.finish();
}

fn bench_emit_and_solve(c: &mut Criterion) {
    let mut g = c.benchmark_group("emit_and_solve");
    for &n in &[100usize, 1_000, 10_000] {
        let mut back = Backing::new(n, 64);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                t.reset();
                emit_window(&mut t, black_box(n));
                t.solve(black_box(VP)).expect("balanced");
                black_box(t.rect(0));
            });
        });
    }
    g.finish();
}

/// Resize path: same emitted tree, only the viewport differs. There is no
/// caching, so this should cost exactly one solve. If it ever diverges from
/// the solve numbers something stateful crept in.
fn bench_resize(c: &mut Criterion) {
    let mut g = c.benchmark_group("resize_resolve");
    for &n in &[1_000usize, 10_000] {
        let mut back = Backing::new(n, 64);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        emit_window(&mut t, n);
        let mut flip = false;
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                flip = !flip;
                let vp = if flip {
                    [1280.0, 720.0]
                } else {
                    [1920.0, 1080.0]
                };
                t.solve(black_box(vp)).expect("balanced");
                black_box(t.rect(0));
            });
        });
    }
    g.finish();
}

/// Depth versus width at 1024 boxes. The passes are linear scans, so nesting
/// depth should be close to free. A deep tree costing much more would mean the
/// child walks are quadratic somewhere.
fn bench_shape(c: &mut Criterion) {
    let mut g = c.benchmark_group("shape_1024");
    {
        let mut back = Backing::new(1024, 600);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        emit_deep(&mut t, 512);
        g.bench_function("deep_512_levels", |b| {
            b.iter(|| {
                t.solve(black_box(VP)).expect("balanced");
                black_box(t.rect(0));
            });
        });
    }
    {
        let mut back = Backing::new(1024, 8);
        let mut t = BoxTable::new(back.storage()).expect("columns agree");
        emit_wide(&mut t, 1024);
        g.bench_function("wide_1023_children", |b| {
            b.iter(|| {
                t.solve(black_box(VP)).expect("balanced");
                black_box(t.rect(0));
            });
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_solve, bench_emit, bench_emit_and_solve, bench_resize, bench_shape
}
criterion_main!(benches);
