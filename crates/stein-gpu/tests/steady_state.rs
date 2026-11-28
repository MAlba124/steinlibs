//! Recording a frame allocates nothing once the lists are warm. Its own test
//! binary, since the counting allocator is process wide.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_gpu::{Commands, GpuPtr, GpuRange, Hazard, Stage};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

fn frame(c: &mut Commands) {
    c.clear();
    c.barrier(Stage::TRANSFER, Stage::FRAGMENT, Hazard::NONE);
    for i in 0..100u32 {
        c.draw(&[GpuPtr(u64::from(i)), GpuPtr(0)], 6, i);
    }
    c.copy(GpuRange::default(), GpuPtr(64));
}

#[test]
fn a_warm_frame_allocates_nothing() {
    let mut c = Commands::with_capacity(128, 128 * 16, 4);
    frame(&mut c);
    stein_arena::counting::arm();
    for _ in 0..10 {
        frame(&mut c);
    }
    assert_eq!(stein_arena::counting::count(), 0);
}
