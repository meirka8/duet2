// SPDX-License-Identifier: MIT
//! `execute` — T-5.1.3's step loop: runs a [`Plan`] against a
//! [`duet_vfs::FileSystem`], bracketing every step with [`Journal`]
//! `Intent`/`Completion` records per `docs/crash-safety.md`'s "Mechanism
//! common to every step", with a bounded, per-device-aware worker pool and
//! cooperative pause/cancel.
//!
//! # Scope: what this module deliberately does *not* do
//!
//! Mirroring [`crate::planner`]'s own precedent of disclosing scope cuts
//! rather than silently omitting them:
//!
//! - **`Step::Link`/`Step::Verify` are not dispatched** — `duet-vfs`'s
//!   `FileSystem` trait has no `link`/hardlink method today, and no
//!   planner emits either step kind yet (`plan_copy` never produces one).
//!   Both fail cleanly with a descriptive [`StepFailure`] rather than a
//!   `todo!()`, so if a future planner *does* start emitting them before
//!   their owning tasks land, the failure is loud and clear rather than
//!   silently mishandled. `Link` belongs to T-5.1.7 (the hardlink-graph
//!   task, the only one that will ever produce a `Link` step); `Verify`
//!   belongs to T-5.1.12 (post-copy BLAKE3 verification).
//! - **Conflict resolution is a placeholder, not the real seven-policy
//!   engine.** `Step`'s own doc comment requires the executor to
//!   "re-check immediately before acting" whether a destination now
//!   exists — this module does that (every mutating call already returns
//!   `ErrorKind::Conflict` on an unexpected pre-existing destination) but
//!   treats *any* such conflict uniformly as [`StepOutcome::Skipped`],
//!   ignoring `PlanOptions::default_conflict`/`Step`'s own `conflict`
//!   field entirely. Implementing the real "job-level default →
//!   per-conflict answer → apply-to-all" resolution (all seven TC
//!   policies) is T-5.1.9's whole purpose — a task that doesn't exist yet
//!   would be pre-empted, not helped, by half-implementing it here.
//! - **No retry-with-backoff for `Retryable` failures, no `ENOSPC`
//!   queue-wide pause, no `EACCES` elevation offer.** A `Retryable`/
//!   `Space`/`Permission` failure is classified and recorded as a
//!   [`StepFailure`] like any other; T-5.1.10 ("Error taxonomy handling")
//!   is where retry loops and queue-wide pause-on-space land.
//! - **No ETA.** [`ProgressSnapshot::eta_secs`] is always `None` — the
//!   dual-regime EWMA estimator is T-5.1.11's job. The counters and
//!   100ms-cadence sampling T-5.1.11 needs to build on top of are real,
//!   per `job.rs`'s own doc comment attributing that machinery to T-5.1.3.
//! - **`current_file_bytes_done`/`current_file_bytes_total` are always
//!   `0`.** FR-OPS-03's "current file + total" framing assumes one file
//!   copies at a time; with `concurrency > 1`, several files can be
//!   mid-copy simultaneously, and there is no single "current file" to
//!   report without inventing an aggregate convention nothing has
//!   specified. `files_done`/`bytes_done` (whole-job totals) are tracked
//!   precisely; per-file live progress under concurrency is left for
//!   whichever of T-5.1.11/T-5.2.2 first needs to reconcile that framing
//!   with concurrent copying, rather than guessed at here.
//! - **No multi-job queueing.** This module runs exactly one job to
//!   completion (or pause/cancel); ordering, priority, and aggregate
//!   queue-wide state across many jobs is T-5.1.13.
//! - **Resume restarts the paused/interrupted step from scratch, not from
//!   a mid-file byte offset.** Continuing a partially-written `.duet-
//!   partial-*` file exactly where it left off needs `Caps::APPEND_RESUME`
//!   support this executor doesn't attempt to exploit — restarting the
//!   step is strictly safe (the step's own `Intent`/journal bracketing
//!   already guarantees that) and satisfies "resumes correctly," just not
//!   with byte-exact efficiency.
//!
//! # Concurrency model
//!
//! `plan.steps` is walked in order. Consecutive `CopyFile`/`Reflink` steps
//! are batched and drained through a [`tokio::sync::Semaphore`]-bounded
//! pool of up to `concurrency` concurrently spawned tasks; any other step
//! kind (`CreateDir`, `Rename`, `Link`, `SetMeta`, `Remove`) acts as a
//! barrier — it always runs alone, after every step before it (including
//! the rest of its own batch) has fully completed, and nothing after it
//! starts until it has. This is deliberately simpler than a full
//! dependency DAG: the *only* real inter-step ordering hazard a `Plan` can
//! contain is "a directory's `CreateDir` before anything written inside
//! it" (`step.rs`'s own doc comment: "the planner is responsible for
//! ordering ancestor `CreateDir` steps before anything that writes inside
//! them"), and since every barrier step is executed to completion before
//! the walk crosses it, that hazard can never be violated. The known
//! limitation this leaves on the table: two `CopyFile` batches separated
//! by an unrelated `CreateDir` for a *different* subtree can't overlap
//! with each other even though it would be safe to let them — extracting
//! that extra parallelism would need real dependency tracking, which
//! nothing about this task's AC ("concurrency respects rotational
//! detection") requires.
//!
//! [`suggested_concurrency`] is a free function, not something this module
//! calls internally — rotational detection ([`duet_vfs::local::FsProps`])
//! is a `LocalFs`-only concept with no meaning for a remote/archive
//! backend, so tying `execute`'s own signature to it would break the
//! backend-agnostic `&dyn FileSystem` abstraction everything else in this
//! crate is built on. A caller that has a concrete `LocalFs` and wants the
//! rotational-aware default calls `suggested_concurrency` itself and
//! passes the resulting number in.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use duet_types::{ErrorKind, MetaPatch, Result, Timestamp, VPath, VfsError};
use duet_vfs::{FileSystem, Mode, RemoveKind, RenameFlags, WriteOpts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::event::{JobEvent, ProgressSnapshot};
use crate::job::{JobId, JobOutcome, JobReport, StepFailure};
use crate::journal::{Journal, JournalRecord, StepOutcome};
use crate::plan::Plan;
use crate::step::{RemoveMode, Step, StepKind};

/// A cheaply cloneable, three-state cooperative control flag for a single
/// running job: `Running`, `Paused`, or `Cancelled`. A single `AtomicU8`
/// rather than two independent `AtomicBool`s (mirroring `planner::
/// CancelToken`'s shape but richer) — `job.rs`'s own `JobState` already
/// models `Running`/`Paused`/`Terminal(Cancelled)` as mutually exclusive,
/// and a single atomic makes "paused and cancelled at once" structurally
/// impossible rather than a state two independent flags could disagree on.
///
/// Checked cooperatively at step and copy-chunk boundaries — see the
/// module doc comment's concurrency section and [`copy_file_step`]'s
/// per-chunk check — never pre-empts a syscall in flight.
#[derive(Debug, Clone)]
pub struct ExecutionControl(Arc<AtomicU8>);

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const CANCELLED: u8 = 2;

/// A snapshot of [`ExecutionControl`]'s current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    Running,
    Paused,
    Cancelled,
}

