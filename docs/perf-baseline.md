# Performance baselines and the CI regression gate (T-3.3.4 / T-3.3.5)

## What's here

- `benches/` — a standalone `duet-bench` crate (criterion harness) with:
  - `src/corpus.rs` — a deterministic, seeded synthetic-directory-tree generator (10/1k/100k/1M entry scales; unicode names, sparse files, hardlink farms, broken symlinks, deep nesting). Same seed always produces byte-identical corpus structure.
  - `benches/entry_store.rs` — in-memory benchmarks of `duet-index::EntryStore` (population, name lookup) across all four scales.
  - `benches/local_listing.rs` — on-disk directory listing benchmarks. Currently measures `std::fs::read_dir` as a reference floor; **swap in `duet_vfs::LocalFs::read_dir` once a caller here is set up to drive its async trait** (see that file's doc comment — `LocalFs` itself landed in T-3.1.x, this bench just hasn't been updated to use it yet). 1M-scale listing is gated behind `DUET_BENCH_1M=1` (real disk I/O, slow) and not run in CI.
  - `src/bin/bench_gate.rs` — T-3.3.5's regression gate. Compares the benchmark run that was just performed against a named baseline by reading criterion's own `estimates.json` files directly (not criterion's built-in `--baseline` diff feature — see below for why), and exits non-zero if any benchmark's mean regressed more than a threshold (default 10%).
- `benches/baselines/main/` — the **committed** baseline: just the small `estimates.json` file per benchmark (not criterion's much larger raw `sample.json`/plot data), copied out of `target/criterion/*/main/` after a `--save-baseline main` run. This is what CI restores before running benchmarks, so a regression introduced in the very commit being tested is actually caught — a baseline regenerated fresh every CI run could never do that.

## Recording or updating the baseline

When a deliberate, accepted performance change lands (a real optimization, or an accepted regression with a documented reason), regenerate the committed baseline:

```sh
cargo bench -p duet-bench --bench entry_store --bench local_listing -- --save-baseline main
rm -rf benches/baselines/main
mkdir -p benches/baselines/main
( cd target/criterion && find . -path '*/main/estimates.json' -exec bash -c \
    'mkdir -p "../../benches/baselines/main/$(dirname "$1")" && cp "$1" "../../benches/baselines/main/$1"' _ {} \; )
git add benches/baselines/main
```

## Running the gate locally

```sh
# Restore the committed baseline into criterion's expected location:
mkdir -p target/criterion && cp -r benches/baselines/main/* target/criterion/
# Run the benchmarks (writes fresh results to target/criterion/*/new/):
cargo bench -p duet-bench --bench entry_store --bench local_listing
# Check for regressions:
cargo run -p duet-bench --bin bench_gate --release -- --baseline main --threshold 10
```

## Why `bench_gate` doesn't use `cargo bench -- --baseline main`

Criterion's own `--baseline <name>` flag prints a nice inline "Performance has improved/regressed" summary, but it reads *both* `estimates.json` and the much larger `sample.json` (raw per-iteration timings) from the baseline directory. Committing `sample.json` for every benchmark would bloat the repo for no real benefit — `bench_gate` only needs the summary statistics (`estimates.json`'s `mean.point_estimate`), which it reads directly and compares itself. So CI runs a plain `cargo bench` (which always writes fresh results to `new/` regardless of any `--baseline` flag) and lets `bench_gate` do the actual comparison.

## Verified: the gate actually catches a regression

As part of landing T-3.3.5, a deliberate ~20-45% regression (an unnecessary 64-byte heap allocation added to `EntryStore::push`'s hot path) was introduced, benchmarked, and confirmed to make `bench_gate` exit non-zero and correctly name the four regressed benchmarks (`entry_store_population/{10,1k,100k,1M}`, +13.7% to +47.2%) — then reverted. This was a real, executed test, not just a design description.

## Measured 2026-09-06: the acceptance criteria that had never been recorded

`documentation/review-2026-09-04.md` §1.3 listed five performance ACs the
WBS had marked done without a number. This section records them. Harness:
`crates/duet-ops/examples/copy_bench.rs` (a real job through the real
planner and executor against a real `LocalFs`, exactly as the queue runs
it) plus `cp` timed from the shell alongside it. Machine: 24 cores, 62 GiB
RAM; `/tmp` tmpfs; `/home` btrfs on NVMe under LUKS; `storage_2` ext4 on
a rotational SATA disk (ST4000DM004). Release builds; each number is the
better of two runs unless stated.

### T-5.1.4 — copy ladder vs `cp` (4 GiB single file): **met**

| Filesystem | `cp` | duet | duet / `cp` | Notes |
|---|---|---|---|---|
| tmpfs | 2.00 s | 2.03 s (2.1 GB/s) | 99% | `copy_file_range` path; memcpy-bound on both sides |
| btrfs (NVMe, LUKS) | `--reflink=never` 5.4 s + 15–19 s writeback; `--reflink=always` 6 ms | **4 ms** | n/a (reflink) | FICLONE taken automatically; `filefrag -v` shows duet's copy sharing the source's physical extent (`59258024..`), identical to `cp --reflink=always`; content verified with `cmp` |
| ext4 (rotational HDD) | 31 s / 60 s (`cp && sync`, two runs) | 27 s / 38 s (114–158 MB/s) | ≈ parity | both bound by the disk's writeback; run-to-run variance on this disk is ±50%, so treat as "no measurable difference" |

Also observed: duet's copy of the 4 GiB file on the HDD left only 1.75 GiB of the *source* in page cache afterwards (against 4 GiB for `cp`), consistent with the ladder's `fadvise` hints doing their job; a second run was fully cached. Not measured rigorously (no `vmtouch` on this machine; `fincore` used).

### T-5.1.2 — journal write overhead ≤ 3% of copy time: **met for large files, badly missed for small ones**

Per-record cost of `Journal::append` (each call fsyncs, as in production; `copy_bench journal`):

| Journal location | µs per record | per Intent+Completion pair |
|---|---|---|
| tmpfs | 1.3 | 2.6 |
| btrfs (NVMe, LUKS) — where `~/.local/state/duet` lives on this machine | 1 513 | 3 025 |
| ext4 (rotational HDD) | 106 568 | 213 136 |

For a 4 GiB file (two records, ~3 ms on btrfs against a 2–30 s copy) the overhead is < 0.2%: met. For many small files it is the whole cost. A corpus of 10 000 × 4 KiB files, journal placed where production places it (`COPY_BENCH_STATE_DIR=$HOME/...`):

| Destination | `cp -r` (+ `sync`) | duet, journal on tmpfs | duet, journal on `$HOME` (production) |
|---|---|---|---|
| tmpfs | 0.14 s (+0.00) | 0.70 s | **178 s** |
| btrfs (NVMe) | 0.45 s (+0.17) | 1.37 s | **211 s** |

The job writes **40 006 journal records for 10 000 files** (four per file: intent and completion for the copy step and again for the deferred `SetMeta` step; 11 MB of JSON), and every record is written and fsync'd on its own by one serial journal thread whose reply the executor awaits before the next side effect. On the home filesystem that is ~4.5 ms per record, so the executor is capped at roughly 55 files/s regardless of what the copy itself costs; on a rotational home disk it would be ~2 files/s (extrapolated, not run). Even with the fsyncs made free (journal on tmpfs), the per-file pipeline is 3–5× `cp -r` (~70 µs/file of channel round-trips, temp-file + rename, and `SetMeta`).

This was the most important performance finding to date and was not a tuning problem: fsync-per-record was the design.

**Fixed the same day** (`docs/crash-safety.md`, "Batching"): the journal thread group-commits (one `write` + one `fsync` per batch), redo-safe steps (`CreateDir`, `CopyFile`, `Reflink`, `SetMeta`, `Verify`) have their intents journaled 64 at a time ahead of execution and their completions queued without waiting, and everything else keeps the strict protocol. Same corpus, same machine, same 40 006 records:

| Destination | `cp -r` (+ `sync`) | duet, journal on tmpfs | duet, journal on `$HOME` (production) |
|---|---|---|---|
| tmpfs | 0.14 s (+0.05) | 0.30 s (was 0.70) | **0.82–0.93 s (was 178 s)** |
| btrfs (NVMe) | 0.42 s (+0.08) | 0.91–1.12 s (was 1.37) | **1.56–2.43 s (was 211 s)** |

That is a 100–200× improvement where it mattered, and it puts the whole per-file pipeline at 3–6× `cp -r` with the journal fully durable, against `cp -r`'s no-durability-at-all. The large-file path is unchanged (4 GiB on tmpfs: 1.75 s, `cp` 1.90 s; six journal records). Remaining per-file cost is now the temp-file + rename write path and the four records per file; folding `SetMeta` into its copy step's records would halve the latter and is the next step if small-file throughput is ever the bottleneck again.

### T-5.1.1 — planning 100k files ≤ 2 s: **met**

`plans_100k_files_within_two_seconds_with_accurate_totals` (release, tmpfs corpus): the whole test, corpus creation included, completes in **1.44 s**, so the planning walk it asserts on is comfortably under the 2 s budget. In a debug build the same test is a known contention flake (it is isolated in `.config/nextest.toml` for that reason), which is a property of debug-build speed under a full-parallel test run, not of the planner.

### T-4.2.1 / NFR-05 — 1M-row table at 120 Hz, zero allocations per frame: **not measurable headlessly**

`cargo run -p duet-ui --release --example bench_file_table` opens a real GPUI window and samples frame times from the compositor's frame callbacks. Run from a headless shell the window receives no frames and the benchmark parks forever (18 minutes at 2% CPU in `epoll_wait` on 2026-09-06); it needs to be run from a desktop session with the window visible, which the author must do. What it printed before parking: 1 000 000 synthetic rows generated, pushed into `EntryStore` and sorted in **754 ms**, ~**66 MB** (NFR-06's ≤ 120 MB for 1M entries holds for the model); GPUI framework baseline RSS 248 MB before any data.

### T-5.2.2 — progress redraw ≤ 0.5 ms/frame: **not measured**

No harness exists for it; it needs the same visible-window setup as the row benchmark. Left open.
