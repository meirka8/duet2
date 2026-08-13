//! A counting global allocator, used to verify T-4.2.1's "zero allocations
//! per frame while scrolling" AC.
//!
//! This is the same design S-1's spike used
//! (`spikes/s1-virtualised-table/src/alloc_track.rs`), reused verbatim in
//! spirit rather than reinvented: global atomics are fine here because,
//! unlike `duet-index`'s `cfg(test)` copy of this same idea
//! (`crates/duet-index/src/alloc_track.rs`, which needs thread-local
//! counters to survive `cargo test`'s parallel harness), this is a
//! single-threaded, single-purpose example binary with no concurrent
//! tests to interfere with the counts.

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

/// Read current process RSS in kilobytes from `/proc/self/status` --
/// Linux-only, matching the rest of this workspace's platform target.
pub fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let rest = rest.trim().trim_end_matches("kB").trim();
            return rest.parse().unwrap_or(0);
        }
    }
    0
}
