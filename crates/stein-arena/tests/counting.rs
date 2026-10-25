//! The counting allocator installed globally, armed and with a side thread.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::counting;

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

#[test]
fn armed_counts_and_side_threads_count_apart() {
    let before = Vec::<u8>::with_capacity(64);
    assert_eq!(counting::count(), 0, "nothing counts before arming");
    counting::arm();
    let on_main = Vec::<u8>::with_capacity(100);
    let main_count = counting::count();
    assert!(main_count >= 1, "{main_count}");
    let t = std::thread::spawn(|| {
        counting::side();
        let v: Vec<Vec<u8>> = (0..10).map(|_| Vec::with_capacity(8)).collect();
        v.len()
    });
    // Spawning allocates here. The side thread stays out of the main count.
    let after_spawn = counting::count();
    assert_eq!(t.join().unwrap(), 10);
    assert!(counting::side_count() >= 11, "{}", counting::side_count());
    assert!(
        counting::count() - after_spawn < 11,
        "the side thread's allocations are not in the main count"
    );
    drop((before, on_main));
}
