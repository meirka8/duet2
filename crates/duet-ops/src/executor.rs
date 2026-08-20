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
//! - **`Step::Verify`'s `Blake3` algorithm is not dispatched** —
//!   `VerifyAlgorithm::SizeOnly` is real (T-5.1.5); `Blake3` fails cleanly
//!   with a descriptive [`StepFailure`] rather than a `todo!()`, since
//!   post-copy BLAKE3 verification is T-5.1.12's own scope.
//! - **`current_file_bytes_done`/`current_file_bytes_total` are always
//!   `0`.** FR-OPS-03's "current file + total" framing assumes one file
//!   copies at a time; with `concurrency > 1`, several files can be
//!   mid-copy simultaneously, and there is no single "current file" to
//!   report without inventing an aggregate convention nothing has
//!   specified. `files_done`/`bytes_done` (whole-job totals) are tracked
//!   precisely; per-file live progress under concurrency is left for
//!   whichever future task first needs to reconcile that framing with
//!   concurrent copying, rather than guessed at here.
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
//!
//! # Conflict resolution (T-5.1.9)
//!
//! `CreateDir`, `CopyFile`/`Reflink`, and `Rename` are the three step kinds
//! that ever produce or replace a destination path — the only ones a real
//! conflict (`ErrorKind::Conflict` from the relevant mutating call) can
//! happen to. [`resolve_conflict`] implements design.md §9.3's tiering,
//! highest precedence first: a `Step`'s own pre-resolved `conflict` field →
//! an already-established per-job "apply to all" answer
//! (`ExecutorContext::sticky_conflict`, constructed fresh inside every
//! `execute()` call, so one job's answer can never leak into another's) →
//! a live [`crate::conflict::ConflictResolver`], if `execute()` was given
//! one → `PlanOptions::default_conflict`. All seven [`ConflictPolicy`]
//! values are real:
//! - `Skip`/`Overwrite` are unconditional.
//! - `OverwriteIfOlder`/`OverwriteIfDifferentSize` re-`stat` both sides and
//!   overwrite only if the comparison favours it, else behave like `Skip`.
//! - `RenameTarget` requires the resolution to carry an explicit
//!   `alternate` destination (nothing in this crate can invent a name a
//!   human is supposed to choose); `AutoRename` computes one itself
//!   ([`auto_rename_target`]: `name (2).ext`, `name (3).ext`, ...).
//! - `Abort` calls `ExecutionControl::cancel()` — the same mechanism a
//!   user-initiated cancel uses — which is why `JobOutcome::Cancelled`'s
//!   own doc comment already lists "the user (or an Abort conflict answer)"
//!   together; no separate terminal state was needed.
//!
//! One real, pre-existing bug fixed as part of building this: `CreateDir`
//! on a destination that already exists *as a directory* now succeeds
//! silently instead of going through conflict resolution at all — merging
//! into an already-existing directory tree (copying the same source twice,
//! resuming a job, ...) is completely ordinary behaviour, not a competing
//! claim on the same path the way an existing *file* at a `CopyFile`'s
//! destination is. Only a non-directory occupying a `CreateDir`'s `dest`
//! goes through the real seven-policy engine.
//!
//! # Error taxonomy & retry (T-5.1.10)
//!
//! design.md §9.3's classification (`duet_types::ErrorKind`, already fixed
//! by T-2.2.x — this task wires *behaviour* onto it, not new classification
//! logic): `Retryable` failures (`EINTR`/`EAGAIN`/a dropped connection) get
//! bounded exponential backoff, retried from the same `run_step_with_retry`
//! loop that already handles pause/cancel — see [`retry_backoff`] and its
//! constants. `Space` failures (`ENOSPC`/`EDQUOT`) call
//! `ExecutionControl::pause()` (the same primitive a user-initiated pause
//! uses) and emit [`JobEvent::QueuePausedForSpace`], then retry indefinitely
//! once resumed — the queue-wide half of "pause the whole queue, not just
//! the job" is [`crate`]'s own disclosed gap (T-5.1.13, multi-job queueing,
//! doesn't exist yet); within a single job, pausing *this* job and emitting
//! a clearly-distinguished event is the whole of what's achievable here,
//! and exactly what a future queue manager listening for this event would
//! need to propagate the pause further. `Permission` failures
//! (`EACCES`/`EPERM`) are not retried — FR-OPS-13's actual elevation
//! mechanism (polkit, a D-Bus helper) is T-9.1.13's scope, which this task
//! doesn't attempt to pre-empt — but the resulting [`StepFailure`] message
//! says plainly that elevation would help, so the report is actionable
//! rather than a bare "permission denied."
//!
//! Both retry paths reuse the exact same journal-bracketing machinery a
//! pause/cancel-interrupted retry already uses (`continue` back to the top
//! of `run_step_with_retry`'s loop — a fresh `Intent` record, a fresh
//! `.duet-partial-*` name for copy-class steps): multiple `Intent` records
//! for one `step_index` before its `Completion` is an already-established,
//! already-recovery-safe pattern, not something new this task introduces.
//!
//! # Progress and ETA (T-5.1.11)
//!
//! design.md §9.3: "ETA uses an exponentially-weighted moving average with
//! separate small-file and large-file regimes, because a naive average
//! lies badly on mixed sets" — a corpus of 10,000 tiny config files plus
//! one 4 GiB video is bottlenecked on per-file syscall overhead for the
//! former and raw throughput for the latter, and averaging the two
//! together produces an estimate that's wrong for both. Every `CopyFile`/
//! `Reflink` step is classified by [`is_small_file`] against
//! [`SMALL_FILE_REGIME_THRESHOLD_BYTES`] (the same 1 MiB figure
//! [`COPY_BUFFER_BYTES`] already uses, on the reasoning that a file
//! smaller than one copy buffer is dominated by per-file overhead no
//! matter what) into one of two regimes, each tracked by its own pair of
//! atomics on [`ProgressCounters`] (`small_files_done`/`large_bytes_done`)
//! alongside the pre-existing whole-job `files_done`/`bytes_done` (left
//! untouched — this is additive, not a replacement).
//!
//! [`EtaEstimator`] is the pure (no atomics, no async) core: fed a
//! `(small_done, large_done)` pair once per 100ms sampler tick (mirroring
//! `throughput_bytes_per_sec`'s own existing "assume exactly 100ms between
//! ticks" convention rather than measuring real elapsed time — consistent
//! with the pre-existing `* 10` conversion just below), it maintains an
//! independent EWMA rate for each regime (files/sec for small, bytes/sec
//! for large), updated only on ticks where that regime actually made
//! progress — a barrier-step stretch (a `SetMeta`/`CreateDir` run with no
//! `CopyFile` in it) leaves both rates exactly where they were rather than
//! decaying them toward zero, which would otherwise make the ETA spike
//! upward for every barrier step in a plan. `sample()` combines the two
//! regimes' individual ETAs with `max`, not a sum: both regimes are
//! drained by the same shared, `concurrency`-bounded worker pool (not two
//! independent resource lanes), so each regime's own observed rate
//! already reflects whatever share of the pool it's actually been
//! getting, and the job as a whole finishes once the slower-to-clear
//! regime clears. The combined ETA is `None` (matching
//! [`ProgressSnapshot::eta_secs`]'s own doc comment) until *both* regimes
//! with outstanding work have an established rate — always true for at
//! least the first tick or two of any job with both regimes present.
//!
//! **"Never runs backwards for more than one sample"** (T-5.1.11's own
//! AC): a noisy single-tick rate dip is expected and shouldn't be hidden
//! (the estimate should still visibly react), but an ETA that keeps
//! climbing tick after tick reads as broken, not as an estimator being
//! honest about a slowdown. `EtaEstimator` allows exactly one consecutive
//! increase over the previous reported value; a second one in a row is
//! clamped to hold at the last reported value instead of climbing further,
//! until a tick produces a value that isn't an increase, which clears the
//! clamp. See [`EtaEstimator::sample`]'s own doc comment and
//! `eta_never_increases_for_more_than_one_consecutive_sample` for the
//! exact rule and a test that would fail without it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use duet_types::{ErrorKind, MetaPatch, Result, Timestamp, VPath, VfsError};
use duet_vfs::{FileSystem, Mode, RemoveKind, RenameFlags, WriteOpts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::conflict::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, ConflictScope,
};
use crate::event::{JobEvent, ProgressSnapshot};
use crate::job::{JobId, JobOutcome, JobReport, StepFailure};
use crate::journal::{Journal, JournalRecord, StepOutcome};
use crate::plan::Plan;
use crate::step::{RemoveMode, Step, StepKind, VerifyAlgorithm};

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
    /// Every step's outcome recorded so far this run, keyed by
    /// `step_index` -- what [`Step::Remove`]/[`Step::Verify`]'s own
    /// `depends_on` field is checked against (T-5.1.5's dependency-gating
    /// mechanism; see [`dependency_block_reason`]). A plain `Mutex`, not
    /// an atomic/lock-free structure: writes happen once per step
    /// (occasionally contended across a concurrent copy-class batch, but
    /// briefly), and every read is a single-key lookup -- nothing here is
    /// hot-path enough to justify more machinery.
    outcomes: Arc<Mutex<HashMap<u32, StepOutcome>>>,
    /// `PlanOptions::default_conflict`, copied out for convenient access —
    /// the lowest-precedence tier [`resolve_conflict`] falls back to.
    default_conflict: ConflictPolicy,
    /// A live conflict decision-maker, if the caller supplied one. `None`
    /// means every conflict resolves via `default_conflict` alone, with no
    /// live consultation.
    resolver: Option<Arc<dyn ConflictResolver>>,
    /// This job's "apply to all" answer, once a live resolver has given
    /// one — `None` until then. Constructed fresh inside every `execute()`
    /// call (see [`execute`]'s own body), never passed in from outside, so
    /// one job's sticky answer can never leak into a different job's
    /// `execute()` call even if the same `resolver` `Arc` is reused across
    /// both (T-5.1.9's AC: "no policy leaks between jobs").
    sticky_conflict: Arc<Mutex<Option<ConflictResolution>>>,
}

/// The raw, non-atomic-across-fields counters a job's [`ProgressSnapshot`]
/// is sampled from every 100ms -- see `job.rs`'s own doc comment: "those
/// atomics are an executor-internal implementation detail... `Job` itself
/// is the queue-visible *snapshot* type."
///
/// `small_files_done`/`large_bytes_done` (T-5.1.11) are additive to, not a
/// replacement for, `files_done`/`bytes_done`: they feed
/// [`EtaEstimator`]'s dual-regime rate estimate specifically, split by
/// [`is_small_file`] -- see the module doc comment's "Progress and ETA"
/// section.
#[derive(Debug, Default)]
struct ProgressCounters {
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    small_files_done: AtomicU64,
    large_bytes_done: AtomicU64,
}

/// The size boundary [`is_small_file`] classifies a `CopyFile`/`Reflink`
/// step's regime against -- deliberately the same value as
/// [`COPY_BUFFER_BYTES`] (not a separately-tuned constant): a file smaller
/// than one copy buffer is, by construction, dominated by per-file open/
/// close/rename syscall overhead rather than sustained throughput, which
/// is exactly the "small-file regime" design.md §9.3 describes.
const SMALL_FILE_REGIME_THRESHOLD_BYTES: u64 = COPY_BUFFER_BYTES as u64;

/// `true` if `size` (a `CopyFile`/`Reflink` step's planned byte count)
/// belongs to the small-file regime -- see
/// [`SMALL_FILE_REGIME_THRESHOLD_BYTES`].
fn is_small_file(size: u64) -> bool {
    size < SMALL_FILE_REGIME_THRESHOLD_BYTES
}

/// Sums `plan.steps` into `(total_small_files, total_large_bytes)` --
/// [`EtaEstimator`]'s two "how much work exists in each regime" totals,
/// computed once up front (mirroring how [`PlanTotals`] itself is computed
/// once at plan time, not re-derived every tick). Zero-byte files count as
/// small (the file-count-based small regime handles a zero-byte file
/// exactly as well as any other tiny one).
fn regime_totals(steps: &[Step]) -> (u64, u64) {
    let mut total_small_files = 0u64;
    let mut total_large_bytes = 0u64;
    for step in steps {
        if let Step::CopyFile { size, .. } | Step::Reflink { size, .. } = step {
            if is_small_file(*size) {
                total_small_files += 1;
            } else {
                total_large_bytes += *size;
            }
        }
    }
    (total_small_files, total_large_bytes)
}

/// The pure (no atomics, no I/O, no async) dual-regime EWMA ETA core --
/// see the module doc comment's "Progress and ETA" section for the full
/// design rationale. Kept separate from [`spawn_progress_sampler`] so its
/// tick-by-tick behaviour (convergence speed, the backward-run clamp) is
/// directly unit-testable against a synthetic sequence of `sample()` calls
/// instead of needing a real 10-second wall-clock test.
struct EtaEstimator {
    total_small_files: u64,
    total_large_bytes: u64,
    small_rate_ema: Option<f64>,
    large_rate_ema: Option<f64>,
    last_small_done: u64,
    last_large_done: u64,
    last_reported: Option<u64>,
    /// `true` once this tick's report is a same-or-lower-than-`last_reported`
    /// value has NOT yet happened since the last increase -- i.e. whether
    /// the *next* increase, if any, must be clamped. See `sample`'s doc
    /// comment.
    backward_pending: bool,
}

/// How heavily each tick's freshly observed instantaneous rate is weighted
/// against the running EWMA -- large enough to converge well within
/// T-5.1.11's own "within 20% after the first 10s" AC (10s at 100ms/tick
/// is up to 100 ticks; this converges in a handful), small enough that one
/// unusually fast or slow 100ms tick doesn't swing the estimate wildly.
/// No numeric guidance exists in design.md/task.md for the exact figure --
/// chosen and tested empirically, mirroring `RETRYABLE_MAX_ATTEMPTS`'s own
/// "documented, deliberately conservative choice" precedent.
const ETA_EWMA_ALPHA: f64 = 0.3;

impl EtaEstimator {
    fn new(total_small_files: u64, total_large_bytes: u64) -> Self {
        EtaEstimator {
            total_small_files,
            total_large_bytes,
            small_rate_ema: None,
            large_rate_ema: None,
            last_small_done: 0,
            last_large_done: 0,
            last_reported: None,
            backward_pending: false,
        }
    }

