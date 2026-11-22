// SPDX-License-Identifier: MIT
//! The data-safety suite (T-10.2.1, design.md §14.4, NFR-08) -- Duet's
//! release gate for file operations.
//!
//! One assertion, checked after every fault this suite can inject:
//!
//! > for every injection point, either the source is intact and the
//! > destination is absent/partial-and-marked, or the destination is
//! > complete. Never anything else.
//!
//! Layout:
//!
//! - [`model`] -- an independent prediction of what a `Plan` does to a
//!   tree, and the checker that turns the sentence above into a path-by
//!   -path comparison. The suite never asks the engine what the right
//!   answer is.
//! - [`harness`] -- the child process that runs a job and is killed in it,
//!   and the same resume/discard path the product's recovery dialog uses.
//! - [`sigkill_matrix`] -- `docs/crash-safety.md`'s table, one test per
//!   row, each killed at exactly its interruption point.
//! - [`sigkill_sweep`] -- real copy/move/delete plans killed at every
//!   boundary of every step, and at arbitrary instants.
//! - [`faults`] -- I/O errors and mid-transfer disconnects, injected at
//!   every backend call of a job.
//! - [`permissions`] -- real `EACCES` from real permission bits.
//! - [`enospc`] -- real `ENOSPC` from a real, full filesystem.
//! - [`privileged`] -- the cases that need root: a loop-device filesystem
//!   and `dm-flakey`. `#[ignore]`d in an ordinary run and run by CI's
//!   `data-safety (privileged)` job; see that module.
//! - [`checklist`] -- keeps the table in `docs/crash-safety.md` and the
//!   tests here from drifting apart.
//!
//! No test here may be marked flaky, retried into passing, or skipped when
//! its environment is missing: a case that cannot run fails, saying why.

mod checklist;
mod conflicts;
mod enospc;
mod faults;
mod harness;
mod model;
mod permissions;
mod privileged;
mod scenarios;
mod sigkill_matrix;
mod sigkill_sweep;

/// The child half of every crash test: a no-op unless this process was
/// spawned by [`harness::Child::spawn`], in which case it runs one job and
/// never returns. It has to be a `#[test]` because re-executing the test
/// binary with `--exact` is the only way to reach code in it.
#[test]
fn child_process_entry() {
    harness::child_main();
}
