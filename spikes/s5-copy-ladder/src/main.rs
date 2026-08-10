//! S-5 spike: "what's the fastest correct way to copy files on Linux, and
//! is io_uring worth adopting for 1.0?"
//!
//! Headless, non-UI micro-benchmark harness. See documentation/spikes/S-5.md
//! for the write-up this tool's output feeds into.

mod corpus;
mod meminfo;
mod report;
mod strategies;
mod uring_copy;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use report::Measurement;

#[derive(Parser)]
#[command(name = "s5", about = "S-5 copy-strategy-ladder benchmark spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print filesystem type + reflink/io_uring capability info for `root`.
    Probe {
        #[arg(long)]
        root: PathBuf,
    },
    /// Run the full benchmark ladder against both corpora.
    Run {
        /// Directory to build the corpus + destinations in (should be on
        /// the real disk you want measured, not tmpfs).
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 2048)]
        large_mib: u64,
        #[arg(long, default_value_t = 50_000)]
        small_count: usize,
        #[arg(long, default_value_t = 1)]
        small_min_kib: usize,
        #[arg(long, default_value_t = 8)]
        small_max_kib: usize,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Skip corpus cleanup at the end (for manual inspection).
        #[arg(long, default_value_t = false)]
        keep: bool,
    },
}

fn fs_type(path: &Path) -> Result<String> {
    let out = Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(path)
        .output()
        .context("spawn stat")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn reflink_supported(root: &Path) -> Result<bool> {
    let a = root.join(".s5_reflink_probe_src");
    let b = root.join(".s5_reflink_probe_dst");
    corpus::remove_file_if_exists(&a)?;
    corpus::remove_file_if_exists(&b)?;
    std::fs::write(&a, b"s5-reflink-probe")?;
    let result = strategies::copy_ficlone(&a, &b);
    corpus::remove_file_if_exists(&a)?;
    corpus::remove_file_if_exists(&b)?;
    Ok(result.is_ok())
}

fn run_probe(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let ty = fs_type(root)?;
    println!("root:           {}", root.display());
    println!("filesystem:     {ty}");
    match reflink_supported(root) {
        Ok(true) => println!("FICLONE reflink: supported"),
        Ok(false) => println!("FICLONE reflink: NOT supported (or probe failed)"),
        Err(e) => println!("FICLONE reflink: probe error: {e:#}"),
    }
    match uring_copy::probe() {
        Ok(()) => println!("io_uring:       available (io_uring_setup succeeded)"),
        Err(e) => println!("io_uring:       UNAVAILABLE: {e:#}"),
    }
    Ok(())
}

/// Byte-for-byte compare via `cmp -s` (fast C implementation, short-circuits
/// on first difference, avoids reading multi-GB files into our own heap).
fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    let status = Command::new("cmp").arg("-s").arg(a).arg(b).status()?;
    Ok(status.success())
}