    /// Folds one 100ms tick's cumulative regime-done counters into the
    /// estimator and returns the ETA (in whole seconds) to report for this
    /// tick, or `None` if a regime with outstanding work has no rate
    /// estimate yet.
    ///
    /// Backward-run rule (T-5.1.11's own AC, verbatim: "never runs
    /// backwards for more than one sample"): if this tick's freshly
    /// computed estimate is strictly greater than the *previously
    /// reported* value, it is allowed through once: `last_reported`
    /// updates to the new, higher value, but a note is kept that the next
    /// increase (if any) must be suppressed. If the very next tick would
    /// also increase, the increase is dropped and `last_reported` is
    /// repeated unchanged instead -- so any real slowdown is still visible
    /// (one honest jump), but the value can never climb two ticks running.
    /// Any tick whose estimate is flat or lower clears the pending flag,
    /// so a later, separate slowdown gets its own one free jump again.
    fn sample(&mut self, small_done: u64, large_done: u64) -> Option<u64> {
        let small_delta = small_done.saturating_sub(self.last_small_done);
        let large_delta = large_done.saturating_sub(self.last_large_done);
        self.last_small_done = small_done;
        self.last_large_done = large_done;

        if small_delta > 0 {
            let instant_rate = small_delta as f64 * 10.0; // per-100ms -> per-second
            self.small_rate_ema = Some(match self.small_rate_ema {
                None => instant_rate,
                Some(prev) => ETA_EWMA_ALPHA * instant_rate + (1.0 - ETA_EWMA_ALPHA) * prev,
            });
        }
        if large_delta > 0 {
            let instant_rate = large_delta as f64 * 10.0;
            self.large_rate_ema = Some(match self.large_rate_ema {
                None => instant_rate,
                Some(prev) => ETA_EWMA_ALPHA * instant_rate + (1.0 - ETA_EWMA_ALPHA) * prev,
            });
        }

        let remaining_small = self.total_small_files.saturating_sub(small_done);
        let remaining_large = self.total_large_bytes.saturating_sub(large_done);

        let small_eta = if remaining_small == 0 {
            Some(0.0)
        } else {
            self.small_rate_ema
                .filter(|r| *r > 0.0)
                .map(|r| remaining_small as f64 / r)
        };
        let large_eta = if remaining_large == 0 {
            Some(0.0)
        } else {
            self.large_rate_ema
                .filter(|r| *r > 0.0)
                .map(|r| remaining_large as f64 / r)
        };

        // `max`, not `+`: both regimes are drained by the same shared,
        // `concurrency`-bounded worker pool (see the module doc comment's
        // "Concurrency model" section), not two independent resource
        // lanes -- each regime's own observed rate already reflects
        // whatever share of the pool it's actually been getting while
        // sharing it with the other regime. The job as a whole finishes
        // once the slower-to-clear regime clears, not after the sum of
        // both as if they ran one after the other.
        let raw = match (small_eta, large_eta) {
            (Some(s), Some(l)) => Some(s.max(l).round() as u64),
            _ => None,
        };

        let reported = match (raw, self.last_reported) {
            (Some(raw), Some(last)) if raw > last => {
                if self.backward_pending {
                    Some(last)
                } else {
                    self.backward_pending = true;
                    Some(raw)
                }
            }
            (Some(raw), _) => {
                self.backward_pending = false;
                Some(raw)
            }
            (None, _) => None,
        };
        self.last_reported = reported;
        reported
    }
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
///
/// `resolver` is consulted for a conflict no pre-resolved `Step::conflict`
/// field and no already-established per-job "apply to all" answer already
/// covers — see the module doc comment's "Conflict resolution" section.
/// `None` means every such conflict falls back to
/// `plan.options.default_conflict` with no live consultation at all.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    fs: Arc<dyn FileSystem>,
    job_id: JobId,
    plan: Plan,
    journal: Journal,
    concurrency: usize,
    events: mpsc::UnboundedSender<JobEvent>,
    control: ExecutionControl,
    resolver: Option<Arc<dyn ConflictResolver>>,
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

    let default_conflict = plan.options.default_conflict;
    let ctx = ExecutorContext {
        fs,
        job_id,
        journal,
        control,
        counters: Arc::new(ProgressCounters::default()),
        events: events.clone(),
        outcomes: Arc::new(Mutex::new(HashMap::new())),
        default_conflict,
        resolver,
        sticky_conflict: Arc::new(Mutex::new(None)),
    };

    let (total_small_files, total_large_bytes) = regime_totals(&plan.steps);
    let sampler = spawn_progress_sampler(
        job_id,
        Arc::clone(&ctx.counters),
        events.clone(),
        total_small_files,
        total_large_bytes,
    );

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
    // T-5.1.10: bounded across the whole step (not reset by a `Space`
    // pause/resume episode or a pause/cancel-interrupted retry) -- see
    // `retry_backoff`'s own doc comment for the bound.
    let mut retryable_attempts = 0u32;
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

        let outcome = if let Some(reason) = dependency_block_reason(ctx, step) {
            // Short-circuit before ever calling `dispatch` -- a
            // dependency-blocked step performs no side effect at all, so
            // there's nothing to attempt.
            StepOutcome::Skipped { reason }
        } else {
            let attempt = dispatch(ctx, step_index, step, partial_name.as_deref()).await;
            match attempt {
                // Several `StepFailure`s built deep inside `dispatch` (e.g.
                // a `RenameTarget` conflict with no alternate name, or a
                // `naive_copy` I/O error) don't know their own step index
                // at construction time and are built with a placeholder
                // `step_index: 0` -- fixed up here, once, in the one place
                // that actually knows it, rather than threading the real
                // index through every failure-construction site.
                Ok(StepAttempt::Done(StepOutcome::Failed(mut failure))) => {
                    failure.step_index = step_index;
                    StepOutcome::Failed(failure)
                }
                Ok(StepAttempt::Done(outcome)) => outcome,
                Ok(StepAttempt::Interrupted) => continue, // pause/cancel mid-copy -- retry
                Err(e) if e.kind() == ErrorKind::Space => {
                    // T-5.1.10: ENOSPC/EDQUOT -- pause (the same primitive
                    // a user-initiated pause uses) and retry this step
                    // indefinitely once resumed, instead of failing the
                    // job outright. See the module doc comment's "Error
                    // taxonomy & retry" section for why this only pauses
                    // *this* job, not a queue that doesn't exist yet
                    // (T-5.1.13).
                    ctx.control.pause();
                    let _ = ctx
                        .events
                        .send(JobEvent::QueuePausedForSpace { job_id: ctx.job_id });
                    continue;
                }
                Err(e)
                    if e.kind().is_retryable() && retryable_attempts < RETRYABLE_MAX_ATTEMPTS =>
                {
                    retryable_attempts += 1;
                    if retry_backoff(ctx, retryable_attempts).await == ControlState::Cancelled {
                        return StepRun::Cancelled;
                    }
                    continue;
                }
                Err(e) => StepOutcome::Failed(StepFailure {
                    step_index,
                    path: step_primary_path(step),
                    kind: e.kind(),
                    message: permission_hint(e.kind(), &e.to_string()),
                }),
            }
        };
        ctx.outcomes
            .lock()
            .unwrap()
            .insert(step_index, outcome.clone());

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
/// removal/metadata, `dest` for the read-only `Verify`.
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

/// The `step_index` a step's own execution is contingent on, if any -- see
/// [`Step::Remove`]/[`Step::Verify`]'s own doc comments for what this
/// field exists to prevent (T-5.1.5: a cross-device move's terminal
/// `Remove` running even though the copy it was supposed to follow failed
/// or was never reached).
fn step_depends_on(step: &Step) -> Option<u32> {
    match step {
        Step::Remove { depends_on, .. }
        | Step::Verify { depends_on, .. }
        | Step::SetMeta { depends_on, .. }
        | Step::Link { depends_on, .. } => *depends_on,
        _ => None,
    }
}

/// `Some(reason)` if `step` has an unmet [`step_depends_on`] dependency and
/// must be skipped without ever being dispatched; `None` if it's clear to
/// proceed (no dependency at all, or the dependency's own outcome wasn't
/// `Failed`).
///
/// Deliberately gates on "not `Failed`", not on "`Succeeded`": a fresh
/// `execute()` call resuming a previously-interrupted job re-walks
/// `plan.steps` from the top and re-attempts everything, including steps
/// that already durably succeeded before the interruption -- per this
/// module's own "resume restarts the step" scope note, a `CopyFile` step
/// whose destination was already fully written last time re-runs into
/// `ErrorKind::Conflict` and is (correctly, safely) recorded `Skipped`,
/// not `Succeeded`, on the second run. Gating a dependent `Remove` on
/// strict `Succeeded` would treat that perfectly-fine resumed copy as
/// unmet and strand the move forever, unable to ever remove a source
/// whose destination has been correct since the *first* run. `Skipped`
/// carries no information about *why* -- design.md's conflict-resolution
/// story (T-5.1.9, not built yet) is what would eventually distinguish
/// "already correctly there" from "a totally unrelated file happens to
/// occupy this path" -- but refusing to proceed only on a definite,
/// unambiguous `Failed` is the safe, conservative choice available today:
/// it never blocks a legitimate resume, and it never lets a `Remove`
/// through when its prerequisite is *known* to have gone wrong.
fn dependency_block_reason(ctx: &ExecutorContext, step: &Step) -> Option<String> {
    let dep = step_depends_on(step)?;
    let outcomes = ctx.outcomes.lock().unwrap();
    match outcomes.get(&dep) {
        Some(StepOutcome::Failed(failure)) => Some(format!(
            "prerequisite step {dep} failed ({}), refusing to proceed",
            failure.message
        )),
        _ => None,
    }
}

/// The bound on how many times a `Retryable` failure gets retried before
/// `run_step_with_retry` gives up and reports it as a genuine
/// [`StepOutcome::Failed`]. A documented, deliberately conservative choice
/// (mirroring `suggested_concurrency`'s own precedent for "no numeric
/// guidance exists anywhere in design.md/task.md") -- 5 attempts covers a
/// real transient blip (a dropped connection, `EINTR`/`EAGAIN`) without
/// letting a step spin close to forever against a condition that will
/// never clear.
const RETRYABLE_MAX_ATTEMPTS: u32 = 5;
/// The first backoff delay; doubles on each subsequent attempt, capped at
/// [`RETRYABLE_MAX_BACKOFF`]. Deliberately short relative to typical retry-
/// with-backoff guidance elsewhere (seconds, not tens of milliseconds) so
/// this crate's own test suite stays fast while still exercising genuine
/// exponential growth and a real bound -- nothing in design.md/task.md
/// specifies an exact figure.
const RETRYABLE_INITIAL_BACKOFF: Duration = Duration::from_millis(20);
const RETRYABLE_MAX_BACKOFF: Duration = Duration::from_millis(500);

/// Sleeps out `attempt`'s bounded-exponential-backoff delay (design.md
/// §9.3: "Retryable errors get bounded exponential backoff"), in short
/// increments so a cancel lands promptly instead of waiting out the full
/// delay -- mirroring [`wait_out_pause`]'s own poll-and-check cadence.
/// Returns the resulting [`ControlState`] so the caller can tell a
/// cancel-during-backoff apart from a delay that ran to completion.
async fn retry_backoff(ctx: &ExecutorContext, attempt: u32) -> ControlState {
    // `attempt` is 1-based (the first retry passes 1); attempt N sleeps
    // `INITIAL * 2^(N-1)`, capped at `RETRYABLE_MAX_BACKOFF`.
    let delay = RETRYABLE_INITIAL_BACKOFF
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
        .min(RETRYABLE_MAX_BACKOFF);
    let mut waited = Duration::ZERO;
    while waited < delay {
        if ctx.control.state() == ControlState::Cancelled {
            return ControlState::Cancelled;
        }
        let chunk = Duration::from_millis(10).min(delay - waited);
        tokio::time::sleep(chunk).await;
        waited += chunk;
    }
    ctx.control.state()
}

/// Appends a plain-language hint to `message` when `kind` is
/// `ErrorKind::Permission` -- FR-OPS-13's actual elevation offer (a real
/// polkit/D-Bus prompt) is T-9.1.13's scope, not built here, but the report
/// should still say plainly that elevation would help rather than leaving
/// a bare "permission denied" for the user to puzzle out.
fn permission_hint(kind: ErrorKind, message: &str) -> String {
    if kind == ErrorKind::Permission {
        format!(
            "{message} (permission denied -- retrying elevated isn't available yet; \
             tracked as T-9.1.13)"
        )
    } else {
        message.to_string()
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
    step_index: u32,
    step: &Step,
    partial_name: Option<&str>,
) -> Result<StepAttempt> {
    match step {
        Step::CreateDir { dest, mode } => create_dir_step(ctx, step_index, dest, *mode).await,
        Step::CopyFile {
            source,
            dest,
            size,
            conflict,
        }
        | Step::Reflink {
            source,
            dest,
            size,
            conflict,
        } => {
            copy_file_step(
                ctx,
                step_index,
                *conflict,
                source,
                dest,
                *size,
                partial_name,
            )
            .await
        }
        Step::Rename {
            source,
            dest,
            conflict,
        } => rename_step(ctx, step_index, *conflict, source, dest).await,
        Step::SetMeta { target, patch, .. } => set_meta_step(&*ctx.fs, target, patch).await,
        Step::Remove { target, mode, .. } => remove_step(&*ctx.fs, target, *mode).await,
        Step::Link { source, dest, .. } => link_step(ctx, step_index, source, dest).await,
        Step::Verify {
            source,
            dest,
            algorithm,
            ..
        } => verify_step(ctx, source, dest, *algorithm).await,
    }
}

/// `CreateDir`'s own conflict handling: unlike `CopyFile`/`Reflink`/
/// `Rename`, an existing directory at `dest` isn't a real conflict at all
/// (see the module doc comment's "Conflict resolution" section) — only a
/// non-directory occupying `dest` goes through [`resolve_conflict`] and the
/// real seven-policy engine.
async fn create_dir_step(
    ctx: &ExecutorContext,
    step_index: u32,
    dest: &VPath,
    mode: Option<u32>,
) -> Result<StepAttempt> {
    match ctx.fs.create_dir(dest, mode.map(Mode::new)).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            let existing = ctx.fs.stat(dest, false).await?;
            if existing.is_dir() {
                return Ok(StepAttempt::Done(StepOutcome::Succeeded));
            }
            // A non-directory occupies `dest` -- a genuine conflict.
            // `Step::CreateDir` carries no source path (see `mover.rs`'s
            // own doc comment on that gap), so `dest` stands in for both
            // sides of the prompt; a live resolver still gets a real
            // `dest_meta`, just an uninformative `source_meta`.
            let resolution = resolve_conflict(ctx, step_index, None, dest, dest).await?;
            apply_create_dir_conflict_resolution(ctx, resolution, dest, mode).await
        }
        Err(e) => Err(e),
    }
}

async fn apply_create_dir_conflict_resolution(
    ctx: &ExecutorContext,
    resolution: ConflictResolution,
    dest: &VPath,
    mode: Option<u32>,
) -> Result<StepAttempt> {
    match resolution.policy {
        ConflictPolicy::Skip => Ok(skip_attempt(dest, "skip")),
        ConflictPolicy::Abort => {
            ctx.control.cancel();
            Ok(skip_attempt(
                dest,
                "abort -- user chose to stop the job at this conflict",
            ))
        }
        // `Step::CreateDir` carries no source path to compare `dest`
        // against, so "overwrite if older/different size" has nothing to
        // measure -- conservatively degrade to `Skip` (this crate's
        // "never clobber silently" default philosophy) rather than guess.
        // A file occupying the path a directory is meant to go is a rare,
        // essentially adversarial case; `Overwrite` itself still works
        // unconditionally, since it needs no comparison.
        ConflictPolicy::OverwriteIfOlder | ConflictPolicy::OverwriteIfDifferentSize => {
            Ok(skip_attempt(
                dest,
                "no source to compare mtime/size against for a directory",
            ))
        }
        ConflictPolicy::Overwrite => {
            ctx.fs.remove(dest, RemoveKind::File).await?;
            ctx.fs.create_dir(dest, mode.map(Mode::new)).await?;
            Ok(StepAttempt::Done(StepOutcome::Succeeded))
        }
        ConflictPolicy::RenameTarget => match resolution.alternate {
            Some(alt) => {
                ctx.fs.create_dir(&alt, mode.map(Mode::new)).await?;
                Ok(StepAttempt::Done(StepOutcome::Succeeded))
            }
            None => Ok(rename_target_needs_a_name(dest)),
        },
        ConflictPolicy::AutoRename => {
            let alt = auto_rename_target(ctx, dest).await?;
            ctx.fs.create_dir(&alt, mode.map(Mode::new)).await?;
            Ok(StepAttempt::Done(StepOutcome::Succeeded))
        }
    }
}

