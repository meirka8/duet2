// SPDX-License-Identifier: MIT
//! `JobEvent` — the stream a UI subscribes to (design.md §9.3, §8.2).
//!
//! §8.2: "Message flow is one-directional: UI -> command -> core service
//! (async task) -> event stream -> UI applies a diff. No core component
//! holds a UI handle; the UI subscribes." `JobEvent` is that event stream
//! for the operation engine specifically. §9.3: "Updated on a 100 ms timer
//! sampling atomic counters — decoupled from I/O chunk size so throughput
//! never distorts the refresh rate."

use serde::{Deserialize, Serialize};

use crate::conflict::ConflictPrompt;
use crate::job::{JobId, JobOutcome, JobReport, StepFailure};
use crate::plan::PlanTotals;
use crate::step::StepKind;

/// A point-in-time sample of a job's progress counters, taken by the
/// executor's 100 ms timer (design.md §9.3). Deliberately plain `u64`
/// fields, not `AtomicU64` — this is a *copy* taken off the live atomics at
/// sample time, cheap to clone and send across the event channel to the UI
/// thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    pub files_done: u64,
    pub bytes_done: u64,
    /// Progress within the file currently being copied — the "current
    /// file" half of FR-OPS-03's "two-level progress (current file +
    /// total)".
    pub current_file_bytes_done: u64,
    pub current_file_bytes_total: u64,
    pub throughput_bytes_per_sec: u64,
    /// `None` until the dual-regime EMA (design.md §9.3) has enough samples
    /// to estimate; never negative, and the executor is responsible for the
    /// "never runs backwards for more than one sample" AC (T-5.1.11), not
    /// this type.
    pub eta_secs: Option<u64>,
}

/// An event a running/queued [`Job`](crate::Job) emits, for the UI (or any
/// other subscriber — the queue manager, a headless test harness) to
/// observe without polling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobEvent {
    /// The job was accepted into the queue.
    Queued {
        job_id: JobId,
    },
    /// The async source walk (T-5.1.1) began.
    PlanningStarted {
        job_id: JobId,
    },
    /// The source walk finished; `totals` are now final and progress
    /// reporting can be honest from this point on (FR-OPS-03).
    Planned {
        job_id: JobId,
        totals: PlanTotals,
    },
    /// Execution of `plan.steps` began.
    Started {
        job_id: JobId,
    },
    /// A 100 ms-cadence progress sample.
    Progress {
        job_id: JobId,
        snapshot: ProgressSnapshot,
    },
    /// Step `step_index` began executing — the moment that corresponds to
    /// the step's journal `Intent` record becoming durable.
    StepStarted {
        job_id: JobId,
        step_index: u32,
        kind: StepKind,
    },
    /// Step `step_index` finished successfully.
    StepCompleted {
        job_id: JobId,
        step_index: u32,
    },
    /// Step `step_index` was skipped by a conflict policy (not an error).
    StepSkipped {
        job_id: JobId,
        step_index: u32,
        reason: String,
    },
    /// Step `step_index` failed. Does not necessarily end the job —
    /// `ErrorKind::Retryable` failures retry with backoff before this event
    /// is even emitted; `Permission`/`Fatal` failures emit this and then
    /// either skip-and-continue or end the job, per the active
    /// `ConflictPolicy`/user choice.
    StepFailed {
        job_id: JobId,
        failure: StepFailure,
    },
    /// A conflict was detected and needs a user answer (or the job-level
    /// default already resolved it — see `PlanOptions::default_conflict`;
    /// this event fires only when interactive resolution is actually
    /// needed). The UI responds out-of-band (through a command, not
    /// modelled in this crate), which the executor consumes as a
    /// `ConflictResolution`.
    /// `prompt` is boxed: it carries two full `Metadata` values (xattr
    /// maps, ACL bytes, ...), which would otherwise make every other, far
    /// more common `JobEvent` variant pay for stack space it never uses
    /// (`clippy::large_enum_variant`).
    ConflictDetected {
        job_id: JobId,
        prompt: Box<ConflictPrompt>,
    },
    /// The whole queue paused because a step hit `ErrorKind::Space`
    /// (design.md §9.3: "pause the whole queue, not just the job").
    QueuePausedForSpace {
        job_id: JobId,
    },
    Paused {
        job_id: JobId,
    },
    Resumed {
        job_id: JobId,
    },
    /// The job reached a terminal state. Always carries the same
    /// `JobReport` that ends up in `JobState::Terminal`.
    Finished {
        job_id: JobId,
        outcome: JobOutcome,
        report: JobReport,
    },
}

impl JobEvent {
    /// The job this event pertains to, common to every variant — lets a
    /// subscriber route events to per-job UI state without a big `match`.
    pub fn job_id(&self) -> JobId {
        match self {
            JobEvent::Queued { job_id }
            | JobEvent::PlanningStarted { job_id }
            | JobEvent::Planned { job_id, .. }
            | JobEvent::Started { job_id }
            | JobEvent::Progress { job_id, .. }
            | JobEvent::StepStarted { job_id, .. }
            | JobEvent::StepCompleted { job_id, .. }
            | JobEvent::StepSkipped { job_id, .. }
            | JobEvent::StepFailed { job_id, .. }
            | JobEvent::ConflictDetected { job_id, .. }
            | JobEvent::QueuePausedForSpace { job_id }
            | JobEvent::Paused { job_id }
            | JobEvent::Resumed { job_id }
            | JobEvent::Finished { job_id, .. } => *job_id,
        }
    }
}
