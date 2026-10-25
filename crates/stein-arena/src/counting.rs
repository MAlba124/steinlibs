//! Counting global allocator, for proving a program stops allocating after setup.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::System;

pub struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static TRACE: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

static SIDE: AtomicUsize = AtomicUsize::new(0);

// Read inside the allocator, so must not allocate or drop. Const `Cell` of `Copy` does neither.
thread_local! {
    static REPORTING: Cell<bool> = const { Cell::new(false) };
    static ASIDE: Cell<bool> = const { Cell::new(false) };
    static MINE: Cell<usize> = const { Cell::new(0) };
}

/// Count this thread's allocations apart, in [`side_count`].
pub fn side() {
    let _ = ASIDE.try_with(|a| a.set(true));
}

/// Allocations since [`arm`] on threads that called [`side`].
pub fn side_count() -> usize {
    SIDE.load(Ordering::SeqCst)
}

/// Start counting. Call once setup is complete.
pub fn arm() {
    ARMED.store(true, Ordering::SeqCst);
}

/// Report the first `n` allocations after arming with a backtrace.
pub fn trace(n: usize) {
    TRACE.store(n, Ordering::SeqCst);
}

/// Allocations since [`arm`].
pub fn count() -> usize {
    COUNT.load(Ordering::SeqCst)
}

/// Allocations since [`arm`] by the calling thread, in [`count`] or [`side_count`].
pub fn thread_count() -> usize {
    MINE.try_with(|m| m.get()).unwrap_or(0)
}

/// Bytes asked for since [`arm`].
pub fn bytes() -> usize {
    BYTES.load(Ordering::SeqCst)
}

fn record(layout: Layout) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let reporting = REPORTING.try_with(|r| r.get()).unwrap_or(true);
    if reporting {
        return;
    }
    let _ = MINE.try_with(|m| m.set(m.get() + 1));
    if ASIDE.try_with(|a| a.get()).unwrap_or(false) {
        SIDE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    if n < TRACE.load(Ordering::Relaxed) {
        let _ = REPORTING.try_with(|r| r.set(true));
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("allocation {} of {} bytes:\n{bt}", n + 1, layout.size());
        let _ = REPORTING.try_with(|r| r.set(false));
    }
}

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(Layout::from_size_align(new_size, layout.align()).unwrap_or(layout));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