/// Attempts `fs.rename(from, dest, NO_REPLACE)`; on a real conflict,
/// resolves it and applies whichever of the seven policies won. Shared by
/// [`rename_step`] and [`copy_file_step`]'s publish step -- `source` (used
/// only for the conflict prompt and `OverwriteIfOlder`/
/// `OverwriteIfDifferentSize`'s comparisons) and `from` (the actual rename
/// operand) coincide for a same-device `Rename` step but differ for a
/// `CopyFile`/`Reflink` publish, where `from` is the staged partial, not
/// the original source.
async fn rename_with_conflict_resolution(
    ctx: &ExecutorContext,
    step_index: u32,
    pre_resolved: Option<ConflictPolicy>,
    source: &VPath,
    from: &VPath,
    dest: &VPath,
) -> Result<StepAttempt> {
    match ctx.fs.rename(from, dest, RenameFlags::NO_REPLACE).await {
        Ok(()) => return Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() != ErrorKind::Conflict => return Err(e),
        Err(_) => {}
    }
    let resolution = resolve_conflict(ctx, step_index, pre_resolved, source, dest).await?;
    apply_rename_conflict_resolution(ctx, resolution, source, from, dest).await
}

async fn apply_rename_conflict_resolution(
    ctx: &ExecutorContext,
    resolution: ConflictResolution,
    source: &VPath,
    from: &VPath,
    dest: &VPath,
) -> Result<StepAttempt> {
    match resolution.policy {
        ConflictPolicy::Skip => Ok(skip_attempt(dest, "skip")),
        ConflictPolicy::Abort => {
            ctx.control.cancel();
            Ok(skip_attempt(
                dest,
                "abort -- user chose to stop the job at this conflict",
            ))
        }
        ConflictPolicy::Overwrite => replace_rename(ctx, from, dest).await,
        ConflictPolicy::OverwriteIfOlder => {
            let source_meta = ctx.fs.stat(source, false).await?;
            let dest_meta = ctx.fs.stat(dest, false).await?;
            match (dest_meta.modified, source_meta.modified) {
                (Some(d), Some(s)) if d < s => replace_rename(ctx, from, dest).await,
                _ => Ok(skip_attempt(
                    dest,
                    "destination is not older than the source",
                )),
            }
        }
        ConflictPolicy::OverwriteIfDifferentSize => {
            let source_meta = ctx.fs.stat(source, false).await?;
            let dest_meta = ctx.fs.stat(dest, false).await?;
            if dest_meta.size != source_meta.size {
                replace_rename(ctx, from, dest).await
            } else {
                Ok(skip_attempt(
                    dest,
                    "destination is the same size as the source",
                ))
            }
        }
        ConflictPolicy::RenameTarget => match resolution.alternate {
            Some(alt) => rename_to_alternate(ctx, from, &alt).await,
            None => Ok(rename_target_needs_a_name(dest)),
        },
        ConflictPolicy::AutoRename => {
            let alt = auto_rename_target(ctx, dest).await?;
            rename_to_alternate(ctx, from, &alt).await
        }
    }
}

/// Forces the destination to be replaced -- used once the resolved policy
/// has already decided to overwrite unconditionally (`Overwrite`, or
/// `OverwriteIfOlder`/`OverwriteIfDifferentSize` once their own comparison
/// favoured it).
async fn replace_rename(ctx: &ExecutorContext, from: &VPath, dest: &VPath) -> Result<StepAttempt> {
    ctx.fs.rename(from, dest, RenameFlags::empty()).await?;
    Ok(StepAttempt::Done(StepOutcome::Succeeded))
}

/// Renames `from` onto `alt` (an alternate, expected-to-be-free
/// destination chosen by `RenameTarget`'s resolver answer or
/// [`auto_rename_target`]) with `NO_REPLACE` -- a second conflict here
/// (the alternate name itself collided, an extremely unlikely race) is
/// reported as a failure rather than looped on indefinitely.
async fn rename_to_alternate(
    ctx: &ExecutorContext,
    from: &VPath,
    alt: &VPath,
) -> Result<StepAttempt> {
    match ctx.fs.rename(from, alt, RenameFlags::NO_REPLACE).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
                step_index: 0, // overwritten by the caller
                path: Some(alt.clone()),
                kind: ErrorKind::Conflict,
                message: format!("{alt} also already exists; refusing to guess another name"),
            })))
        }
        Err(e) => Err(e),
    }
}

fn skip_attempt(dest: &VPath, reason: &str) -> StepAttempt {
    StepAttempt::Done(StepOutcome::Skipped {
        reason: format!("{dest} already exists ({reason})"),
    })
}

fn rename_target_needs_a_name(dest: &VPath) -> StepAttempt {
    StepAttempt::Done(StepOutcome::Failed(StepFailure {
        step_index: 0, // overwritten by the caller
        path: Some(dest.clone()),
        kind: ErrorKind::Fatal,
        message: format!(
            "{dest}: RenameTarget requires an alternate destination, but the conflict \
             resolver didn't supply one (ConflictResolution::alternate was None) -- use \
             AutoRename for an engine-chosen name, or have the resolver provide one"
        ),
    }))
}

/// The number of `name (N).ext` candidates [`auto_rename_target`] will try
/// before giving up -- generous enough that hitting it means something is
/// genuinely wrong (a directory pre-populated with hundreds of
/// consecutively-numbered collisions), not a real "ran out of names" case.
const AUTO_RENAME_MAX_ATTEMPTS: u32 = 1000;

/// Finds the first non-colliding `name (2).ext`, `name (3).ext`, ... sibling
/// of `dest` by probing `fs.stat` -- the engine-chosen name
/// `ConflictPolicy::AutoRename` promises, with no prompt involved.
async fn auto_rename_target(ctx: &ExecutorContext, dest: &VPath) -> Result<VPath> {
    let fatal = |msg: String| -> Box<VfsError> {
        Box::new(VfsError::new(ErrorKind::Fatal, msg).with_path(dest.clone()))
    };
    let parent = dest
        .parent()
        .ok_or_else(|| fatal("no parent to auto-rename within".to_string()))?;
    let name = dest.inner().file_name().unwrap_or("").to_string();
    let std_name = std::path::Path::new(&name);
    let stem = std_name
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&name)
        .to_string();
    let ext = std_name
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_string());

    for n in 2..=AUTO_RENAME_MAX_ATTEMPTS {
        let candidate_name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent
            .join(&candidate_name)
            .map_err(|e| fatal(e.to_string()))?;
        match ctx.fs.stat(&candidate, false).await {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(fatal(format!(
        "could not find a free auto-rename name near {dest} after {AUTO_RENAME_MAX_ATTEMPTS} attempts"
    )))
}

/// Resolves what to do about a real conflict at `dest`, honoring design.md
/// §9.3's tiering -- see the module doc comment's "Conflict resolution"
/// section for the full precedence order and rationale. `source` is used
/// only to build a [`ConflictPrompt`] if a live resolver actually needs to
/// be consulted (the pre-resolved and sticky tiers need no metadata at
/// all).
async fn resolve_conflict(
    ctx: &ExecutorContext,
    step_index: u32,
    pre_resolved: Option<ConflictPolicy>,
    source: &VPath,
    dest: &VPath,
) -> Result<ConflictResolution> {
    if let Some(policy) = pre_resolved {
        return Ok(ConflictResolution::once(policy));
    }
    if let Some(sticky) = ctx.sticky_conflict.lock().unwrap().clone() {
        return Ok(sticky);
    }
    let Some(resolver) = ctx.resolver.clone() else {
        return Ok(ConflictResolution::once(ctx.default_conflict));
    };
    let source_meta = ctx.fs.stat(source, false).await?;
    let dest_meta = ctx.fs.stat(dest, false).await?;
    let prompt = ConflictPrompt {
        step_index,
        source: source.clone(),
        dest: dest.clone(),
        source_meta,
        dest_meta,
    };
    let resolution = resolver.resolve(&prompt);
    if resolution.scope == ConflictScope::AllRemaining {
        *ctx.sticky_conflict.lock().unwrap() = Some(resolution.clone());
    }
    let _ = ctx.events.send(JobEvent::ConflictDetected {
        job_id: ctx.job_id,
        prompt: Box::new(prompt),
    });
    Ok(resolution)
}

async fn rename_step(
    ctx: &ExecutorContext,
    step_index: u32,
    conflict: Option<ConflictPolicy>,
    source: &VPath,
    dest: &VPath,
) -> Result<StepAttempt> {
    rename_with_conflict_resolution(ctx, step_index, conflict, source, source, dest).await
}

/// `Step::Link` dispatch (T-5.1.7): `fs.link(source, dest)`, going through
/// the same seven-policy conflict engine as `CreateDir` on a real conflict
/// -- `Step::Link` carries no `conflict` field of its own (like
/// `CreateDir`), so `pre_resolved` is always `None` here.
async fn link_step(
    ctx: &ExecutorContext,
    step_index: u32,
    source: &VPath,
    dest: &VPath,
) -> Result<StepAttempt> {
    match ctx.fs.link(source, dest).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            let resolution = resolve_conflict(ctx, step_index, None, source, dest).await?;
            apply_link_conflict_resolution(ctx, resolution, source, dest).await
        }
        Err(e) => Err(e),
    }
}

async fn apply_link_conflict_resolution(
    ctx: &ExecutorContext,
    resolution: ConflictResolution,
    source: &VPath,
    dest: &VPath,
) -> Result<StepAttempt> {
    match resolution.policy {
        ConflictPolicy::Skip => Ok(skip_attempt(dest, "skip")),
        ConflictPolicy::Abort => {
            ctx.control.cancel();
            Ok(skip_attempt(
                dest,
                "abort -- user chose to stop the job at this conflict",
            ))
        }
        ConflictPolicy::Overwrite => replace_link(ctx, source, dest).await,
        ConflictPolicy::OverwriteIfOlder => {
            let source_meta = ctx.fs.stat(source, false).await?;
            let dest_meta = ctx.fs.stat(dest, false).await?;
            match (dest_meta.modified, source_meta.modified) {
                (Some(d), Some(s)) if d < s => replace_link(ctx, source, dest).await,
                _ => Ok(skip_attempt(
                    dest,
                    "destination is not older than the source",
                )),
            }
        }
        ConflictPolicy::OverwriteIfDifferentSize => {
            let source_meta = ctx.fs.stat(source, false).await?;
            let dest_meta = ctx.fs.stat(dest, false).await?;
            if dest_meta.size != source_meta.size {
                replace_link(ctx, source, dest).await
            } else {
                Ok(skip_attempt(
                    dest,
                    "destination is the same size as the source",
                ))
            }
        }
        ConflictPolicy::RenameTarget => match resolution.alternate {
            Some(alt) => link_to_alternate(ctx, source, &alt).await,
            None => Ok(rename_target_needs_a_name(dest)),
        },
        ConflictPolicy::AutoRename => {
            let alt = auto_rename_target(ctx, dest).await?;
            link_to_alternate(ctx, source, &alt).await
        }
    }
}

/// Forces `dest` to be replaced with a hardlink to `source` -- there is no
/// atomic "link-over-destination" primitive (unlike `rename`'s
/// `RENAME_EXCHANGE`/plain-replace semantics), so this necessarily removes
/// `dest` first, non-atomically, before linking.
async fn replace_link(ctx: &ExecutorContext, source: &VPath, dest: &VPath) -> Result<StepAttempt> {
    ctx.fs.remove(dest, RemoveKind::File).await?;
    ctx.fs.link(source, dest).await?;
    Ok(StepAttempt::Done(StepOutcome::Succeeded))
}

/// Links `source` to `alt` (an alternate, expected-to-be-free destination
/// chosen by `RenameTarget`'s resolver answer or [`auto_rename_target`]) --
/// a second conflict here (the alternate name itself collided) is reported
/// as a failure rather than looped on indefinitely, mirroring
/// [`rename_to_alternate`].
async fn link_to_alternate(
    ctx: &ExecutorContext,
    source: &VPath,
    alt: &VPath,
) -> Result<StepAttempt> {
    match ctx.fs.link(source, alt).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
                step_index: 0, // overwritten by the caller
                path: Some(alt.clone()),
                kind: ErrorKind::Conflict,
                message: format!("{alt} also already exists; refusing to guess another name"),
            })))
        }
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
    match fs.remove(target, kind).await {
        Ok(()) => Ok(StepAttempt::Done(StepOutcome::Succeeded)),
        // `EmptyDir` on a directory that isn't empty yet (a file inside it
        // hasn't been removed -- its own copy failed, say) surfaces as
        // `ErrorKind::Conflict` (`ENOTEMPTY`) per `RemoveKind::EmptyDir`'s
        // own doc comment. This is T-5.1.5's self-gating mechanism for
        // directory cleanup at the end of a cross-device move: no
        // `depends_on` bookkeeping needed for these steps at all, since a
        // non-empty directory simply and correctly refuses to go away on
        // its own, exactly like any other conflict skip.
        Err(e) if mode == RemoveMode::EmptyDir && e.kind() == ErrorKind::Conflict => {
            Ok(StepAttempt::Done(StepOutcome::Skipped {
                reason: format!(
                    "{target} is not empty yet -- a step that writes into it \
                                  must have failed or been skipped"
                ),
            }))
        }
        Err(e) => Err(e),
    }
}

/// Compares `source` and `dest` per `algorithm` -- `SizeOnly` (a `stat` on
/// each side, no content read) is fully implemented; `Blake3` is
/// deliberately left unimplemented (T-5.1.12's own scope, per its task.md
/// entry: "Post-copy verification (BLAKE3) as a job flag") rather than
/// pulled in here just for this task's own cross-device-move AC, which
/// design.md itself only requires "verify (if enabled)" for -- `SizeOnly`
/// is a real, useful verification level on its own (catches a truncated
/// or otherwise short/long copy), not a stub standing in for the real
/// thing.
/// One [`hash_file`] attempt's result: either it hashed the whole file, or
/// a pause/cancel request landed mid-read -- mirrors [`StepAttempt`] at a
/// smaller scope (a single side of a [`VerifyAlgorithm::Blake3`]
/// comparison, not a whole step).
enum HashAttempt {
    Done(blake3::Hash),
    Interrupted,
}

