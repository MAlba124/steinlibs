//! Single-core throughput of the chunked decoder, in 64 KiB reads.
//!
//! ```text
//! taskset -c 5 cargo run --release -p stein-http --example codec_speed
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::time::Instant;

use stein_http::h1::{chunk, dechunk};

const READ: usize = 64 * 1024;
const RUNS: usize = 9;

/// Best of `RUNS`, in seconds.
fn best(mut run: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..RUNS {
        let t = Instant::now();
        run();
        best = best.min(t.elapsed().as_secs_f64());
    }
    best
}

fn gb_per_s(bytes: usize, secs: f64) -> f64 {
    bytes as f64 / secs / 1e9
}

/// `total` bytes in chunks of `size`.
fn chunked(total: usize, size: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..size).map(|i| (i * 31) as u8).collect();
    let mut wire = Vec::new();
    let mut out = vec![0u8; chunk::framed_len(size as u64) as usize];
    for _ in 0..total / size {
        let n = chunk::encode(&mut out, &data).unwrap();
        wire.extend_from_slice(&out[..n]);
    }
    wire.extend_from_slice(chunk::LAST);
    wire
}

/// Returns body bytes and data runs.
fn dechunk_all(wire: &[u8]) -> (usize, usize) {
    let mut dec = dechunk::Decoder::new(dechunk::Limits::default());
    let (mut body, mut runs) = (0, 0);
    for src in wire.chunks(READ) {
        let mut at = 0;
        loop {
            let s = dec.step(&src[at..]).unwrap();
            at += s.consumed;
            match s.event {
                dechunk::Event::Data { at: from, len } => {
                    black_box(&src[from..from + len]);
                    body += len;
                    runs += 1;
                }
                dechunk::Event::NeedMore | dechunk::Event::End => break,
            }
        }
    }
    assert!(dec.is_done());
    (body, runs)
}

const TOTAL: usize = 256 << 20;

fn dechunk_cases() {
    for (name, size) in [("chunks of 1 MiB", 1 << 20), ("chunks of 16 KiB", 16 << 10)] {
        let wire = chunked(TOTAL, size);
        let secs = best(|| {
            black_box(dechunk_all(black_box(&wire)));
        });
        let rate = gb_per_s(wire.len(), secs);
        println!("dechunk, {name:<18} {rate:8.2} GB/s");
    }
    for (name, size) in [("chunks of 64 B", 64), ("chunks of 16 B", 16)] {
        let wire = chunked(TOTAL / 4, size);
        let (_, runs) = dechunk_all(&wire);
        let secs = best(|| {
            black_box(dechunk_all(black_box(&wire)));
        });
        let rate = gb_per_s(wire.len(), secs);
        let chunks = runs as f64 / secs / 1e6;
        println!("dechunk, {name:<18} {rate:8.2} GB/s  {chunks:6.1} M chunks/s");
    }
}

fn main() {
    dechunk_cases();
}