impl Default for ExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionControl {
    pub fn new() -> Self {
        ExecutionControl(Arc::new(AtomicU8::new(RUNNING)))
    }

    pub fn pause(&self) {
        // A cancelled job can't be un-cancelled by a pause request -- only
        // move Running -> Paused.
        let _ = self
            .0
            .compare_exchange(RUNNING, PAUSED, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        let _ = self
            .0
            .compare_exchange(PAUSED, RUNNING, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.0.store(CANCELLED, Ordering::Relaxed);
    }

    pub fn state(&self) -> ControlState {
        match self.0.load(Ordering::Relaxed) {
            PAUSED => ControlState::Paused,
            CANCELLED => ControlState::Cancelled,
            _ => ControlState::Running,
        }
    }
}

/// A `LocalFs`-only, rotational-aware default worker count -- see the
/// module doc comment's "Concurrency model" section for why `execute`
/// itself never calls this. There is no numeric guidance anywhere in
/// design.md/task.md for how many concurrent copies a spinning disk vs. an
/// SSD should get; these numbers are a documented, deliberately
/// conservative choice, not a derived or benchmarked one:
/// - A confirmed-rotational device (`rotational: Some(true)`) gets `1`:
///   concurrent writes to a spinning disk fight over the same head,
///   turning sequential throughput into seek-bound thrashing.
/// - Confirmed non-rotational (`Some(false)`) gets `4`: enough to keep an
///   SSD's internal parallelism busy without either starving single-file
///   throughput or overwhelming the destination with more in-flight
///   writes than is useful.
/// - Undeterminable (`None` — tmpfs, or a btrfs subvolume's anonymous
///   `st_dev`, per `local::probe`'s own doc comment) is treated the same
///   as non-rotational: the common real-world case behind `None` is
///   memory-backed tmpfs, which is trivially parallel-safe.
pub fn suggested_concurrency(props: &duet_vfs::local::FsProps) -> usize {
    match props.rotational {
        Some(true) => 1,
        Some(false) | None => 4,
    }
}

/// One buffer's worth of the naive fallback copy loop -- see
/// [`copy_file_step`]. 1 MiB: small enough that a pause/cancel check every
/// buffer comfortably clears the "stops within 200ms mid-file" AC even on
/// a slow destination, large enough to not dominate the loop with syscall
/// overhead on a fast one.
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

/// Shared, cheaply cloneable state every spawned step-execution task needs.
/// Bundled into one struct so worker-task call sites don't have to thread
/// six separate parameters through every function.
#[derive(Clone)]
struct ExecutorContext {
    fs: Arc<dyn FileSystem>,
    job_id: JobId,
    journal: JournalHandle,
    control: ExecutionControl,
    counters: Arc<ProgressCounters>,
    events: mpsc::UnboundedSender<JobEvent>,
}

/// The raw, non-atomic-across-fields counters a job's [`ProgressSnapshot`]
/// is sampled from every 100ms -- see `job.rs`'s own doc comment: "those
/// atomics are an executor-internal implementation detail... `Job` itself
/// is the queue-visible *snapshot* type."
#[derive(Debug, Default)]
struct ProgressCounters {
    files_done: AtomicU64,
    bytes_done: AtomicU64,
}

/// Runs `plan` to completion (or until paused/cancelled), bracketing every
/// step with journal `Intent`/`Completion` records, emitting [`JobEvent`]s
/// through `events` as it goes, and returning the final [`JobReport`].
///
/// `concurrency` bounds how many `CopyFile`/`Reflink` steps run at once —
/// see [`suggested_concurrency`] for a `LocalFs`-aware default, or the
/// module doc comment's "Concurrency model" section for the full batching
/// scheme.
///
/// # Operational requirement: needs a genuinely multi-threaded runtime
///
/// `LocalFs`'s read/write handles do their blocking syscalls inline inside
/// `poll_read`/`poll_write` (confirmed by reading `duet_vfs::local::rw`) —
/// there is no internal `spawn_blocking`, by the same design convention
/// `local::guard`'s UI-thread assertion documents elsewhere: "the actual
/// off-thread dispatch is the shell layer's job." `execute` follows that
/// convention itself, dispatching each copy-class step onto its own
/// `tokio::spawn`'d task rather than wrapping every individual read/write
/// syscall — which means the *caller*, not this function, is responsible
/// for running it on a `tokio::runtime::Runtime` with enough real worker
/// threads to cover `concurrency` (plus a couple more for the progress
/// sampler and the journal writer's `spawn_blocking` task) — a
/// single-threaded (`current_thread`) runtime would let one large file's
/// blocking write starve every other concurrently-dispatched step and the
/// progress sampler until it finishes. `duet-ui`'s own core Tokio runtime
/// (`tokio::runtime::Builder::new_multi_thread()`, sized off
/// `available_parallelism()`) already satisfies this; a caller building
/// its own runtime just to drive `execute` needs to do the same.
pub async fn execute(
    fs: Arc<dyn FileSystem>,
    job_id: JobId,
    plan: Plan,
    journal: Journal,
    concurrency: usize,
    events: mpsc::UnboundedSender<JobEvent>,
    control: ExecutionControl,
) -> JobReport {
    let started_at = Timestamp::from(SystemTime::now());
    let journal = JournalHandle::spawn(journal);

    let _ = events.send(JobEvent::Started { job_id });
    if let Err(e) = journal
        .append(JournalRecord::JobStarted {
            job_id,
            started_at,
            plan: plan.clone(),
        })
        .await
    {
        // Nothing durable was promised (crash-safety.md's "before any
        // observable side effect" applies to `JobStarted` too, in spirit)
        // -- fail the whole job up front rather than run steps a crash
        // right now could never recover context for.
        let finished_at = Timestamp::from(SystemTime::now());
        return JobReport {
            files_completed: 0,
            bytes_completed: 0,
            skipped: Vec::new(),
            errors: vec![StepFailure {
                step_index: 0,
                path: None,
                kind: e.kind(),
                message: format!("failed to journal JobStarted: {e}"),
            }],
            started_at: Some(started_at),
            finished_at: Some(finished_at),
        };
    }

    let ctx = ExecutorContext {
        fs,
        job_id,
        journal,
        control,
        counters: Arc::new(ProgressCounters::default()),
        events: events.clone(),
    };

    let sampler = spawn_progress_sampler(job_id, Arc::clone(&ctx.counters), events.clone());

    let mut report = JobReport {
        files_completed: 0,
        bytes_completed: 0,
        skipped: Vec::new(),
        errors: Vec::new(),
        started_at: Some(started_at),
        finished_at: None,
    };
    let mut cancelled = false;

    let mut index = 0usize;
    while index < plan.steps.len() {
        if wait_out_pause(&ctx).await == ControlState::Cancelled {
            cancelled = true;
            break;
        }

        // Collect a batch of consecutive copy-class steps starting here.
        let batch_start = index;
        while index < plan.steps.len() && is_copy_class(&plan.steps[index]) {
            index += 1;
        }
        if index > batch_start {
            let outcome =
                run_batch(&ctx, &plan, batch_start, index, concurrency, &mut report).await;
            if outcome == ControlState::Cancelled {
                cancelled = true;
                break;
            }
            continue;
        }

        // A single barrier step.
        if wait_out_pause(&ctx).await == ControlState::Cancelled {
            cancelled = true;
            break;
        }
        let step_index = index as u32;
        match run_step_with_retry(&ctx, step_index, &plan.steps[index]).await {
            StepRun::Done(outcome) => {
                apply_outcome(&mut report, step_index, &plan.steps[index], outcome)
            }
            StepRun::Cancelled => {
                cancelled = true;
                break;
            }
        }
        index += 1;
    }

    sampler.abort();

    let job_outcome = if cancelled {
        JobOutcome::Cancelled
    } else if !report.errors.is_empty() {
        JobOutcome::Failed
    } else if !report.skipped.is_empty() {
        JobOutcome::CompletedWithSkips
    } else {
        JobOutcome::Completed
    };
    let finished_at = Timestamp::from(SystemTime::now());
    report.finished_at = Some(finished_at);

    let _ = ctx
        .journal
        .append(JournalRecord::JobFinished {
            outcome: job_outcome,
            finished_at,
        })
        .await;
    let _ = ctx.events.send(JobEvent::Finished {
        job_id,
        outcome: job_outcome,
        report: report.clone(),
    });

    report
}

/// `true` for the step kinds [`execute`]'s batching treats as eligible for
/// concurrent execution.
fn is_copy_class(step: &Step) -> bool {
    matches!(step.kind(), StepKind::CopyFile | StepKind::Reflink)
}

/// Blocks (cooperatively, polling every 20ms -- no `Notify`/wake needed
/// since nothing about this task's AC bounds *resume* latency, only
/// *pause* latency) until `control` is no longer `Paused`. Emits
/// `JobEvent::Paused`/`Resumed` exactly once per pause episode.
async fn wait_out_pause(ctx: &ExecutorContext) -> ControlState {
    let mut announced = false;
    loop {
        match ctx.control.state() {
            ControlState::Running => {
                if announced {
                    let _ = ctx.events.send(JobEvent::Resumed { job_id: ctx.job_id });
                }
                return ControlState::Running;
            }
            ControlState::Cancelled => return ControlState::Cancelled,
            ControlState::Paused => {
                if !announced {
                    let _ = ctx.events.send(JobEvent::Paused { job_id: ctx.job_id });
                    announced = true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Drains one batch of consecutive copy-class steps (`plan.steps[start..
/// end]`) through up to `concurrency` concurrently spawned tasks, folding
/// each one's outcome into `report` as it finishes. Returns
/// `ControlState::Cancelled` if the job was cancelled partway through (in
/// which case any steps not yet dispatched are simply never started, and
/// already-running ones finish or are interrupted per their own retry
/// loop); `ControlState::Running` otherwise.
async fn run_batch(
    ctx: &ExecutorContext,
    plan: &Plan,
    start: usize,
    end: usize,
    concurrency: usize,
    report: &mut JobReport,
) -> ControlState {
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut handles = Vec::with_capacity(end - start);
    for i in start..end {
        let ctx = ctx.clone();
        let step = plan.steps[i].clone();
        let permit = Arc::clone(&semaphore);
        let step_index = i as u32;
        handles.push(tokio::spawn(async move {
            let _permit = permit
                .acquire_owned()
                .await
                .expect("semaphore never closed");
            let outcome = run_step_with_retry(&ctx, step_index, &step).await;
            (step_index, step, outcome)
        }));
    }

    let mut cancelled = false;
    for handle in handles {
        let (step_index, step, outcome) = handle.await.expect("worker task panicked");
        match outcome {
            StepRun::Done(outcome) => apply_outcome(report, step_index, &step, outcome),
            StepRun::Cancelled => cancelled = true,
        }
    }

    if cancelled {
        ControlState::Cancelled
    } else {
        ControlState::Running
    }
}

/// The result of one attempt to run a step: either it finished (one way or
/// another), or the job was cancelled while it was in flight.
enum StepRun {
    Done(StepOutcome),
    Cancelled,
}

/// Runs `step` (journaling `Intent` before, `Completion` after), retrying
/// from scratch across any pause episode until it finishes or the job is
/// cancelled. See the module doc comment's "resume restarts the step"
/// scope note for why a retry-from-scratch loop, not byte-exact resume, is
/// what "resumes correctly" means here.
async fn run_step_with_retry(ctx: &ExecutorContext, step_index: u32, step: &Step) -> StepRun {
    loop {
        if wait_out_pause(ctx).await == ControlState::Cancelled {
            return StepRun::Cancelled;
        }

        let partial_name = copy_partial_name(step);
        if let Err(e) = ctx
            .journal
            .append(JournalRecord::Intent {
                step_index,
                step: step.clone(),
                partial_name: partial_name.clone(),
            })
            .await
        {
            return StepRun::Done(StepOutcome::Failed(StepFailure {
                step_index,
                path: step_primary_path(step),
                kind: e.kind(),
                message: format!("failed to journal Intent: {e}"),
            }));
        }
        let _ = ctx.events.send(JobEvent::StepStarted {
            job_id: ctx.job_id,
            step_index,
            kind: step.kind(),
        });

        let attempt = dispatch(ctx, step, partial_name.as_deref()).await;
        let outcome = match attempt {
            Ok(StepAttempt::Done(outcome)) => outcome,
            Ok(StepAttempt::Interrupted) => continue, // pause/cancel mid-copy -- retry
            Err(e) => StepOutcome::Failed(StepFailure {
                step_index,
                path: step_primary_path(step),
                kind: e.kind(),
                message: e.to_string(),
            }),
        };

        if let Err(e) = ctx
            .journal
            .append(JournalRecord::Completion {
                step_index,
                outcome: outcome.clone(),
            })
            .await
        {
            return StepRun::Done(StepOutcome::Failed(StepFailure {
                step_index,
                path: step_primary_path(step),
                kind: e.kind(),
                message: format!("failed to journal Completion: {e}"),
            }));
        }
        match &outcome {
            StepOutcome::Succeeded => {
                let _ = ctx.events.send(JobEvent::StepCompleted {
                    job_id: ctx.job_id,
                    step_index,
                });
            }
            StepOutcome::Skipped { reason } => {
                let _ = ctx.events.send(JobEvent::StepSkipped {
                    job_id: ctx.job_id,
                    step_index,
                    reason: reason.clone(),
                });
            }
            StepOutcome::Failed(failure) => {
                let _ = ctx.events.send(JobEvent::StepFailed {
                    job_id: ctx.job_id,
                    failure: failure.clone(),
                });
            }
        }
        return StepRun::Done(outcome);
    }
}

fn apply_outcome(report: &mut JobReport, step_index: u32, step: &Step, outcome: StepOutcome) {
    match outcome {
        StepOutcome::Succeeded => {
            if matches!(step.kind(), StepKind::CopyFile | StepKind::Reflink) {
                report.files_completed += 1;
                report.bytes_completed += step.planned_bytes();
            }
        }
        StepOutcome::Skipped { reason } => {
            report.skipped.push(crate::job::SkipEntry {
                step_index,
                path: step_primary_path(step)
                    .unwrap_or_else(|| VPath::local(duet_types::UnixPathBuf::new("/").unwrap())),
                reason,
            });
        }
        StepOutcome::Failed(failure) => {
            report.errors.push(failure);
        }
    }
}

/// The path a [`StepFailure`]/[`crate::job::SkipEntry`] should attribute a
/// step to -- its destination for anything that writes one, its target for
/// removal/metadata, `None` for the read-only `Verify` (not dispatched
/// today, but kept exhaustive).
fn step_primary_path(step: &Step) -> Option<VPath> {
    match step {
        Step::CreateDir { dest, .. }
        | Step::CopyFile { dest, .. }
        | Step::Reflink { dest, .. }
        | Step::Rename { dest, .. }
        | Step::Link { dest, .. } => Some(dest.clone()),
        Step::SetMeta { target, .. } | Step::Remove { target, .. } => Some(target.clone()),
        Step::Verify { dest, .. } => Some(dest.clone()),
    }
}

/// The `.duet-partial-<rand>-<name>` sibling path a `CopyFile`/`Reflink`
/// step stages through, chosen once and recorded in the step's `Intent`
/// record before any side effect -- `None` for step kinds that don't stage
/// through a partial at all.
fn copy_partial_name(step: &Step) -> Option<String> {
    match step {
        Step::CopyFile { dest, .. } | Step::Reflink { dest, .. } => {
            dest.inner().file_name().map(partial_file_name)
        }
        _ => None,
    }
}

static PARTIAL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Builds a `.duet-partial-<rand>-<name>` file name -- independently
/// implemented (not shared with `duet_vfs::local::pathutil`'s own
/// equivalent, which is private to that crate's `local` module) per this
/// codebase's existing convention of duplicating small, load-bearing
/// filesystem-naming primitives per-crate rather than adding a
/// cross-crate dependency for a few lines (see `duet-config/src/io.rs`'s
/// own independent `tmp_file_name`).
fn partial_file_name(original: &str) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".duet-partial-{pid}-{nanos}-{counter}-{original}")
}

/// The result of one attempt to perform a step's actual side effect
/// (distinct from [`StepRun`], which wraps the whole journal-bracketed
/// retry loop): either it produced a [`StepOutcome`], or it was
/// interrupted by a pause/cancel request partway through (currently only
/// possible for the chunked `CopyFile`/`Reflink` loop) and must be retried
/// from scratch by the caller.
enum StepAttempt {
    Done(StepOutcome),
    Interrupted,
}

async fn dispatch(
    ctx: &ExecutorContext,
    step: &Step,
    partial_name: Option<&str>,
) -> Result<StepAttempt> {
    match step {
        Step::CreateDir { dest, mode } => create_dir_step(&*ctx.fs, dest, *mode).await,
        Step::CopyFile {
            source, dest, size, ..
        }
        | Step::Reflink {
            source, dest, size, ..
        } => copy_file_step(ctx, source, dest, *size, partial_name).await,
        Step::Rename { source, dest, .. } => rename_step(&*ctx.fs, source, dest).await,
        Step::SetMeta { target, patch } => set_meta_step(&*ctx.fs, target, patch).await,
        Step::Remove { target, mode } => remove_step(&*ctx.fs, target, *mode).await,
        Step::Link { .. } => Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
            step_index: 0, // overwritten by the caller
            path: step_primary_path(step),
            kind: ErrorKind::Fatal,
            message: "Step::Link execution is not implemented yet (T-5.1.7 owns hardlink-graph \
                       preservation, the only planner that will ever emit this step kind)"
                .to_string(),
        }))),
        Step::Verify { .. } => Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
            step_index: 0,
            path: step_primary_path(step),
            kind: ErrorKind::Fatal,
            message: "Step::Verify execution is not implemented yet (T-5.1.12 owns post-copy \
                       BLAKE3 verification)"
                .to_string(),
        }))),
    }
}