/// Reads `path` in [`COPY_BUFFER_BYTES`] chunks, feeding each into a BLAKE3
/// hasher -- streaming rather than buffering the whole file in memory
/// (T-5.1.12: a multi-gigabyte file must not need a multi-gigabyte
/// allocation just to verify it), and checking `control` every chunk, the
/// same cooperative-cancellation cadence [`naive_copy`] already uses.
/// Takes owned/`Arc` arguments (not borrowed) so [`verify_step`] can
/// `tokio::spawn` two of these concurrently -- genuine parallelism across
/// real OS threads, not just cooperative interleaving on one, matters here
/// the same way it does for the copy-class worker pool: `LocalFs`'s reads
/// block their thread inline (this module's own "Operational requirement"
/// doc comment on [`execute`]).
async fn hash_file(
    fs: Arc<dyn FileSystem>,
    path: VPath,
    control: ExecutionControl,
) -> Result<HashAttempt> {
    let mut reader = fs.open_read(&path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        if control.state() != ControlState::Running {
            return Ok(HashAttempt::Interrupted);
        }
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| Box::new(VfsError::from_io(e)))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(HashAttempt::Done(hasher.finalize()))
}

/// Compares `source` and `dest` per `algorithm` -- `SizeOnly` (a `stat` on
/// each side, no content read) is cheap but only catches a truncated or
/// otherwise wrong-length copy; `Blake3` (T-5.1.12, FR-OPS-08) reads and
/// hashes both sides in full, concurrently, and catches genuine content
/// corruption `SizeOnly` cannot (a bit-flip that doesn't change length).
async fn verify_step(
    ctx: &ExecutorContext,
    source: &VPath,
    dest: &VPath,
    algorithm: VerifyAlgorithm,
) -> Result<StepAttempt> {
    match algorithm {
        VerifyAlgorithm::SizeOnly => {
            let source_meta = ctx.fs.stat(source, false).await?;
            let dest_meta = ctx.fs.stat(dest, false).await?;
            if source_meta.size == dest_meta.size {
                Ok(StepAttempt::Done(StepOutcome::Succeeded))
            } else {
                Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
                    step_index: 0, // overwritten by the caller
                    path: Some(dest.clone()),
                    kind: ErrorKind::Fatal,
                    message: format!(
                        "size mismatch: {source} is {} bytes, {dest} is {} bytes",
                        source_meta.size, dest_meta.size
                    ),
                })))
            }
        }
        VerifyAlgorithm::Blake3 => {
            let source_task = tokio::spawn(hash_file(
                Arc::clone(&ctx.fs),
                source.clone(),
                ctx.control.clone(),
            ));
            let dest_task = tokio::spawn(hash_file(
                Arc::clone(&ctx.fs),
                dest.clone(),
                ctx.control.clone(),
            ));
            let (source_result, dest_result) = tokio::join!(source_task, dest_task);
            let source_attempt = source_result.expect("hash task panicked")?;
            let dest_attempt = dest_result.expect("hash task panicked")?;
            match (source_attempt, dest_attempt) {
                (HashAttempt::Interrupted, _) | (_, HashAttempt::Interrupted) => {
                    Ok(StepAttempt::Interrupted)
                }
                (HashAttempt::Done(source_hash), HashAttempt::Done(dest_hash)) => {
                    if source_hash == dest_hash {
                        Ok(StepAttempt::Done(StepOutcome::Succeeded))
                    } else {
                        Ok(StepAttempt::Done(StepOutcome::Failed(StepFailure {
                            step_index: 0, // overwritten by the caller
                            path: Some(dest.clone()),
                            kind: ErrorKind::Fatal,
                            message: format!(
                                "content mismatch: {source} and {dest} have different BLAKE3 \
                                 digests ({source_hash} vs {dest_hash})"
                            ),
                        })))
                    }
                }
            }
        }
    }
}

/// The scratch partial path itself unexpectedly already existing --
/// essentially never a real "the destination already exists" conflict
/// (`partial`'s name is randomly generated per-attempt), so it doesn't go
/// through the seven-policy engine at all; treated as a simple skip rather
/// than a hard error, on the theory that whatever's occupying our scratch
/// name will be gone by the next attempt.
fn partial_collision(partial: &VPath) -> StepOutcome {
    StepOutcome::Skipped {
        reason: format!("scratch path {partial} unexpectedly already exists"),
    }
}

