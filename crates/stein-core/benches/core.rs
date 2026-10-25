//! Benchmarks for `Pool` and `KeyedStore`.

// Setup failures are bench bugs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use stein_core::handle::{Handle, Pool};
use stein_core::keyed::KeyedStore;

const CAP: usize = 4096;

/// splitmix-style key so probe homes spread.
fn key(i: usize) -> u64 {
    let mut z = i as u64 ^ 0x9e37_79b9_7f4a_7c15;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^ (z >> 27)
}

fn bench_pool(c: &mut Criterion) {
    let mut g = c.benchmark_group("pool_4096");

    // Insert plus remove at half occupancy.
    {
        let mut slots: Vec<Option<u64>> = vec![None; CAP];
        let mut gens = vec![0u32; CAP];
        let mut free = vec![0u32; CAP];
        let mut p = Pool::new(&mut slots, &mut gens, &mut free).expect("lengths agree");
        for i in 0..CAP / 2 {
            p.insert(i as u64).expect("capacity");
        }
        g.bench_function("insert_remove_pair", |b| {
            b.iter(|| {
                let h = p.insert(black_box(7u64)).expect("capacity");
                black_box(p.remove(black_box(h)));
            });
        });
    }

    // Scattered order so generation checks do not share a cache line.
    {
        let mut slots: Vec<Option<u64>> = vec![None; CAP];
        let mut gens = vec![0u32; CAP];
        let mut free = vec![0u32; CAP];
        let mut p = Pool::new(&mut slots, &mut gens, &mut free).expect("lengths agree");
        let mut handles: Vec<Handle<u64>> = Vec::with_capacity(CAP);
        for i in 0..CAP {
            handles.push(p.insert(i as u64).expect("capacity"));
        }
        // 1621 is coprime with 4096.
        let order: Vec<Handle<u64>> = (0..1024).map(|i| handles[(i * 1621) % CAP]).collect();
        g.throughput(Throughput::Elements(1024));
        g.bench_function("get_hit_x1024", |b| {
            b.iter(|| {
                let mut acc = 0u64;
                for h in &order {
                    if let Some(v) = p.get(black_box(*h)) {
                        acc = acc.wrapping_add(*v);
                    }
                }
                black_box(acc)
            });
        });

        // Stale handles should cost the same as a hit.
        let stale: Vec<Handle<u64>> = order
            .iter()
            .map(|h| Handle::from_user_data(h.to_user_data() ^ 2))
            .collect();
        g.bench_function("get_stale_x1024", |b| {
            b.iter(|| {
                let mut misses = 0u32;
                for h in &stale {
                    misses += u32::from(p.get(black_box(*h)).is_none());
                }
                black_box(misses)
            });
        });
    }
    g.finish();
}

fn bench_keyed(c: &mut Criterion) {
    let mut g = c.benchmark_group("keyed_4096");
    g.throughput(Throughput::Elements(1024));
    // 3072 is the exact load ceiling for capacity 4096.
    for &live in &[1024usize, 2048, 3072] {
        let mut keys = vec![0u64; CAP];
        let mut values: Vec<Option<u64>> = vec![None; CAP];
        let mut touched = vec![0u32; CAP];
        let mut s = KeyedStore::new(&mut keys, &mut values, &mut touched).expect("power of two");
        for i in 0..live {
            s.get_or_insert_with(key(i), 0, || i as u64)
                .expect("under ceiling");
        }

        g.bench_function(BenchmarkId::new("get_hit", live), |b| {
            b.iter(|| {
                let mut acc = 0u64;
                for i in 0..1024 {
                    if let Some(v) = s.get(black_box(key(i))) {
                        acc = acc.wrapping_add(*v);
                    }
                }
                black_box(acc)
            });
        });

        // Misses walk a chain to its first gap, the worst case probe.
        g.bench_function(BenchmarkId::new("get_miss", live), |b| {
            b.iter(|| {
                let mut found = 0u32;
                for i in 0..1024usize {
                    found += u32::from(s.get(black_box(key(i + CAP * 2))).is_some());
                }
                black_box(found)
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
    targets = bench_pool, bench_keyed
}
criterion_main!(benches);
