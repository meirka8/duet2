// SPDX-License-Identifier: MIT
//! `QueueManager` — T-5.1.13: turns the [`Job`]/[`JobState`] interface
//! shapes T-2.3.1 already defined into a real, running multi-job scheduler
//! on top of [`crate::executor::execute`].
//!
//! design.md §9.3: "Jobs are queued, not fire-and-forget... a tray in the
//! status bar showing aggregate progress, expanding to a full manager with
//! per-job pause/resume/cancel/reorder/priority." This module is that
//! manager's engine-side half — no UI here, just the state machine and
//! scheduling a UI (or a headless caller) drives through.
//!
//! # Scheduling
//!
//! [`QueueManager::enqueue`] hands back a fresh [`JobId`] (allocated here,
//! not by the caller — a real queue, not a caller-numbered slot) and the
//! job starts immediately if a `max_concurrent` slot is free, else waits
//! `Queued`. Whenever a slot frees (a job reaches [`JobState::Terminal`]),
//! the highest-priority (`Job::priority`: lower runs first) still-`Queued`
//! job starts next, ties broken by submission order. This is a plain
//! "pick the best candidate and start it" scheduler, not a persistent
//! background loop with its own wake/notify machinery — every state
//! transition that could free up a slot or add a candidate
//! (`enqueue`/a job finishing) directly attempts to start the next job
//! itself ([`try_start_next`]), which keeps transitions synchronous and
//! easy to reason about rather than racing against a separate scheduler
//! task's own latency. `try_start_next` is a free function (not a
//! `QueueManager` method) purely so it can be called from inside a
//! spawned job's own completion task, which only has the shared
//! `Arc<Mutex<QueueState>>`/channel in scope, not a whole `&QueueManager`.
//!
//! # Reordering (T-5.1.13's own AC)
//!
//! "Reordering a running job is either supported or cleanly refused" —
//! this implementation refuses: [`QueueManager::reorder`] only ever
//! changes a still-`Queued` job's priority
//! (`Err(QueueError::NotQueued)` otherwise). A job that has already
//! started has no meaningful "place in line" left to move; the queue only
//! orders jobs that haven't started yet.
//!
//! # Queue-wide `ENOSPC` propagation
//!
//! `executor.rs`'s own module doc comment discloses exactly this gap:
//! "the queue-wide half of 'pause the whole queue, not just the job' is
//! [this crate's] own disclosed gap (T-5.1.13, multi-job queueing, doesn't
//! exist yet)." This module closes it: when any running job emits
//! [`JobEvent::QueuePausedForSpace`], every active job's
//! [`ExecutionControl`] is paused too ([`pause_queue_for_space`]), and no
//! `Queued` job is allowed to start until [`QueueManager::resume_queue`]
//! is called (presumably once the operator has freed disk space — this
//! crate has no disk-space-polling mechanism of its own, matching
//! `executor.rs`'s own precedent of surfacing the condition rather than
//! guessing when it has cleared).
//!
//! # Scope cuts (disclosed, not silently omitted)
//!
//! - **Pause/resume only apply to a `Running`/`Paused` job.** A `Queued`
//!   job has no [`ExecutionControl`] yet (one is created when it starts);
//!   "hold this job even once its turn comes" is a distinct feature this
//!   task's own AC doesn't ask for, and isn't built here —
//!   `QueueManager::pause`/`resume` return `Err(QueueError::NotRunning)`
//!   for a job in any other state.
//! - **No queue persistence across process restarts.** `Job`/`Plan` are
//!   both already `Serialize`/`Deserialize` (T-2.3.1's own interface
//!   design), but `QueueManager` itself keeps state only in memory; saving
//!   and reloading the queue itself (as opposed to the crash-safety
//!   journal recovery `crate::journal` already provides per-job) isn't
//!   part of this task's AC.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duet_types::{ErrorKind, Result, Timestamp};
use duet_vfs::FileSystem;
use tokio::sync::mpsc;

use crate::conflict::ConflictResolver;
use crate::event::JobEvent;
use crate::executor::{ExecutionControl, execute};
use crate::job::{Job, JobId, JobKind, JobOutcome, JobReport, JobState, StepFailure};
use crate::journal::Journal;
use crate::plan::Plan;

