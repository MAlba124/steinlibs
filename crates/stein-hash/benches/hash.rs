//! Hash throughput over a 2 MiB torrent piece.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

fn piece() -> Vec<u8> {
    (0..2 << 20)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn hashes(c: &mut Criterion) {
    let data = piece();
    let mut g = c.benchmark_group("piece");
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("sha1", |b| b.iter(|| stein_hash::sha1(black_box(&data))));
    g.finish();
    // Eight pieces at once, one per lane.
    let pieces: Vec<Vec<u8>> = (0..8u8)
        .map(|i| data.iter().map(|b| b ^ i).collect())
        .collect();
    let msgs: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
    let level = fearless_simd::Level::new();
    let mut out = [[0u8; 20]; 8];
    let mut g = c.benchmark_group("eight_pieces");
    g.throughput(Throughput::Bytes(8 * data.len() as u64));
    g.bench_function("sha1_many", |b| {
        b.iter(|| stein_hash::sha1_many(level, black_box(&msgs), &mut out))
    });
    g.bench_function("sha1_scalar", |b| {
        b.iter(|| {
            for (m, o) in msgs.iter().zip(out.iter_mut()) {
                *o = stein_hash::sha1(black_box(m));
            }
        })
    });
    g.finish();
}

criterion_group!(benches, hashes);
criterion_main!(benches);
