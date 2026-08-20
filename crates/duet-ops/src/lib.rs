// SPDX-License-Identifier: MIT
//! Operation engine: planner, executor, journal, queue, conflict policy.
//!
//! T-2.3.1 (design.md §9.3): the operation engine's core interfaces —
//! `Plan`, `Step`, `Job`, `JobEvent`, `ConflictPolicy`, `Journal` — as real,
//! compiling, serializable types. This is a Phase 2 interface-design task:
//! field and variant *shapes* are complete per the design doc, but most
//! runtime behaviour (real queue management across many jobs) is out of
//! scope and left `todo!()`, to be filled in by dedicated later Phase 5
//! tasks (T-5.1.9…T-5.1.13). T-5.1.1, T-5.1.2, and T-5.1.3 are the first
//! three to land: [`planner::plan_copy`] (the async/cancellable source
//! walk), [`journal::Journal`]/[`journal::JournalReader`] (the real
//! append-fsync-replay file I/O), and [`executor::execute`] (the step loop
//! that actually runs a `Plan` against a `FileSystem`).
//!
//! Structure mirrors design.md §9.3's own framing, "plan -> execute ->
//! journal":
//! - [`step`] / [`plan`] — what a job intends to do, and the honest totals
//!   that make FR-OPS-03 progress reporting possible from the start.
//! - [`planner`] — T-5.1.1: walks a source set through `duet_vfs`'s
//!   `FileSystem` trait and materialises the `Plan` above, cancellably.
//! - [`mover`] — T-5.1.5: same idea, for moves — a same-device entry
//!   becomes one zero-cost `Rename`; cross-device becomes a `CopyFile` +
//!   (optional `Verify`) + `Remove` sequence, dependency-gated so the
//!   source is never removed unless its copy is known to have succeeded.
//! - [`deleter`] — T-5.1.8: `JobKind::Delete`'s two modes. Permanent
//!   delete needs no walk of its own (`RemoveMode::Recursive` already
//!   recurses safely at the `duet-vfs` layer, T-3.1.3); trash mode is
//!   [`mover::plan_move`] reused wholesale, targeting a caller-supplied
//!   trash directory — the full freedesktop trash spec itself is
//!   T-5.3.1/T-5.3.2's later, separate scope.
//! - [`executor`] — T-5.1.3: runs a `Plan`'s steps against a `FileSystem`,
//!   bracketing every one with journal `Intent`/`Completion` records, with
//!   a bounded per-device-aware worker pool and cooperative pause/cancel.
//!   See its own module doc comment for the (deliberately disclosed)
//!   scope cuts — retry/backoff, ETA, and multi-job queueing are each a
//!   separate, later task.
//! - [`conflict`] — T-5.1.9: the FR-OPS-04 policy set, the per-conflict
//!   prompt data the UI needs, and [`conflict::ConflictResolver`], the seam
//!   a live UI (or a test) plugs into. All seven TC policies are real,
//!   resolved by the executor in design.md §9.3's own tiering, highest
//!   precedence first: a `Step`'s own pre-resolved `conflict` field (set
//!   once, at plan time) → an already-established per-job "apply to all"
//!   answer → a live `ConflictResolver`, if `execute()` was given one →
//!   `PlanOptions::default_conflict` (the job-level default, and the only
//!   tier available with no live resolver at all).
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
mod deleter;
mod event;
mod executor;
mod job;
mod journal;
mod mover;
mod plan;
mod planner;
mod step;

pub use conflict::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, ConflictScope,
};
pub use deleter::{DeleteMode, plan_delete};
pub use event::{JobEvent, ProgressSnapshot};
pub use executor::{ControlState, ExecutionControl, execute, suggested_concurrency};
pub use job::{Job, JobId, JobKind, JobOutcome, JobReport, JobState, SkipEntry, StepFailure};
pub use journal::{Journal, JournalReader, JournalRecord, RecoveryReport, StepOutcome};
pub use mover::plan_move;
pub use plan::{Plan, PlanOptions, PlanTotals};
pub use planner::{CancelToken, PlannerError, plan_copy};
pub use step::{RemoveMode, Step, StepKind, VerifyAlgorithm};