/// Why a [`QueueManager`] operation couldn't be carried out.
#[derive(Debug)]
pub enum QueueError {
    /// No job with this [`JobId`] is known to the queue (never enqueued,
    /// or the ID is simply wrong — this module never drops a job's record
    /// on its own, so "unknown ID" is the only way to reach this).
    NotFound(JobId),
    /// [`QueueManager::reorder`] on a job that isn't `Queued` anymore.
    NotQueued(JobId),
    /// [`QueueManager::pause`]/[`QueueManager::resume`] on a job with no
    /// live [`ExecutionControl`] (not currently `Running`/`Paused`).
    NotRunning(JobId),
}

/// One job's full bookkeeping — the [`Job`] a caller sees plus everything
/// the scheduler needs to actually run and reorder it.
struct JobEntry {
    job: Job,
    fs: Arc<dyn FileSystem>,
    state_dir: PathBuf,
    concurrency: usize,
    resolver: Option<Arc<dyn ConflictResolver>>,
    /// Submission order, for stable tie-breaking among equal priorities —
    /// `Job::priority`'s own doc comment: "ties broken by queue-insertion
    /// order."
    seq: u64,
    /// Set once the job actually starts running; `None` while `Queued`
    /// (there is nothing to pause/resume/cancel-in-place yet) and left in
    /// place after the job finishes (harmless, and simplifies
    /// [`pause_queue_for_space`]'s "every active job" scan not needing a
    /// separate liveness check beyond `Job::state`).
    control: Option<ExecutionControl>,
}

#[derive(Default)]
struct QueueState {
    jobs: HashMap<JobId, JobEntry>,
    next_id: u64,
    next_seq: u64,
    running: usize,
    /// `true` once any job has hit `ErrorKind::Space` and no
    /// [`QueueManager::resume_queue`] call has cleared it since — while
    /// set, no `Queued` job is started, matching design.md's "pause the
    /// whole queue, not just the job."
    space_paused: bool,
    /// Jobs this module itself paused in response to a `Space` condition,
    /// as opposed to a job the *caller* separately paused via
    /// [`QueueManager::pause`] — [`QueueManager::resume_queue`] only
    /// resumes the former; a deliberately, separately paused job stays
    /// paused until the caller explicitly resumes it too, so one
    /// operator's decision to hold a job never gets silently overridden by
    /// an unrelated space-recovery action.
    space_paused_jobs: HashSet<JobId>,
}

/// A running, in-memory multi-job scheduler over [`crate::executor::execute`].
/// See the module doc comment for the full design.
pub struct QueueManager {
    state: Arc<Mutex<QueueState>>,
    max_concurrent: usize,
    /// Every job's [`JobEvent`]s, tagged with `job_id` (as they already
    /// are — see [`JobEvent::job_id`]), forwarded here as they happen —
    /// one channel a UI subscribes to instead of one per job.
    events: mpsc::UnboundedSender<JobEvent>,
}

impl QueueManager {
    pub fn new(max_concurrent: usize, events: mpsc::UnboundedSender<JobEvent>) -> Self {
        QueueManager {
            state: Arc::new(Mutex::new(QueueState::default())),
            max_concurrent: max_concurrent.max(1),
            events,
        }
    }

