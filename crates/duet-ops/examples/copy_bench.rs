// SPDX-License-Identifier: MIT
//! Headless measurement harness for two acceptance criteria that had never
//! been recorded (review 2026-09-04, §1.3):
//!
//! - **T-5.1.4**: the copy strategy ladder (FICLONE → `copy_file_range` →
//!   buffered) must reach ≥ 95% of `cp` for a large file. `copy` runs one
//!   real job through the real planner and executor against a real
//!   `LocalFs`, exactly as the queue does, and prints wall time and
//!   throughput; the `cp` side is timed from the shell alongside it.
//! - **T-5.1.2**: journal write overhead ≤ 3% of copy time. `journal`
//!   appends N `Intent`/`Completion` pairs (each `append` fsyncs, as in
//!   production) and prints the per-record cost, which divided by the
//!   per-file copy time of any real corpus gives the overhead.
//!
//! Usage:
//!   cargo run -p duet-ops --release --example copy_bench -- copy <src-file-or-dir> <dest-dir> [--verify]
//!     (COPY_BENCH_STATE_DIR=<dir> puts the job's journal where production would, e.g. under $HOME)
//!   cargo run -p duet-ops --release --example copy_bench -- journal <state-dir> <records>
//!
//! Results are recorded in `docs/perf-baseline.md`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use duet_ops::{
    CancelToken, ExecutionControl, JobEvent, JobId, JobKind, Journal, JournalRecord, Plan,
    PlanOptions, Step, StepOutcome, execute, plan_copy,
};
use duet_types::{Timestamp, UnixPathBuf, VPath};
use duet_vfs::{FileSystem, LocalFs};

fn vpath(p: &Path) -> VPath {
    VPath::local(UnixPathBuf::new(p.to_str().expect("utf-8 path")).expect("valid unix path"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("copy") if args.len() >= 4 => {
            let verify = args.iter().any(|a| a == "--verify");
            copy(Path::new(&args[2]), Path::new(&args[3]), verify);
        }
        Some("journal") if args.len() == 4 => {
            let n: u32 = args[3].parse().expect("record count");
            journal(Path::new(&args[2]), n);
        }
        _ => {
            eprintln!(
                "usage:\n  copy_bench copy <src-file> <dest-dir> [--verify]\n  copy_bench journal <state-dir> <records>"
            );
            std::process::exit(2);
        }
    }
}

fn copy(src: &Path, dest_dir: &Path, verify: bool) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let size = std::fs::metadata(src).expect("stat src").len();
        // Where the job's crash-safety journal lives matters as much as
        // where the files go: production uses `$XDG_STATE_HOME/duet`
        // (typically the home filesystem), and every journal record is
        // fsync'd there. `COPY_BENCH_STATE_DIR` puts the journal where a
        // real install would; the default (`$TMPDIR`, usually tmpfs)
        // measures the copy path with the journal's fsyncs made free.
        let state_root = std::env::var_os("COPY_BENCH_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let state_dir = state_root.join(format!("duet-copy-bench-{}", std::process::id()));
        std::fs::create_dir_all(&state_dir).expect("state dir");

        let plan_start = Instant::now();
        let plan = plan_copy(
            fs.as_ref(),
            &[vpath(src)],
            &vpath(dest_dir),
            PlanOptions {
                verify,
                ..PlanOptions::default()
            },
            &CancelToken::new(),
        )
        .await
        .expect("plan_copy");
        let plan_elapsed = plan_start.elapsed();

        let journal = Journal::open(JobId(1), &state_dir).expect("journal");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
        let drain = tokio::spawn(async move {
            let mut samples = 0u32;
            while let Some(event) = rx.recv().await {
                if matches!(event, JobEvent::Progress { .. }) {
                    samples += 1;
                }
                if matches!(event, JobEvent::Finished { .. }) {
                    break;
                }
            }
            samples
        });

        let start = Instant::now();
        let report = execute(
            fs,
            JobId(1),
            JobKind::Copy,
            plan,
            journal,
            4,
            tx,
            ExecutionControl::new(),
            None,
        )
        .await;
        let elapsed = start.elapsed();
        let samples = drain.await.unwrap_or(0);

        let secs = elapsed.as_secs_f64();
        println!(
            "copy_bench copy: {} bytes in {:.3}s = {:.1} MB/s (plan {:.1}ms, {} progress samples, verify={}, errors={}, skipped={})",
            size,
            secs,
            size as f64 / secs / 1e6,
            plan_elapsed.as_secs_f64() * 1e3,
            samples,
            verify,
            report.errors.len(),
            report.skipped.len()
        );
        for e in &report.errors {
            println!("  error: {} {:?}", e.message, e.path);
        }
        if let Ok(journal_text) = std::fs::read_to_string(state_dir.join("jobs").join("1.journal")) {
            println!(
                "  journal: {} records, {} bytes",
                journal_text.lines().count(),
                journal_text.len()
            );
        }
        let _ = std::fs::remove_dir_all(&state_dir);
    });
}

fn journal(state_dir: &Path, n: u32) {
    std::fs::create_dir_all(state_dir).expect("state dir");
    let mut journal = Journal::open(JobId(7), state_dir).expect("journal");
    let dest = vpath(Path::new(
        "/tmp/duet-copy-bench/some/reasonably/long/destination/path/file.bin",
    ));
    journal
        .append(&JournalRecord::JobStarted {
            job_id: JobId(7),
            started_at: Timestamp::from(SystemTime::now()),
            plan: Plan::new(Vec::new(), PlanOptions::default()),
            kind: JobKind::Copy,
        })
        .expect("append");
    let start = Instant::now();
    for i in 0..n {
        journal
            .append(&JournalRecord::Intent {
                step_index: i,
                step: Step::CreateDir {
                    dest: dest.clone(),
                    mode: None,
                },
                partial_name: Some(format!(".duet-partial-{i:016x}-file.bin")),
            })
            .expect("append intent");
        journal
            .append(&JournalRecord::Completion {
                step_index: i,
                outcome: StepOutcome::Succeeded,
            })
            .expect("append completion");
    }
    let elapsed = start.elapsed();
    let records = u64::from(n) * 2;
    println!(
        "copy_bench journal: {} records (each fsync'd) in {:.3}s = {:.1} µs/record, {:.1} µs per Intent+Completion pair",
        records,
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1e6 / records as f64,
        elapsed.as_secs_f64() * 1e6 / f64::from(n)
    );
}
