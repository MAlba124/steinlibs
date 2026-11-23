//! Tracing into a stderr that refuses writes. Own binary, it swaps fd 2.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::counting;

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

#[test]
fn a_failed_trace_write_neither_panics_nor_aborts() {
    let saved = rustix::io::dup(std::io::stderr()).unwrap();
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    rustix::stdio::dup2_stderr(&full).unwrap();
    counting::trace(4);
    counting::arm();
    let v = std::hint::black_box(Vec::<u8>::with_capacity(64));
    drop(v);
    let n = counting::count();
    rustix::stdio::dup2_stderr(&saved).unwrap();
    assert!(n >= 1, "{n}");
}