    /// Submits an already-planned job. Returns immediately with a fresh
    /// [`JobId`]; the job starts right away if a concurrency slot is free
    /// (and the queue isn't currently space-paused), otherwise it waits
    /// `Queued` in priority order. `state_dir` is where this job's own
    /// crash-safety journal (`crate::journal::Journal::open`) is opened —
    /// same convention `execute`'s own tests use, just performed
    /// internally here rather than by the caller.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue(
        &self,
        kind: JobKind,
        plan: Plan,
        priority: i32,
        fs: Arc<dyn FileSystem>,
        state_dir: PathBuf,
        concurrency: usize,
        resolver: Option<Arc<dyn ConflictResolver>>,
    ) -> JobId {
        let id;
        {
            let mut state = self.state.lock().unwrap();
            id = JobId(state.next_id);
            state.next_id += 1;
            let seq = state.next_seq;
            state.next_seq += 1;
            let job = Job {
                id,
                kind,
                plan,
                state: JobState::Queued,
                priority,
            };
            state.jobs.insert(
                id,
                JobEntry {
                    job,
                    fs,
                    state_dir,
                    concurrency,
                    resolver,
                    seq,
                    control: None,
                },
            );
        }
        try_start_next(&self.state, self.max_concurrent, &self.events);
        id
    }

    /// Changes a still-`Queued` job's priority. `Err(QueueError::NotQueued)`
    /// once it has started — see the module doc comment's "Reordering"
    /// section for why that's a refusal, not a best-effort no-op.
    pub fn reorder(&self, id: JobId, priority: i32) -> Result<(), QueueError> {
        let mut state = self.state.lock().unwrap();
        let entry = state.jobs.get_mut(&id).ok_or(QueueError::NotFound(id))?;
        if !matches!(entry.job.state, JobState::Queued) {
            return Err(QueueError::NotQueued(id));
        }
        entry.job.priority = priority;
        Ok(())
    }

    /// Pauses a `Running` job (or is a no-op on an already-`Paused` one).
    /// `Err(QueueError::NotRunning)` for a `Queued`/`Terminal` job — see
    /// the module doc comment's "Scope cuts" section.
    pub fn pause(&self, id: JobId) -> Result<(), QueueError> {
        let state = self.state.lock().unwrap();
        let entry = state.jobs.get(&id).ok_or(QueueError::NotFound(id))?;
        match &entry.job.state {
            JobState::Running { .. } | JobState::Paused { .. } => {
                entry.control.as_ref().unwrap().pause();
                Ok(())
            }
            _ => Err(QueueError::NotRunning(id)),
        }
    }

    /// Resumes a `Paused` job (or is a no-op on an already-`Running` one).
    /// Also clears this job from `space_paused_jobs`, if present, since an
    /// explicit per-job resume is a stronger, more specific signal than
    /// the queue-wide space recovery it might otherwise still be waiting
    /// to be swept up by.
    pub fn resume(&self, id: JobId) -> Result<(), QueueError> {
        let mut state = self.state.lock().unwrap();
        let entry = state.jobs.get(&id).ok_or(QueueError::NotFound(id))?;
        match &entry.job.state {
            JobState::Running { .. } | JobState::Paused { .. } => {
                entry.control.as_ref().unwrap().resume();
                state.space_paused_jobs.remove(&id);
                Ok(())
            }
            _ => Err(QueueError::NotRunning(id)),
        }
    }

    /// Cancels a job. A still-`Queued` job never actually runs at all — it
    /// goes straight to `Terminal { outcome: Cancelled, .. }` and a
    /// synthetic [`JobEvent::Finished`] is emitted so a subscriber sees
    /// the same "always ends with a report" shape regardless of whether
    /// the job ever started (`JobReport`'s own doc comment). A
    /// `Running`/`Paused` job is cancelled the normal way, through its
    /// `ExecutionControl` — the already-running `execute()` call notices
    /// and ends the job itself, and the spawned completion task (see
    /// [`spawn_job`]) does the rest. Already-`Terminal` is a harmless
    /// no-op, not an error (racing a cancel against a job that just
    /// finished on its own shouldn't be surprising to a caller).
    pub fn cancel(&self, id: JobId) -> Result<(), QueueError> {
        let mut state = self.state.lock().unwrap();
        let entry = state.jobs.get_mut(&id).ok_or(QueueError::NotFound(id))?;
        match &entry.job.state {
            JobState::Queued => {
                let now = Timestamp::from(std::time::SystemTime::now());
                let report = JobReport {
                    started_at: None,
                    finished_at: Some(now),
                    ..JobReport::default()
                };
                entry.job.state = JobState::Terminal {
                    outcome: JobOutcome::Cancelled,
                    report: report.clone(),
                };
                let _ = self.events.send(JobEvent::Finished {
                    job_id: id,
                    outcome: JobOutcome::Cancelled,
                    report,
                });
                drop(state);
                try_start_next(&self.state, self.max_concurrent, &self.events);
                Ok(())
            }
            JobState::Running { .. } | JobState::Paused { .. } => {
                entry.control.as_ref().unwrap().cancel();
                Ok(())
            }
            // `QueueManager` never puts a job in `Planning` itself (jobs
            // arrive here already-planned -- see the module doc comment);
            // treated the same as `Terminal`, a harmless no-op, rather
            // than panicking on a state this module can't actually reach.
            JobState::Terminal { .. } | JobState::Planning => Ok(()),
        }
    }

    /// Clears the queue-wide space pause (see the module doc comment):
    /// resumes every job this module auto-paused for `Space`, then lets
    /// the scheduler start any `Queued` job that was being held back.
    pub fn resume_queue(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.space_paused = false;
            let ids: Vec<JobId> = state.space_paused_jobs.drain().collect();
            for id in ids {
                if let Some(control) = state.jobs.get(&id).and_then(|e| e.control.as_ref()) {
                    control.resume();
                }
            }
        }
        try_start_next(&self.state, self.max_concurrent, &self.events);
    }

    /// A point-in-time snapshot of every job the queue knows about — the
    /// "tray in the status bar showing aggregate progress" view.
    pub fn snapshot(&self) -> Vec<Job> {
        let state = self.state.lock().unwrap();
        state.jobs.values().map(|e| e.job.clone()).collect()
    }

    pub fn job(&self, id: JobId) -> Option<Job> {
        let state = self.state.lock().unwrap();
        state.jobs.get(&id).map(|e| e.job.clone())
    }
}

