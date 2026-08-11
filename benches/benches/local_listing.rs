//! On-disk directory listing benchmarks — NFR-03/04's actual target
//! ("open a directory with N entries and have it render responsively").
//!
//! # Deferred: `duet-vfs::LocalFs`
//!
//! design.md §9.1 routes every real listing through `FileSystem::read_dir`,
//! and NFR-03/04 are meant to be measured against that trait's local
//! backend. As of this task, `duet-vfs/src/local/mod.rs` is still the
//! Phase-2-era stub ("Populated starting Phase 3") on `main` — sibling
//! task T-3.1.x (branch `feature/phase-3-3.1-localfs`) owns landing
//! `LocalFs`. Its worktree has real in-progress work (`local/guard.rs`,
//! `local/pathutil.rs`) as of this benchmark's authoring, but it is
//! uncommitted there and not merged to `main` or pushed to its branch, so
//! nothing stable exists yet to benchmark against. Rather than block
//! T-3.3.4 on that landing, this file benchmarks `std::fs::read_dir`
//! directly against the same corpus as a reference point: it establishes
//! the raw-syscall floor `LocalFs::read_dir` should land close to
//! (design.md's read_dir is a thin `getdents64` wrapper with chunked
//! streaming, not a heavier abstraction).
//!
//! **To wire in the real backend once it lands:** replace the
//! `std::fs::read_dir` call in `bench_read_dir_full_scan` with a
//! `LocalFs::read_dir(&vpath, ListOpts::default())` stream drained to
//! completion, add `duet-vfs` (and a minimal tokio current-thread runtime
//! to drive the `async_trait` future) to this crate's `[dependencies]`,
//! and delete this doc comment's deferral note.
//!
//! # Why 1M is opt-in
//!
//! Materializing a 1M-entry corpus is tens of seconds of real disk I/O
//! before the first sample even starts, on top of the disk space itself
//! (see the crate root doc comment). Gated behind `DUET_BENCH_1M=1` so a
//! plain `cargo bench` stays fast; CI's gate (T-3.3.5) does not set it.

use std::path::PathBuf;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use duet_bench::corpus::{self, CorpusScale};

const SEED: u64 = 0xD0E7_B3A5_C0DE_5EED;

fn scales() -> Vec<CorpusScale> {
    let mut scales = vec![CorpusScale::Ten, CorpusScale::OneK, CorpusScale::HundredK];
    if std::env::var("DUET_BENCH_1M").as_deref() == Ok("1") {
        scales.push(CorpusScale::OneM);
    }
    scales
}

/// A materialized corpus, cleaned up (`tempfile::TempDir`'s `Drop`) once
/// the benchmark group finishes with it.
struct MaterializedCorpus {
    _dir: tempfile::TempDir,
    root: PathBuf,
    entry_count: u64,
}

fn build_corpus(scale: CorpusScale) -> MaterializedCorpus {
    let dir = tempfile::tempdir().expect("create tempdir for corpus");
    let plan = corpus::plan(scale, SEED);
    let stats = corpus::materialize(dir.path(), &plan).expect("materialize corpus");
    MaterializedCorpus {
        root: dir.path().to_path_buf(),
        _dir: dir,
        entry_count: stats.entries_written,
    }
}

/// Full `read_dir` + `metadata()` (lstat, not following symlinks) scan of
/// the corpus root — the "open this huge folder and show name+size+kind
/// for every row" cost NFR-03/04 target.
fn bench_read_dir_full_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("local_listing_read_dir_full_scan");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    for scale in scales() {
        let corpus = build_corpus(scale);
        group.throughput(Throughput::Elements(corpus.entry_count));
        group.bench_with_input(
            BenchmarkId::from_parameter(scale.label()),
            &corpus.root,
            |b, root| {
                b.iter(|| {
                    let mut count = 0u64;
                    let mut total_size = 0u64;
                    for entry in std::fs::read_dir(root).expect("read_dir") {
                        let entry = entry.expect("dir entry");
                        // `symlink_metadata`-equivalent (DirEntry::metadata
                        // does not follow symlinks on Unix) == lstat
                        // semantics, matching `FileSystem::stat(follow:
                        // false)` (a listing must not follow symlinks just
                        // to show a row).
                        if let Ok(meta) = entry.metadata() {
                            total_size += meta.len();
                        }
                        count += 1;
                    }
                    std::hint::black_box((count, total_size))
                });
            },
        );
    }
    group.finish();
}

/// `read_dir` alone (directory entries, no per-entry `stat`) — isolates
/// `getdents64` cost from the metadata-fetch cost the full scan above
/// includes, since a brief-mode listing (design.md §9.1's `ListOpts`) may
/// skip the latter.
fn bench_read_dir_names_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("local_listing_read_dir_names_only");
    group.sample_size(10);
    for scale in scales() {
        let corpus = build_corpus(scale);
        group.throughput(Throughput::Elements(corpus.entry_count));
        group.bench_with_input(
            BenchmarkId::from_parameter(scale.label()),
            &corpus.root,
            |b, root| {
                b.iter(|| {
                    let mut count = 0u64;
                    for entry in std::fs::read_dir(root).expect("read_dir") {
                        std::hint::black_box(entry.expect("dir entry").file_name());
                        count += 1;
                    }
                    std::hint::black_box(count)
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_read_dir_full_scan, bench_read_dir_names_only);
criterion_main!(benches);
