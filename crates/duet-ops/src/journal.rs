// SPDX-License-Identifier: MIT
//! `Journal` — the crash-safety backbone (design.md §9.3, FR-OPS-07).
//!
//! "Each job appends to `~/.local/state/duet/jobs/<id>.journal`, an fsync'd
//! append-only record of intended and completed steps." Guarantees:
//!
//! - "Destination files are written to `.duet-partial-<rand>` and renamed
//!   into place only when complete and (optionally) verified. A SIGKILL
//!   therefore leaves the old destination intact and a visible partial
//!   file."
//! - "On next launch, incomplete journals surface as '3 interrupted
//!   operations — review'. The user can resume ..., discard partials, or
//!   inspect."
//! - "Deletes are journaled before execution so the undo stack (FR-OPS-14)
//!   has something to work from for trash operations."
//!
//! This module defines the record shapes those guarantees are built on
//! ([`JournalRecord`]) and the read/write handle shapes
//! ([`Journal`]/[`JournalReader`]) that own them. The actual file I/O
//! (open, append-with-fsync, replay-on-scan) is T-5.1.2's job — everything
//! here that touches disk is `todo!()` — but the record format is real,
//! serializable, and is what a recovery reader needs to reconstruct "what
//! was in progress when we died" without guessing.

use std::path::{Path, PathBuf};

use duet_types::{Result, Timestamp, VfsError};
use serde::{Deserialize, Serialize};

use crate::job::{JobId, JobOutcome, StepFailure};
use crate::plan::Plan;
use crate::step::Step;

/// One line in a job's append-only journal file.
///
/// Every [`Step`] produces exactly one [`JournalRecord::Intent`] before it
/// starts and exactly one [`JournalRecord::Completion`] after it ends
/// (successfully, skipped, or failed) — a recovery reader replays the
/// sequence, and any `Intent` without a matching `Completion` is exactly
/// the crash window FR-OPS-07 has to cover: "an interrupted operation
/// leaves either the old file intact or a clearly-marked partial file,
/// never a silently truncated destination."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JournalRecord {
    /// First record in the file: the job started, with the full plan it is
    /// about to execute. Persisting the whole `Plan` (not just its id)
    /// means a recovery reader needs nothing beyond this one journal file
    /// to reconstruct the job — no dependency on the queue's in-memory
    /// state having survived the crash too.
    JobStarted {
        job_id: JobId,
        started_at: Timestamp,
        plan: Plan,
    },
    /// Declares intent to execute `step` (at `step_index` in the job's
    /// plan) before any side effect of it happens. This record must be
    /// durable (fsync'd) *before* the executor does anything the step
    /// implies — that ordering is the whole crash-safety contract.
    Intent {
        step_index: u32,
        step: Step,
        /// For steps that stage a destination through a temp sibling
        /// (`CopyFile`/`Reflink`), the exact `.duet-partial-<rand>` name
        /// chosen, so recovery can find (and offer to discard or resume)
        /// the orphaned file without re-deriving the random suffix or
        /// globbing the directory and guessing which partial belongs to
        /// which step.
        partial_name: Option<String>,
    },
    /// `step_index` finished, one way or another. The only record that can
    /// retire an `Intent` — see [`StepOutcome`].
    Completion {
        step_index: u32,
        outcome: StepOutcome,
    },
    /// The job reached a terminal state; no further records follow for
    /// this job id (the journal file itself is retained for audit/undo
    /// history per FR-OPS-14, not deleted).
    JobFinished {
        outcome: JobOutcome,
        finished_at: Timestamp,
    },
}

/// How a journaled step ended, recorded in its [`JournalRecord::Completion`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepOutcome {
    Succeeded,
    /// A `ConflictPolicy` caused the step to be skipped rather than
    /// executed.
    Skipped {
        reason: String,
    },
    Failed(StepFailure),
}

/// What a recovery scan found for one job's journal — enough to drive the
/// startup "N interrupted operations — review" UI (T-5.2.5) and its
/// resume/discard/inspect actions, without either action needing to
/// re-parse the raw journal file itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub job_id: JobId,
    pub plan: Plan,
    /// Steps whose `Intent` has no matching `Completion` — the exact
    /// remaining work (design.md §9.3: "resume (re-plan the remainder ...)").
    pub incomplete_steps: Vec<u32>,
    /// `(step_index, partial_name)` pairs an `Intent` recorded that
    /// "discard" should clean up and "resume" should either continue
    /// writing to or replace.
    pub orphaned_partials: Vec<(u32, String)>,
    /// `None` if the journal has no `JobFinished` record at all (the job
    /// was mid-execution when the process died); `Some` if it reached a
    /// terminal state but, e.g., a `Completion` for the last step and the
    /// `JobFinished` record were written non-atomically and only one
    /// landed before the crash.
    pub last_outcome: Option<JobOutcome>,
}