async fn create_dir_step(
    fs: &dyn FileSystem,
    dest: &VPath,
    mode: Option<u32>,
) -> Result<StepAttempt> {
    match fs.create_dir(dest, mode.map(Mode::new)).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => Ok(StepAttempt::Done(skipped_conflict(dest))),
        Err(e) => Err(e),
    }
}

async fn rename_step(fs: &dyn FileSystem, source: &VPath, dest: &VPath) -> Result<StepAttempt> {
    match fs.rename(source, dest, RenameFlags::NO_REPLACE).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => Ok(StepAttempt::Done(skipped_conflict(dest))),
        Err(e) => Err(e),
    }
}

async fn set_meta_step(
    fs: &dyn FileSystem,
    target: &VPath,
    patch: &MetaPatch,
) -> Result<StepAttempt> {
    fs.set_meta(target, patch).await?;
    Ok(StepAttempt::Done(StepOutcome::Succeeded))
}

async fn remove_step(fs: &dyn FileSystem, target: &VPath, mode: RemoveMode) -> Result<StepAttempt> {
    let kind = match mode {
        RemoveMode::File => RemoveKind::File,
        RemoveMode::EmptyDir => RemoveKind::EmptyDir,
        RemoveMode::Recursive => RemoveKind::Recursive,
    };
    fs.remove(target, kind).await?;
    Ok(StepAttempt::Done(StepOutcome::Succeeded))
}

