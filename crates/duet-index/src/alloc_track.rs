//! A counting global allocator used to verify T-3.2.1's "zero per-entry heap
//! allocation for names under 24 bytes" acceptance criterion.
//!
//! This is the same idea S-1's spike used at
//! `spikes/s1-virtualised-table/src/alloc_track.rs` to verify its own
//! "no per-row allocation while scrolling" AC -- reused here rather than
//! reinvented, per T-3.2.1's brief, with one deliberate change: counters
//! and the tracking flag are **thread-local**, not global atomics. S-1's
//! spike was a single-threaded standalone benchmark binary, so a global
//! atomic counter was fine; this crate's test measures allocations while
//! running under `cargo test`'s default *parallel* test harness (multiple
//! test functions on multiple OS threads in the same process), where a
//! process-wide counter would also pick up unrelated allocations from
//! whichever other tests happen to be running concurrently on other
//! threads -- observed directly while building this: a global-counter
//! version of this test flaked with 500k+ "allocations" for a 1M-entry
//! push loop that should see a few hundred, entirely explained by other
//! tests running in parallel. Thread-local counters see only the calling
//! thread's own allocation traffic, which is what T-3.2.1's AC actually
//! asks about.
//!
//! Test-only (`cfg(test)`): this module is compiled solely into the
//! crate's unit-test binary, where [`CountingAllocator`] is installed as
//! the `#[global_allocator]` (see `lib.rs`). It never affects the real
//! library build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOC_COUNT: Cell<u64> = const { Cell::new(0) };
    static ALLOC_BYTES: Cell<u64> = const { Cell::new(0) };
    static DEALLOC_COUNT: Cell<u64> = const { Cell::new(0) };
    static DEALLOC_BYTES: Cell<u64> = const { Cell::new(0) };
    static REALLOC_COUNT: Cell<u64> = const { Cell::new(0) };
}

pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.get() {
            ALLOC_COUNT.set(ALLOC_COUNT.get() + 1);
            ALLOC_BYTES.set(ALLOC_BYTES.get() + layout.size() as u64);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.get() {
            DEALLOC_COUNT.set(DEALLOC_COUNT.get() + 1);
            DEALLOC_BYTES.set(DEALLOC_BYTES.get() + layout.size() as u64);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACKING.get() {
            ALLOC_COUNT.set(ALLOC_COUNT.get() + 1);
            ALLOC_BYTES.set(ALLOC_BYTES.get() + layout.size() as u64);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.get() {
            REALLOC_COUNT.set(REALLOC_COUNT.get() + 1);
            ALLOC_BYTES.set(ALLOC_BYTES.get() + new_size as u64);
            DEALLOC_BYTES.set(DEALLOC_BYTES.get() + layout.size() as u64);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AllocSnapshot {
    pub alloc_count: u64,
    pub alloc_bytes: u64,
    pub dealloc_count: u64,
    pub dealloc_bytes: u64,
    pub realloc_count: u64,
}

impl AllocSnapshot {
    pub fn total_events(&self) -> u64 {
        self.alloc_count + self.dealloc_count + self.realloc_count
    }
}

pub fn snapshot() -> AllocSnapshot {
    AllocSnapshot {
        alloc_count: ALLOC_COUNT.get(),
        alloc_bytes: ALLOC_BYTES.get(),
        dealloc_count: DEALLOC_COUNT.get(),
        dealloc_bytes: DEALLOC_BYTES.get(),
        realloc_count: REALLOC_COUNT.get(),
    }
}

pub fn reset() {
    ALLOC_COUNT.set(0);
    ALLOC_BYTES.set(0);
    DEALLOC_COUNT.set(0);
    DEALLOC_BYTES.set(0);
    REALLOC_COUNT.set(0);
}

pub fn set_tracking(on: bool) {
    TRACKING.set(on);
}

/// Runs `f` with allocation tracking on for the *calling thread only*,
/// returning `f`'s result alongside the delta snapshot observed strictly
/// during `f`'s execution on this thread. Counters are reset immediately
/// before `f` runs and tracking is switched off immediately after.
///
/// If `f` itself spawns other threads (e.g. Rayon workers), allocations on
/// those threads are *not* counted -- each thread's tracking flag starts
/// `false` independently. That is the correct behavior for T-3.2.1's AC,
/// which is about `EntryStore::push`'s own allocation profile on its
/// caller's thread, not about work some unrelated parallel subsystem does
/// on other threads.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocSnapshot) {
    reset();
    set_tracking(true);
    let result = f();
    set_tracking(false);
    (result, snapshot())
}
