//! The three enumeration strategies under benchmark. Each returns a
//! `StratResult` with wall-clock elapsed time and a cheap checksum
//! (entries seen, bytes summed) so the compiler cannot dead-code-eliminate
//! the work and so results are sanity-checkable across strategies.

use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;
use rustix::fs::{self, AtFlags, FileType, StatxFlags};

use crate::util::{list_entries, open_dir};

pub struct StratResult {
    pub elapsed_secs: f64,
    pub entries: usize,
    pub bytes_seen: u64,
}

/// Strategy 1 -- naive: getdents64 to list names, then a full
/// `fstatat`-equivalent (`statat`, `AT_SYMLINK_NOFOLLOW`) on *every* entry,
/// even when the caller only wanted the name and type. This is the
/// baseline: what you get if you stat unconditionally.
pub fn naive_stat_all(dir: &Path) -> StratResult {
    let dir_fd = open_dir(dir);
    let start = Instant::now();
    let entries = list_entries(&dir_fd);
    let mut bytes_seen = 0u64;
    for e in &entries {
        let st = fs::statat(&dir_fd, e.name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
            .unwrap_or_else(|err| panic!("statat({:?}) failed: {err}", e.name));
        bytes_seen += st.st_size as u64;
    }
    let elapsed_secs = start.elapsed().as_secs_f64();
    StratResult {
        elapsed_secs,
        entries: entries.len(),
        bytes_seen,
    }
}

/// Strategy 2 -- d_type-aware: getdents64 only. `d_type` is already in the
/// kernel's answer, so when the caller just needs name + type (the FR-VFS-01
/// / T-3.1.1 "names + types" use case) no `stat` call is issued at all. The
/// only per-entry work is classifying the already-known `d_type`.
pub fn dtype_scan(dir: &Path) -> StratResult {
    let dir_fd = open_dir(dir);
    let start = Instant::now();
    let entries = list_entries(&dir_fd);
    let mut regular = 0u64;
    let mut unknown = 0u64;
    for e in &entries {
        match e.d_type {
            FileType::RegularFile => regular += 1,
            FileType::Unknown => unknown += 1,
            _ => {}
        }
    }
    let elapsed_secs = start.elapsed().as_secs_f64();
    StratResult {
        elapsed_secs,
        entries: entries.len(),
        // Reuse bytes_seen as a diagnostic counter here: how many entries
        // needed a d_type fallback because the fs reported DT_UNKNOWN. Both
        // ext4 and btrfs report real d_type for regular files, so this
        // should be 0 on both; a positive count would mean the fast path
        // silently degrades on some fs and is worth flagging.
        bytes_seen: regular.wrapping_add(unknown << 32),
    }
}

/// Strategy 3 -- parallel batched statx: one getdents64 scan to collect
/// names (unavoidable), then `statx(..., AT_STATX_DONT_SYNC)` for full
/// metadata batched across a worker thread pool, one dir fd shared
/// read-only across threads (safe: stat family calls do not mutate fd
/// state).
pub fn statx_parallel(dir: &Path, threads: usize) -> StratResult {
    let dir_fd = open_dir(dir);
    let start = Instant::now();
    let entries = list_entries(&dir_fd);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .expect("build thread pool");

    let bytes_seen: u64 = pool.install(|| {
        entries
            .par_iter()
            .map(|e| {
                let st = fs::statx(
                    &dir_fd,
                    e.name.as_c_str(),
                    AtFlags::STATX_DONT_SYNC | AtFlags::SYMLINK_NOFOLLOW,
                    StatxFlags::BASIC_STATS,
                )
                .unwrap_or_else(|err| panic!("statx({:?}) failed: {err}", e.name));
                st.stx_size
            })
            .sum()
    });

    let elapsed_secs = start.elapsed().as_secs_f64();
    StratResult {
        elapsed_secs,
        entries: entries.len(),
        bytes_seen,
    }
}