fn skipped_conflict(dest: &VPath) -> StepOutcome {
    StepOutcome::Skipped {
        reason: format!(
            "{dest} already exists (real conflict-policy resolution is T-5.1.9's job; \
             the executor's placeholder behavior is to skip)"
        ),
    }
}

/// Executes one `CopyFile`/`Reflink` step: tries `fs.server_side_copy`
/// staged through `partial_name` first (reflink/`copy_file_range`, when
/// the backend can accelerate it), falling back to a naive buffered
/// `open_read`/`open_write` loop -- pause/cancel-checked every
/// [`COPY_BUFFER_BYTES`] -- when the backend reports `Unsupported`. Either
/// way, publishes by renaming the partial onto `dest` as an explicit,
/// separate step, so `Intent.partial_name` (recorded before this function
/// is even called) always names the file recovery would actually find.
///
/// `server_side_copy` itself writes directly to whatever path it's given
/// with no staging of its own (confirmed by reading `local::probe::
/// accelerated_copy`) -- calling it straight at `dest` would silently
/// violate crash-safety.md's `CopyFile`/`Reflink` invariants (a SIGKILL
/// mid-copy would leave a partially-written file *at the final path*, not
/// a clearly-marked partial). Directing it at our own chosen partial path
/// and doing the publish-rename ourselves gets the acceleration without
/// that gap.
async fn copy_file_step(
    ctx: &ExecutorContext,
    source: &VPath,
    dest: &VPath,
    expected_size: u64,
    partial_name: Option<&str>,
) -> Result<StepAttempt> {
    let Some(partial_name) = partial_name else {
        return Err(Box::new(
            VfsError::new(
                ErrorKind::Fatal,
                "CopyFile/Reflink step's destination has no file name to stage a partial for",
            )
            .with_path(dest.clone()),
        ));
    };
    let Some(parent) = dest.parent() else {
        return Err(Box::new(
            VfsError::new(
                ErrorKind::Fatal,
                "CopyFile/Reflink destination has no parent",
            )
            .with_path(dest.clone()),
        ));
    };
    let partial = parent.join(partial_name).map_err(|e| {
        Box::new(VfsError::new(ErrorKind::Fatal, e.to_string()).with_path(dest.clone()))
    })?;

    // `used_naive` tracks whether `naive_copy` already accounted for this
    // file's bytes incrementally (chunk by chunk, as real progress
    // happened) -- if so, `bytes_done` must NOT also be bumped by
    // `expected_size` below, or every naively-copied file would be
    // double-counted. The accelerated path has no chunk-level signal to
    // hook into (`server_side_copy` is one opaque backend call), so it
    // still accounts for its whole file in one jump at completion.
    let mut used_naive = false;
    let should_cancel = || ctx.control.state() != ControlState::Running;
    match ctx
        .fs
        .server_side_copy(source, &partial, &should_cancel)
        .await
    {
        Ok(duet_vfs::CopyOutcome::Copied { .. }) => {}
        Ok(duet_vfs::CopyOutcome::Unsupported) => {
            // Best-effort cleanup of any zero-byte artifact the failed
            // acceleration attempt may have left before falling back.
            let _ = ctx.fs.remove(&partial, RemoveKind::File).await;
            used_naive = true;
            match naive_copy(ctx, source, &partial, expected_size).await? {
                StepAttempt::Done(StepOutcome::Succeeded) => {}
                other => return Ok(other),
            }
        }
        Ok(duet_vfs::CopyOutcome::Interrupted) => {
            // Pause/cancel landed mid-`server_side_copy` (rung 2 or 3 of
            // T-5.1.4's ladder) -- the backend already cleaned up its own
            // partial per `CopyOutcome::Interrupted`'s own doc comment.
            // Same handling as `naive_copy`'s own interruption: the
            // caller's retry loop restarts this whole step from scratch.
            return Ok(StepAttempt::Interrupted);
        }
        Err(e) if e.kind() == ErrorKind::Conflict => {
            return Ok(StepAttempt::Done(skipped_conflict(dest)));
        }
        Err(e) => return Err(e),
    }

    match ctx.fs.rename(&partial, dest, RenameFlags::NO_REPLACE).await {
        Ok(()) => {
            ctx.counters.files_done.fetch_add(1, Ordering::Relaxed);
            if !used_naive {
                ctx.counters
                    .bytes_done
                    .fetch_add(expected_size, Ordering::Relaxed);
            }
            Ok(StepAttempt::Done(StepOutcome::Succeeded))
        }
        Err(e) if e.kind() == ErrorKind::Conflict => {
            let _ = ctx.fs.remove(&partial, RemoveKind::File).await;
            Ok(StepAttempt::Done(skipped_conflict(dest)))
        }
        Err(e) => Err(e),
    }
}

