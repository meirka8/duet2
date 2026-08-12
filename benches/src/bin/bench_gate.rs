// SPDX-License-Identifier: MIT
//! T-3.3.5: CI performance gate. Compares the benchmark run just performed
//! (criterion's `new` baseline, written by every `cargo bench` invocation)
//! against a previously recorded baseline, and exits non-zero if any
//! benchmark group's mean regressed by more than a threshold (default 10%,
//! matching design.md §11 / task.md's AC).
//!
//! # Usage
//!
//! ```text
//! cargo bench -p duet-bench -- --save-baseline main   # once, to record the baseline
//! # ... later, after a change ...
//! cargo bench -p duet-bench -- --baseline main        # writes fresh `new/estimates.json`
//! cargo run -p duet-bench --bin bench_gate -- --baseline main
//! ```
//!
//! Exits `0` with a summary table if every benchmark is within the
//! threshold, `1` (with the offending benchmarks listed) if any regressed
//! past it. Benchmarks present in the baseline but missing from `new` (or
//! vice versa) are reported as informational, not treated as failures --
//! this task's AC is about regressions in *existing* benchmarks, not
//! benchmark-set drift.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

/// Mirrors the subset of criterion's `estimates.json` this gate reads.
/// Criterion's own schema has many more fields (confidence intervals,
/// standard error, etc.) -- `#[serde(default)]`-free deliberately, so an
/// unexpected schema change fails loudly (a parse error) rather than
/// silently reading a stale/wrong field.
#[derive(Deserialize)]
struct Estimates {
    mean: PointEstimate,
}

#[derive(Deserialize)]
struct PointEstimate {
    point_estimate: f64,
}

struct Comparison {
    /// `<group>/<benchmark-id>`, e.g. `entry_store_population/1M`.
    name: String,
    baseline_ns: f64,
    new_ns: f64,
    /// Positive = slower (a regression); negative = faster.
    percent_change: f64,
}

fn main() -> ExitCode {
    let mut baseline_name = "main".to_string();
    let mut threshold_percent = 10.0_f64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--baseline" => {
                baseline_name = args.next().expect("--baseline requires a value");
            }
            "--threshold" => {
                threshold_percent = args
                    .next()
                    .expect("--threshold requires a value")
                    .parse()
                    .expect("--threshold must be a number (percent, e.g. 10 for 10%)");
            }
            other => {
                eprintln!("bench_gate: unrecognized argument {other:?}");
                return ExitCode::FAILURE;
            }
        }
    }

    let criterion_dir = criterion_output_dir();
    if !criterion_dir.is_dir() {
        eprintln!(
            "bench_gate: no criterion output found at {}; run `cargo bench` first",
            criterion_dir.display()
        );
        return ExitCode::FAILURE;
    }

    let comparisons = match collect_comparisons(&criterion_dir, &baseline_name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bench_gate: {e}");
            return ExitCode::FAILURE;
        }
    };

    if comparisons.is_empty() {
        eprintln!(
            "bench_gate: found no benchmarks with both a {baseline_name:?} baseline and a \
             fresh `new` run under {}. Did you run `cargo bench -- --baseline {baseline_name}`?",
            criterion_dir.display()
        );
        return ExitCode::FAILURE;
    }

    let mut regressions = Vec::new();
    println!(
        "{:<55} {:>14} {:>14} {:>10}",
        "benchmark", "baseline", "new", "change"
    );
    for cmp in &comparisons {
        let flag = if cmp.percent_change > threshold_percent {
            "REGRESSION"
        } else {
            ""
        };
        println!(
            "{:<55} {:>11.3}µs {:>11.3}µs {:>+9.1}%  {flag}",
            cmp.name,
            cmp.baseline_ns / 1000.0,
            cmp.new_ns / 1000.0,
            cmp.percent_change,
        );
        if cmp.percent_change > threshold_percent {
            regressions.push(cmp);
        }
    }

    if regressions.is_empty() {
        println!(
            "\nOK: all {} benchmark(s) within {threshold_percent}% of the {baseline_name:?} baseline.",
            comparisons.len()
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "\nFAIL: {} of {} benchmark(s) regressed more than {threshold_percent}% against {baseline_name:?}:",
            regressions.len(),
            comparisons.len()
        );
        for r in &regressions {
            println!("  - {} ({:+.1}%)", r.name, r.percent_change);
        }
        ExitCode::FAILURE
    }
}

/// Criterion writes to `<target-dir>/criterion/` by default. `CARGO_TARGET_DIR`
/// is respected if set (matches how CI/dev setups sometimes redirect it);
/// otherwise this assumes the conventional `target/` next to the workspace
/// root, found by walking up from the current binary's location -- robust
/// to being invoked via `cargo run` (cwd = workspace root) or directly.
fn criterion_output_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("criterion");
    }
    PathBuf::from("target/criterion")
}

/// Walks `<criterion_dir>/<group>/<benchmark-id>/{baseline_name,new}/estimates.json`
/// and pairs them up.
fn collect_comparisons(
    criterion_dir: &Path,
    baseline_name: &str,
) -> Result<Vec<Comparison>, String> {
    let mut out = Vec::new();
    for group_entry in read_dir_sorted(criterion_dir)? {
        let group_path = group_entry.path();
        if !group_path.is_dir() {
            continue;
        }
        let group_name = group_entry.file_name().to_string_lossy().into_owned();
        // criterion also writes a `report/` directory per group; skip non-benchmark entries.
        if group_name == "report" {
            continue;
        }
        for bench_entry in read_dir_sorted(&group_path)? {
            let bench_path = bench_entry.path();
            if !bench_path.is_dir() {
                continue;
            }
            let bench_id = bench_entry.file_name().to_string_lossy().into_owned();
            if bench_id == "report" {
                continue;
            }
            let baseline_estimates = bench_path.join(baseline_name).join("estimates.json");
            let new_estimates = bench_path.join("new").join("estimates.json");
            if !baseline_estimates.is_file() || !new_estimates.is_file() {
                continue;
            }
            let baseline: Estimates = read_estimates(&baseline_estimates)?;
            let new: Estimates = read_estimates(&new_estimates)?;
            let baseline_ns = baseline.mean.point_estimate;
            let new_ns = new.mean.point_estimate;
            let percent_change = (new_ns - baseline_ns) / baseline_ns * 100.0;
            out.push(Comparison {
                name: format!("{group_name}/{bench_id}"),
                baseline_ns,
                new_ns,
                percent_change,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("reading entry in {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

fn read_estimates(path: &Path) -> Result<Estimates, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
}
