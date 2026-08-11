// SPDX-License-Identifier: MIT
//! `Job` — the queued unit wrapping a [`Plan`] with state (design.md §9.3).
//!
//! "Jobs are queued, not fire-and-forget... The job never dies silently: it
//! ends `Completed`, `CompletedWithSkips`, `Cancelled`, or `Failed`, always
//! with a report." [`JobState::Terminal`] encodes that "always with a
//! report" as a type-level fact rather than a convention every call site
//! has to remember: there is no way to construct a terminal `JobState`
//! without also supplying a [`JobReport`].
//!
//! Note on atomics (design.md §9.3: "Updated on a 100 ms timer sampling
//! atomic counters"): those atomics are an executor-internal implementation
//! detail (T-5.1.3) — `Arc<AtomicU64>` counters shared between worker
//! threads and the progress-sampling timer. `Job` itself is the
//! queue-visible *snapshot* type (what a queue manager UI lists, what could
//! be persisted/inspected), so its progress-shaped data lives in plain
//! non-atomic counters, populated by sampling the real atomics — see
//! [`crate::event::ProgressSnapshot`], which is exactly that sample.

use duet_types::{ErrorKind, Timestamp, VPath};
use serde::{Deserialize, Serialize};

use crate::plan::Plan;

/// Identifies a single queued or running job. Unique within one running
/// `duet` process's queue (and within `~/.local/state/duet/jobs/`, since
/// the journal file name is derived from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

/// What kind of user-facing operation a [`Job`] represents. Distinct from
/// the `Step` kinds a `Plan` is made of: one `JobKind::Copy` job's plan may
/// contain `CreateDir`, `CopyFile`, *and* `Link` steps, but it is still one
/// conceptual "copy" the queue manager shows as a single row (design.md
/// §9.3: "a tray in the status bar showing aggregate progress").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    Copy,
    Move,
    /// `permanent: false` -> trash (FR-CFG-07); `true` -> permanent delete,
    /// bypassing trash (Shift+Del).
    Delete {
        permanent: bool,
    },
    CreateDir,
    CreateSymlink,
    CreateHardlink,
}

/// A queued job: a `Plan` plus its current lifecycle state and queue
/// position. This is the type a queue manager (T-5.1.13) lists, reorders,
/// and drives pause/resume/cancel through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub plan: Plan,
    pub state: JobState,
    /// Manual queue ordering weight (T-5.1.13: "job ordering, priorities,
    /// reordering"). Lower runs first; ties broken by queue-insertion
    /// order. Plain `i32` rather than an enum: reordering needs an
    /// unbounded, densely-comparable space to insert between two existing
    /// priorities without renumbering the whole queue.
    pub priority: i32,
}

/// A [`Job`]'s lifecycle state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobState {
    /// Waiting in the queue; not yet planning or running.
    Queued,
    /// The async, cancellable source walk (T-5.1.1) is in progress; totals
    /// are not yet final.
    Planning,
    /// Executing `plan.steps[current_step]` (or about to — the boundary
    /// between "about to run step N" and "running step N" is exactly what
    /// the journal's `Intent` record for step N marks, see
    /// [`crate::journal`]).
    Running { current_step: u32 },
    /// Paused mid-plan by the user or by an `ErrorKind::Space` queue-wide
    /// pause (design.md §9.3's error taxonomy: "ENOSPC/EDQUOT -> pause the
    /// whole queue"). `current_step` is where it will resume.
    Paused { current_step: u32 },
    /// The job reached one of its four possible ends. See [`JobOutcome`]
    /// for which; a `JobReport` is *always* attached, by construction.
    Terminal {
        outcome: JobOutcome,
        report: JobReport,
    },
}

impl JobState {
    /// `true` for any of the three states that still occupy a worker/queue
    /// slot (as opposed to `Terminal`, which does not).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            JobState::Queued | JobState::Planning | JobState::Running { .. }
        )
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, JobState::Terminal { .. })
    }
}

/// The four ways a [`Job`] can end (design.md §9.3, verbatim list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobOutcome {
    /// Every step succeeded.
    Completed,
    /// Every step either succeeded or was deliberately skipped by a
    /// conflict policy (`ConflictPolicy::Skip` and friends) — distinct from
    /// `Completed` because the destination tree is not byte-identical to
    /// what a naive "copy everything" would have produced, and the UI
    /// should say so rather than claiming an unqualified success.
    CompletedWithSkips,
    /// The user (or an `Abort` conflict answer) stopped the job before it
    /// finished. Not an error.
    Cancelled,
    /// A step failed with an error the job could not recover from or the
    /// user chose not to retry/skip past.
    Failed,
}

/// The report every terminal [`Job`] carries (design.md §9.3: "always with
/// a report"). Feeds both the operation-manager summary UI and, for
/// `errors`, the "50 permission errors -> one actionable list" view
/// (T-5.2.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobReport {
    pub files_completed: u64,
    pub bytes_completed: u64,
    pub skipped: Vec<SkipEntry>,
    pub errors: Vec<StepFailure>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// One step a [`ConflictPolicy`](crate::ConflictPolicy) caused to be
/// skipped rather than executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkipEntry {
    pub step_index: u32,
    pub path: VPath,
    /// Human-readable reason (e.g. "destination newer than source" for
    /// `OverwriteIfOlder`), not just the policy name, since the report is
    /// meant to be read directly by a user reviewing what happened.
    pub reason: String,
}

/// A single step's failure, classified through `duet_types::ErrorKind` per
/// design.md §9.3's error taxonomy ("reuse it, don't redefine" — T-2.3.1's
/// brief). This is a serializable *summary* of a failure, not the full
/// `VfsError` (which carries a `Box<dyn Error>` source that generally can't
/// round-trip through serde) — enough to drive the error-report UI
/// (T-5.2.4) and to journal a step's outcome (T-2.3.2/[`crate::journal`]),
/// which is all either consumer needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepFailure {
    pub step_index: u32,
    pub path: Option<VPath>,
    pub kind: ErrorKind,
    pub message: String,
}
