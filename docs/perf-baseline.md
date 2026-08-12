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