/// Starts as many `Queued` jobs as there are free `max_concurrent` slots
/// (none, if `space_paused`), picking the lowest `(priority, seq)`
/// candidate each time — see the module doc comment's "Scheduling"
/// section for why this is a free function.
fn try_start_next(
    state: &Arc<Mutex<QueueState>>,
    max_concurrent: usize,
    events: &mpsc::UnboundedSender<JobEvent>,
) {
    loop {
        let started = {
            let mut guard = state.lock().unwrap();
            if guard.space_paused || guard.running >= max_concurrent {
                None
            } else {
                let next_id = guard
                    .jobs
                    .values()
                    .filter(|e| matches!(e.job.state, JobState::Queued))
                    .min_by_key(|e| (e.job.priority, e.seq))
                    .map(|e| e.job.id);
                next_id.map(|id| {
                    let control = ExecutionControl::new();
                    let entry = guard.jobs.get_mut(&id).unwrap();
                    entry.job.state = JobState::Running { current_step: 0 };
                    entry.control = Some(control.clone());
                    guard.running += 1;
                    let entry = guard.jobs.get(&id).unwrap();
                    (
                        id,
                        entry.job.plan.clone(),
                        Arc::clone(&entry.fs),
                        entry.state_dir.clone(),
                        entry.concurrency,
                        entry.resolver.clone(),
                        control,
                    )
                })
            }
        };
        match started {
            Some((id, plan, fs, state_dir, concurrency, resolver, control)) => {
                spawn_job(
                    Arc::clone(state),
                    max_concurrent,
                    events.clone(),
                    id,
                    plan,
                    fs,
                    state_dir,
                    concurrency,
                    resolver,
                    control,
                );
            }
            None => break,
        }
    }
}

