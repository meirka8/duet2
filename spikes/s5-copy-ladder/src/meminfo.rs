//! Tiny `/proc/meminfo` reader used as a page-cache-eviction proxy.
//!
//! `vmtouch` is not installed in the spike environment and installing system
//! packages is out of scope for a headless benchmark spike, so we fall back
//! to sampling the `Cached:` field of `/proc/meminfo` before and after each
//! strategy run. This is coarser than `vmtouch -v <file>` (which reports
//! residency of a *specific* file's pages) because `Cached` is a system-wide
//! counter that any concurrent process can perturb. We mitigate that by:
//!   - running the benchmark host as quiescent as practical,
//!   - taking the sample immediately before/after each strategy call,
//!   - reporting deltas rather than absolute values,
//!   - cross-checking the *sign* and *relative magnitude* across strategies
//!     rather than trusting any single absolute number.

use anyhow::{bail, Context, Result};
use std::fs;

/// Read one `key: value kB` style field from `/proc/meminfo`, in kB.
pub fn read_meminfo_kb(field: &str) -> Result<u64> {
    let text = fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let value = rest
                .split_whitespace()
                .next()
                .context("malformed /proc/meminfo line")?;
            return Ok(value.parse::<u64>()?);
        }
    }
    bail!("field `{field}` not found in /proc/meminfo")
}

pub fn cached_kb() -> Result<u64> {
    read_meminfo_kb("Cached:")
}

/// Not currently plotted, kept for anyone extending this spike: `Buffers`
/// (block-device metadata cache) is a smaller, separate counter from
/// `Cached` (page cache for file content), which is what this spike cares
/// about.
#[allow(dead_code)]
pub fn buffers_kb() -> Result<u64> {
    read_meminfo_kb("Buffers:")
}
