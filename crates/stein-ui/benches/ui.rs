//! Per-frame cost baselines for the immediate-mode layer.
//!
//! The frame cycle is the whole story: begin, emit, end. Red flags: the idle
//! cycle not being dominated by layout solve, damage on identical frames
//! costing more than a linear compare, or the state table degrading before its
//! load ceiling.

// Bench setup failures are bugs in the bench, not conditions to handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use stein_core::keyed::KeyedStore;
use stein_layout::{Axis, BoxSpec, Insets, LayoutStorage, Options, Rect, SizeSpec};
use stein_ui::draw::{CustomRegion, Quad, damage};
use stein_ui::{Input, Ui, UiStorage, WidgetState, key};

const VP: [f32; 2] = [1280.0, 720.0];

/// Owns every slice the Ui borrows.
struct Backing {
    lkey: Vec<u64>,
    lparent: Vec<u32>,
    lsubtree: Vec<u32>,
    lsize: Vec<[SizeSpec; 2]>,
    laxis: Vec<Axis>,
    lmeasured: Vec<[f32; 2]>,
    lpad: Vec<Insets>,
    lopts: Vec<Options>,
    lcomputed: Vec<[f32; 2]>,
    lcontent: Vec<[f32; 2]>,
    lrect: Vec<Rect>,
    lclip: Vec<Rect>,
    lstack: Vec<u32>,
    skeys: Vec<u64>,
    svalues: Vec<Option<WidgetState>>,
    stouched: Vec<u32>,
    quads: Vec<Quad>,
    prev: Vec<Quad>,
    custom: Vec<CustomRegion>,
    kstack: Vec<u64>,
    fill: Vec<u32>,
}

impl Backing {
    fn new(boxes: usize, state: usize, quads: usize) -> Self {
        let blank = Quad::solid(Rect::default(), Rect::default(), 0);
        let region = CustomRegion {
            key: 0,
            rect: Rect::default(),
            clip: Rect::default(),
        };
        Backing {
            lkey: vec![0; boxes],
            lparent: vec![0; boxes],
            lsubtree: vec![0; boxes],
            lsize: vec![[SizeSpec::px(0.0); 2]; boxes],
            laxis: vec![Axis::X; boxes],
            lmeasured: vec![[0.0; 2]; boxes],
            lpad: vec![Insets::ZERO; boxes],
            lopts: vec![Options::default(); boxes],
            lcomputed: vec![[0.0; 2]; boxes],
            lcontent: vec![[0.0; 2]; boxes],
            lrect: vec![Rect::default(); boxes],
            lclip: vec![Rect::default(); boxes],
            lstack: vec![0; 32],
            skeys: vec![0; state],
            svalues: vec![None; state],
            stouched: vec![0; state],
            quads: vec![blank; quads],
            prev: vec![blank; quads],
            custom: vec![region; 8],
            kstack: vec![0; 32],
            fill: vec![0; boxes],
        }
    }

    fn storage(&mut self) -> UiStorage<'_> {
        UiStorage {
            layout: LayoutStorage {
                key: &mut self.lkey,
                parent: &mut self.lparent,
                subtree_end: &mut self.lsubtree,
                size: &mut self.lsize,
                axis: &mut self.laxis,
                measured: &mut self.lmeasured,
                pad: &mut self.lpad,
                opts: &mut self.lopts,
                computed: &mut self.lcomputed,
                content: &mut self.lcontent,
                rect: &mut self.lrect,
                clip: &mut self.lclip,
                stack: &mut self.lstack,
            },
            state_keys: &mut self.skeys,
            state_values: &mut self.svalues,
            state_touched: &mut self.stouched,
            quads: &mut self.quads,
            prev_quads: &mut self.prev,
            custom: &mut self.custom,
            key_stack: &mut self.kstack,
            fill: &mut self.fill,
        }
    }
}

/// Panels of buttons, 10 boxes per panel, stable keys, per-panel scroll state.
/// Every button paints, so the quad list scales with the widget count.
fn emit_widgets(ui: &mut Ui<'_>, widgets: usize) {
    let root = key::hash_str("root");
    ui.open(
        root,
        BoxSpec::container(Axis::Y).fill().pad(8.0).gap(4.0),
        0,
    )
    .expect("cap");
    for p in 0..widgets / 10 {
        let pk = key::seeded(root, key::hash_str("panel"), p as u64);
        ui.open(
            pk,
            BoxSpec::container(Axis::X)
                .w(SizeSpec::grow())
                .pad(4.0)
                .gap(2.0),
            0,
        )
        .expect("cap");
        if let Some(st) = ui.state(pk) {
            black_box(st.scroll);
        }
        for b in 0..9u64 {
            let bk = key::seeded(pk, key::hash_str("button"), b);
            let fill = 0xff20_3040 + b as u32;
            let r = ui
                .leaf(
                    bk,
                    BoxSpec::leaf().w(SizeSpec::px(60.0)).h(SizeSpec::px(24.0)),
                    fill,
                )
                .expect("cap");
            black_box(r);
        }
        ui.close().expect("balanced");
    }
    ui.close().expect("balanced");
}