/// Handle to a single job's append-only, fsync'd journal file, conventionally
/// at `~/.local/state/duet/jobs/<id>.journal` (design.md §9.3). This type's
/// *shape* — one journal per job, opened for append, records written
/// through [`Journal::append`] — is T-2.3.1's scope; the actual file
/// creation/append/fsync syscalls are T-5.1.2's.
#[derive(Debug)]
pub struct Journal {
    job_id: JobId,
    path: PathBuf,
}

impl Journal {
    /// Opens (creating if necessary) the journal file for `job_id` under
    /// `state_dir`, positioned to append. Does not itself write a
    /// `JobStarted` record — callers append that explicitly as the first
    /// call to [`Journal::append`], keeping "what the first record is" a
    /// visible decision at the call site rather than implicit in `open`.
    ///
    /// # Errors
    /// - `Permission`/`Space`/`Fatal` — as for any local file creation,
    ///   classified via [`duet_types::ErrorKind::from_io_error`].
    pub fn open(job_id: JobId, state_dir: &Path) -> Result<Self> {
        let _ = (job_id, state_dir);
        todo!("T-5.1.2: O_APPEND | O_CREAT open of state_dir/jobs/{{job_id}}.journal")
    }

    /// The path this journal is (or will be) written at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The job this journal belongs to.
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    /// Appends `record`, fsync'ing before returning. FR-OPS-07's guarantee
    /// depends entirely on this call not returning until `record` is
    /// durable on disk, not merely written to the page cache — a crash
    /// immediately after a successful `append()` call must never lose that
    /// record.
    ///
    /// # Errors
    /// - `Space` — `ENOSPC`/`EDQUOT` writing or extending the journal file
    ///   itself (distinct from the job's own data running out of space,
    ///   but handled identically: pause the queue).
    /// - `Retryable`/`Fatal` — as for any local file I/O.
    pub fn append(&mut self, record: &JournalRecord) -> Result<()> {
        let _ = record;
        todo!("T-5.1.2: serialize the record, write, fsync(2) before returning")
    }
}

/// Reads journal files back for crash recovery. A separate type from
/// [`Journal`] (rather than a method on it) because a startup recovery scan
/// is read-only and needs no write access to the state directory — it runs
/// *before* any job resumes and potentially before the queue even exists.
#[derive(Debug, Default)]
pub struct JournalReader;

impl JournalReader {
    /// Enumerates every `*.journal` file under `state_dir` and replays each
    /// into a [`RecoveryReport`]. A journal ending in a `JobFinished` record
    /// (successful or not) still produces a report — the startup UI reads
    /// `last_outcome`/`incomplete_steps` to decide whether "N interrupted
    /// operations" even needs mentioning it, rather than this method
    /// silently filtering finished jobs out (a filtered-out finished job
    /// with an *orphaned partial* — journal wrote `Completion` but a crash
    /// hit right after, before the partial's rename — would otherwise be
    /// invisible to recovery, exactly the kind of gap FR-OPS-07 rules out).
    ///
    /// # Errors
    /// - `Permission`/`Fatal` — reading `state_dir` or a journal file
    ///   failed outright (as opposed to a journal being merely truncated
    ///   mid-record, which is an *expected* SIGKILL artifact this method
    ///   must handle by treating the trailing partial record as absent, not
    ///   erroring the whole scan).
    pub fn scan(state_dir: &Path) -> Result<Vec<RecoveryReport>> {
        let _ = state_dir;
        todo!("T-5.1.2/T-5.2.5: enumerate *.journal files, replay each, pair Intent/Completion")
    }
}

/// Constructs a [`VfsError`] for journal I/O failures, keeping journal code
/// on the same `duet_types::Result<T>` (= `Result<T, Box<VfsError>>`) error
/// path as the rest of the VFS/ops stack rather than a separate
/// `std::io::Result`, so a `Journal::open`/`append` failure is classified
/// through the same `ErrorKind` taxonomy (design.md §9.3) a `FileSystem`
/// call would use. Not yet called anywhere (both call sites above are
/// `todo!()`); kept here as the shape T-5.1.2 wires up.
#[allow(dead_code)]
fn io_error(err: std::io::Error) -> Box<VfsError> {
    Box::new(VfsError::from_io(err))
}