/// Executes one `CopyFile`/`Reflink` step: tries `fs.server_side_copy`
/// staged through `partial_name` first (reflink/`copy_file_range`, when
/// the backend can accelerate it), falling back to a naive buffered
/// `open_read`/`open_write` loop -- pause/cancel-checked every
/// [`COPY_BUFFER_BYTES`] -- when the backend reports `Unsupported`. Either
/// way, publishes by renaming the partial onto `dest` (through
/// [`rename_with_conflict_resolution`], the same seven-policy engine a
/// `Rename` step uses) as an explicit, separate step, so
/// `Intent.partial_name` (recorded before this function is even called)
/// always names the file recovery would actually find.
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
    step_index: u32,
    conflict: Option<ConflictPolicy>,
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

    // The accelerated path now reports real incremental byte counts of its
    // own (T-5.1.4's ladder rungs each call `on_progress` as they go -- see
    // `local::probe::accelerated_copy`/`sparse_buffered_copy`), so this
    // closure credits `ctx.counters` directly from those calls, the same
    // way `naive_copy` already credits its own chunk loop. `credited_this_
    // attempt` tracks how much *this* attempt has added so it can be
    // rolled back cleanly if the attempt is interrupted, falls back, or
    // errors out -- the retry loop restarts the whole step from scratch,
    // so partial credit must never survive into the next attempt (exactly
    // `naive_copy`'s existing invariant, extended to this path).
    let credited_this_attempt = AtomicU64::new(0);
    let is_large = !is_small_file(expected_size);
    let on_progress = |bytes: u64| {
        if bytes > 0 {
            credited_this_attempt.fetch_add(bytes, Ordering::Relaxed);
            ctx.counters.bytes_done.fetch_add(bytes, Ordering::Relaxed);
            if is_large {
                ctx.counters
                    .large_bytes_done
                    .fetch_add(bytes, Ordering::Relaxed);
            }
        }
        ctx.control.state() != ControlState::Running
    };
    match ctx
        .fs
        .server_side_copy(source, &partial, &on_progress)
        .await
    {
        Ok(duet_vfs::CopyOutcome::Copied { .. }) => {
            // Top up to `expected_size` for whatever this rung didn't (or
            // structurally can't) report incrementally. This is a no-op
            // for FICLONE and copy_file_range, which already report the
            // full size via `on_progress` above; it's not a no-op for
            // `sparse_buffered_copy`, whose hole-skipping means its own
            // reported total can fall short of the file's logical size by
            // design (see that function's own doc comment).
            let remainder =
                expected_size.saturating_sub(credited_this_attempt.load(Ordering::Relaxed));
            if remainder > 0 {
                ctx.counters
                    .bytes_done
                    .fetch_add(remainder, Ordering::Relaxed);
                if is_large {
                    ctx.counters
                        .large_bytes_done
                        .fetch_add(remainder, Ordering::Relaxed);
                }
            }
        }
        Ok(duet_vfs::CopyOutcome::Unsupported) => {
            // Roll back whatever this attempt already credited before
            // falling back to naive_copy, which does its own independent
            // incremental accounting from zero -- same reasoning as the
            // Interrupted/error rollback below, just via a different path
            // (falling back, not retrying).
            let credited = credited_this_attempt.load(Ordering::Relaxed);
            if credited > 0 {
                ctx.counters
                    .bytes_done
                    .fetch_sub(credited, Ordering::Relaxed);
                if is_large {
                    ctx.counters
                        .large_bytes_done
                        .fetch_sub(credited, Ordering::Relaxed);
                }
            }
            // Best-effort cleanup of any zero-byte artifact the failed
            // acceleration attempt may have left before falling back.
            let _ = ctx.fs.remove(&partial, RemoveKind::File).await;
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
            // caller's retry loop restarts this whole step from scratch,
            // so this attempt's partial credit must be rolled back first.
            let credited = credited_this_attempt.load(Ordering::Relaxed);
            if credited > 0 {
                ctx.counters
                    .bytes_done
                    .fetch_sub(credited, Ordering::Relaxed);
                if is_large {
                    ctx.counters
                        .large_bytes_done
                        .fetch_sub(credited, Ordering::Relaxed);
                }
            }
            return Ok(StepAttempt::Interrupted);
        }
        Err(e) if e.kind() == ErrorKind::Conflict => {
            return Ok(StepAttempt::Done(partial_collision(&partial)));
        }
        Err(e) => {
            // T-5.1.10: a hard error here (e.g. `ENOSPC`, about to be
            // retried by the caller) gets the same best-effort cleanup and
            // rollback as the `Interrupted`/`Unsupported` branches above,
            // for the same reason -- don't leave a stray, possibly
            // partially-written `.duet-partial-*` file (or stale byte
            // credit) for a retry to trip over or a full disk to get
            // fuller from.
            let credited = credited_this_attempt.load(Ordering::Relaxed);
            if credited > 0 {
                ctx.counters
                    .bytes_done
                    .fetch_sub(credited, Ordering::Relaxed);
                if is_large {
                    ctx.counters
                        .large_bytes_done
                        .fetch_sub(credited, Ordering::Relaxed);
                }
            }
            let _ = ctx.fs.remove(&partial, RemoveKind::File).await;
            return Err(e);
        }
    }

    let attempt =
        rename_with_conflict_resolution(ctx, step_index, conflict, source, &partial, dest).await?;
    match &attempt {
        StepAttempt::Done(StepOutcome::Succeeded) => {
            ctx.counters.files_done.fetch_add(1, Ordering::Relaxed);
            // `bytes_done`/`large_bytes_done` are no longer touched here:
            // both the accelerated path (via `on_progress` above, plus its
            // top-up to `expected_size`) and the naive fallback (via
            // `naive_copy`'s own chunk loop, if that's the path this
            // attempt took) have already credited this file's bytes
            // incrementally by the time execution reaches this point.
            // Crediting `expected_size` again here would double-count
            // every successful copy.
            //
            // T-5.1.11: `small_files_done` is a real, unrelated, one-
            // per-file counter (not bytes) that `EtaEstimator`'s small-file
            // regime tracks by count rather than by size, so it still
            // needs an explicit increment regardless of which copy path
            // was used.
            if is_small_file(expected_size) {
                ctx.counters
                    .small_files_done
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        StepAttempt::Done(StepOutcome::Skipped { .. }) => {
            // Not published anywhere -- clean up the staged copy so a
            // skipped step doesn't leave an orphaned `.duet-partial-*`
            // file behind.
            let _ = ctx.fs.remove(&partial, RemoveKind::File).await;
        }
        _ => {}
    }
    Ok(attempt)
}

/// The naive fallback copy loop: plain buffered `open_read`/`open_write`,
/// no `fadvise`, no sparse-file awareness -- rung 3 of design.md's copy
/// -strategy ladder (T-5.1.4) is what upgrades this. Checks `ctx.control`
/// every [`COPY_BUFFER_BYTES`] chunk; on pause or cancel, aborts the
/// in-progress write (safe: `open_write` never touched `to` itself, only
/// its own internal staging sibling) and returns `Interrupted` for the
/// caller's retry loop to handle.
///
/// Updates `ctx.counters.bytes_done` (and, when `expected_size` is in the
/// large-file regime, `ctx.counters.large_bytes_done` -- see
/// [`is_small_file`]) incrementally, chunk by chunk, as real progress
/// happens -- both so the 100ms progress sampler reflects genuine
/// intra-file progress on a large single file (not just discrete per-file
/// jumps), and so a paused job's progress observably stops advancing
/// promptly rather than only becoming visible once the whole (now-
/// abandoned) file would have finished.
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
            if !is_small_file(expected_size) {
                ctx.counters
                    .large_bytes_done
                    .fetch_sub(copied, Ordering::Relaxed);
            }
            return Ok(StepAttempt::Interrupted);
        }
        let n = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                // A real I/O error (T-5.1.10: e.g. `ENOSPC`/`EINTR`, about
                // to be retried by the caller) gets the exact same
                // cleanup as a control-state interruption above -- an
                // aborted writer and a rolled-back byte count -- so a
                // retried attempt starts from a clean slate instead of
                // compounding a stray temp file onto an already-full
                // disk.
                let _ = writer.abort().await;
                ctx.counters.bytes_done.fetch_sub(copied, Ordering::Relaxed);
                if !is_small_file(expected_size) {
                    ctx.counters
                        .large_bytes_done
                        .fetch_sub(copied, Ordering::Relaxed);
                }
                return Err(Box::new(VfsError::from_io(e)));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = writer.write_all(&buf[..n]).await {
            let _ = writer.abort().await;
            ctx.counters.bytes_done.fetch_sub(copied, Ordering::Relaxed);
            if !is_small_file(expected_size) {
                ctx.counters
                    .large_bytes_done
                    .fetch_sub(copied, Ordering::Relaxed);
            }
            return Err(Box::new(VfsError::from_io(e)));
        }
        copied += n as u64;
        ctx.counters
            .bytes_done
            .fetch_add(n as u64, Ordering::Relaxed);
        if !is_small_file(expected_size) {
            ctx.counters
                .large_bytes_done
                .fetch_add(n as u64, Ordering::Relaxed);
        }
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
    total_small_files: u64,
    total_large_bytes: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut last_bytes = 0u64;
        let mut eta = EtaEstimator::new(total_small_files, total_large_bytes);
        loop {
            interval.tick().await;
            let bytes_done = counters.bytes_done.load(Ordering::Relaxed);
            let files_done = counters.files_done.load(Ordering::Relaxed);
            let small_files_done = counters.small_files_done.load(Ordering::Relaxed);
            let large_bytes_done = counters.large_bytes_done.load(Ordering::Relaxed);
            let throughput = bytes_done.saturating_sub(last_bytes) * 10; // per-100ms -> per-second
            last_bytes = bytes_done;
            let eta_secs = eta.sample(small_files_done, large_bytes_done);
            let snapshot = ProgressSnapshot {
                files_done,
                bytes_done,
                current_file_bytes_done: 0,
                current_file_bytes_total: 0,
                throughput_bytes_per_sec: throughput,
                eta_secs,
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
    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use duet_types::UnixPathBuf;
    use duet_vfs::LocalFs;
    use proptest::prelude::*;
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
        run_with_resolver(fs, plan, state_dir, concurrency, None).await
    }

    async fn run_with_resolver(
        fs: Arc<dyn FileSystem>,
        plan: Plan,
        state_dir: &Path,
        concurrency: usize,
        resolver: Option<Arc<dyn ConflictResolver>>,
    ) -> (JobReport, Vec<JobEvent>) {
        let journal = Journal::open(JobIdT(1), state_dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let report = execute(
            fs,
            JobIdT(1),
            plan,
            journal,
            concurrency,
            tx,
            control,
            resolver,
        )
        .await;
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
        async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
            self.inner.link(source, dest).await
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
            on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let result = if self.force_unsupported {
                Ok(duet_vfs::CopyOutcome::Unsupported)
            } else {
                self.inner.server_side_copy(from, to, on_progress).await
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
        async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
            self.inner.link(source, dest).await
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
            _on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    /// A `FileSystem` test double wrapping a real [`LocalFs`], whose
    /// `open_read` streams a file's *real* content (unlike
    /// [`ThrottledFs`]'s synthetic zero-filled stream, which ignores the
    /// requested path entirely) but paced to a fixed `bytes_per_sec`
    /// regardless of how fast the underlying storage actually is --
    /// T-5.1.11's `eta_accuracy_is_within_20_percent_after_ten_seconds`
    /// benchmark needs a real, sustained, multi-second copy to validate
    /// ETA accuracy against, and tmpfs (this crate's test tempdir backend)
    /// is fast enough that even several GiB copies in a couple of real
    /// seconds -- nowhere near long enough to observe the AC's own "after
    /// the first 10s" window without an unreasonable multi-tens-of-GiB
    /// corpus. Reuses [`ThrottledReader`]'s exact "background task feeds a
    /// duplex pipe at a controlled pace" shape, just forwarding real bytes
    /// read from `inner` instead of synthesizing them. `open_write` is
    /// left unthrottled (delegates straight to `inner`): pacing the read
    /// side alone is sufficient to bound overall copy throughput, and
    /// throttling both would just double-count the same slowdown.
    struct PacedFs {
        inner: LocalFs,
        bytes_per_sec: u64,
    }

    #[async_trait::async_trait]
    impl FileSystem for PacedFs {
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
            let mut real = self.inner.open_read(p).await?;
            const CHUNK: usize = 256 * 1024;
            let (mut tx, rx) = tokio::io::duplex(CHUNK);
            let bytes_per_sec = self.bytes_per_sec;
            tokio::spawn(async move {
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let n = match real.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    if tx.write_all(&buf[..n]).await.is_err() {
                        break; // reader side dropped (e.g. the step was aborted)
                    }
                    tokio::time::sleep(Duration::from_secs_f64(n as f64 / bytes_per_sec as f64))
                        .await;
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
        async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
            self.inner.link(source, dest).await
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
            _on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    /// A `FileSystem` test double wrapping a real [`LocalFs`], whose
    /// `open_read` fails with a configurable [`ErrorKind`] a configurable
    /// number of times before delegating for real -- T-5.1.10's injection
    /// point for "transient errors retry and succeed" and "`ENOSPC` pauses
    /// the job." `server_side_copy` always reports `Unsupported`, forcing
    /// every copy through [`naive_copy`] (whose first call is
    /// `open_read`), so failures are deterministic regardless of how a
    /// real backend would actually accelerate a same-filesystem copy.
    struct FlakyFs {
        inner: LocalFs,
        /// Remaining number of `open_read` calls that should fail before
        /// succeeding for real. An `Arc` so a test can reset it after the
        /// fact (simulating "the operator freed disk space") without
        /// tearing down the running job.
        remaining_failures: Arc<std::sync::atomic::AtomicU32>,
        fail_kind: ErrorKind,
        open_read_calls: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait::async_trait]
    impl FileSystem for FlakyFs {
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
            self.open_read_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = self.remaining_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                // Only actually decrement while still positive -- a test
                // resetting `remaining_failures` to 0 concurrently (to
                // simulate space being freed) must not race this into
                // underflow.
                let _ = self.remaining_failures.compare_exchange(
                    remaining,
                    remaining - 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                return Err(Box::new(
                    VfsError::new(self.fail_kind, "injected failure").with_path(p.clone()),
                ));
            }
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
        async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
            self.inner.link(source, dest).await
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
            _on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
        ) -> Result<duet_vfs::CopyOutcome> {
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    #[tokio::test]
    async fn a_transient_error_retries_with_backoff_and_succeeds() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

        let open_read_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fs: Arc<dyn FileSystem> = Arc::new(FlakyFs {
            inner: LocalFs,
            remaining_failures: Arc::new(std::sync::atomic::AtomicU32::new(2)),
            fail_kind: ErrorKind::Retryable,
            open_read_calls: Arc::clone(&open_read_calls),
        });

        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&src.path().join("a.txt")),
                dest: vpath_for(&dst.path().join("a.txt")),
                size: 5,
                conflict: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, 1);
        assert_eq!(
            open_read_calls.load(Ordering::SeqCst),
            3,
            "2 failures + 1 real attempt"
        );
        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_transient_error_gives_up_after_the_retry_bound_and_fails() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

        let open_read_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fs: Arc<dyn FileSystem> = Arc::new(FlakyFs {
            inner: LocalFs,
            // Never stops failing -- more than any bounded retry budget.
            remaining_failures: Arc::new(std::sync::atomic::AtomicU32::new(1_000)),
            fail_kind: ErrorKind::Retryable,
            open_read_calls: Arc::clone(&open_read_calls),
        });

        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&src.path().join("a.txt")),
                dest: vpath_for(&dst.path().join("a.txt")),
                size: 5,
                conflict: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].kind, ErrorKind::Retryable);
        assert_eq!(
            open_read_calls.load(Ordering::SeqCst),
            RETRYABLE_MAX_ATTEMPTS + 1,
            "the initial attempt plus every bounded retry, then give up"
        );
        assert!(!dst.path().join("a.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enospc_pauses_the_job_and_completes_once_space_is_freed_and_resumed() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

        let remaining_failures = Arc::new(std::sync::atomic::AtomicU32::new(1_000_000));
        let fs: Arc<dyn FileSystem> = Arc::new(FlakyFs {
            inner: LocalFs,
            remaining_failures: Arc::clone(&remaining_failures),
            fail_kind: ErrorKind::Space,
            open_read_calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });

        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&src.path().join("a.txt")),
                dest: vpath_for(&dst.path().join("a.txt")),
                size: 5,
                conflict: None,
            }],
            PlanOptions::default(),
        );

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let handle = tokio::spawn(execute(
            fs,
            JobIdT(1),
            plan,
            journal,
            1,
            tx,
            control.clone(),
            None,
        ));

        // Wait for the job to actually pause itself (no fixed sleep --
        // poll the real control state, matching this file's own
        // established "don't guess a timing budget" precedent).
        let deadline = Instant::now() + Duration::from_secs(5);
        while control.state() != ControlState::Paused {
            assert!(Instant::now() < deadline, "job never paused for space");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Simulate the operator freeing space, then resume.
        remaining_failures.store(0, Ordering::SeqCst);
        control.resume();

        let report = handle.await.unwrap();
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, 1);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, JobEvent::QueuePausedForSpace { .. })),
            "a clear, distinguishable pause-for-space event must be surfaced"
        );
        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_permission_failure_is_not_retried_and_hints_at_elevation() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

        let open_read_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fs: Arc<dyn FileSystem> = Arc::new(FlakyFs {
            inner: LocalFs,
            remaining_failures: Arc::new(std::sync::atomic::AtomicU32::new(1_000)),
            fail_kind: ErrorKind::Permission,
            open_read_calls: Arc::clone(&open_read_calls),
        });

        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&src.path().join("a.txt")),
                dest: vpath_for(&dst.path().join("a.txt")),
                size: 5,
                conflict: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].kind, ErrorKind::Permission);
        assert!(
            report.errors[0].message.contains("T-9.1.13"),
            "message should hint that elevation isn't available yet: {:?}",
            report.errors[0].message
        );
        assert_eq!(
            open_read_calls.load(Ordering::SeqCst),
            1,
            "a Permission failure must not be retried at all"
        );
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
            1,
            "CreateDir must succeed silently (merging into an already-existing directory is \
             normal, not a conflict -- T-5.1.9's own fix); only CopyFile (file already exists) \
             should skip"
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
            None,
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
            None,
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

    /// The core safety property T-5.1.5's `depends_on` field exists for,
    /// exercised directly against a hand-built plan rather than through
    /// `plan_move` -- a `Remove` step must never run if the step it
    /// depends on failed, or a move could delete the only copy of a file
    /// whose destination write never actually succeeded.
    #[tokio::test]
    async fn remove_step_with_a_failed_dependency_is_skipped_not_executed() {
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let survivor = dst.path().join("must_survive.txt");
        std::fs::write(&survivor, b"do not delete me").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        // Step 0: a CopyFile that is certain to fail (source doesn't
        // exist) -- standing in for "the move's copy failed."
        // Step 1: a Remove of an unrelated, real file, wired to depend on
        // step 0 -- if the gating mechanism didn't exist, this would run
        // unconditionally and delete `survivor`.
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&dst.path().join("does-not-exist.txt")),
                    dest: vpath_for(&dst.path().join("dest-that-never-happens.txt")),
                    size: 5,
                    conflict: None,
                },
                Step::Remove {
                    target: vpath_for(&survivor),
                    mode: RemoveMode::File,
                    depends_on: Some(0),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(
            report.errors.len(),
            1,
            "the CopyFile step must genuinely fail"
        );
        assert_eq!(
            report.skipped.len(),
            1,
            "the Remove step must be skipped, not attempted"
        );
        assert!(
            survivor.exists(),
            "the dependency-gated Remove must never have run -- the file must still exist"
        );
    }

    /// The companion case: a `Remove` whose dependency step genuinely
    /// succeeded must proceed normally.
    #[tokio::test]
    async fn remove_step_with_a_succeeded_dependency_proceeds() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        let source_path = src.path().join("a.txt");
        let dest_path = dst.path().join("a.txt");

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&source_path),
                    dest: vpath_for(&dest_path),
                    size: 5,
                    conflict: None,
                },
                Step::Remove {
                    target: vpath_for(&source_path),
                    mode: RemoveMode::File,
                    depends_on: Some(0),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert!(
            !source_path.exists(),
            "the source must be removed after a successful copy"
        );
        assert!(dest_path.exists());
    }

    #[tokio::test]
    async fn verify_size_only_succeeds_when_sizes_match_and_fails_when_they_dont() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"12345").unwrap();
        std::fs::write(dir.path().join("b_same.txt"), b"67890").unwrap();
        std::fs::write(dir.path().join("c_different.txt"), b"1").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![
                Step::Verify {
                    source: vpath_for(&dir.path().join("a.txt")),
                    dest: vpath_for(&dir.path().join("b_same.txt")),
                    algorithm: VerifyAlgorithm::SizeOnly,
                    depends_on: None,
                },
                Step::Verify {
                    source: vpath_for(&dir.path().join("a.txt")),
                    dest: vpath_for(&dir.path().join("c_different.txt")),
                    algorithm: VerifyAlgorithm::SizeOnly,
                    depends_on: None,
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(
            report.errors.len(),
            1,
            "only the size-mismatched pair should fail"
        );
        assert!(report.errors[0].message.contains("size mismatch"));
    }

    // ---- T-5.1.12: BLAKE3 post-copy verification -------------------------

    #[tokio::test]
    async fn verify_blake3_succeeds_on_identical_content() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"identical payload").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"identical payload").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::Verify {
                source: vpath_for(&dir.path().join("a.txt")),
                dest: vpath_for(&dir.path().join("b.txt")),
                algorithm: VerifyAlgorithm::Blake3,
                depends_on: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    /// T-5.1.12's own AC, verbatim: "verification detects a deliberately
    /// corrupted destination." Same size as the source (so `SizeOnly`
    /// would have missed this entirely) but one flipped byte.
    #[tokio::test]
    async fn verify_blake3_detects_a_deliberately_corrupted_destination() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"correct payload!").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"corrupt payload!").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::Verify {
                source: vpath_for(&dir.path().join("a.txt")),
                dest: vpath_for(&dir.path().join("b.txt")),
                algorithm: VerifyAlgorithm::Blake3,
                depends_on: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("content mismatch"));
    }

    #[tokio::test]
    async fn copying_with_verify_enabled_runs_blake3_and_still_completes() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello, verified world").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions {
                verify: true,
                ..PlanOptions::default()
            },
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            plan.steps
                .iter()
                .any(|s| matches!(s.kind(), StepKind::Verify)),
            "plan_copy must emit a Verify step when options.verify is set"
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("a.txt")).unwrap(),
            "hello, verified world"
        );
    }

    /// Proves [`hash_file`]'s own cancellation check (not just the outer
    /// step-retry loop's) is real: a `ThrottledFs`-backed verify that would
    /// otherwise take seconds to stream must stop within a bounded time
    /// once cancelled, rather than reading the throttled stream to
    /// completion regardless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_blake3_honors_cancellation_and_does_not_hang() {
        let state = TempDir::new().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(ThrottledFs {
            inner: LocalFs,
            total_bytes: 64 * 1024 * 1024,
            chunk_bytes: 64 * 1024,
            delay_per_chunk: Duration::from_millis(50),
        });
        // ThrottledFs's own `open_read` ignores the path entirely and
        // always hands back the same synthetic throttled stream -- real
        // paths aren't needed to prove cancellation is honored.
        let plan = Plan::new(
            vec![Step::Verify {
                source: vpath_for(Path::new("/does-not-matter-a")),
                dest: vpath_for(Path::new("/does-not-matter-b")),
                algorithm: VerifyAlgorithm::Blake3,
                depends_on: None,
            }],
            PlanOptions::default(),
        );

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let control_for_task = control.clone();
        let handle = tokio::spawn(execute(
            fs,
            JobIdT(1),
            plan,
            journal,
            1,
            tx,
            control_for_task,
            None,
        ));

        // The full throttled stream takes ~1024 chunks * 50ms = ~51s to
        // read; let a little real progress happen, then cancel.
        tokio::time::sleep(Duration::from_millis(100)).await;
        control.cancel();

        let report = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("job never responded to cancellation")
            .expect("executor task panicked");
        assert!(
            report.errors.is_empty(),
            "a cancelled job ends Cancelled, not Failed: {:?}",
            report.errors
        );
    }

    /// T-5.1.12's own AC: "throughput cost measured and documented." Gated
    /// like `duet-vfs`'s own `DUET_BENCH_LARGE_COPY`
    /// (`crates/duet-vfs/src/local/probe.rs`) -- real disk I/O on a
    /// multi-hundred-MiB file, not something to run on every `cargo test`.
    /// Copies the same source twice (once with `verify: false`, once with
    /// `verify: true`) and prints the wall-clock overhead BLAKE3 hashing
    /// both sides adds on top of the copy itself.
    #[tokio::test]
    async fn verify_throughput_overhead_is_measured_and_documented() {
        if std::env::var("DUET_BENCH_VERIFY_LARGE").as_deref() != Ok("1") {
            eprintln!(
                "verify_throughput_overhead_is_measured_and_documented: skipped by default \
                 (real disk I/O) -- set DUET_BENCH_VERIFY_LARGE=1 to run (optionally \
                 DUET_BENCH_VERIFY_LARGE_MIB=512 to change the file size)"
            );
            return;
        }
        let mib: u64 = std::env::var("DUET_BENCH_VERIFY_LARGE_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512);
        let size = mib * 1024 * 1024;

        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("bench.src");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&src_path).unwrap();
            let chunk = vec![7u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < size {
                let n = (chunk.len() as u64).min(size - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }

        async fn run_copy(src: &Path, verify: bool) -> Duration {
            let dst_dir = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let cancel = crate::planner::CancelToken::new();
            let plan = crate::planner::plan_copy(
                &*fs,
                &[vpath_for(src)],
                &vpath_for(dst_dir.path()),
                PlanOptions {
                    verify,
                    ..PlanOptions::default()
                },
                &cancel,
            )
            .await
            .unwrap();

            let start = std::time::Instant::now();
            let (report, _events) = run(fs, plan, state.path(), 4).await;
            let elapsed = start.elapsed();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            elapsed
        }

        let without_verify = run_copy(&src_path, false).await;
        let with_verify = run_copy(&src_path, true).await;
        let overhead_pct =
            (with_verify.as_secs_f64() / without_verify.as_secs_f64().max(f64::EPSILON) - 1.0)
                * 100.0;
        eprintln!(
            "verify_throughput_overhead_is_measured_and_documented: {mib} MiB -- \
             copy without verify={without_verify:?}, copy with verify={with_verify:?} \
             (BLAKE3 verification adds {overhead_pct:.1}% wall-clock overhead)"
        );
    }

    // ---- T-5.1.11: progress and ETA --------------------------------------

    #[test]
    fn eta_is_none_before_any_regime_has_a_rate() {
        let mut eta = EtaEstimator::new(10, 1_000_000);
        assert_eq!(eta.sample(0, 0), None);
    }

    /// Outstanding work in *both* regimes, but only one has produced a rate
    /// yet -- the combined estimate must stay `None` until both do (see the
    /// module doc comment's "Progress and ETA" section), not silently
    /// report a number that only accounts for half the job.
    #[test]
    fn eta_is_none_while_only_one_of_two_regimes_with_outstanding_work_has_a_rate() {
        let mut eta = EtaEstimator::new(1_000, 1_000_000);
        assert_eq!(eta.sample(0, 100_000), None, "large moved, small hasn't");
        assert!(
            eta.sample(50, 200_000).is_some(),
            "both regimes have now produced at least one real delta"
        );
    }

    /// A regime with no planned work at all (e.g. an all-small-files job,
    /// `total_large_bytes == 0`) must not block the estimate forever
    /// waiting for a rate that regime will never produce.
    #[test]
    fn eta_ignores_a_regime_with_no_outstanding_work() {
        let mut eta = EtaEstimator::new(1_000, 0);
        assert_eq!(eta.sample(0, 0), None);
        // 10 files in one 100ms tick -> 100 files/sec instantaneous rate;
        // 990 remaining / 100/s = 9.9s, rounds to 10. The empty large
        // regime contributes a fixed `Some(0.0)` and never blocks this.
        assert_eq!(eta.sample(10, 0), Some(10));
    }

    /// Constant, non-adversarial per-tick progress in both regimes -- the
    /// EWMA reaches its fixed point immediately (a constant instantaneous
    /// rate blended with itself via `alpha` is still that same rate), so
    /// the reported ETA should land very close to the true remaining time
    /// computed directly from the (known, constant) rates. This is the
    /// arithmetic half of T-5.1.11's own AC ("within 20% after 10s") --
    /// [`verify_throughput_overhead_is_measured_and_documented`]'s sibling
    /// `DUET_BENCH_ETA_ACCURACY`-gated test below covers the real-I/O,
    /// real-wall-clock half.
    #[test]
    fn eta_lands_close_to_the_true_remaining_time_for_steady_mixed_rates() {
        // Small regime: 20 files/tick (200 files/sec). Large regime: 2 MiB/
        // tick (20 MiB/sec) -- both well above the small-file threshold's
        // own 1 MiB, so this is a genuine mixed-regime scenario.
        let total_small_files = 100_000u64;
        let total_large_bytes = 2_000_000_000u64; // ~1.9 GiB
        let mut eta = EtaEstimator::new(total_small_files, total_large_bytes);

        let mut small_done = 0u64;
        let mut large_done = 0u64;
        let mut last = None;
        // 100 ticks = 10s, matching the AC's own "after the first 10s".
        for _ in 0..100 {
            small_done += 20;
            large_done += 2_000_000;
            last = eta.sample(small_done, large_done);
        }

        let small_rate = 200.0; // files/sec
        let large_rate = 20_000_000.0; // bytes/sec
        let true_small_eta = (total_small_files - small_done) as f64 / small_rate;
        let true_large_eta = (total_large_bytes - large_done) as f64 / large_rate;
        let true_eta = true_small_eta.max(true_large_eta);

        let reported = last.expect("10s of steady progress in both regimes must have a rate");
        let error = (reported as f64 - true_eta).abs() / true_eta;
        assert!(
            error <= 0.20,
            "reported {reported}s, true remaining {true_eta:.1}s -- {:.1}% off, AC allows 20%",
            error * 100.0
        );
    }

    /// T-5.1.11's own AC, verbatim: "never runs backwards for more than one
    /// sample." Establishes a fast rate, then feeds several consecutively
    /// *slower* (but still nonzero -- a real slowdown, not a barrier-step
    /// pause) ticks in a row, which pulls the EWMA down and would otherwise
    /// make the raw ETA climb every single tick. Across the reported
    /// sequence, no value may be strictly greater than the one immediately
    /// before it more than once in a row.
    #[test]
    fn eta_never_increases_for_more_than_one_consecutive_sample() {
        let mut eta = EtaEstimator::new(0, 1_000_000_000);
        let mut large_done = 0u64;
        let mut reported = Vec::new();

        // Warm up at a fast, steady rate (10 MiB/tick = 100 MiB/sec).
        for _ in 0..10 {
            large_done += 10_000_000;
            reported.push(eta.sample(0, large_done));
        }
        // Then a real, sustained slowdown: 1 MiB/tick for many ticks.
        for _ in 0..30 {
            large_done += 1_000_000;
            reported.push(eta.sample(0, large_done));
        }

        let values: Vec<u64> = reported.into_iter().flatten().collect();
        let mut consecutive_increases = 0u32;
        for pair in values.windows(2) {
            if pair[1] > pair[0] {
                consecutive_increases += 1;
                assert!(
                    consecutive_increases <= 1,
                    "eta increased for two samples in a row: {values:?}"
                );
            } else {
                consecutive_increases = 0;
            }
        }
        // The slowdown must still be visible somewhere (the clamp
        // suppresses a *second* consecutive climb, not every climb).
        assert!(
            values.windows(2).any(|p| p[1] > p[0]),
            "a genuine sustained slowdown never showed up as a single honest jump: {values:?}"
        );
    }

    /// End-to-end sanity through the real `execute()`/sampler pipeline (no
    /// throttling, no env-gate): a small mixed corpus is enough to prove
    /// the wiring is real -- `eta_secs` starts `None` and becomes `Some`
    /// once both regimes have moved. The literal "within 20% after 10s"
    /// number is [`eta_accuracy_is_within_20_percent_after_ten_seconds`]'s
    /// job, gated behind real wall-clock time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copying_a_mixed_corpus_reports_a_real_eta_before_finishing() {
        let src = TempDir::new().unwrap();
        for i in 0..80 {
            std::fs::write(src.path().join(format!("small-{i}.txt")), b"tiny").unwrap();
        }
        // 16 MiB, comfortably above SMALL_FILE_REGIME_THRESHOLD_BYTES (1 MiB).
        std::fs::write(src.path().join("large.bin"), vec![7u8; 16 * 1024 * 1024]).unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        // `delay`: an artificial per-step floor so this genuinely spans
        // several 100ms sampler ticks even on tmpfs, where the real I/O
        // for a corpus this size would otherwise finish inside a single
        // tick and never exercise the estimator at all.
        let fs: Arc<dyn FileSystem> = Arc::new(TestFs {
            inner: LocalFs,
            force_unsupported: true, // force naive_copy's incremental path
            delay: Duration::from_millis(5),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
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

        let (report, events) = run(fs, plan, state.path(), 2).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let etas: Vec<Option<u64>> = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { snapshot, .. } => Some(snapshot.eta_secs),
                _ => None,
            })
            .collect();
        assert!(
            etas.iter().any(|e| e.is_some()),
            "a job with real mixed progress must report a real ETA at some point: {etas:?}"
        );
    }

    /// T-5.1.11's own AC, verbatim, at real scale: "ETA on a mixed corpus
    /// is within 20% after the first 10 s." Needs genuine, sustained
    /// progress spanning at least 10 real seconds -- gated like this
    /// crate's existing `DUET_BENCH_VERIFY_LARGE`/`duet-vfs`'s own
    /// `DUET_BENCH_LARGE_COPY` precedent, not run by default. Uses
    /// [`PacedFs`] rather than raw tmpfs throughput: tmpfs copies several
    /// GiB in a couple of real seconds (confirmed empirically while
    /// building this test), so reaching a genuine 10+ second run on raw
    /// disk speed alone would need an unreasonably large corpus (tens of
    /// GiB) and would vary wildly by machine; a fixed, paced read rate
    /// keeps the corpus modest (under 1 GiB) and the measured duration
    /// portable across environments.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eta_accuracy_is_within_20_percent_after_ten_seconds() {
        if std::env::var("DUET_BENCH_ETA_ACCURACY").as_deref() != Ok("1") {
            eprintln!(
                "eta_accuracy_is_within_20_percent_after_ten_seconds: skipped by default \
                 (runs for ~15s) -- set DUET_BENCH_ETA_ACCURACY=1 to run (optionally \
                 DUET_BENCH_ETA_ACCURACY_MIB=750 to change the large-file total)"
            );
            return;
        }
        let mib: u64 = std::env::var("DUET_BENCH_ETA_ACCURACY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(750);
        // 50 MiB/s paced read rate -- chosen so the default 750 MiB corpus
        // runs for ~15s (comfortably past the AC's 10s mark with margin
        // for the small-file batch and setup), without either an
        // unreasonably large corpus or an unreasonably long test.
        const PACED_BYTES_PER_SEC: u64 = 50 * 1024 * 1024;

        let src = TempDir::new().unwrap();
        for i in 0..500 {
            std::fs::write(src.path().join(format!("small-{i}.txt")), vec![1u8; 2048]).unwrap();
        }
        let large_dir = src.path().join("large");
        std::fs::create_dir(&large_dir).unwrap();
        let per_file_mib = mib / 4;
        for i in 0..4 {
            let path = large_dir.join(format!("bench-{i}.bin"));
            let mut f = std::fs::File::create(&path).unwrap();
            use std::io::Write;
            let chunk = vec![9u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            let target = per_file_mib * 1024 * 1024;
            while written < target {
                let n = (chunk.len() as u64).min(target - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(PacedFs {
            inner: LocalFs,
            bytes_per_sec: PACED_BYTES_PER_SEC,
        });
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

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let start = std::time::Instant::now();
        // `concurrency: 1` -- `PacedFs` throttles each independently
        // opened read stream to `PACED_BYTES_PER_SEC` on its own; running
        // several large-file copies concurrently would multiply the
        // effective aggregate throughput by however many are in flight at
        // once, making the paced rate meaningless as a bound. One stream
        // at a time keeps the sustained rate exactly `PACED_BYTES_PER_SEC`.
        let handle = tokio::spawn(execute(fs, JobIdT(1), plan, journal, 1, tx, control, None));

        // Find the Progress sample closest to (but not before) t=10s.
        let mut predicted_at_10s: Option<u64> = None;
        let mut ten_second_mark: Option<Duration> = None;
        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Progress { snapshot, .. }
                    if start.elapsed() >= Duration::from_secs(10) =>
                {
                    if predicted_at_10s.is_none() {
                        predicted_at_10s = snapshot.eta_secs;
                        ten_second_mark = Some(start.elapsed());
                    }
                }
                JobEvent::Finished { .. } => break,
                _ => {}
            }
        }
        let report = handle.await.expect("executor task panicked");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let total_elapsed = start.elapsed();

        let predicted = predicted_at_10s
            .expect("a paced multi-hundred-MiB mixed copy still running past 10s must have a real ETA by then");
        let sample_time = ten_second_mark.unwrap();
        let actual_remaining = (total_elapsed.as_secs_f64() - sample_time.as_secs_f64()).max(0.0);
        let error = (predicted as f64 - actual_remaining).abs() / actual_remaining.max(1.0);
        eprintln!(
            "eta_accuracy_is_within_20_percent_after_ten_seconds: {mib} MiB large + 500 small \
             files, paced at {} MiB/s -- sampled at {sample_time:?}, predicted {predicted}s \
             remaining, actual {actual_remaining:.1}s remaining ({:.1}% off)",
            PACED_BYTES_PER_SEC / (1024 * 1024),
            error * 100.0
        );
        assert!(
            error <= 0.20,
            "predicted {predicted}s, actual {actual_remaining:.1}s remaining -- {:.1}% off, \
             T-5.1.11's own AC allows 20%",
            error * 100.0
        );
    }

    /// The bug this whole fix targets, reproduced end to end through
    /// `copy_file_step` and fixed: an accelerated (same-device) copy of a
    /// large file must credit `ctx.counters.bytes_done`/`large_bytes_done`
    /// incrementally as real `copy_file_range` chunks land, not in one
    /// lump once `server_side_copy` returns -- and a pause/resume episode
    /// mid-copy (which discards the interrupted attempt and retries the
    /// whole step from scratch, per this module's own "resume restarts
    /// the step" design) must not leave those counters double- or
    /// under-counted once the retried attempt finishes.
    ///
    /// Deliberately uses a real `LocalFs` on a real tempdir, not `TestFs`/
    /// `ThrottledFs`/`PacedFs` -- all three of those force
    /// `CopyOutcome::Unsupported`, which would only ever exercise
    /// `naive_copy` (already known-correct before this fix) and prove
    /// nothing about the accelerated path this bug actually lives in.
    /// 900 MiB is large enough to span several real `copy_file_range`
    /// chunks (64 MiB each) and several 100ms sampler ticks at this
    /// development machine's measured tmpfs `copy_file_range` throughput
    /// (~2 GiB/s, confirmed via a plain `cp` timing while writing this
    /// test) -- scaled down from, but the same shape as, the user's own
    /// reported ~2 GiB-per-file, 4-file, same-device job, so the test
    /// finishes in well under a second of real copying instead of tens of
    /// seconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accelerated_copy_reports_incremental_progress_and_survives_pause_resume() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        const SIZE: u64 = 900 * 1024 * 1024;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(src.path().join("big.bin")).unwrap();
            let chunk = vec![42u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < SIZE {
                let n = (chunk.len() as u64).min(SIZE - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }

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

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let control_for_task = control.clone();
        let handle = tokio::spawn(execute(
            Arc::clone(&fs),
            JobIdT(1),
            plan,
            journal,
            1,
            tx,
            control_for_task,
            None,
        ));

        // Collect every observed `bytes_done` sample until the job
        // finishes, pausing (then resuming) as soon as the first genuinely
        // partial sample lands -- exercising the interrupted-attempt
        // rollback path, not just the happy path's incremental crediting.
        let mut samples: Vec<u64> = Vec::new();
        let mut paused_once = false;
        loop {
            let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await
            else {
                panic!("no event within 10s -- job likely stalled: samples so far {samples:?}");
            };
            match event {
                JobEvent::Progress { snapshot, .. } => {
                    samples.push(snapshot.bytes_done);
                    if !paused_once && snapshot.bytes_done > 0 && snapshot.bytes_done < SIZE {
                        paused_once = true;
                        control.pause();
                        // The copy_file_range loop only checks on_progress
                        // once per up-to-64-MiB chunk -- give it a moment
                        // to actually land mid-copy before resuming.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        control.resume();
                    }
                }
                JobEvent::Finished { .. } => break,
                _ => {}
            }
        }
        let report = handle.await.expect("executor task panicked");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, 1);
        assert!(
            paused_once,
            "test setup bug: never observed a partial sample to pause on -- samples: {samples:?}"
        );

        eprintln!(
            "accelerated_copy_reports_incremental_progress_and_survives_pause_resume: {} \
             bytes_done samples observed: {samples:?}",
            samples.len()
        );

        let partial_mid_copy = samples.iter().filter(|&&b| b > 0 && b < SIZE).count();
        assert!(
            partial_mid_copy >= 1,
            "expected at least one Progress sample with 0 < bytes_done < {SIZE} -- observed \
             only {samples:?}, which looks like the old one-big-jump-at-completion bug this \
             fix targets"
        );

        // No observed sample may ever exceed the file's real size -- an
        // overshoot would be direct evidence of double-counting (the
        // interrupted attempt's credit surviving into the retried
        // attempt's credit instead of being rolled back first). Note this
        // deliberately does *not* assert the *last* observed sample equals
        // `SIZE` exactly: `execute()` aborts the 100ms progress sampler
        // (`sampler.abort()`) immediately once the step loop finishes and
        // *before* sending `JobEvent::Finished`, with no guaranteed final
        // "100% done" tick in between -- so the last sample this test
        // happens to observe is racing task teardown, not a meaningful
        // signal. `copy_file_step_counters_survive_a_pause_resume_retry_
        // without_double_or_under_counting` below checks the exact final
        // counter value instead, by reading it directly (no sampler, no
        // race) right after the step's own task handle is joined.
        let max_seen = samples.iter().copied().max().unwrap_or(0);
        assert!(
            max_seen <= SIZE,
            "observed bytes_done={max_seen} exceeding the file's real size ({SIZE}) -- direct \
             evidence of double-counting across the pause/resume episode. Samples: {samples:?}"
        );
        // Also expect to see a genuine *decrease* somewhere in the
        // sequence -- direct evidence the rollback this fix adds actually
        // fired (the interrupted attempt's partial credit being
        // subtracted back out) rather than just happening to never
        // trigger in this run.
        let saw_a_decrease = samples.windows(2).any(|w| w[1] < w[0]);
        assert!(
            saw_a_decrease,
            "expected to observe bytes_done decrease at least once (the pause-triggered \
             rollback subtracting the interrupted attempt's partial credit back out) -- \
             samples: {samples:?}"
        );
        // `plan_copy` copies the *source directory itself* into `dst`
        // (matching `copies_a_directory_tree_and_completes_with_no_skips_
        // or_errors`'s own established precedent for this), so the result
        // lands at `dst/<src's own dir name>/big.bin`, not `dst/big.bin`.
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        let copied = std::fs::metadata(dst.path().join(src_name).join("big.bin"))
            .unwrap()
            .len();
        assert_eq!(
            copied, SIZE,
            "the destination file itself must be the full, correct size"
        );
    }

    /// The precise, race-free counterpart to the test above: instead of
    /// inferring correctness from the 100ms `JobEvent::Progress` sampler
    /// (which, per that test's own doc comment, is aborted before any
    /// guaranteed "100% done" tick is sent), this test constructs an
    /// [`ExecutorContext`] directly and calls [`run_step_with_retry`]
    /// itself, reading `ctx.counters` straight from the `Arc` both while
    /// the step is in flight (to decide when to pause) and immediately
    /// after the task handle is joined (to check the exact final tally) --
    /// no sampler, no 100ms granularity, no shutdown race.
    ///
    /// This is the test that most directly proves the "resume restarts the
    /// step" invariant this codebase already relies on for `naive_copy`
    /// (see that function's own doc comment) now holds for the
    /// accelerated path too: a pause landing mid-`copy_file_range`
    /// produces `CopyOutcome::Interrupted`, whose partial credit this fix
    /// rolls back (see `copy_file_step`'s `Interrupted` arm), and the
    /// retried attempt starts crediting from zero again -- so the final
    /// tally must be exactly the file's real size, never more (double-
    /// counted) or less (under-counted).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_file_step_counters_survive_a_pause_resume_retry_without_double_or_under_counting()
    {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        const SIZE: u64 = 300 * 1024 * 1024; // several copy_file_range chunks
        {
            use std::io::Write;
            let mut f = std::fs::File::create(src.path().join("big.bin")).unwrap();
            let chunk = vec![7u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < SIZE {
                let n = (chunk.len() as u64).min(SIZE - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let journal = JournalHandle::spawn(Journal::open(JobIdT(1), state.path()).unwrap());
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let counters = Arc::new(ProgressCounters::default());
        let ctx = ExecutorContext {
            fs,
            job_id: JobIdT(1),
            journal,
            control: control.clone(),
            counters: Arc::clone(&counters),
            events: events_tx,
            outcomes: Arc::new(Mutex::new(HashMap::new())),
            default_conflict: ConflictPolicy::Skip,
            resolver: None,
            sticky_conflict: Arc::new(Mutex::new(None)),
        };

        let step = Step::CopyFile {
            source: vpath_for(&src.path().join("big.bin")),
            dest: vpath_for(&dst.path().join("big.bin")),
            size: SIZE,
            conflict: None,
        };

        let ctx_task = ctx.clone();
        let handle = tokio::spawn(async move { run_step_with_retry(&ctx_task, 0, &step).await });

        // Poll the raw counter directly until real, partial (neither zero
        // nor complete) progress is observed, then pause -- exercising
        // this fix's Interrupted -> rollback -> retry-from-scratch path,
        // not just the happy path's incremental crediting.
        // This loop only ever exits one of two ways: it breaks having just
        // observed (and paused on) real partial progress, or the deadline
        // assertion below panics the test first -- so reaching the code
        // after it is itself the proof that partial progress was seen;
        // no separate "did we ever see it" flag is needed.
        let mut max_seen = 0u64;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let b = counters.bytes_done.load(Ordering::Relaxed);
            max_seen = max_seen.max(b);
            assert!(
                b <= SIZE,
                "bytes_done ({b}) observed exceeding the file's real size ({SIZE}) while still \
                 in flight -- direct evidence of double-counting"
            );
            if b > 0 && b < SIZE {
                // Pause immediately and stop polling here -- once paused,
                // the step genuinely cannot finish until `resume()` is
                // called below, so this loop must not keep waiting on
                // `handle.is_finished()` (it never will, until resumed).
                control.pause();
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never observed partial progress within 10s -- max bytes_done seen: {max_seen}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Give `wait_out_pause`'s 20ms poll a moment to actually observe
        // the paused state (and the interrupted attempt's rollback to
        // complete) before resuming.
        tokio::time::sleep(Duration::from_millis(50)).await;
        control.resume();

        let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "run_step_with_retry never finished within 10s of resuming -- max \
                     bytes_done seen before pause: {max_seen}"
                )
            })
            .expect("run_step_with_retry task panicked");
        match outcome {
            StepRun::Done(StepOutcome::Succeeded) => {}
            StepRun::Done(other) => panic!("expected the step to succeed, got {other:?}"),
            StepRun::Cancelled => panic!("expected the step to succeed, got Cancelled"),
        }

        // No sampler, no shutdown race: this is the exact, final counter
        // state, read right after the task that mutated it was joined.
        let final_bytes = counters.bytes_done.load(Ordering::Relaxed);
        let final_large_bytes = counters.large_bytes_done.load(Ordering::Relaxed);
        eprintln!(
            "copy_file_step_counters_survive_a_pause_resume_retry_without_double_or_under_\
             counting: final bytes_done={final_bytes} large_bytes_done={final_large_bytes} \
             (file size {SIZE})"
        );
        assert_eq!(
            final_bytes, SIZE,
            "bytes_done after a pause/resume retry must equal the file's real size exactly -- \
             {final_bytes} vs {SIZE} means the rollback either dropped or double-counted bytes"
        );
        assert_eq!(
            final_large_bytes, SIZE,
            "large_bytes_done must also equal the file's real size exactly -- this file is \
             comfortably above the small-file regime threshold"
        );

        let copied = std::fs::metadata(dst.path().join("big.bin")).unwrap().len();
        assert_eq!(
            copied, SIZE,
            "the destination file itself must be the full, correct size"
        );
    }

    /// The literal symptom from the bug report this whole fix addresses:
    /// copying several large same-device files showed `0 B/s` throughput
    /// (and a bogus `ETA 0:00`) for the entire duration of a still-in-
    /// progress copy, because the old accelerated path only ever credited
    /// a whole file's bytes in one jump at completion -- so
    /// `throughput_bytes_per_sec` (computed from the *delta* between
    /// consecutive 100ms samples) read `0` on every sample except the one
    /// instant a file's bytes landed, then `0` again on every sample
    /// after. This test asserts the fixed behaviour directly: while a
    /// large file is genuinely still copying, more than one *consecutive*
    /// 100ms sample must report nonzero throughput -- not a single spike
    /// surrounded by zeros.
    ///
    /// Real `LocalFs` on a real tempdir, same reasoning as the test above
    /// for why `TestFs`/`ThrottledFs`/`PacedFs` would prove nothing here.
    /// Multiple ~400 MiB files (same shape as the user's own multi-file
    /// report, scaled down for test speed) copied with `concurrency: 2` --
    /// closer to how the UI actually drives a multi-file job than a
    /// single huge file would be.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_file_accelerated_copy_reports_sustained_nonzero_throughput() {
        let src = TempDir::new().unwrap();
        const FILES: usize = 4;
        const PER_FILE: u64 = 400 * 1024 * 1024;
        for i in 0..FILES {
            use std::io::Write;
            let mut f = std::fs::File::create(src.path().join(format!("f{i}.bin"))).unwrap();
            let chunk = vec![(i as u8).wrapping_add(1); 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < PER_FILE {
                let n = (chunk.len() as u64).min(PER_FILE - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let total_size = PER_FILE * FILES as u64;

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

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        let handle = tokio::spawn(execute(fs, JobIdT(1), plan, journal, 2, tx, control, None));

        let mut throughput_samples: Vec<(u64, u64)> = Vec::new(); // (bytes_done, throughput)
        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Progress { snapshot, .. } => {
                    throughput_samples
                        .push((snapshot.bytes_done, snapshot.throughput_bytes_per_sec));
                }
                JobEvent::Finished { .. } => break,
                _ => {}
            }
        }
        let report = handle.await.expect("executor task panicked");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, FILES as u64);

        eprintln!(
            "multi_file_accelerated_copy_reports_sustained_nonzero_throughput: {} samples: \
             {throughput_samples:?}",
            throughput_samples.len()
        );

        // Find the longest run of *consecutive* samples, all mid-copy
        // (bytes_done < total_size) and all reporting nonzero throughput.
        // The old bug could produce at most a run of length 1 (one lucky
        // sample landing right when a whole file's bytes jumped in) --
        // this asserts a real, sustained run instead.
        let mut best_run = 0usize;
        let mut current_run = 0usize;
        for &(bytes_done, throughput) in &throughput_samples {
            if bytes_done < total_size && throughput > 0 {
                current_run += 1;
                best_run = best_run.max(current_run);
            } else {
                current_run = 0;
            }
        }
        assert!(
            best_run >= 2,
            "expected at least 2 consecutive mid-copy samples with nonzero throughput -- \
             longest run observed was {best_run}. Samples: {throughput_samples:?} (this is \
             exactly the reported '0 B/s throughout, one spike at most' bug)"
        );
    }

    // ---- T-5.1.9: conflict-resolution engine ----------------------------

    /// A scripted [`ConflictResolver`] test double: returns pre-programmed
    /// answers in order, falling back to a fixed `Skip` once exhausted, and
    /// counts how many times it was actually consulted -- what the
    /// sticky-state tests below check to prove the resolver stops being
    /// called once an `AllRemaining` answer has been established.
    struct ScriptedResolver {
        answers: Mutex<VecDeque<ConflictResolution>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedResolver {
        fn new(answers: Vec<ConflictResolution>) -> Self {
            ScriptedResolver {
                answers: Mutex::new(answers.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ConflictResolver for ScriptedResolver {
        fn resolve(&self, _prompt: &ConflictPrompt) -> ConflictResolution {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ConflictResolution::once(ConflictPolicy::Skip))
        }
    }

    fn set_mtime_secs_ago(path: &Path, secs_ago: u64) {
        let mtime = SystemTime::now() - Duration::from_secs(secs_ago);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
    }

    #[tokio::test]
    async fn conflict_policy_overwrite_replaces_the_destination() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&source, b"new content").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&source),
                dest: vpath_for(&dest),
                size: 11,
                conflict: Some(ConflictPolicy::Overwrite),
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert_eq!(report.files_completed, 1);
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
    }

    #[tokio::test]
    async fn conflict_policy_overwrite_if_older_replaces_only_when_dest_is_older() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        // Case 1: destination is older than the source -- overwritten.
        let source1 = dir.path().join("s1.txt");
        let dest1 = dir.path().join("d1.txt");
        std::fs::write(&source1, b"new").unwrap();
        std::fs::write(&dest1, b"old").unwrap();
        set_mtime_secs_ago(&dest1, 3600);

        // Case 2: destination is newer than the source -- left alone.
        let source2 = dir.path().join("s2.txt");
        let dest2 = dir.path().join("d2.txt");
        std::fs::write(&source2, b"new").unwrap();
        set_mtime_secs_ago(&source2, 3600);
        std::fs::write(&dest2, b"old").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&source1),
                    dest: vpath_for(&dest1),
                    size: 3,
                    conflict: Some(ConflictPolicy::OverwriteIfOlder),
                },
                Step::CopyFile {
                    source: vpath_for(&source2),
                    dest: vpath_for(&dest2),
                    size: 3,
                    conflict: Some(ConflictPolicy::OverwriteIfOlder),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.files_completed, 1,
            "only the older destination should be overwritten"
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(std::fs::read(&dest1).unwrap(), b"new");
        assert_eq!(
            std::fs::read(&dest2).unwrap(),
            b"old",
            "a destination newer than the source must survive"
        );
    }

    #[tokio::test]
    async fn conflict_policy_overwrite_if_different_size_replaces_only_when_sizes_differ() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let source1 = dir.path().join("s1.txt");
        let dest1 = dir.path().join("d1.txt");
        std::fs::write(&source1, b"12345").unwrap();
        std::fs::write(&dest1, b"1").unwrap(); // different size

        let source2 = dir.path().join("s2.txt");
        let dest2 = dir.path().join("d2.txt");
        std::fs::write(&source2, b"12345").unwrap();
        std::fs::write(&dest2, b"67890").unwrap(); // same size

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&source1),
                    dest: vpath_for(&dest1),
                    size: 5,
                    conflict: Some(ConflictPolicy::OverwriteIfDifferentSize),
                },
                Step::CopyFile {
                    source: vpath_for(&source2),
                    dest: vpath_for(&dest2),
                    size: 5,
                    conflict: Some(ConflictPolicy::OverwriteIfDifferentSize),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.files_completed, 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(std::fs::read(&dest1).unwrap(), b"12345");
        assert_eq!(
            std::fs::read(&dest2).unwrap(),
            b"67890",
            "a same-size destination must survive"
        );
    }

    #[tokio::test]
    async fn conflict_policy_rename_target_uses_the_resolvers_alternate_destination() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        let alternate = dir.path().join("dest (renamed).txt");
        std::fs::write(&source, b"payload").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&source),
                dest: vpath_for(&dest),
                size: 7,
                conflict: None,
            }],
            PlanOptions::default(),
        );
        let resolver: Arc<dyn ConflictResolver> =
            Arc::new(ScriptedResolver::new(vec![ConflictResolution::rename_to(
                vpath_for(&alternate),
            )]));

        let (report, _events) = run_with_resolver(fs, plan, state.path(), 1, Some(resolver)).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"old",
            "the original, conflicting destination must be untouched"
        );
        assert_eq!(std::fs::read(&alternate).unwrap(), b"payload");
    }

    #[tokio::test]
    async fn conflict_policy_rename_target_without_an_alternate_fails_clearly() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&source, b"payload").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&source),
                dest: vpath_for(&dest),
                size: 7,
                conflict: Some(ConflictPolicy::RenameTarget),
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("RenameTarget"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }

    #[tokio::test]
    async fn conflict_policy_auto_rename_picks_the_first_free_numbered_name() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&source, b"payload").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        // Pre-occupy "dest (2).txt" too, so the engine has to skip past it.
        std::fs::write(dir.path().join("dest (2).txt"), b"taken").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CopyFile {
                source: vpath_for(&source),
                dest: vpath_for(&dest),
                size: 7,
                conflict: Some(ConflictPolicy::AutoRename),
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            std::fs::read(dir.path().join("dest (3).txt")).unwrap(),
            b"payload",
            "the first two candidate names were taken; must land on the third"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }

    #[tokio::test]
    async fn conflict_policy_abort_stops_the_job_as_cancelled_not_failed() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source1 = dir.path().join("s1.txt");
        let dest1 = dir.path().join("d1.txt");
        std::fs::write(&source1, b"x").unwrap();
        std::fs::write(&dest1, b"old").unwrap();
        let source2 = dir.path().join("s2.txt");
        let dest2 = dir.path().join("d2.txt"); // no conflict -- must never even run
        std::fs::write(&source2, b"y").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&source1),
                    dest: vpath_for(&dest1),
                    size: 1,
                    conflict: Some(ConflictPolicy::Abort),
                },
                Step::CopyFile {
                    source: vpath_for(&source2),
                    dest: vpath_for(&dest2),
                    size: 1,
                    conflict: None,
                },
            ],
            PlanOptions::default(),
        );

        let journal = Journal::open(JobIdT(1), state.path()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        // concurrency = 1 so the semaphore fully serialises the batch --
        // the second step cannot even start until the abort step (which
        // calls `ExecutionControl::cancel()` before returning) has
        // released its permit.
        let report = execute(fs, JobIdT(1), plan, journal, 1, tx, control, None).await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            std::fs::read(&dest1).unwrap(),
            b"old",
            "abort itself must not overwrite the conflicting destination"
        );
        assert!(
            !dest2.exists(),
            "the job must stop at the abort -- the second, non-conflicting CopyFile must never run"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            JobEvent::Finished {
                outcome: JobOutcome::Cancelled,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn create_dir_merges_silently_into_an_already_existing_directory() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let dest = dir.path().join("existing");
        std::fs::create_dir(&dest).unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CreateDir {
                dest: vpath_for(&dest),
                mode: None,
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report.skipped.is_empty(),
            "an already-existing directory is not a conflict"
        );
        assert!(dest.is_dir());
    }

    #[tokio::test]
    async fn create_dir_with_a_file_occupying_the_path_is_a_real_conflict() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let dest = dir.path().join("occupied");
        std::fs::write(&dest, b"a file, not a directory").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CreateDir {
                dest: vpath_for(&dest),
                mode: None,
            }],
            PlanOptions::default(), // default_conflict: Skip
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.skipped.len(), 1);
        assert!(
            dest.is_file(),
            "the default (skip) must not clobber the existing file"
        );
    }

    #[tokio::test]
    async fn create_dir_overwrite_replaces_a_file_occupying_the_path() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let dest = dir.path().join("occupied");
        std::fs::write(&dest, b"a file, not a directory").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::CreateDir {
                dest: vpath_for(&dest),
                mode: None,
            }],
            PlanOptions {
                default_conflict: ConflictPolicy::Overwrite,
                verify: false,
            },
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert!(
            dest.is_dir(),
            "Overwrite must replace the file with a directory"
        );
    }

    #[tokio::test]
    async fn rename_step_overwrite_replaces_the_destination() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&source, b"new content").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let plan = Plan::new(
            vec![Step::Rename {
                source: vpath_for(&source),
                dest: vpath_for(&dest),
                conflict: Some(ConflictPolicy::Overwrite),
            }],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());
        assert!(
            !source.exists(),
            "a Rename step consumes its source regardless of policy"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
    }

    /// For each of `n` files under `base` named `{prefix}_s{i}.txt` /
    /// `{prefix}_d{i}.txt`, writes a `"new"` source and a pre-existing
    /// `"old"` destination -- a batch of `n` guaranteed `CopyFile`
    /// conflicts, used by the property test below.
    fn make_conflicting_files(base: &Path, prefix: &str, n: usize) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut sources = Vec::new();
        let mut dests = Vec::new();
        for i in 0..n {
            let s = base.join(format!("{prefix}_s{i}.txt"));
            let d = base.join(format!("{prefix}_d{i}.txt"));
            std::fs::write(&s, b"new").unwrap();
            std::fs::write(&d, b"old").unwrap();
            sources.push(s);
            dests.push(d);
        }
        (sources, dests)
    }

    fn copy_steps(sources: &[PathBuf], dests: &[PathBuf]) -> Vec<Step> {
        sources
            .iter()
            .zip(dests)
            .map(|(s, d)| Step::CopyFile {
                source: vpath_for(s),
                dest: vpath_for(d),
                size: 3,
                conflict: None,
            })
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// T-5.1.9's AC: "property test over random conflict sequences
        /// shows no policy leaks between jobs." Runs two *separate*
        /// `execute()` calls (two "jobs"), each with its own random-length
        /// run of `CopyFile` conflicts and its own random position where
        /// the resolver hands out an "apply to all" answer -- reusing the
        /// exact same `ScriptedResolver` `Arc` for both. If a job's sticky
        /// state ever leaked into the other's `ExecutorContext`, job 2
        /// would either skip consulting the resolver for its own
        /// conflicts entirely, or apply job 1's leftover policy instead of
        /// its own -- both of which the assertions below would catch.
        #[test]
        fn sticky_apply_to_all_never_leaks_between_two_separate_jobs(
            n1 in 1usize..5,
            raw_trigger1 in 1usize..5,
            n2 in 1usize..5,
            raw_trigger2 in 1usize..5,
        ) {
            let trigger1 = raw_trigger1.min(n1);
            let trigger2 = raw_trigger2.min(n2);

            // `current_thread`, matching `#[tokio::test]`'s own default
            // flavor elsewhere in this file -- `Runtime::new()` builds a
            // genuinely multi-threaded runtime, under which the three
            // spawned copy-class tasks could race to acquire the
            // concurrency=1 semaphore in any order, breaking this test's
            // assumption that step order matches resolver-consultation
            // order. Safe here the same way every other plain
            // `#[tokio::test]` in this file already relies on: the files
            // are tiny, so `LocalFs`'s inline blocking syscalls never
            // starve anything long enough to matter.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let dir = TempDir::new().unwrap();
                let state1 = TempDir::new().unwrap();
                let state2 = TempDir::new().unwrap();

                // Job 1's script: one-off `Overwrite` for every conflict
                // before `trigger1`, then an `AllRemaining` `Skip` right at
                // `trigger1` -- nothing scripted after that, since the
                // sticky answer must cover the rest without consulting
                // the resolver again.
                let mut answers = Vec::new();
                for i in 1..=trigger1 {
                    if i < trigger1 {
                        answers.push(ConflictResolution::once(ConflictPolicy::Overwrite));
                    } else {
                        answers.push(ConflictResolution::apply_to_all(ConflictPolicy::Skip));
                    }
                }
                // Job 2's own script, appended after job 1's -- if job 1's
                // sticky state leaked into job 2, these would never be
                // consumed at all.
                for i in 1..=trigger2 {
                    if i < trigger2 {
                        answers.push(ConflictResolution::once(ConflictPolicy::Skip));
                    } else {
                        answers.push(ConflictResolution::apply_to_all(ConflictPolicy::Overwrite));
                    }
                }
                let resolver = Arc::new(ScriptedResolver::new(answers));
                let resolver_dyn: Arc<dyn ConflictResolver> = resolver.clone();

                let (job1_sources, job1_dests) = make_conflicting_files(dir.path(), "job1", n1);
                let plan1 = Plan::new(copy_steps(&job1_sources, &job1_dests), PlanOptions::default());
                let (report1, _events1) = run_with_resolver(
                    Arc::new(LocalFs),
                    plan1,
                    state1.path(),
                    1,
                    Some(Arc::clone(&resolver_dyn)),
                )
                .await;

                prop_assert!(report1.errors.is_empty(), "{:?}", report1.errors);
                prop_assert_eq!(
                    resolver.call_count(),
                    trigger1,
                    "job 1 must stop consulting the resolver right after its own apply-to-all answer"
                );
                for (i, d) in job1_dests.iter().enumerate() {
                    let n = i + 1;
                    let content = std::fs::read(d).unwrap();
                    if n < trigger1 {
                        prop_assert_eq!(content, b"new".to_vec());
                    } else {
                        prop_assert_eq!(content, b"old".to_vec());
                    }
                }

                let (job2_sources, job2_dests) = make_conflicting_files(dir.path(), "job2", n2);
                let plan2 = Plan::new(copy_steps(&job2_sources, &job2_dests), PlanOptions::default());
                let (report2, _events2) = run_with_resolver(
                    Arc::new(LocalFs),
                    plan2,
                    state2.path(),
                    1,
                    Some(Arc::clone(&resolver_dyn)),
                )
                .await;

                prop_assert!(report2.errors.is_empty(), "{:?}", report2.errors);
                prop_assert_eq!(
                    resolver.call_count(),
                    trigger1 + trigger2,
                    "job 2 must independently consult the resolver for its own conflicts -- \
                     if job 1's sticky answer had leaked, job 2 would never call the resolver \
                     at all"
                );
                for (i, d) in job2_dests.iter().enumerate() {
                    let n = i + 1;
                    let content = std::fs::read(d).unwrap();
                    if n < trigger2 {
                        prop_assert_eq!(content, b"old".to_vec());
                    } else {
                        prop_assert_eq!(content, b"new".to_vec());
                    }
                }

                Ok(())
            })?;
        }
    }

    // ---- T-5.1.6: metadata preservation, end to end -----------------

    #[tokio::test]
    async fn copying_preserves_mode_beyond_the_writers_default() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let script = src.path().join("script.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        // Deliberately not 0644 (`naive_copy`'s own `WriteOpts::create_new()`
        // default) -- proves the follow-up `SetMeta` step, not the writer's
        // own default mode, is what mode fidelity actually comes from.
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[vpath_for(&script)],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let (report, _events) = run(fs, plan, state.path(), 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let dest_meta = std::fs::metadata(dst.path().join("script.sh")).unwrap();
        assert_eq!(
            dest_meta.permissions().mode() & 0o777,
            0o750,
            "the source's exact mode must survive, not the writer's own default"
        );
    }

    #[tokio::test]
    async fn copying_preserves_mtime_and_xattrs() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source_path = src.path().join("a.txt");
        std::fs::write(&source_path, b"hello").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let source_vpath = vpath_for(&source_path);
        // Set a distinctive mtime and a plain xattr on the source before
        // planning -- both go through the same `FileSystem::set_meta`
        // trait method `Step::SetMeta` itself dispatches through, so this
        // needs no raw syscall access from this crate's own test suite.
        fs.set_meta(
            &source_vpath,
            &MetaPatch {
                modified: Some(Timestamp::new(1_700_000_000, 0)),
                set_xattrs: BTreeMap::from([("user.duet.test".to_string(), b"payload".to_vec())]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let cancel = crate::planner::CancelToken::new();
        let plan = crate::planner::plan_copy(
            &*fs,
            &[source_vpath],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let (report, _events) = run(fs.clone(), plan, state.path(), 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let dest_vpath = vpath_for(&dst.path().join("a.txt"));
        let dest_meta = fs.stat(&dest_vpath, false).await.unwrap();
        assert_eq!(
            dest_meta.modified,
            Some(Timestamp::new(1_700_000_000, 0)),
            "mtime must survive the copy"
        );
        assert_eq!(
            dest_meta
                .xattrs
                .unwrap_or_default()
                .get("user.duet.test")
                .map(Vec::as_slice),
            Some(b"payload".as_slice()),
            "xattrs must survive the copy"
        );
    }

    #[tokio::test]
    async fn copying_a_directory_does_not_disturb_its_restored_mtime() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("b.txt"), b"world").unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        // A distinctive mtime on the source directory itself -- if its
        // own `SetMeta` ran before `a.txt`/`b.txt` were copied into it
        // (instead of deferred to the very end of the plan, T-5.1.6's own
        // fix), creating those two directory entries would bump this
        // right back up to "now," and the assertion below would fail.
        fs.set_meta(
            &vpath_for(src.path()),
            &MetaPatch {
                modified: Some(Timestamp::new(1_600_000_000, 0)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

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

        let (report, _events) = run(fs.clone(), plan, state.path(), 1).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        let dest_dir_vpath = vpath_for(&dst.path().join(src_name));
        let dest_meta = fs.stat(&dest_dir_vpath, false).await.unwrap();
        assert_eq!(
            dest_meta.modified,
            Some(Timestamp::new(1_600_000_000, 0)),
            "the directory's restored mtime must survive its own children being \
             copied into it afterward"
        );
    }

    #[tokio::test]
    async fn a_setmeta_step_is_skipped_not_attempted_when_its_copy_step_failed() {
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let dest_path = dst.path().join("never-created.txt");

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        // Step 0: a CopyFile certain to fail (source doesn't exist).
        // Step 1: a SetMeta on the same (never-created) destination,
        // wired to depend on step 0 -- without dependency-gating, this
        // would attempt to `chmod`/`utimensat` a path that doesn't exist.
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&dst.path().join("does-not-exist.txt")),
                    dest: vpath_for(&dest_path),
                    size: 5,
                    conflict: None,
                },
                Step::SetMeta {
                    target: vpath_for(&dest_path),
                    patch: MetaPatch {
                        mode: Some(0o600),
                        ..Default::default()
                    },
                    depends_on: Some(0),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(
            report.errors.len(),
            1,
            "the CopyFile step must genuinely fail"
        );
        assert_eq!(
            report.skipped.len(),
            1,
            "the dependency-gated SetMeta must be skipped, not attempted"
        );
        assert!(!dest_path.exists());
    }

    #[tokio::test]
    async fn a_link_step_is_skipped_not_attempted_when_its_source_copy_step_failed() {
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let link_dest = dst.path().join("would-be-hardlink.txt");

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        // Step 0: a CopyFile certain to fail (source doesn't exist) -- the
        // "first occurrence" a hardlink-graph dedup would normally link
        // against.
        // Step 1: a Link wired to depend on it -- without dependency-
        // gating, this would attempt to hardlink against a path that was
        // never created.
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: vpath_for(&dst.path().join("does-not-exist.txt")),
                    dest: vpath_for(&dst.path().join("never-created.txt")),
                    size: 5,
                    conflict: None,
                },
                Step::Link {
                    source: vpath_for(&dst.path().join("never-created.txt")),
                    dest: vpath_for(&link_dest),
                    depends_on: Some(0),
                },
            ],
            PlanOptions::default(),
        );

        let (report, _events) = run(fs, plan, state.path(), 1).await;

        assert_eq!(
            report.errors.len(),
            1,
            "the CopyFile step must genuinely fail"
        );
        assert_eq!(
            report.skipped.len(),
            1,
            "the dependency-gated Link must be skipped, not attempted"
        );
        assert!(!link_dest.exists());
    }
}