/// Full frame at steady state: identical frames, so damage is None and the
/// window would not present. This is the floor cost of simply being open.
fn bench_frame_cycle(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame_cycle_idle");
    for &n in &[100usize, 1_000] {
        let mut back = Backing::new(n + 16, 4096, n);
        let mut ui = Ui::new(back.storage()).expect("storage agrees");
        let input = Input {
            pointer: [640.0, 360.0],
            pointer_down: false,
            wheel: [0.0, 0.0],
            dt: 1.0 / 60.0,
            ..Input::default()
        };
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                ui.begin_frame(black_box(input), black_box(VP));
                emit_widgets(&mut ui, n);
                let f = ui.end_frame().expect("balanced");
                black_box(f);
            });
        });
    }
    g.finish();
}

fn quad_grid(n: usize) -> Vec<Quad> {
    let clip = Rect {
        min: [0.0, 0.0],
        max: VP,
    };
    (0..n)
        .map(|i| {
            let x = (i % 40) as f32 * 32.0;
            let y = (i / 40) as f32 * 24.0;
            Quad::solid(
                Rect {
                    min: [x, y],
                    max: [x + 30.0, y + 22.0],
                },
                clip,
                0xff00_0000 + i as u32,
            )
        })
        .collect()
}

/// The diff that decides whether to present. Identical frames are the idle
/// path and must stay cheap; a red flag is the one-changed case costing
/// anything beyond the full compare.
fn bench_damage(c: &mut Criterion) {
    let mut g = c.benchmark_group("damage_1000_quads");
    g.throughput(Throughput::Elements(1_000));
    let prev = quad_grid(1_000);

    let next = prev.clone();
    g.bench_function("identical", |b| {
        b.iter(|| black_box(damage(black_box(&prev), black_box(&next))));
    });

    let mut next = prev.clone();
    next[500].color = 0xffff_ffff;
    g.bench_function("one_changed", |b| {
        b.iter(|| black_box(damage(black_box(&prev), black_box(&next))));
    });

    let mut next = prev.clone();
    for q in &mut next {
        q.color ^= 0x00ff_ff00;
    }
    g.bench_function("all_changed", |b| {
        b.iter(|| black_box(damage(black_box(&prev), black_box(&next))));
    });
    g.finish();
}

fn state_key(i: usize) -> u64 {
    key::seeded(0x5157, key::hash_str("w"), i as u64)
}

fn fill_store(s: &mut KeyedStore<'_, WidgetState>, live: usize) {
    for i in 0..live {
        s.get_or_insert_with(state_key(i), 0, WidgetState::default)
            .expect("under ceiling");
    }
}

/// Hit-path probes at low and near-ceiling load. The 0.75 ceiling exists so
/// probes stay short; a red flag is the high-load case being several times
/// the low-load one.
fn bench_state_table(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_get_or_insert_hit");
    g.throughput(Throughput::Elements(256));
    for &live in &[256usize, 3_072] {
        let mut kbuf = vec![0u64; 4096];
        let mut vbuf: Vec<Option<WidgetState>> = vec![None; 4096];
        let mut tbuf = vec![0u32; 4096];
        let mut s = KeyedStore::new(&mut kbuf, &mut vbuf, &mut tbuf).expect("power of two");
        fill_store(&mut s, live);
        let mut frame = 1u32;
        g.bench_function(BenchmarkId::new("load", live), |b| {
            b.iter(|| {
                frame = frame.wrapping_add(1);
                for i in 0..256 {
                    let v = s
                        .get_or_insert_with(black_box(state_key(i)), frame, WidgetState::default)
                        .expect("present");
                    black_box(v);
                }
            });
        });
    }
    g.finish();
}

fn bench_prune(c: &mut Criterion) {
    let mut g = c.benchmark_group("prune_4096_cap");

    // Nothing stale. This is what every ordinary frame pays.
    {
        let mut kbuf = vec![0u64; 4096];
        let mut vbuf: Vec<Option<WidgetState>> = vec![None; 4096];
        let mut tbuf = vec![0u32; 4096];
        let mut s = KeyedStore::new(&mut kbuf, &mut vbuf, &mut tbuf).expect("power of two");
        fill_store(&mut s, 768);
        g.bench_function("idle_768_live", |b| {
            b.iter(|| black_box(s.prune(black_box(1), 240)));
        });
    }

    // Refill 768 entries then evict them all. Timing includes the refill,
    // which the idle numbers above let you subtract.
    {
        let mut kbuf = vec![0u64; 4096];
        let mut vbuf: Vec<Option<WidgetState>> = vec![None; 4096];
        let mut tbuf = vec![0u32; 4096];
        let mut s = KeyedStore::new(&mut kbuf, &mut vbuf, &mut tbuf).expect("power of two");
        g.bench_function("refill_and_evict_768", |b| {
            b.iter(|| {
                fill_store(&mut s, 768);
                black_box(s.prune(black_box(1000), 10));
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
    targets = bench_frame_cycle, bench_damage, bench_state_table, bench_prune
}
criterion_main!(benches);
