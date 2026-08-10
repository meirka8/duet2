//! Corpus generation and filesystem probing for the S-5 copy-ladder spike.

use anyhow::{Context, Result};
use rand::RngCore;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Generate one large file of pseudo-random bytes (not sparse, not trivially
/// compressible, so buffered/reflink/copy_file_range strategies all do real
/// work rather than skipping holes or dedup-compressing zeros).
pub fn gen_large_file(path: &Path, size_bytes: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = File::create(path).with_context(|| format!("create {path:?}"))?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut rng = rand::thread_rng();
    let mut written = 0u64;
    while written < size_bytes {
        rng.fill_bytes(&mut buf);
        let remain = (size_bytes - written) as usize;
        let chunk = remain.min(buf.len());
        f.write_all(&buf[..chunk])?;
        written += chunk as u64;
    }
    f.sync_all()?;
    Ok(())
}

/// Generate `count` small files of `min_bytes..=max_bytes` random size each,
/// flat inside `dir`. Returns the list of paths created (stable order).
pub fn gen_small_files(
    dir: &Path,
    count: usize,
    min_bytes: usize,
    max_bytes: usize,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    let mut rng = rand::thread_rng();
    let mut paths = Vec::with_capacity(count);
    let mut buf = vec![0u8; max_bytes.max(1)];
    for i in 0..count {
        let sz = if max_bytes > min_bytes {
            min_bytes + (rng.next_u32() as usize % (max_bytes - min_bytes + 1))
        } else {
            min_bytes
        };
        rng.fill_bytes(&mut buf[..sz]);
        let p = dir.join(format!("file_{i:06}.dat"));
        let mut f = File::create(&p)?;
        f.write_all(&buf[..sz])?;
        paths.push(p);
    }
    Ok(paths)
}

pub fn remove_dir_all_if_exists(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("remove_dir_all {dir:?}"))?;
    }
    Ok(())
}

pub fn remove_file_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("remove_file {path:?}"))?;
    }
    Ok(())
}

pub fn dir_total_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        total += entry.metadata()?.len();
    }
    Ok(total)
}
