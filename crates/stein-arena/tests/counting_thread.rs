//! Per-thread counts against the process-wide count.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc;

use stein_arena::counting;

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

/// Allocate `n` times, return this thread's count delta.
fn allocate(n: usize) -> usize {
    let before = counting::thread_count();
    let v: Vec<Vec<u8>> = Vec::with_capacity(n);
    let v = (0..n - 1).fold(v, |mut v, _| {
        v.push(Vec::with_capacity(16));
        v
    });
    let counted = counting::thread_count() - before;
    drop(v);
    counted
}

#[test]
fn each_thread_counts_only_its_own() {
    let unarmed = Vec::<u8>::with_capacity(32);
    assert_eq!(counting::thread_count(), 0, "nothing counts before arming");
    counting::arm();
    // Gated so neither thread finishes before the other starts.
    let (go_a, wait_a) = mpsc::channel::<()>();
    let (go_b, wait_b) = mpsc::channel::<()>();
    let a = std::thread::spawn(move || {
        wait_a.recv().unwrap();
        let own = allocate(1000);
        (own, counting::thread_count())
    });
    let b = std::thread::spawn(move || {
        counting::side();
        wait_b.recv().unwrap();
        let own = allocate(300);
        (own, counting::thread_count())
    });
    let spawned = counting::thread_count();
    go_a.send(()).unwrap();
    go_b.send(()).unwrap();
    let mine = allocate(50);
    let (a_own, a_all) = a.join().unwrap();
    let (b_own, b_all) = b.join().unwrap();
    assert_eq!(mine, 50, "this thread saw only its own");
    assert_eq!(a_own, 1000, "the first saw only its own");
    assert_eq!(b_own, 300, "a side thread counts its own too");
    // Own allocations plus a little runtime overhead.
    assert!((1000..1100).contains(&a_all), "{a_all}");
    assert!((300..400).contains(&b_all), "{b_all}");
    // Neither thread is in this thread's count.
    let here = counting::thread_count();
    assert!(
        here >= spawned + 50 && here < spawned + 150,
        "{here} {spawned}"
    );
    // Main count has this thread and `a`, side count has `b`.
    assert!(counting::count() >= 1000 + 50, "{}", counting::count());
    assert!(counting::count() < 1300 + spawned, "{}", counting::count());
    assert!(
        (300..400).contains(&counting::side_count()),
        "{}",
        counting::side_count()
    );
    drop(unarmed);
}