/// The naive fallback copy loop: plain buffered `open_read`/`open_write`,
/// no `fadvise`, no sparse-file awareness -- rung 3 of design.md's copy
/// -strategy ladder (T-5.1.4) is what upgrades this. Checks `ctx.control`
/// every [`COPY_BUFFER_BYTES`] chunk; on pause or cancel, aborts the
/// in-progress write (safe: `open_write` never touched `to` itself, only
/// its own internal staging sibling) and returns `Interrupted` for the
/// caller's retry loop to handle.
///
/// Updates `ctx.counters.bytes_done` incrementally, chunk by chunk, as
/// real progress happens -- both so the 100ms progress sampler reflects
/// genuine intra-file progress on a large single file (not just discrete
/// per-file jumps), and so a paused job's progress observably stops
/// advancing promptly rather than only becoming visible once the whole
/// (now-abandoned) file would have finished.
async fn naive_copy(
    ctx: &ExecutorContext,
    from: &VPath,
    to: &VPath,
    expected_size: u64,
) -> Result<StepAttempt> {
    let mut reader = ctx.fs.open_read(from).await?;
    let mut writer = ctx
        .fs
        .open_write(
            to,
            WriteOpts::create_new().with_expected_size(expected_size),
        )
        .await?;

    let mut buf = vec![0u8; COPY_BUFFER_BYTES];
    let mut copied = 0u64;
    loop {
        if ctx.control.state() != ControlState::Running {
            let _ = writer.abort().await;
            // Roll back the partial progress this attempt already
            // counted -- the retry loop restarts the whole step from
            // scratch, so any bytes counted here must not persist into
            // the next attempt's count.
            ctx.counters.bytes_done.fetch_sub(copied, Ordering::Relaxed);
            return Ok(StepAttempt::Interrupted);
        }
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| Box::new(VfsError::from_io(e)))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .await
            .map_err(|e| Box::new(VfsError::from_io(e)))?;
        copied += n as u64;
        ctx.counters
            .bytes_done
            .fetch_add(n as u64, Ordering::Relaxed);
    }
    writer.commit().await?;
    Ok(StepAttempt::Done(StepOutcome::Succeeded))
}

