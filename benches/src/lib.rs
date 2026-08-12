// SPDX-License-Identifier: MIT
//! `duet-bench`: T-3.3.4's deterministic corpus generator and T-3.3.5's
//! local CI-gate check.
//!
//! # What lives here
//!
//! - [`corpus`] — the seeded, deterministic synthetic-directory generator
//!   (10/1k/100k/1M entry scales; unicode names, sparse files, hardlink
//!   farms, broken symlinks, deep nesting).
//! - `benches/entry_store.rs` — criterion benchmarks of `duet-index`'s
//!   `EntryStore` (population, name lookup, ordered iteration) at every
//!   corpus scale, purely in-memory (no disk I/O — see that file's doc
//!   comment for why `EntryStore` benches don't need materialized files).
//! - `benches/local_listing.rs` — criterion benchmarks of a real,
//!   on-disk directory listing via `std::fs::read_dir`, as a stand-in for
//!   `duet-vfs`'s `LocalFs::read_dir` (design.md §9.1's NFR-03/04 target),
//!   which hadn't landed on `main` as of this task — see that file's doc
//!   comment for the deferral note and what to swap in once it does.
//! - `src/bin/bench_gate.rs` — T-3.3.5's regression gate: compares two
//!   criterion baselines and exits non-zero if any benchmark group
//!   regressed more than a threshold (default 10%).
//!
//! # Running the benchmarks
//!
//! ```text
//! # Record the baseline everything else is compared against:
//! cargo bench -p duet-bench -- --save-baseline main
//!
//! # After making a change, compare against it:
//! cargo bench -p duet-bench -- --baseline main
//!
//! # Or run the CI gate's own pass/fail logic locally:
//! cargo bench -p duet-bench -- --save-baseline pr
//! cargo run -p duet-bench --bin bench_gate -- --baseline main --candidate pr
//! ```
//!
//! Criterion's default output directory (`target/criterion`) is git-
//! ignored (it's under `target/`), so baselines that need to live in the
//! repo are written to `benches/criterion-baselines/` instead by setting
//! `CRITERION_HOME` — see `scripts/bench-gate.sh` for the wrapper that
//! does this consistently for both local runs and CI.
//!
//! # A note on corpus disk usage
//!
//! The 1M-entry scale materializes roughly 100-150 MB of real file content
//! (see `corpus::plain_size`'s distribution) plus a handful of MB of
//! directory-entry/metadata overhead — deliberately kept far under the
//! corpus's *logical* footprint (sparse files claim 8 MiB+ each but write
//! ~0 real bytes). Even so, generating and then deleting a 1M-entry corpus
//! is real I/O; benches that use it are opt-in via the `DUET_BENCH_1M=1`
//! environment variable rather than running by default.

pub mod corpus;
