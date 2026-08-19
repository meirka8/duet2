// SPDX-License-Identifier: MIT
//! Operation engine: planner, executor, journal, queue, conflict policy.
//!
//! T-2.3.1 (design.md §9.3): the operation engine's core interfaces —
//! `Plan`, `Step`, `Job`, `JobEvent`, `ConflictPolicy`, `Journal` — as real,
//! compiling, serializable types. This is a Phase 2 interface-design task:
//! field and variant *shapes* are complete per the design doc, but most
//! runtime behaviour (the executor's step loop, real queue management) is
//! out of scope and left `todo!()`, to be filled in by their own dedicated
//! Phase 5 tasks (T-5.1.3…T-5.1.13). T-5.1.1 and T-5.1.2 are the first two
//! of those to land: [`planner::plan_copy`] (the async/cancellable source
//! walk) and [`journal::Journal`]/[`journal::JournalReader`] (the real
//! append-fsync-replay file I/O).
//!
//! Structure mirrors design.md §9.3's own framing, "plan -> execute ->
//! journal":
//! - [`step`] / [`plan`] — what a job intends to do, and the honest totals
//!   that make FR-OPS-03 progress reporting possible from the start.
//! - [`planner`] — T-5.1.1: walks a source set through `duet_vfs`'s
//!   `FileSystem` trait and materialises the `Plan` above, cancellably.
//! - [`conflict`] — the FR-OPS-04 policy set and the per-conflict prompt
//!   data the UI needs.
//! - [`job`] / [`event`] — the queued unit and the event stream a UI (or
//!   test harness) subscribes to instead of polling (design.md §8.2).
//! - [`journal`] — T-5.1.2, the FR-OPS-07 crash-safety backbone:
//!   append-only, fsync'd intent/completion records a recovery reader
//!   ([`journal::JournalReader::scan`]) can replay after a SIGKILL. See
//!   `docs/crash-safety.md` (T-2.3.2) for the interruption-point-by
//!   -interruption-point proof sketch this record format exists to
//!   support, and `journal`'s own module doc comment for exactly how much
//!   of that proof this task covers versus leaves to T-10.2.1.

mod conflict;
mod event;
mod job;
mod journal;
mod plan;
mod planner;
mod step;

pub use conflict::{ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictScope};
pub use event::{JobEvent, ProgressSnapshot};
pub use job::{Job, JobId, JobKind, JobOutcome, JobReport, JobState, SkipEntry, StepFailure};
pub use journal::{Journal, JournalReader, JournalRecord, RecoveryReport, StepOutcome};
pub use plan::{Plan, PlanOptions, PlanTotals};
pub use planner::{CancelToken, PlannerError, plan_copy};
pub use step::{RemoveMode, Step, StepKind, VerifyAlgorithm};
