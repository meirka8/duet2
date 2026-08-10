//! The single-file copy-strategy ladder, implemented directly on top of
//! `rustix` syscalls per design.md §7.5 (no libc unsafety sprawl in the
//! caller). Each function copies `src` to `dst` (creating/truncating `dst`)
//! and returns bytes copied + wall time. `cp`/`cp -a` baselines shell out to
//! the real coreutils binary for a fair comparison.

use anyhow::{ensure, Context, Result};
use rustix::fs as rfs;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct StrategyOutcome {
    pub bytes: u64,
    pub duration: Duration,
}

/// `ioctl(FICLONE)` reflink. Only works same-filesystem on btrfs / xfs
/// (reflink=1) / bcachefs. Returns Err on EOPNOTSUPP/EXDEV/EINVAL etc, which
/// the caller should treat as "not available here", not a bug.
pub fn copy_ficlone(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let start = Instant::now();
    let src_f = File::open(src).context("open src")?;
    let dst_f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .context("open dst")?;
    rfs::ioctl_ficlone(&dst_f, &src_f).context("FICLONE ioctl")?;
    let bytes = src_f.metadata()?.len();
    Ok(StrategyOutcome {
        bytes,
        duration: start.elapsed(),
    })
}

/// `copy_file_range(2)` — kernel-side copy, no userspace bounce buffer.
pub fn copy_file_range(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let start = Instant::now();
    let src_f = File::open(src).context("open src")?;
    let dst_f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .context("open dst")?;
    let len = src_f.metadata()?.len();
    let mut off_in: u64 = 0;
    let mut off_out: u64 = 0;
    let mut remaining = len as usize;
    while remaining > 0 {
        let n = rfs::copy_file_range(
            &src_f,
            Some(&mut off_in),
            &dst_f,
            Some(&mut off_out),
            remaining,
        )
        .context("copy_file_range")?;
        if n == 0 {
            break; // short read at EOF (shouldn't happen given len, but be safe)
        }
        remaining -= n;
    }
    Ok(StrategyOutcome {
        bytes: len,
        duration: start.elapsed(),
    })
}

/// Buffered copy with `SEEK_HOLE`/`SEEK_DATA`-free straight-line reads (the
/// corpus files are not sparse), 1-4 MiB buffers, optional
/// `posix_fadvise(POSIX_FADV_DONTNEED)` on the source behind the read
/// cursor so a large copy doesn't evict the user's page cache.
pub fn copy_buffered(
    src: &Path,
    dst: &Path,
    buf_size: usize,
    fadvise_dontneed: bool,
    fsync: bool,
) -> Result<StrategyOutcome> {
    let start = Instant::now();
    let mut src_f = File::open(src).context("open src")?;
    let mut dst_f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .context("open dst")?;
    let mut buf = vec![0u8; buf_size];
    let mut total: u64 = 0;
    loop {
        let n = src_f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst_f.write_all(&buf[..n])?;
        total += n as u64;
        if fadvise_dontneed {
            let region_start = total - n as u64;
            // Best-effort: a failure here must not fail the copy.
            let _ = rfs::fadvise(
                &src_f,
                region_start,
                NonZeroU64::new(n as u64),
                rfs::Advice::DontNeed,
            );
        }
    }
    if fsync {
        dst_f.sync_all()?;
    }
    Ok(StrategyOutcome {
        bytes: total,
        duration: start.elapsed(),
    })
}

fn run_cp(args: &[&str], src: &Path, dst: &Path) -> Result<Duration> {
    let start = Instant::now();
    let status = Command::new("/usr/bin/cp")
        .args(args)
        .arg(src)
        .arg(dst)
        .status()
        .context("spawn cp")?;
    ensure!(status.success(), "cp {:?} failed", args);
    Ok(start.elapsed())
}

pub fn copy_cp_file(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let duration = run_cp(&[], src, dst)?;
    let bytes = fs::metadata(dst)?.len();
    Ok(StrategyOutcome { bytes, duration })
}

pub fn copy_cp_a_file(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let duration = run_cp(&["-a"], src, dst)?;
    let bytes = fs::metadata(dst)?.len();
    Ok(StrategyOutcome { bytes, duration })
}

/// `cp -r src dst` where `dst` does not yet exist: dst becomes a flat copy
/// of src's contents (same layout our other many-small-files strategies
/// produce).
pub fn copy_cp_dir(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let duration = run_cp(&["-r"], src, dst)?;
    let bytes = crate::corpus::dir_total_bytes(dst)?;
    Ok(StrategyOutcome { bytes, duration })
}

pub fn copy_cp_a_dir(src: &Path, dst: &Path) -> Result<StrategyOutcome> {
    let duration = run_cp(&["-a"], src, dst)?;
    let bytes = crate::corpus::dir_total_bytes(dst)?;
    Ok(StrategyOutcome { bytes, duration })
}
