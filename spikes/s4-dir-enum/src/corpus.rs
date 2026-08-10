//! Deterministic flat-directory corpus generator.
//!
//! Names and sizes are derived from `(seed, index)` with a tiny xorshift
//! mix, so the same `(seed, count)` always reproduces byte-identical
//! filenames and size distribution. Content is not random garbage but a
//! cheap repeated fill, since these benches exercise metadata syscalls, not
//! read throughput; size is still nonzero for ~1 in 3 files so `statx`
//! actually has varying `stx_size`/block counts to report.

use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;
use rustix::fs::{self, Mode, OFlags, CWD};

/// Cheap deterministic 64-bit mix (splitmix64), used only to keep the
/// corpus reproducible from a seed -- not a cryptographic concern here.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn file_name(seed: u64, index: u64) -> String {
    // Fixed-width so average name length is stable across corpus sizes.
    format!("f{:016x}", mix(seed ^ index))
}

fn file_size(seed: u64, index: u64) -> usize {
    let r = mix(seed ^ index ^ 0xA5A5_A5A5_A5A5_A5A5);
    match r % 3 {
        0 => 0,
        1 => (r >> 8) as usize % 512,
        _ => (r >> 8) as usize % 4096,
    }
}

/// Generate `count` flat files under `dir` (created if absent; must be
/// empty or absent -- refuses to run against a non-empty existing dir to
/// avoid silently mixing corpora). Parallelized across `threads` workers,
/// each opening its own `openat` handle to `dir` and creating files
/// relative to that directory fd.
pub fn generate(dir: &Path, count: u64, seed: u64, threads: usize) {
    std::fs::create_dir_all(dir).expect("create_dir_all(dir)");
    let existing = std::fs::read_dir(dir)
        .expect("read_dir(dir)")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".gitignore")
        .count();
    if existing != 0 {
        panic!(
            "refusing to generate into non-empty dir {} ({existing} entries already present); \
             clean it first",
            dir.display()
        );
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .expect("build thread pool");

    let start = Instant::now();
    let total_bytes = pool.install(|| {
        (0..count)
            .into_par_iter()
            .map_init(
                || {
                    let dir_fd = fs::openat(
                        CWD,
                        dir,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .expect("openat(dir) in worker");
                    let zeros = vec![0u8; 4096];
                    (dir_fd, zeros)
                },
                |(dir_fd, zeros), i| {
                    let name = file_name(seed, i);
                    let size = file_size(seed, i);
                    let fd = fs::openat(
                        &*dir_fd,
                        name.as_str(),
                        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
                        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
                    )
                    .unwrap_or_else(|e| panic!("openat(create {name}) failed: {e}"));
                    if size > 0 {
                        rustix::io::write(&fd, &zeros[..size])
                            .unwrap_or_else(|e| panic!("write({name}) failed: {e}"));
                    }
                    size as u64
                },
            )
            .sum::<u64>()
    });
    let elapsed = start.elapsed();

    eprintln!(
        "GEN dir={} count={count} seed={seed} threads={threads} elapsed_ms={:.1} bytes={total_bytes}",
        dir.display(),
        elapsed.as_secs_f64() * 1000.0
    );
}