fn dirs_equal(a_files: &[PathBuf], dst_dir: &Path) -> Result<bool> {
    for src in a_files {
        let name = src.file_name().unwrap();
        let dst = dst_dir.join(name);
        if !dst.exists() {
            return Ok(false);
        }
        if !files_equal(src, &dst)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn cached_kb_or_zero() -> u64 {
    meminfo::cached_kb().unwrap_or(0)
}

fn run_large_file_ladder(root: &Path, large_mib: u64, rows: &mut Vec<Measurement>) -> Result<()> {
    let large_dir = root.join("large");
    corpus::remove_dir_all_if_exists(&large_dir)?;
    std::fs::create_dir_all(&large_dir)?;
    let src = large_dir.join("source.bin");
    println!("generating {large_mib} MiB source file at {src:?} ...");
    corpus::gen_large_file(&src, large_mib * 1024 * 1024)?;

    // Warm the source into page cache once so every strategy starts from
    // the same "hot cache" condition (fair read-side comparison; also lets
    // the fadvise-vs-not comparison mean something, since fadvise's effect
    // is to *evict* — there must be something resident to evict).
    {
        let _ = std::fs::read(&src).context("warm source into page cache")?;
    }

    type StratFn = fn(&Path, &Path) -> Result<strategies::StrategyOutcome>;
    let strategies: Vec<(&str, StratFn)> = vec![
        ("ficlone", strategies::copy_ficlone as StratFn),
        ("copy_file_range", strategies::copy_file_range as StratFn),
        ("buffered_1mib", |s, d| {
            strategies::copy_buffered(s, d, 1024 * 1024, false, true)
        }),
        ("buffered_4mib", |s, d| {
            strategies::copy_buffered(s, d, 4 * 1024 * 1024, false, true)
        }),
        ("buffered_4mib_fadvise", |s, d| {
            strategies::copy_buffered(s, d, 4 * 1024 * 1024, true, true)
        }),
        ("cp", strategies::copy_cp_file as StratFn),
        ("cp_a", strategies::copy_cp_a_file as StratFn),
    ];

    for (name, f) in strategies {
        let dst = large_dir.join(format!("dst_{name}"));
        corpus::remove_file_if_exists(&dst)?;
        let before = cached_kb_or_zero();
        let outcome = f(&src, &dst);
        let after = cached_kb_or_zero();
        match outcome {
            Ok(o) => {
                let ok = files_equal(&src, &dst).unwrap_or(false);
                rows.push(Measurement::new(
                    "large",
                    name,
                    o.bytes,
                    1,
                    o.duration,
                    before,
                    after,
                    Some(ok),
                    "",
                ));
            }
            Err(e) => {
                rows.push(Measurement::new(
                    "large",
                    name,
                    0,
                    0,
                    Duration::ZERO,
                    before,
                    after,
                    None,
                    format!("FAILED: {e:#}"),
                ));
            }
        }
        corpus::remove_file_if_exists(&dst)?;
    }

    if !std::env::var("S5_KEEP_LARGE_SOURCE").is_ok() {
        corpus::remove_dir_all_if_exists(&large_dir)?;
    }
    Ok(())
}

fn run_small_files_ladder(
    root: &Path,
    count: usize,
    min_kib: usize,
    max_kib: usize,
    rows: &mut Vec<Measurement>,
) -> Result<()> {
    let small_dir = root.join("small");
    corpus::remove_dir_all_if_exists(&small_dir)?;
    let src_dir = small_dir.join("source");
    println!("generating {count} small files ({min_kib}-{max_kib} KiB) at {src_dir:?} ...");
    let files = corpus::gen_small_files(&src_dir, count, min_kib * 1024, max_kib * 1024)?;
    let total_src_bytes = corpus::dir_total_bytes(&src_dir)?;

    // Per-file strategies driven by our own loop.
    type StratFn = fn(&Path, &Path) -> Result<strategies::StrategyOutcome>;
    let per_file_strategies: Vec<(&str, StratFn)> = vec![
        ("ficlone", strategies::copy_ficlone as StratFn),
        ("copy_file_range", strategies::copy_file_range as StratFn),
        // No fsync: matches cp/cp -a's default durability (data lands in
        // page cache, not forced to disk per file). This is the fair
        // apples-to-apples comparison against the cp baselines.
        ("buffered_256kib", |s, d| {
            strategies::copy_buffered(s, d, 256 * 1024, false, false)
        }),
        ("buffered_256kib_fadvise", |s, d| {
            strategies::copy_buffered(s, d, 256 * 1024, true, false)
        }),
        // Explicit per-file fsync, isolated as its own row: this is what a
        // crash-safe-per-file journal policy (FR-OPS-07) would cost on top
        // of the fair baseline above, for a many-small-files workload.
        ("buffered_256kib_fsync", |s, d| {
            strategies::copy_buffered(s, d, 256 * 1024, false, true)
        }),
    ];

    for (name, f) in per_file_strategies {
        let dst_dir = small_dir.join(format!("dst_{name}"));
        corpus::remove_dir_all_if_exists(&dst_dir)?;
        std::fs::create_dir_all(&dst_dir)?;
        let before = cached_kb_or_zero();
        let start = std::time::Instant::now();
        let mut total_bytes = 0u64;
        let mut err: Option<anyhow::Error> = None;
        for src in &files {
            let name_only = src.file_name().unwrap();
            let dst = dst_dir.join(name_only);
            match f(src, &dst) {
                Ok(o) => total_bytes += o.bytes,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let duration = start.elapsed();
        let after = cached_kb_or_zero();
        match err {
            None => {
                let ok =
                    dirs_equal(&files, &dst_dir).unwrap_or(false) && total_bytes == total_src_bytes;
                rows.push(Measurement::new(
                    "small",
                    name,
                    total_bytes,
                    files.len() as u64,
                    duration,
                    before,
                    after,
                    Some(ok),
                    "",
                ));
            }
            Some(e) => rows.push(Measurement::new(
                "small",
                name,
                total_bytes,
                0,
                duration,
                before,
                after,
                None,
                format!("FAILED: {e:#}"),
            )),
        }
        corpus::remove_dir_all_if_exists(&dst_dir)?;
    }

    // io_uring batching.
    {
        let dst_dir = small_dir.join("dst_io_uring");
        corpus::remove_dir_all_if_exists(&dst_dir)?;
        std::fs::create_dir_all(&dst_dir)?;
        let pairs: Vec<(PathBuf, PathBuf)> = files
            .iter()
            .map(|s| (s.clone(), dst_dir.join(s.file_name().unwrap())))
            .collect();
        let before = cached_kb_or_zero();
        let outcome = uring_copy::uring_batch_copy(&pairs, 256, 1024);
        let after = cached_kb_or_zero();
        match outcome {
            Ok(o) => {
                let ok =
                    dirs_equal(&files, &dst_dir).unwrap_or(false) && o.bytes == total_src_bytes;
                rows.push(Measurement::new(
                    "small",
                    "io_uring_batch256",
                    o.bytes,
                    o.files as u64,
                    o.duration,
                    before,
                    after,
                    Some(ok),
                    "",
                ));
            }
            Err(e) => rows.push(Measurement::new(
                "small",
                "io_uring_batch256",
                0,
                0,
                Duration::ZERO,
                before,
                after,
                None,
                format!("FAILED/UNAVAILABLE: {e:#}"),
            )),
        }
        corpus::remove_dir_all_if_exists(&dst_dir)?;
    }

    // cp -r / cp -a baselines over the whole directory.
    for (name, f) in [
        (
            "cp_r_dir",
            strategies::copy_cp_dir as fn(&Path, &Path) -> Result<strategies::StrategyOutcome>,
        ),
        ("cp_a_dir", strategies::copy_cp_a_dir),
    ] {
        let dst_dir = small_dir.join(format!("dst_{name}"));
        corpus::remove_dir_all_if_exists(&dst_dir)?;
        let before = cached_kb_or_zero();
        let outcome = f(&src_dir, &dst_dir);
        let after = cached_kb_or_zero();
        match outcome {
            Ok(o) => {
                let ok = dirs_equal(&files, &dst_dir).unwrap_or(false);
                rows.push(Measurement::new(
                    "small",
                    name,
                    o.bytes,
                    files.len() as u64,
                    o.duration,
                    before,
                    after,
                    Some(ok),
                    "",
                ));
            }
            Err(e) => rows.push(Measurement::new(
                "small",
                name,
                0,
                0,
                Duration::ZERO,
                before,
                after,
                None,
                format!("FAILED: {e:#}"),
            )),
        }
        corpus::remove_dir_all_if_exists(&dst_dir)?;
    }

    corpus::remove_dir_all_if_exists(&small_dir)?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Probe { root } => run_probe(&root)?,
        Cmd::Run {
            root,
            large_mib,
            small_count,
            small_min_kib,
            small_max_kib,
            out,
            keep: _keep,
        } => {
            std::fs::create_dir_all(&root)?;
            let ty = fs_type(&root)?;
            println!("=== S-5 copy-ladder benchmark ===");
            println!("root: {} (fs: {ty})", root.display());
            println!(
                "kernel: {}",
                String::from_utf8_lossy(&Command::new("uname").arg("-r").output()?.stdout).trim()
            );

            let mut rows = Vec::new();
            run_large_file_ladder(&root, large_mib, &mut rows)?;
            run_small_files_ladder(&root, small_count, small_min_kib, small_max_kib, &mut rows)?;

            report::print_table(&rows);

            if let Some(out_path) = out {
                let json = serde_json::to_string_pretty(&rows)?;
                std::fs::write(&out_path, json)?;
                println!("wrote {}", out_path.display());
            }

            // best effort: remove the now-empty root scaffold dirs we made
            let _ = std::fs::remove_dir(root.join("large"));
            let _ = std::fs::remove_dir(root.join("small"));
        }
    }
    Ok(())
}
