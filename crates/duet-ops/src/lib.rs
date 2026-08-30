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
//! - [`deleter`] — T-5.1.8/T-5.3.1: `JobKind::Delete`'s two modes.
//!   Permanent delete needs no walk of its own (`RemoveMode::Recursive`
//!   already recurses safely at the `duet-vfs` layer, T-3.1.3). Trash mode
//!   (T-5.3.1) is the full freedesktop trash spec: `duet_platform::trash`
//!   resolves each target's own trash destination (home trash, or the
//!   correct per-mount `$topdir/.Trash{,-$uid}` for a target on another
//!   filesystem) and this module turns that into a `Step::WriteTrashInfo`
//!   and `Step::Rename` pair per target. T-5.3.2 (a browsable/restorable
//!   trash view) is still later, separate scope.
//! - [`creators`] — T-5.2.7's four "create one thing" planners
//!   ([`creators::plan_mkdir`], [`creators::plan_rename_in_place`],
//!   [`creators::plan_symlink`], [`creators::plan_hardlink`]): the
//!   backing jobs for F7, Shift+F6, and the symlink/hardlink dialogs.
//!   Deliberately the smallest planners here — no walk, no totals worth
//!   computing — but still real journaled jobs, so a `mkdir` gets the same
//!   off-UI-thread execution, crash evidence, and conflict engine a copy
//!   does. See its own module doc comment.
//! - [`attributes`] — T-5.2.8: [`attributes::plan_attributes`], the
//!   `Ctrl+A` "Change Attributes" dialog's backing planner. Adds no `Step`
//!   variant and no executor arm — `Step::SetMeta`/`MetaPatch` have both
//!   been real since T-3.1.5/T-5.1.6 — it is only the walk that decides
//!   which paths one patch applies to, which is what makes "recursive
//!   apply runs through the operation queue, not synchronously" (its own
//!   AC) true by construction. See its module doc comment for the one
//!   genuinely load-bearing decision in it: a symlink met during the
//!   recursive walk is skipped, because `set_meta`'s mode half is a
//!   symlink-*following* `chmodat` and design.md §13 forbids following one
//!   implicitly during a recursive chmod.
//! - [`executor`] — T-5.1.3: runs a `Plan`'s steps against a `FileSystem`,
//!   bracketing every one with journal `Intent`/`Completion` records, with
//!   a bounded per-device-aware worker pool and cooperative pause/cancel.
//!   See its own module doc comment for the (deliberately disclosed)
//!   scope cuts — retry/backoff (T-5.1.10) and ETA (T-5.1.11) are each a
//!   separate task; multi-job queueing is [`queue`], below.
//! - [`queue`] — T-5.1.13: [`queue::QueueManager`], a bounded-concurrency
//!   scheduler over many [`job::Job`]s at once — priority ordering,
//!   pause/resume/cancel/reorder, and the queue-wide `ErrorKind::Space`
//!   propagation `executor`'s own module doc comment disclosed as not yet
//!   built ("pause the whole queue, not just the job").
//! - [`conflict`] — T-5.1.9: the FR-OPS-04 policy set, the per-conflict
//!   prompt data the UI needs, and [`conflict::ConflictResolver`], the seam
//!   a live UI (or a test) plugs into. All seven TC policies are real,
//!   resolved by the executor in design.md §9.3's own tiering, highest
//!   precedence first: a `Step`'s own pre-resolved `conflict` field (set
//!   once, at plan time) → an already-established per-job "apply to all"
//!   answer → a live `ConflictResolver`, if `execute()` was given one →
//!   `PlanOptions::default_conflict` (the job-level default, and the only
//!   tier available with no live resolver at all).
//! - [`rerun`] — T-5.2.4: [`rerun::plan_from_report`], the pure
//!   `Plan` + `JobReport` -> smaller `Plan` rebuild behind the error/skip
//!   report's own "re-run failed" action (`docs/commands.md`'s
//!   `ops.queue.retry_failed`). The one planner here that walks nothing at
//!   all: everything it needs is already in the finished job.
//! - [`recovery`] — T-5.2.5 (FR-OPS-07), phase 1 of 2:
//!   [`recovery::plan_from_recovery`], the same "keep only these step
//!   indices, remap `depends_on`" rebuild as `rerun` but sourced from a
//!   crash-recovered [`journal::RecoveryReport`]'s `incomplete_steps`
//!   instead of a finished job's errors/skips, plus
//!   [`recovery::orphaned_partial_path`], which re-derives an orphaned
//!   `.duet-partial-*` file's real path from a `RecoveryReport` so a
//!   "discard" action can delete it. This crate's half of the "N
//!   interrupted operations — review" startup story: the actual scan
//!   already exists ([`journal::JournalReader::scan`]), and
//!   [`journal::Journal::resolve`] closes a job back out once its
//!   dangling intents have been resumed or discarded. Building the
//!   startup UI itself on top of this is phase 2's job, in `duet-ui`.
//! - [`job`] / [`event`] — the queued unit and the event stream a UI (or
//!   test harness) subscribes to instead of polling (design.md §8.2).
//! - [`journal`] — T-5.1.2, the FR-OPS-07 crash-safety backbone:
//!   append-only, fsync'd intent/completion records a recovery reader
//!   ([`journal::JournalReader::scan`]) can replay after a SIGKILL. See
//!   `docs/crash-safety.md` (T-2.3.2) for the interruption-point-by
//!   -interruption-point proof sketch this record format exists to
//!   support, and `journal`'s own module doc comment for exactly how much
//!   of that proof this task covers versus leaves to T-10.2.1.

mod attributes;
mod conflict;
mod creators;
mod deleter;
mod event;
mod executor;
mod job;
mod journal;
mod mover;
mod plan;
mod planner;
mod queue;
mod recovery;
mod rerun;
mod step;

pub use attributes::plan_attributes;
pub use conflict::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, ConflictScope,
};
pub use creators::{plan_hardlink, plan_mkdir, plan_rename_in_place, plan_symlink};
pub use deleter::{DeleteMode, plan_delete};
pub use event::{JobEvent, ProgressSnapshot};
pub use executor::{ControlState, ExecutionControl, execute, suggested_concurrency};
pub use job::{Job, JobId, JobKind, JobOutcome, JobReport, JobState, SkipEntry, StepFailure};
pub use journal::{Journal, JournalReader, JournalRecord, RecoveryReport, StepOutcome};
pub use mover::plan_move;
pub use plan::{Plan, PlanOptions, PlanTotals};
pub use planner::{CancelToken, PlannerError, plan_copy};
pub use queue::{QueueError, QueueManager};
pub use recovery::{orphaned_partial_path, plan_from_recovery};
pub use rerun::plan_from_report;
pub use step::{RemoveMode, Step, StepKind, VerifyAlgorithm};
