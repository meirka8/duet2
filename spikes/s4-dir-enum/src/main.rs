//! Phase 0 spike S-4: directory enumeration/stat throughput.
//!
//! Headless, non-UI benchmark. Not production code -- see
//! documentation/spikes/S-4.md for the writeup and results table.
//!
//! Subcommands:
//!   gen   --dir <path> --count <n> [--seed <u64>] [--threads <n>]
//!   bench --dir <path> --strategy <naive|dtype|statx> [--threads <n>] [--repeat <n>]
//!   clean --dir <path>

mod corpus;
mod strategies;
mod util;

use std::path::PathBuf;

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");

    match cmd {
        "gen" => {
            let dir = PathBuf::from(arg_value(&args, "--dir").expect("--dir required"));
            let count: u64 = arg_value(&args, "--count")
                .expect("--count required")
                .parse()
                .expect("--count must be u64");
            let seed: u64 = arg_value(&args, "--seed")
                .map(|s| s.parse().expect("--seed must be u64"))
                .unwrap_or(0x5EED_0000_0000_0001);
            let threads: usize = arg_value(&args, "--threads")
                .map(|s| s.parse().expect("--threads must be usize"))
                .unwrap_or_else(default_threads);
            corpus::generate(&dir, count, seed, threads);
        }
        "bench" => {
            let dir = PathBuf::from(arg_value(&args, "--dir").expect("--dir required"));
            let strategy = arg_value(&args, "--strategy").expect("--strategy required");
            let threads: usize = arg_value(&args, "--threads")
                .map(|s| s.parse().expect("--threads must be usize"))
                .unwrap_or_else(default_threads);
            let repeat: usize = arg_value(&args, "--repeat")
                .map(|s| s.parse().expect("--repeat must be usize"))
                .unwrap_or(1);
            let label = arg_value(&args, "--label").unwrap_or_else(|| "unlabeled".to_string());

            for rep in 0..repeat {
                // naive/dtype are inherently single-threaded (one getdents
                // scan, no rayon); only statx actually uses --threads.
                let (result, effective_threads) = match strategy.as_str() {
                    "naive" => (strategies::naive_stat_all(&dir), 1),
                    "dtype" => (strategies::dtype_scan(&dir), 1),
                    "statx" => (strategies::statx_parallel(&dir, threads), threads),
                    other => panic!("unknown --strategy {other} (want naive|dtype|statx)"),
                };
                // Machine-parseable result line; a driver script greps for
                // "RESULT," and reduces (min/median) over --repeat runs.
                println!(
                    "RESULT,{label},{count},{strategy},{effective_threads},{rep},{elapsed_ms:.3},{entries},{checksum}",
                    label = label,
                    count = result.entries,
                    strategy = strategy,
                    effective_threads = effective_threads,
                    rep = rep,
                    elapsed_ms = result.elapsed_secs * 1000.0,
                    entries = result.entries,
                    checksum = result.bytes_seen,
                );
            }
        }
        "clean" => {
            let dir = PathBuf::from(arg_value(&args, "--dir").expect("--dir required"));
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .unwrap_or_else(|e| panic!("remove_dir_all({}) failed: {e}", dir.display()));
                eprintln!("CLEAN dir={} removed", dir.display());
            } else {
                eprintln!("CLEAN dir={} already absent", dir.display());
            }
        }
        _ => {
            eprintln!(
                "usage:\n  \
                 s4-dir-enum gen   --dir <path> --count <n> [--seed <u64>] [--threads <n>]\n  \
                 s4-dir-enum bench --dir <path> --strategy <naive|dtype|statx> [--threads <n>] [--repeat <n>] [--label <name>]\n  \
                 s4-dir-enum clean --dir <path>"
            );
            std::process::exit(2);
        }
    }
}