/// Opens `id`'s journal and spawns the task that drives one job to
/// completion: `execute()` itself runs as its own inner spawned task (so
/// it makes real progress independently), while this outer task consumes
/// `id`'s own event stream live — forwarding each event onward, updating
/// `Job::state` as it goes, and reacting to `QueuePausedForSpace` the
/// instant it happens rather than only after the job finishes.
#[allow(clippy::too_many_arguments)]
fn spawn_job(
    state: Arc<Mutex<QueueState>>,
    max_concurrent: usize,
    events: mpsc::UnboundedSender<JobEvent>,
    id: JobId,
    plan: Plan,
    fs: Arc<dyn FileSystem>,
    state_dir: PathBuf,
    concurrency: usize,
    resolver: Option<Arc<dyn ConflictResolver>>,
    control: ExecutionControl,
) {
    let journal = match Journal::open(id, &state_dir) {
        Ok(j) => j,
        Err(e) => {
            // Mirrors `execute()`'s own precedent for a failed
            // `JobStarted` append: nothing durable was promised yet, so
            // fail the job outright rather than run steps a crash right
            // now could never recover context for.
            let mut guard = state.lock().unwrap();
            if let Some(entry) = guard.jobs.get_mut(&id) {
                let now = Timestamp::from(std::time::SystemTime::now());
                let report = JobReport {
                    started_at: None,
                    finished_at: Some(now),
                    errors: vec![StepFailure {
                        step_index: 0,
                        path: None,
                        kind: ErrorKind::Fatal,
                        message: format!("failed to open journal: {e}"),
                    }],
                    ..JobReport::default()
                };
                entry.job.state = JobState::Terminal {
                    outcome: JobOutcome::Failed,
                    report: report.clone(),
                };
                let _ = events.send(JobEvent::Finished {
                    job_id: id,
                    outcome: JobOutcome::Failed,
                    report,
                });
            }
            guard.running = guard.running.saturating_sub(1);
            drop(guard);
            try_start_next(&state, max_concurrent, &events);
            return;
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let exec_handle = tokio::spawn(execute(
            fs,
            id,
            plan,
            journal,
            concurrency.max(1),
            tx,
            control,
            resolver,
        ));

        while let Some(event) = rx.recv().await {
            let is_space_pause = matches!(event, JobEvent::QueuePausedForSpace { .. });
            let is_finished = matches!(event, JobEvent::Finished { .. });
            apply_event(&state, id, &event);
            let _ = events.send(event);
            if is_space_pause {
                pause_queue_for_space(&state, id);
            }
            if is_finished {
                break; // Finished is always execute()'s last event
            }
        }
        let _ = exec_handle.await;

        {
            let mut guard = state.lock().unwrap();
            guard.running = guard.running.saturating_sub(1);
            guard.space_paused_jobs.remove(&id);
        }
        // A slot may now be free (or a slot held by `id`, if it was
        // itself space-paused) -- give the scheduler another chance.
        try_start_next(&state, max_concurrent, &events);
    });
}

/// Updates `id`'s [`JobState`] to reflect one observed [`JobEvent`].
fn apply_event(state: &Arc<Mutex<QueueState>>, id: JobId, event: &JobEvent) {
    let mut guard = state.lock().unwrap();
    let Some(entry) = guard.jobs.get_mut(&id) else {
        return;
    };
    match event {
        JobEvent::StepStarted { step_index, .. } => {
            entry.job.state = JobState::Running {
                current_step: *step_index,
            };
        }
        JobEvent::Paused { .. } => {
            if let JobState::Running { current_step } = entry.job.state {
                entry.job.state = JobState::Paused { current_step };
            }
        }
        JobEvent::Resumed { .. } => {
            if let JobState::Paused { current_step } = entry.job.state {
                entry.job.state = JobState::Running { current_step };
            }
        }
        JobEvent::Finished {
            outcome, report, ..
        } => {
            entry.job.state = JobState::Terminal {
                outcome: *outcome,
                report: report.clone(),
            };
        }
        _ => {}
    }
}