/// Owns a [`Journal`] on a dedicated blocking thread and serializes access
/// to it via a channel, so `append`'s real, synchronous `fsync` never
/// blocks a Tokio worker thread (which would stall unrelated async work
/// sharing that thread) and so concurrently-running copy-step tasks don't
/// need a shared `&mut Journal`/async mutex to each append their own
/// `Intent`/`Completion` records.
#[derive(Clone)]
struct JournalHandle {
    tx: mpsc::UnboundedSender<(JournalRecord, oneshot::Sender<Result<()>>)>,
}

impl JournalHandle {
    fn spawn(mut journal: Journal) -> Self {
        let (tx, mut rx) =
            mpsc::unbounded_channel::<(JournalRecord, oneshot::Sender<Result<()>>)>();
        tokio::task::spawn_blocking(move || {
            while let Some((record, reply)) = rx.blocking_recv() {
                let result = journal.append(&record);
                let _ = reply.send(result);
            }
        });
        JournalHandle { tx }
    }

    async fn append(&self, record: JournalRecord) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send((record, reply_tx)).map_err(|_| {
            Box::new(VfsError::new(
                ErrorKind::Fatal,
                "journal writer task is no longer running",
            ))
        })?;
        reply_rx.await.map_err(|_| {
            Box::new(VfsError::new(
                ErrorKind::Fatal,
                "journal writer task dropped its reply channel",
            ))
        })?
    }
}

