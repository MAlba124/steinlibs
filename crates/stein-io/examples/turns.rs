//! Driver per-op overhead. One thread writes then reads a pipe each turn.
//! Run with `taskset -c 5 cargo run --release -p stein-io --example turns`.
//! Pass `epoll` or `uring` to run one backend, e.g. under `perf stat -e instructions:u`.
//! Trust instruction counts over time on a busy machine.

use std::time::{Duration, Instant};

use stein_io::driver::{APPEND, Backend, Io, Storage};

const TURNS: usize = 200_000;
const RUNS: usize = 7;

fn main() {
    println!("stein-io, {TURNS} turns of a write and a read, best of {RUNS}\n");
    println!("{:<8}  {:>12}  {:>12}", "backend", "ns a turn", "worst run");
    let only = std::env::args().nth(1);
    for back in [Backend::Epoll, Backend::Uring] {
        let name = format!("{back:?}").to_lowercase();
        if only.as_ref().is_some_and(|o| *o != name) {
            continue;
        }
        let runs: Vec<f64> = (0..RUNS).filter_map(|_| run(back)).collect();
        if runs.is_empty() {
            println!("{:<8}  unavailable", format!("{back:?}"));
            continue;
        }
        let best = runs.iter().copied().fold(f64::MAX, f64::min);
        let worst = runs.iter().copied().fold(0.0, f64::max);
        println!("{:<8}  {best:>12.0}  {worst:>12.0}", format!("{back:?}"));
    }
}

/// Drain completions, releasing buffers. Returns the count.
fn harvest(io: &mut Io) -> usize {
    let mut n = 0;
    while let Some(c) = io.next_completion() {
        n += 1;
        if let Some(got) = c.buf {
            io.release(got);
        }
    }
    n
}

fn run(back: Backend) -> Option<f64> {
    let store = Storage::new(16, 16);
    let mut io = Io::with_backend(store, 64, back).ok()?;
    let (r, w) = rustix::pipe::pipe().ok()?;
    let (r, w) = (io.own(r), io.own(w));

    let start = Instant::now();
    for turn in 0..TURNS {
        let b = io.alloc().ok()?;
        io.buf_mut(&b)[..8].copy_from_slice(&(turn as u64).to_ne_bytes());
        io.write(w, b, 8, APPEND).ok()?;
        io.read_stream(r).ok()?;
        let mut left = 2;
        while left > 0 {
            io.tick(Some(Duration::from_secs(1))).ok()?;
            left -= harvest(&mut io);
        }
    }
    Some(start.elapsed().as_nanos() as f64 / TURNS as f64)
}