/// Pauses every active job in response to `source`'s
/// `QueuePausedForSpace` — see the module doc comment's "Queue-wide
/// `ENOSPC` propagation" section. A job already paused for an unrelated,
/// caller-initiated reason still gets its (idempotent, harmless)
/// `ExecutionControl::pause()` call, but is left out of
/// `space_paused_jobs` so [`QueueManager::resume_queue`] won't touch it —
/// except `source` itself, which is always tracked, since it's the job
/// whose own retry loop needs a `resume_queue` call to ever make progress
/// again.
fn pause_queue_for_space(state: &Arc<Mutex<QueueState>>, source: JobId) {
    let mut guard = state.lock().unwrap();
    guard.space_paused = true;
    let already_paused: HashSet<JobId> = guard
        .jobs
        .iter()
        .filter(|(_, e)| matches!(e.job.state, JobState::Paused { .. }))
        .map(|(id, _)| *id)
        .collect();
    let to_pause: Vec<JobId> = guard
        .jobs
        .iter()
        .filter(|(_, e)| {
            matches!(
                e.job.state,
                JobState::Running { .. } | JobState::Paused { .. }
            )
        })
        .map(|(id, _)| *id)
        .collect();
    for id in to_pause {
        let track = id == source || !already_paused.contains(&id);
        if let Some(control) = guard.jobs.get(&id).and_then(|e| e.control.as_ref()) {
            control.pause();
        }
        if track {
            guard.space_paused_jobs.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use duet_types::{ErrorKind, UnixPathBuf, VPath, VfsError};
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::conflict::ConflictPolicy;
    use crate::plan::PlanOptions;
    use crate::planner::{CancelToken, plan_copy};

    fn vpath_for(dir: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(dir.to_str().unwrap()).unwrap())
    }

    /// A `FileSystem` test double wrapping a real [`LocalFs`], used by
    /// this module's own tests to create genuine overlap/ordering windows
    /// real (fast, tmpfs-backed) I/O wouldn't otherwise leave open —
    /// mirroring `executor.rs`'s own `TestFs`/`FlakyFs` test doubles
    /// (private to that file's own test module, so not reusable directly
    /// from here).
    struct SlowFs {
        inner: LocalFs,
        /// Sleeps this long before every `open_read`, widening the window
        /// during which a job stays observably `Running`.
        delay: Duration,
        /// While `true`, every `open_read` fails with `ErrorKind::Space`
        /// instead of delegating -- `run_step_with_retry`'s own `Space`
        /// branch (executor.rs) retries indefinitely once this flips back
        /// to `false`, exactly like a real "disk full, then freed up"
        /// episode.
        space_failing: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl FileSystem for SlowFs {
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
            // Delay *before* checking `space_failing` (not after): a test
            // that starts this job with the flag already `true` still
            // needs a real window to enqueue and confirm other jobs have
            // started before this one's failure fires and propagates a
            // queue-wide pause -- checking first would let it fail
            // essentially instantly, racing ahead of the test's own
            // synchronous setup.
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.space_failing.load(Ordering::SeqCst) {
                return Err(Box::new(
                    VfsError::new(ErrorKind::Space, "injected ENOSPC").with_path(p.clone()),
                ));
            }
            self.inner.open_read(p).await
        }
        async fn open_write(
            &self,
            p: &VPath,
            o: duet_vfs::WriteOpts,
        ) -> Result<Box<dyn duet_vfs::AsyncWriteCommit>> {
            self.inner.open_write(p, o).await
        }
        async fn create_dir(&self, p: &VPath, mode: Option<duet_vfs::Mode>) -> Result<()> {
            self.inner.create_dir(p, mode).await
        }
        async fn remove(&self, p: &VPath, kind: duet_vfs::RemoveKind) -> Result<()> {
            self.inner.remove(p, kind).await
        }
        async fn rename(
            &self,
            from: &VPath,
            to: &VPath,
            flags: duet_vfs::RenameFlags,
        ) -> Result<()> {
            self.inner.rename(from, to, flags).await
        }
        async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
            self.inner.link(source, dest).await
        }
        async fn set_meta(&self, p: &VPath, m: &duet_types::MetaPatch) -> Result<()> {
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
            // Always `Unsupported` -- forces every copy through
            // `naive_copy`, whose first call is `open_read`, so `delay`/
            // `space_failing` above are actually reached.
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    fn slow_fs(delay: Duration) -> Arc<dyn FileSystem> {
        Arc::new(SlowFs {
            inner: LocalFs,
            delay,
            space_failing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Builds a one-file-source `Plan` copying `src` (a directory
    /// containing one small file) into a fresh subdirectory of `dst_root`.
    async fn small_copy_plan(fs: &dyn FileSystem, src: &Path, dst_root: &Path) -> Plan {
        let cancel = CancelToken::new();
        plan_copy(
            fs,
            &[vpath_for(src)],
            &vpath_for(dst_root),
            PlanOptions {
                default_conflict: ConflictPolicy::Overwrite,
                ..PlanOptions::default()
            },
            &cancel,
        )
        .await
        .unwrap()
    }

    fn make_source(dir: &Path, name: &str) -> PathBuf {
        let src = dir.join(name);
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        src
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ten_queued_jobs_never_exceed_max_concurrency_and_all_complete() {
        let root = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();
        let fs = slow_fs(Duration::from_millis(30));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = QueueManager::new(3, tx);

        let mut ids = Vec::new();
        for i in 0..10 {
            let src = make_source(root.path(), &format!("src-{i}"));
            let dst = root.path().join(format!("dst-{i}"));
            std::fs::create_dir(&dst).unwrap();
            let plan = small_copy_plan(&*fs, &src, &dst).await;
            let id = queue.enqueue(
                JobKind::Copy,
                plan,
                0,
                Arc::clone(&fs),
                state_dir.path().to_path_buf(),
                1,
                None,
            );
            ids.push(id);
        }

        let mut active: HashSet<JobId> = HashSet::new();
        let mut max_active = 0usize;
        let mut finished: HashSet<JobId> = HashSet::new();
        let mut outcomes = HashMap::new();
        while finished.len() < ids.len() {
            let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("timed out waiting for queue events")
                .expect("event channel closed early");
            match event {
                JobEvent::Started { job_id } => {
                    active.insert(job_id);
                    max_active = max_active.max(active.len());
                }
                JobEvent::Finished {
                    job_id, outcome, ..
                } => {
                    active.remove(&job_id);
                    finished.insert(job_id);
                    outcomes.insert(job_id, outcome);
                }
                _ => {}
            }
        }

        assert!(
            max_active <= 3,
            "observed {max_active} jobs running at once, max_concurrent was 3"
        );
        for id in &ids {
            assert_eq!(
                outcomes.get(id),
                Some(&JobOutcome::Completed),
                "job {id:?} did not complete cleanly"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_jobs_start_in_priority_order_when_running_one_at_a_time() {
        let root = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();
        // Slow enough that the holder job is still `Running` by the time
        // all four priority-tagged jobs have been enqueued behind it.
        let fs = slow_fs(Duration::from_millis(100));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = QueueManager::new(1, tx);

        let enqueue_one = |name: &str, priority: i32| {
            let src = make_source(root.path(), name);
            let dst = root.path().join(format!("dst-{name}"));
            std::fs::create_dir(&dst).unwrap();
            (src, dst, priority)
        };

        let holder = enqueue_one("holder", 0);
        let holder_plan = small_copy_plan(&*fs, &holder.0, &holder.1).await;
        let holder_id = queue.enqueue(
            JobKind::Copy,
            holder_plan,
            holder.2,
            Arc::clone(&fs),
            state_dir.path().to_path_buf(),
            1,
            None,
        );

        // Submitted out of priority order; lower priority runs first.
        let specs = [("b", 30), ("c", 10), ("d", 20), ("e", 0)];
        let mut ids_by_name = HashMap::new();
        for (name, priority) in specs {
            let (src, dst, priority) = enqueue_one(name, priority);
            let plan = small_copy_plan(&*fs, &src, &dst).await;
            let id = queue.enqueue(
                JobKind::Copy,
                plan,
                priority,
                Arc::clone(&fs),
                state_dir.path().to_path_buf(),
                1,
                None,
            );
            ids_by_name.insert(id, name);
        }

        let mut start_order = Vec::new();
        let mut finished_count = 0usize;
        while finished_count < 5 {
            let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("timed out waiting for queue events")
                .expect("event channel closed early");
            match event {
                JobEvent::Started { job_id } if job_id != holder_id => {
                    start_order.push(ids_by_name[&job_id]);
                }
                JobEvent::Finished { .. } => finished_count += 1,
                _ => {}
            }
        }

        assert_eq!(
            start_order,
            vec!["e", "c", "d", "b"],
            "jobs must start in ascending-priority order once queued behind a running job"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reordering_a_running_job_is_refused_but_a_queued_job_reorders_freely() {
        let root = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();
        let fs = slow_fs(Duration::from_millis(300));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = QueueManager::new(1, tx);

        let src_a = make_source(root.path(), "a");
        let dst_a = root.path().join("dst-a");
        std::fs::create_dir(&dst_a).unwrap();
        let plan_a = small_copy_plan(&*fs, &src_a, &dst_a).await;
        let id_a = queue.enqueue(
            JobKind::Copy,
            plan_a,
            0,
            Arc::clone(&fs),
            state_dir.path().to_path_buf(),
            1,
            None,
        );

        // Wait for A to actually start before testing against it.
        loop {
            match rx.recv().await.unwrap() {
                JobEvent::Started { job_id } if job_id == id_a => break,
                _ => {}
            }
        }

        assert!(
            matches!(queue.reorder(id_a, -100), Err(QueueError::NotQueued(_))),
            "reordering an already-running job must be refused, not silently accepted"
        );

        let src_b = make_source(root.path(), "b");
        let dst_b = root.path().join("dst-b");
        std::fs::create_dir(&dst_b).unwrap();
        let plan_b = small_copy_plan(&*fs, &src_b, &dst_b).await;
        let id_b = queue.enqueue(
            JobKind::Copy,
            plan_b,
            0,
            Arc::clone(&fs),
            state_dir.path().to_path_buf(),
            1,
            None,
        );
        assert_eq!(queue.job(id_b).unwrap().state, JobState::Queued);
        assert!(
            queue.reorder(id_b, -50).is_ok(),
            "reordering a still-queued job must succeed"
        );
        assert_eq!(queue.job(id_b).unwrap().priority, -50);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_space_failure_pauses_every_other_active_job_until_resume_queue() {
        let root = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();

        let space_flag = Arc::new(AtomicBool::new(true));
        // A real delay before A's own space failure fires -- otherwise it
        // fails essentially instantly (before this test even finishes
        // enqueueing B), racing ahead of the very scenario being tested.
        let fs_a: Arc<dyn FileSystem> = Arc::new(SlowFs {
            inner: LocalFs,
            delay: Duration::from_millis(100),
            space_failing: Arc::clone(&space_flag),
        });
        // A healthy but slower job, so it's still genuinely `Running` (not
        // already finished) when A's space failure should pause it.
        let fs_b = slow_fs(Duration::from_millis(200));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = QueueManager::new(2, tx);

        let src_a = make_source(root.path(), "a");
        let dst_a = root.path().join("dst-a");
        std::fs::create_dir(&dst_a).unwrap();
        let plan_a = small_copy_plan(&*fs_a, &src_a, &dst_a).await;
        let id_a = queue.enqueue(
            JobKind::Copy,
            plan_a,
            0,
            Arc::clone(&fs_a),
            state_dir.path().to_path_buf(),
            1,
            None,
        );

        let src_b = make_source(root.path(), "b");
        let dst_b = root.path().join("dst-b");
        std::fs::create_dir(&dst_b).unwrap();
        let plan_b = small_copy_plan(&*fs_b, &src_b, &dst_b).await;
        let id_b = queue.enqueue(
            JobKind::Copy,
            plan_b,
            0,
            Arc::clone(&fs_b),
            state_dir.path().to_path_buf(),
            1,
            None,
        );

        // Event-driven, not polled: wait for the space pause, then wait
        // specifically for B's own `Paused` event -- proof the queue-wide
        // propagation actually reached a job that never itself touched
        // `ErrorKind::Space`.
        let saw_space_pause = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let JobEvent::QueuePausedForSpace { job_id } = rx.recv().await.unwrap() {
                    break job_id;
                }
            }
        })
        .await
        .expect("timed out waiting for QueuePausedForSpace");
        assert_eq!(saw_space_pause, id_a);

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let JobEvent::Paused { job_id } = rx.recv().await.unwrap()
                    && job_id == id_b
                {
                    break;
                }
            }
        })
        .await
        .expect("job B was never paused by A's space failure");
        assert!(matches!(
            queue.job(id_b).unwrap().state,
            JobState::Paused { .. }
        ));

        // "The operator freed disk space."
        space_flag.store(false, Ordering::SeqCst);
        queue.resume_queue();

        let mut finished = HashSet::new();
        let mut outcomes = HashMap::new();
        while finished.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("timed out waiting for both jobs to finish")
                .expect("event channel closed early");
            if let JobEvent::Finished {
                job_id, outcome, ..
            } = event
            {
                finished.insert(job_id);
                outcomes.insert(job_id, outcome);
            }
        }
        assert_eq!(outcomes.get(&id_a), Some(&JobOutcome::Completed));
        assert_eq!(outcomes.get(&id_b), Some(&JobOutcome::Completed));
    }
}