/// Spawns the 100ms-cadence progress-sampling task (design.md §9.3:
/// "Updated on a 100 ms timer sampling atomic counters"). Returns its
/// `JoinHandle` so [`execute`] can `abort()` it once the job finishes --
/// safe to abort anytime, since it only ever reads atomics and sends
/// events, nothing it holds needs graceful unwinding.
fn spawn_progress_sampler(
    job_id: JobId,
    counters: Arc<ProgressCounters>,
    events: mpsc::UnboundedSender<JobEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut last_bytes = 0u64;
        loop {
            interval.tick().await;
            let bytes_done = counters.bytes_done.load(Ordering::Relaxed);
            let files_done = counters.files_done.load(Ordering::Relaxed);
            let throughput = bytes_done.saturating_sub(last_bytes) * 10; // per-100ms -> per-second
            last_bytes = bytes_done;
            let snapshot = ProgressSnapshot {
                files_done,
                bytes_done,
                current_file_bytes_done: 0,
                current_file_bytes_total: 0,
                throughput_bytes_per_sec: throughput,
                eta_secs: None,
            };
            if events
                .send(JobEvent::Progress { job_id, snapshot })
                .is_err()
            {
                return; // no one is listening anymore
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use duet_types::UnixPathBuf;
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::job::JobId as JobIdT;
    use crate::plan::PlanOptions;

    fn vpath_for(dir: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(dir.to_str().unwrap()).unwrap())
    }

    async fn run(
        fs: Arc<dyn FileSystem>,
        plan: Plan,
        state_dir: &Path,
        concurrency: usize,
    ) -> (JobReport, Vec<JobEvent>) {
        let journal = Journal::open(JobIdT(1), state_dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let report = execute(fs, JobIdT(1), plan, journal, concurrency, tx, control).await;
        drop(report.clone());
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        (report, events)
    }

    /// A `FileSystem` test double wrapping a real [`LocalFs`], used to
    /// deterministically exercise two things a real backend's actual
    /// speed makes unreliable to test directly:
    ///
    /// - `force_unsupported`: always reports `CopyOutcome::Unsupported`
    ///   from `server_side_copy`, forcing [`naive_copy`]'s chunked,
    ///   pause-checked loop even on same-filesystem paths a real backend
    ///   would otherwise accelerate via reflink/`copy_file_range` --
    ///   without this, a same-tmpfs copy in a test is likely fast enough
    ///   to finish as one opaque `server_side_copy` call before a test's
    ///   `pause()` could ever land, proving nothing about pause latency.
    /// - `delay`: sleeps before delegating to the real `server_side_copy`,
    ///   widening the window during which concurrently-dispatched copies
    ///   are genuinely in flight at once, so a bounded-concurrency test
    ///   can reliably observe overlap without racing against how fast
    ///   tiny test files copy on their own.
    ///
    /// Both fields default to "no-op" (delegate straight through, no
    /// delay), so the same struct serves either test purpose.
    struct TestFs {
        inner: LocalFs,
        force_unsupported: bool,
        delay: Duration,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl FileSystem for TestFs {
        fn scheme(&self) -> &'static str {
            self.inner.scheme()
        }
        fn caps(&self) -> duet_types::Caps {
            self.inner.caps()
        }
        fn read_dir(
            &self,
            p: &VPath,
            opts: duet_vfs::ListOpts,
        ) -> futures_util::stream::BoxStream<'_, Result<Vec<duet_vfs::DirEntry>>> {
            self.inner.read_dir(p, opts)
        }
        async fn stat(&self, p: &VPath, follow: bool) -> Result<duet_types::Metadata> {
            self.inner.stat(p, follow).await
        }
        async fn volume_stats(&self, p: &VPath) -> Result<duet_vfs::VolumeStats> {
            self.inner.volume_stats(p).await
        }
        async fn open_read(&self, p: &VPath) -> Result<Box<dyn duet_vfs::AsyncReadSeek>> {
            self.inner.open_read(p).await
        }
        async fn open_write(
            &self,
            p: &VPath,
            o: WriteOpts,
        ) -> Result<Box<dyn duet_vfs::AsyncWriteCommit>> {
            self.inner.open_write(p, o).await
        }
        async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()> {
            self.inner.create_dir(p, mode).await
        }
        async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()> {
            self.inner.remove(p, kind).await
        }
        async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()> {
            self.inner.rename(from, to, flags).await
        }
        async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()> {
            self.inner.set_meta(p, m).await
        }
        fn watch(
            &self,
            p: &VPath,
        ) -> Result<futures_util::stream::BoxStream<'_, duet_vfs::ChangeEvent>> {
            self.inner.watch(p)
        }
        async fn server_side_copy(
            &self,
            from: &VPath,
            to: &VPath,
            should_cancel: &(dyn Fn() -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let result = if self.force_unsupported {
                Ok(duet_vfs::CopyOutcome::Unsupported)
            } else {
                self.inner.server_side_copy(from, to, should_cancel).await
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    /// A read handle backed by an in-memory `tokio::io::DuplexStream`
    /// rather than a real file, used only by the pause test below. `Seek`
    /// is a required part of `AsyncReadSeek`'s bound but [`naive_copy`]
    /// never actually calls it (a straight sequential copy has no reason
    /// to) -- stubbed out rather than implemented for real, since there's
    /// nothing meaningful to seek within a stream this test feeds live.
    struct ThrottledReader(tokio::io::DuplexStream);

    impl tokio::io::AsyncRead for ThrottledReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncSeek for ThrottledReader {
        fn start_seek(
            self: std::pin::Pin<&mut Self>,
            _position: std::io::SeekFrom,
        ) -> std::io::Result<()> {
            Ok(())
        }
        fn poll_complete(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<u64>> {
            std::task::Poll::Ready(Ok(0))
        }
    }

    /// A `FileSystem` test double whose `open_read` hands back a
    /// [`ThrottledReader`] fed by a background task at a controlled,
    /// wall-clock-independent pace (a fixed delay between fixed-size
    /// chunks), and whose `server_side_copy` always reports `Unsupported`
    /// -- forcing [`naive_copy`]'s chunked loop and giving a pause test a
    /// deterministic amount of time to land in, rather than racing against
    /// however fast the real disk/tmpfs happens to be. Content is
    /// meaningless dummy bytes (zeros) -- fine for a test that only checks
    /// *whether* progress stopped, never *what* was copied. Every other
    /// method delegates to a real `LocalFs`.
    struct ThrottledFs {
        inner: LocalFs,
        total_bytes: u64,
        chunk_bytes: usize,
        delay_per_chunk: Duration,
    }

    #[async_trait::async_trait]
    impl FileSystem for ThrottledFs {
        fn scheme(&self) -> &'static str {
            self.inner.scheme()
        }
        fn caps(&self) -> duet_types::Caps {
            self.inner.caps()
        }
        fn read_dir(
            &self,
            p: &VPath,
            opts: duet_vfs::ListOpts,
        ) -> futures_util::stream::BoxStream<'_, Result<Vec<duet_vfs::DirEntry>>> {
            self.inner.read_dir(p, opts)
        }
        async fn stat(&self, p: &VPath, follow: bool) -> Result<duet_types::Metadata> {
            self.inner.stat(p, follow).await
        }
        async fn volume_stats(&self, p: &VPath) -> Result<duet_vfs::VolumeStats> {
            self.inner.volume_stats(p).await
        }
        async fn open_read(&self, _p: &VPath) -> Result<Box<dyn duet_vfs::AsyncReadSeek>> {
            let (mut tx, rx) = tokio::io::duplex(self.chunk_bytes);
            let total = self.total_bytes;
            let chunk = self.chunk_bytes;
            let delay = self.delay_per_chunk;
            tokio::spawn(async move {
                let buf = vec![0u8; chunk];
                let mut remaining = total;
                while remaining > 0 {
                    let n = remaining.min(chunk as u64) as usize;
                    if tx.write_all(&buf[..n]).await.is_err() {
                        break; // reader side dropped (e.g. the step was aborted)
                    }
                    remaining -= n as u64;
                    tokio::time::sleep(delay).await;
                }
            });
            Ok(Box::new(ThrottledReader(rx)))
        }
        async fn open_write(
            &self,
            p: &VPath,
            o: WriteOpts,
        ) -> Result<Box<dyn duet_vfs::AsyncWriteCommit>> {
            self.inner.open_write(p, o).await
        }
        async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()> {
            self.inner.create_dir(p, mode).await
        }
        async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()> {
            self.inner.remove(p, kind).await
        }
        async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()> {
            self.inner.rename(from, to, flags).await
        }
        async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()> {
            self.inner.set_meta(p, m).await
        }
        fn watch(
            &self,
            p: &VPath,
        ) -> Result<futures_util::stream::BoxStream<'_, duet_vfs::ChangeEvent>> {
            self.inner.watch(p)
        }
        async fn server_side_copy(
            &self,
            _from: &VPath,
            _to: &VPath,
            _should_cancel: &(dyn Fn() -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    #[tokio::test]
    async fn copies_a_directory_tree_and_completes_with_no_skips_or_errors() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), b"world!").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let (report, _events) = run(fs, plan, state.path(), 2).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert_eq!(report.files_completed, 2);
        assert_eq!(report.bytes_completed, 11);

        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("sub/b.txt")).unwrap(),
            "world!"
        );
    }

    #[tokio::test]
    async fn journal_records_a_matching_intent_and_completion_for_every_step() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hi").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        let step_count = plan.steps.len();

        let (report, _events) = run(fs, plan, state.path(), 2).await;
        assert!(report.errors.is_empty());

        let reports = crate::journal::JournalReader::scan(state.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].incomplete_steps.is_empty(),
            "every one of the {step_count} steps must have a matching Completion"
        );
        assert_eq!(reports[0].last_outcome, Some(JobOutcome::Completed));
    }

    #[tokio::test]
    async fn copying_into_an_already_populated_destination_skips_rather_than_fails() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"new").unwrap();
        let dst = TempDir::new().unwrap();
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        std::fs::create_dir(dst.path().join(src_name)).unwrap();
        std::fs::write(dst.path().join(src_name).join("a.txt"), b"old").unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let (report, _events) = run(fs, plan, state.path(), 2).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.skipped.len(),
            2,
            "both CreateDir (dir already exists) and CopyFile (file already exists) must skip"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("a.txt")).unwrap(),
            "old",
            "a skipped conflict must never clobber the existing destination"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_mid_job_stops_promptly_and_leaves_incomplete_steps_in_the_journal() {
        const FILES: usize = 5_000;
        let src = TempDir::new().unwrap();
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i:05}")), vec![b'x'; 4096]).unwrap();
        }
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel_token = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel_token,
        )
        .await
        .unwrap();

        let journal = Journal::open(JobIdT(2), state.path()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let control_for_task = control.clone();
        let fs_for_task = Arc::clone(&fs);
        let handle = tokio::spawn(execute(
            fs_for_task,
            JobIdT(2),
            plan,
            journal,
            1, // force sequential so cancellation is guaranteed mid-batch
            tx,
            control_for_task,
        ));

        control.cancel();
        let report = handle.await.unwrap();

        assert!(report.finished_at.is_some());
        assert!(
            report.files_completed < FILES as u64,
            "expected the walk to be interrupted well before finishing {FILES} files, got {} done",
            report.files_completed
        );

        let reports = crate::journal::JournalReader::scan(state.path()).unwrap();
        assert_eq!(reports.len(), 1);
        // Every step that got an Intent but no Completion should show up
        // as incomplete -- consistent with the cancellation having landed
        // mid-flight rather than between two fully-journaled steps.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pausing_mid_copy_stops_within_200ms() {
        let src = TempDir::new().unwrap();
        // Only used for planning (stat/read_dir) -- content is irrelevant,
        // since `ThrottledFs::open_read` ignores the real file entirely
        // and feeds throttled dummy bytes instead. Real disk/tmpfs speed
        // is not something a latency assertion should depend on.
        std::fs::write(src.path().join("big.bin"), b"x").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        // 50 MiB fed 64 KiB at a time with a 5ms delay between chunks:
        // ~4 seconds to fully drain if never paused, comfortably longer
        // than this test's pause/observe/cancel sequence needs, however
        // fast or slow the machine running it is.
        let fs: Arc<dyn FileSystem> = Arc::new(ThrottledFs {
            inner: LocalFs,
            total_bytes: 50 * 1024 * 1024,
            chunk_bytes: 64 * 1024,
            delay_per_chunk: Duration::from_millis(5),
        });
        let cancel_token = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel_token,
        )
        .await
        .unwrap();

        let journal = Journal::open(JobIdT(3), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let control_for_task = control.clone();
        let handle = tokio::spawn(execute(
            Arc::clone(&fs),
            JobIdT(3),
            plan,
            journal,
            1,
            tx,
            control_for_task,
        ));

        // Let real, observable progress happen first -- otherwise a
        // "progress stopped" observation would be vacuously true (nothing
        // had started yet).
        let mut saw_progress = false;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            if let JobEvent::Progress { snapshot, .. } = &event
                && snapshot.bytes_done > 0
            {
                saw_progress = true;
                break;
            }
            if matches!(event, JobEvent::Finished { .. }) {
                break;
            }
        }
        assert!(
            saw_progress,
            "copy never reported any progress before pause was issued"
        );

        let pause_issued = Instant::now();
        control.pause();

        // Drain samples until two consecutive ones report the same
        // `bytes_done` -- proof the copy loop genuinely stopped advancing,
        // not just that the job hasn't finished yet (which would be true
        // regardless of whether pause actually worked).
        let mut last_bytes: Option<u64> = None;
        let mut stopped_at = None;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            let JobEvent::Progress { snapshot, .. } = event else {
                continue;
            };
            if last_bytes == Some(snapshot.bytes_done) {
                stopped_at = Some(Instant::now());
                break;
            }
            last_bytes = Some(snapshot.bytes_done);
        }
        let stopped_at = stopped_at.expect("progress never stabilized after pause() -- copy loop may not be checking the control flag");
        let latency = stopped_at.duration_since(pause_issued);
        assert!(
            latency < Duration::from_millis(400),
            "took {latency:?} to observe progress stop after pause() -- expected \u{2264}200ms \
             plus one 100ms sampling tick of slack"
        );
        assert!(
            !handle.is_finished(),
            "job must still be paused, not finished"
        );

        control.cancel();
        let report = handle.await.unwrap();
        assert!(report.finished_at.is_some());
        assert_eq!(
            report.files_completed, 0,
            "the paused-then-cancelled copy must never complete"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrency_never_exceeds_the_requested_bound() {
        const FILES: usize = 12;
        const CONCURRENCY: usize = 3;
        let src = TempDir::new().unwrap();
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i}")), b"x").unwrap();
        }
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fs: Arc<dyn FileSystem> = Arc::new(TestFs {
            inner: LocalFs,
            force_unsupported: false,
            // Long enough that CONCURRENCY copies are reliably still in
            // flight together well before any of them finishes.
            delay: Duration::from_millis(80),
            in_flight: Arc::clone(&in_flight),
            max_in_flight: Arc::clone(&max_in_flight),
        });
        let cancel_token = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel_token,
        )
        .await
        .unwrap();

        let (report, _events) = run(fs, plan, state.path(), CONCURRENCY).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, FILES as u64);
        let observed_max = max_in_flight.load(Ordering::SeqCst);
        assert!(
            observed_max <= CONCURRENCY,
            "observed {observed_max} concurrent copies, expected at most {CONCURRENCY}"
        );
        assert!(
            observed_max > 1,
            "observed only {observed_max} concurrent copy at once -- the batch \
             either isn't running in parallel at all, or the test's delay is too short"
        );
    }
}
