//! A counting global allocator used to verify T-3.2.1's "zero per-entry heap
//! allocation for names under 24 bytes" acceptance criterion.
//!
//! This is the same pattern S-1's spike built at
//! `spikes/s1-virtualised-table/src/alloc_track.rs` to verify its own
//! "no per-row allocation while scrolling" AC -- reused here rather than
//! reinvented, per T-3.2.1's brief. Tracking is gated by an atomic bool so
//! call sites can zero the counters and flip tracking on immediately before
//! the measured phase (the `EntryStore::push` loop) and off immediately
//! after, without the setup allocations (e.g. `EntryStore::with_capacity`,
//! test name-string generation) polluting the count.
//!
//! Test-only (`cfg(test)`): this module is compiled solely into the crate's
//! unit-test binary, where [`crate::alloc_track::CountingAllocator`] is
//! installed as the `#[global_allocator]` (see `lib.rs`). It never affects
//! the real library build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub static TRACKING: AtomicBool = AtomicBool::new(false);

pub static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
pub static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
pub static REALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            REALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
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
        alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        dealloc_count: DEALLOC_COUNT.load(Ordering::Relaxed),
        dealloc_bytes: DEALLOC_BYTES.load(Ordering::Relaxed),
        realloc_count: REALLOC_COUNT.load(Ordering::Relaxed),
    }
}

pub fn reset() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    DEALLOC_COUNT.store(0, Ordering::Relaxed);
    DEALLOC_BYTES.store(0, Ordering::Relaxed);
    REALLOC_COUNT.store(0, Ordering::Relaxed);
}

pub fn set_tracking(on: bool) {
    TRACKING.store(on, Ordering::Relaxed);
}

/// Runs `f` with allocation tracking on, returning `f`'s result alongside
/// the delta snapshot observed strictly during `f`'s execution. Counters
/// are reset immediately before `f` runs and tracking is switched off
/// immediately after, so nothing outside `f`'s dynamic extent is counted.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocSnapshot) {
    reset();
    set_tracking(true);
    let result = f();
    set_tracking(false);
    (result, snapshot())
}
