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
//! `docs/crash-safety.md` (T-2.3.2) is the authoritative interruption-point
//! -by-interruption-point proof this module's shape has to satisfy; see
//! its "Mechanism common to every step" section for the exact
//! fsync-before-side-effect / fsync-before-Completion ordering every
//! [`Journal::append`] call has to uphold.
//!
//! # Wire format
//!
//! Newline-delimited JSON: one [`JournalRecord`] per line, `\n`-terminated.
//! Chosen over a binary/length-prefixed format for two reasons: it matches
//! this crate's existing "JSON is the obvious choice" precedent (`Plan`'s
//! own round-trip tests), and it makes [`JournalReader::scan`]'s hardest
//! requirement — treating a torn trailing record (the expected artifact of
//! a `SIGKILL` landing mid-`write()`) as *absent*, not a scan-ending error —
//! trivial: a torn tail -- a final line with no trailing `\n`, or a run of
//! unparseable lines at the very end -- just gets dropped. Records are
//! appended in *batches* ([`Journal::append_batch`]: one `write`, one
//! `fsync`). A batch is **not** all-or-nothing, even under `SIGKILL`: the
//! kernel copies a large `write(2)` into the page cache a page at a time
//! and a fatal signal arriving partway cuts it short, leaving a prefix of
//! the batch that ends mid-record (this was assumed impossible until the
//! data-safety suite, T-10.2.1, produced it); a power loss mid-writeback
//! does the same. Nothing in a torn batch was acknowledged -- its reply
//! is only sent after the whole batch is fsync'd -- so no step ever
//! proceeded on it. Either way the damage is confined to the tail, and
//! [`Journal::open`] cuts a torn tail off before anything is appended
//! after it: [`JournalReader::scan`] treats an unparseable line
//! *followed by a valid one* as real corruption (a hard error), never as a
//! recoverable gap, because an append-only file cannot produce that shape
//! by crashing.
//!
//! # Directory-entry durability
//!
//! [`Journal::open`], the first time it creates a job's journal file (not
//! on every reopen), also `fsync`s the containing `jobs/` directory once.
//! `docs/crash-safety.md`'s 45 rows are about `SIGKILL` specifically (the
//! process dies, the page cache does not), which can't by itself lose a
//! directory entry that `write`+`fsync` already made it past — but
//! FR-OPS-07/NFR-08's stated threat model also names power loss, where an
//! un-fsync'd directory entry genuinely can vanish on some filesystems even
//! though the file's own content fsync succeeded. Cheap (one extra `fsync`
//! per job, not per record) and closes a real gap `crash-safety.md`
//! doesn't test for but design.md's own NFR-08 scope covers.
//!
//! # Scope: what this module does *not* do
//!
//! Per `documentation/task.md`'s own sequencing note ("the data-safety
//! suite is written during Phase 5, not Phase 10... T-10.2.1 is where it
//! is *completed and gated*"), this module does not attempt
//! `crash-safety.md`'s full 45-point injection matrix — that is
//! `T-10.2.1`'s explicit, separately-scoped job, one named test per row.
//! What this module's own tests *do* establish: the journal format is
//! genuinely torn-write-tolerant (a deterministic truncation test) and a
//! *real* `SIGKILL` against a real child process actually writing through
//! [`Journal::append`] produces a journal [`JournalReader::scan`] can
//! recover cleanly (not a simulated crash) — proof the mechanism the full
//! matrix will exercise 45 times actually works, once, for real. Precise
//! per-syscall-boundary injection (landing a kill exactly between
//! `fsync()` and `rename()`, say) needs deeper tooling than a process-level
//! `SIGKILL` can give from user space (crash-safety.md's own rows note the
//! *filesystem's* atomicity is what does that work) — `dm-flakey`-class
//! block-device fault injection is explicitly T-10.2.1's own tool for that,
//! not this module's.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use duet_types::{ErrorKind, Result, Timestamp, VfsError};
use serde::{Deserialize, Serialize};

use crate::job::{JobId, JobKind, JobOutcome, StepFailure};
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
        /// What kind of user-facing operation this job represents
        /// ([`JobKind`]) — persisted here (T-5.2.5) so a recovery scan can
        /// reconstruct a [`RecoveryReport`] with enough context to label
        /// "3 interrupted operations" by kind ("2 copies, 1 move") without
        /// needing the queue's in-memory `Job` to have survived the crash
        /// too, matching this record's existing "the whole `Plan`, not
        /// just its id" precedent one field over.
        kind: JobKind,
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
    /// This job's [`JobKind`], carried straight from its [`JournalRecord::
    /// JobStarted`] record — see that field's own doc comment.
    pub kind: JobKind,
    pub plan: Plan,
    /// Steps whose `Intent` has no matching `Completion` — the exact
    /// remaining work (design.md §9.3: "resume (re-plan the remainder ...)").
    /// Sorted ascending (natural resume order).
    pub incomplete_steps: Vec<u32>,
    /// `(step_index, partial_name)` pairs an `Intent` recorded that
    /// "discard" should clean up and "resume" should either continue
    /// writing to or replace.
    pub orphaned_partials: Vec<(u32, String)>,
    /// Steps whose durable `Completion` is [`StepOutcome::Failed`] or
    /// [`StepOutcome::Skipped`]: finished as far as Intent/Completion
    /// pairing goes, so absent from `incomplete_steps`, but *without having
    /// had their effect*. Sorted ascending.
    ///
    /// A resume must know these. A step still dangling at the crash may be
    /// gated (`depends_on`) on one of them -- the `Remove` of a
    /// cross-device move's source, whose copy failed a moment before the
    /// process died. Treating that dependency as satisfied merely because
    /// it has a `Completion` would delete a source that was never copied;
    /// see [`crate::recovery::prepare_resume`].
    ///
    /// `#[serde(default)]`: reports serialised before this field existed
    /// deserialise as "none", which is what they could express.
    #[serde(default)]
    pub unsuccessful_steps: Vec<u32>,
    /// Steps of `plan` the journal holds no record for at all -- neither
    /// `Intent` nor `Completion`: the part of the job that had not been
    /// reached when the journal ends. Sorted ascending.
    ///
    /// For a job with no `JobFinished` record this is most of "the exact
    /// remaining work": a process killed at file 300 of 10 000 has a few
    /// dozen dangling intents and nine thousand steps it never got to.
    /// `incomplete_steps` alone names only the former, so a resume built
    /// from it alone would finish the in-flight handful and report the job
    /// done. See [`Self::remaining_steps`].
    ///
    /// For a job that *did* reach `JobFinished` (cancelled, aborted on a
    /// conflict) these are steps it deliberately never ran, and they are
    /// not remaining work.
    #[serde(default)]
    pub not_started_steps: Vec<u32>,
    /// `None` if the journal has no `JobFinished` record at all (the job
    /// was mid-execution when the process died); `Some` if it reached a
    /// terminal state but, e.g., a `Completion` for the last step and the
    /// `JobFinished` record were written non-atomically and only one
    /// landed before the crash.
    pub last_outcome: Option<JobOutcome>,
}

impl RecoveryReport {
    /// `true` when the journal has no `JobFinished` record: the process
    /// died (or the journal became unwritable) while the job was running.
    pub fn was_interrupted(&self) -> bool {
        self.last_outcome.is_none()
    }

    /// Every step that still has to run for the job to be what its plan
    /// says, ascending: the dangling intents, plus -- for a job that never
    /// reached `JobFinished` -- everything it had not got to yet *and*
    /// everything that had failed or been skipped. Those last ones were
    /// only ever reported in the job's in-memory report, which died with
    /// the process; unless they count as outstanding, a job that hit an
    /// error and was then killed would look finished and nobody would
    /// ever be told about the error.
    pub fn remaining_steps(&self) -> Vec<u32> {
        let mut remaining = self.incomplete_steps.clone();
        if self.was_interrupted() {
            remaining.extend(self.not_started_steps.iter().copied());
            remaining.extend(self.unsuccessful_steps.iter().copied());
        }
        remaining.sort_unstable();
        remaining.dedup();
        remaining
    }

    /// Whether a startup recovery pass has anything to offer for this job:
    /// work left to finish, or scratch files left to clean up. A job that
    /// ran to a terminal state and left nothing dangling does not.
    pub fn needs_attention(&self) -> bool {
        !self.remaining_steps().is_empty() || !self.orphaned_partials.is_empty()
    }
}

/// Handle to a single job's append-only, fsync'd journal file, conventionally
/// at `~/.local/state/duet/jobs/<id>.journal` (design.md §9.3).
#[derive(Debug)]
pub struct Journal {
    job_id: JobId,
    path: PathBuf,
    file: std::fs::File,
}

/// `state_dir/jobs` — the one place both [`Journal::open`] and
/// [`JournalReader::scan`] need to agree on, kept as a single private
/// helper so the two can't drift apart.
fn jobs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("jobs")
}

fn io_error(err: std::io::Error) -> Box<VfsError> {
    Box::new(VfsError::from_io(err))
}

/// Builds a `Fatal`-classified error for journal content that isn't a
/// truncated-trailing-record (the one *expected* parse failure) but is
/// genuinely malformed — a parse failure anywhere but the last line, or a
/// journal whose first record isn't `JobStarted`.
fn corrupt(path: &Path, detail: impl std::fmt::Display) -> Box<VfsError> {
    Box::new(VfsError::new(
        ErrorKind::Fatal,
        format!("corrupt journal {}: {detail}", path.display()),
    ))
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
        let dir = jobs_dir(state_dir);
        std::fs::create_dir_all(&dir).map_err(io_error)?;
        let path = dir.join(format!("{}.journal", job_id.0));

        let existed = path.exists();
        if existed {
            Self::repair_torn_tail(&path)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        if !existed {
            // See the module doc comment's "Directory-entry durability"
            // section: only needed once, the first time this job's journal
            // file is created.
            let dir_handle = std::fs::File::open(&dir).map_err(io_error)?;
            dir_handle.sync_all().map_err(io_error)?;
        }

        Ok(Journal { job_id, path, file })
    }

    /// Makes an existing journal safe to append to.
    ///
    /// A crash can leave the file ending mid-record: a `SIGKILL` that
    /// lands while `write(2)` is copying a large batch into the page cache
    /// interrupts it with part of the buffer written (observed by the
    /// data-safety suite, T-10.2.1 -- a batch is *not* all-or-nothing
    /// under `SIGKILL`), and a power loss can persist a prefix of one.
    /// [`JournalReader::scan`] reads past such a tail. But appending to it
    /// -- which is what [`Journal::resolve`] does to close an interrupted
    /// job out -- would glue the first new record onto the fragment,
    /// producing an unparseable line *followed by valid ones*: exactly the
    /// shape the reader rejects as corruption, and one corrupt journal
    /// fails the whole startup scan.
    ///
    /// So before the first append: a trailing run of unparseable lines is
    /// cut off (none of it was ever acknowledged as durable -- the batch
    /// it belonged to never got its reply), and a final record that is
    /// whole but lost only its newline gets the newline. A bad line that
    /// already has valid records after it is real corruption, not a crash
    /// artifact, and is left exactly as found for the reader to report.
    fn repair_torn_tail(path: &Path) -> Result<()> {
        let contents = std::fs::read(path).map_err(io_error)?;
        let mut valid_end = 0usize;
        let mut offset = 0usize;
        let mut unparseable_from: Option<usize> = None;
        let mut missing_newline = false;
        for line in contents.split_inclusive(|byte| *byte == b'\n') {
            let terminated = line.ends_with(b"\n");
            let body = if terminated {
                &line[..line.len() - 1]
            } else {
                line
            };
            let parses = body.is_empty() || serde_json::from_slice::<JournalRecord>(body).is_ok();
            if parses {
                if unparseable_from.is_some() {
                    // Valid data after bad data: not a torn tail.
                    return Ok(());
                }
                valid_end = offset + line.len();
                missing_newline = !terminated && !body.is_empty();
            } else {
                unparseable_from.get_or_insert(offset);
            }
            offset += line.len();
        }
        if unparseable_from.is_none() && !missing_newline {
            return Ok(());
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(io_error)?;
        file.set_len(valid_end as u64).map_err(io_error)?;
        if missing_newline {
            use std::io::Seek as _;
            file.seek(std::io::SeekFrom::End(0)).map_err(io_error)?;
            file.write_all(b"\n").map_err(io_error)?;
        }
        file.sync_all().map_err(io_error)
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
        self.append_batch(std::slice::from_ref(record))
    }

    /// Appends every record in `records`, in order, with **one** `write`
    /// and **one** `fsync` for the whole batch -- the group commit that
    /// makes many-small-file jobs affordable (a 10 000-file copy writes
    /// ~40 000 records; at one fsync each on a typical home filesystem that
    /// was minutes of pure journal time, see `docs/perf-baseline.md`).
    /// The durability contract is unchanged: when this returns `Ok`, every
    /// record in the batch is on disk.
    ///
    /// Crash semantics of a batch: the bytes go down in a single `write`,
    /// but neither a `SIGKILL` landing mid-`write` nor a power loss
    /// mid-writeback is all-or-nothing -- either can leave a *prefix* of
    /// the batch, ending in a torn line. [`JournalReader::scan`] tolerates
    /// that as long as nothing parseable follows it, and [`Journal::open`]
    /// removes it before the next append (see the module doc comment's
    /// "Wire format" section).
    ///
    /// # Errors
    /// As [`Self::append`]. On error nothing about the batch is promised;
    /// callers treat the journal as unusable from then on.
    pub fn append_batch(&mut self, records: &[JournalRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(records.len() * 256);
        for record in records {
            serde_json::to_writer(&mut buf, record)
                .map_err(|e| corrupt(&self.path, format!("failed to serialize record: {e}")))?;
            buf.push(b'\n');
        }
        self.file.write_all(&buf).map_err(io_error)?;
        self.file.sync_all().map_err(io_error)?;
        Ok(())
    }

    /// Durably resolves every one of `report`'s still-dangling intents as
    /// deliberately abandoned (never completed, and not going to be —
    /// either because their remaining work was just handed off to a fresh
    /// job's own journal via T-5.2.5's resume, or because the operator
    /// just discarded the orphaned partials via T-5.2.5's discard), then
    /// closes the job out as `Cancelled`. After this call, this exact
    /// `RecoveryReport` will never again appear in a [`JournalReader::
    /// scan`] — every previously-dangling `Intent` now has a matching
    /// `Completion`, so `fold`'s Intent/Completion pairing no longer finds
    /// anything incomplete, and `last_outcome` becomes
    /// `Some(JobOutcome::Cancelled)`.
    ///
    /// `self` is consumed (not `&mut self`) because nothing should append
    /// to this handle again afterward — the caller re-opens fresh if it
    /// ever needs to touch this job's journal again, matching
    /// [`Journal::open`]'s own "reopening an existing journal appends,
    /// never truncates" contract.
    ///
    /// # Errors
    /// Same as [`Journal::append`] — a `Completion`/`JobFinished` record
    /// failing to durably write. A caller that gets an error here cannot
    /// assume the job was actually closed out; re-scanning is the only way
    /// to know how far this call got.
    pub fn resolve(mut self, report: &RecoveryReport, reason: &str) -> Result<()> {
        for &step_index in &report.incomplete_steps {
            self.append(&JournalRecord::Completion {
                step_index,
                outcome: StepOutcome::Skipped {
                    reason: reason.to_string(),
                },
            })?;
        }
        self.append(&JournalRecord::JobFinished {
            outcome: JobOutcome::Cancelled,
            finished_at: Timestamp::from(std::time::SystemTime::now()),
        })
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
    /// A `state_dir` with no `jobs/` directory yet (nothing has ever run)
    /// yields an empty `Vec`, not an error.
    ///
    /// # Errors
    /// - `Permission`/`Fatal` — reading `state_dir` or a journal file
    ///   failed outright (as opposed to a journal being merely truncated
    ///   mid-record, which is an *expected* SIGKILL artifact this method
    ///   must handle by treating the trailing partial record as absent, not
    ///   erroring the whole scan).
    pub fn scan(state_dir: &Path) -> Result<Vec<RecoveryReport>> {
        let dir = jobs_dir(state_dir);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(e)),
        };

        let mut reports = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("journal") {
                continue;
            }
            if let Some(report) = Self::replay_one(&path)? {
                reports.push(report);
            }
        }
        Ok(reports)
    }

    /// Replays a single journal file. `None` means the file contained no
    /// recoverable records at all (e.g. a `SIGKILL` landed before even the
    /// first `JobStarted` record's `fsync` completed) — not an error, just
    /// nothing to report.
    fn replay_one(path: &Path) -> Result<Option<RecoveryReport>> {
        let contents = std::fs::read_to_string(path).map_err(io_error)?;
        if contents.is_empty() {
            return Ok(None);
        }

        // Records are appended in batches written by one `write` and one
        // `fsync` (see the module doc comment's "Wire format" section), so
        // the crash artifact to tolerate is a torn *tail*: the file's last
        // batch may end in a partial line, or (power loss mid-writeback) in
        // a run of partial/garbage lines. What is never tolerated is a bad
        // line *followed by a good one* -- bytes that failed to parse with
        // durable records after them cannot be a crash artifact of an
        // append-only file, so that is real corruption and a hard error.
        let lines: Vec<&str> = contents.split('\n').filter(|l| !l.is_empty()).collect();
        let mut records = Vec::with_capacity(lines.len());
        let mut torn_from: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(record) => {
                    if let Some(first_bad) = torn_from {
                        return Err(corrupt(
                            path,
                            format!(
                                "unparseable record {:?} is followed by a valid one -- not a torn tail",
                                lines[first_bad]
                            ),
                        ));
                    }
                    records.push(record);
                }
                Err(_) => {
                    torn_from.get_or_insert(i);
                }
            }
        }

        Self::fold(path, records)
    }

    /// Turns an ordered, already-parsed record list into a
    /// [`RecoveryReport`], pairing each `Intent` with its `Completion` (if
    /// any).
    fn fold(path: &Path, records: Vec<JournalRecord>) -> Result<Option<RecoveryReport>> {
        let Some(first) = records.first() else {
            return Ok(None);
        };
        let JournalRecord::JobStarted {
            job_id,
            plan,
            kind,
            started_at: _,
        } = first
        else {
            return Err(corrupt(
                path,
                "first record is not JobStarted -- journal is malformed",
            ));
        };

        let mut intents: std::collections::HashMap<u32, Option<String>> =
            std::collections::HashMap::new();
        let mut completed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        // Last `Completion` wins: a step retried after a recovery, or
        // closed out by `Journal::resolve`, is whatever its final record
        // says it is.
        let mut succeeded: std::collections::HashMap<u32, bool> = std::collections::HashMap::new();
        let mut last_outcome = None;

        for record in &records[1..] {
            match record {
                JournalRecord::Intent {
                    step_index,
                    partial_name,
                    ..
                } => {
                    intents.insert(*step_index, partial_name.clone());
                }
                JournalRecord::Completion {
                    step_index,
                    outcome,
                } => {
                    completed.insert(*step_index);
                    succeeded.insert(*step_index, matches!(outcome, StepOutcome::Succeeded));
                }
                JournalRecord::JobFinished { outcome, .. } => {
                    last_outcome = Some(*outcome);
                }
                JournalRecord::JobStarted { .. } => {
                    return Err(corrupt(path, "a second JobStarted record"));
                }
            }
        }

        let mut incomplete_steps: Vec<u32> = intents
            .keys()
            .filter(|ix| !completed.contains(ix))
            .copied()
            .collect();
        incomplete_steps.sort_unstable();

        let mut orphaned_partials: Vec<(u32, String)> = intents
            .iter()
            .filter(|(ix, _)| !completed.contains(ix))
            .filter_map(|(ix, partial)| partial.clone().map(|name| (*ix, name)))
            .collect();
        orphaned_partials.sort_unstable();

        let not_started_steps: Vec<u32> = (0..plan.steps.len() as u32)
            .filter(|ix| !intents.contains_key(ix) && !completed.contains(ix))
            .collect();

        let mut unsuccessful_steps: Vec<u32> = succeeded
            .iter()
            .filter(|(_, ok)| !**ok)
            .map(|(ix, _)| *ix)
            .collect();
        unsuccessful_steps.sort_unstable();

        Ok(Some(RecoveryReport {
            job_id: *job_id,
            kind: *kind,
            plan: plan.clone(),
            incomplete_steps,
            orphaned_partials,
            unsuccessful_steps,
            not_started_steps,
            last_outcome,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use duet_types::{UnixPathBuf, VPath};
    use tempfile::TempDir;

    use super::*;
    use crate::job::JobOutcome;
    use crate::plan::PlanOptions;

    fn test_vpath(n: u32) -> VPath {
        VPath::local(UnixPathBuf::new(&format!("/tmp/dest-{n}")).unwrap())
    }

    #[test]
    fn open_creates_the_jobs_directory_and_an_empty_journal_file() {
        let dir = TempDir::new().unwrap();
        let journal = Journal::open(JobId(1), dir.path()).unwrap();
        assert_eq!(journal.path(), dir.path().join("jobs").join("1.journal"));
        assert!(journal.path().exists());
        assert_eq!(std::fs::metadata(journal.path()).unwrap().len(), 0);
        assert_eq!(journal.job_id(), JobId(1));
    }

    fn started(job: u64) -> JournalRecord {
        JournalRecord::JobStarted {
            job_id: JobId(job),
            started_at: Timestamp::EPOCH,
            plan: Plan::new(Vec::new(), PlanOptions::default()),
            kind: JobKind::Copy,
        }
    }

    fn finished() -> JournalRecord {
        JournalRecord::JobFinished {
            outcome: JobOutcome::Cancelled,
            finished_at: Timestamp::EPOCH,
        }
    }

    /// T-10.2.1: a kill that lands inside the `write(2)` of a batch leaves
    /// the file ending mid-record. Appending after that fragment (which is
    /// what closing an interrupted job out does) used to glue the new
    /// record onto it and turn a recoverable journal into one the reader
    /// rejects as corrupt -- failing the whole startup scan.
    #[test]
    fn appending_after_a_torn_tail_does_not_corrupt_the_journal() {
        let dir = TempDir::new().unwrap();
        let path = {
            let mut journal = Journal::open(JobId(7), dir.path()).unwrap();
            journal.append(&started(7)).unwrap();
            journal.path().to_path_buf()
        };
        // Half a record, no newline: the torn write.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"Intent":{"step_index":3,"step":{"Rem"#)
            .unwrap();
        drop(file);
        assert_eq!(JournalReader::scan(dir.path()).unwrap().len(), 1);

        Journal::open(JobId(7), dir.path())
            .unwrap()
            .append(&finished())
            .unwrap();

        let reports =
            JournalReader::scan(dir.path()).expect("the journal must still scan after the append");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].last_outcome, Some(JobOutcome::Cancelled));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "the fragment is gone: {text:?}");
    }

    /// The tear can also fall exactly between a record and its newline:
    /// the record is whole and the reader counts it, so it must be kept,
    /// not glued to the next one.
    #[test]
    fn appending_after_a_record_that_lost_only_its_newline_keeps_both() {
        let dir = TempDir::new().unwrap();
        let path = {
            let mut journal = Journal::open(JobId(8), dir.path()).unwrap();
            journal.append(&started(8)).unwrap();
            journal.path().to_path_buf()
        };
        let intact = serde_json::to_string(&JournalRecord::Completion {
            step_index: 0,
            outcome: StepOutcome::Succeeded,
        })
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(intact.as_bytes()).unwrap();
        drop(file);

        Journal::open(JobId(8), dir.path())
            .unwrap()
            .append(&finished())
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "{text:?}");
        assert!(
            text.lines()
                .all(|line| serde_json::from_str::<JournalRecord>(line).is_ok())
        );
        assert_eq!(JournalReader::scan(dir.path()).unwrap().len(), 1);
    }

    /// A bad line with valid records already after it is not a crash
    /// artifact; reopening must not quietly "repair" it away.
    #[test]
    fn reopening_leaves_real_corruption_for_the_reader_to_report() {
        let dir = TempDir::new().unwrap();
        let path = {
            let mut journal = Journal::open(JobId(9), dir.path()).unwrap();
            journal.append(&started(9)).unwrap();
            journal.path().to_path_buf()
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"this is not a record\n").unwrap();
        file.write_all(serde_json::to_string(&finished()).unwrap().as_bytes())
            .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let before = std::fs::read(&path).unwrap();

        drop(Journal::open(JobId(9), dir.path()).unwrap());

        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(JournalReader::scan(dir.path()).is_err());
    }

    #[test]
    fn reopening_an_existing_journal_appends_rather_than_truncating() {
        let dir = TempDir::new().unwrap();
        {
            let mut journal = Journal::open(JobId(2), dir.path()).unwrap();
            journal
                .append(&JournalRecord::JobStarted {
                    job_id: JobId(2),
                    started_at: Timestamp::EPOCH,
                    plan: Plan::new(Vec::new(), PlanOptions::default()),
                    kind: JobKind::Copy,
                })
                .unwrap();
        }
        {
            let mut journal = Journal::open(JobId(2), dir.path()).unwrap();
            journal
                .append(&JournalRecord::JobFinished {
                    outcome: JobOutcome::Completed,
                    finished_at: Timestamp::EPOCH,
                })
                .unwrap();
        }

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].last_outcome, Some(JobOutcome::Completed));
    }

    #[test]
    fn scan_returns_empty_when_no_jobs_have_ever_run() {
        let dir = TempDir::new().unwrap();
        assert_eq!(JournalReader::scan(dir.path()).unwrap(), Vec::new());
    }

    #[test]
    fn scan_reports_a_finished_job_with_no_incomplete_steps() {
        let dir = TempDir::new().unwrap();
        let plan = Plan::new(
            vec![Step::CreateDir {
                dest: test_vpath(0),
                mode: None,
            }],
            PlanOptions::default(),
        );
        let mut journal = Journal::open(JobId(3), dir.path()).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id: JobId(3),
                started_at: Timestamp::EPOCH,
                plan: plan.clone(),
                kind: JobKind::Copy,
            })
            .unwrap();
        journal
            .append(&JournalRecord::Intent {
                step_index: 0,
                step: plan.steps[0].clone(),
                partial_name: None,
            })
            .unwrap();
        journal
            .append(&JournalRecord::Completion {
                step_index: 0,
                outcome: StepOutcome::Succeeded,
            })
            .unwrap();
        journal
            .append(&JournalRecord::JobFinished {
                outcome: JobOutcome::Completed,
                finished_at: Timestamp::EPOCH,
            })
            .unwrap();

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].job_id, JobId(3));
        assert_eq!(reports[0].plan, plan);
        assert!(reports[0].incomplete_steps.is_empty());
        assert!(reports[0].orphaned_partials.is_empty());
        assert_eq!(reports[0].last_outcome, Some(JobOutcome::Completed));
    }

    #[test]
    fn scan_reports_incomplete_steps_and_orphaned_partials_for_a_mid_job_crash() {
        let dir = TempDir::new().unwrap();
        let plan = Plan::new(
            vec![
                Step::CopyFile {
                    source: test_vpath(0),
                    dest: test_vpath(0),
                    size: 5,
                    conflict: None,
                },
                Step::CopyFile {
                    source: test_vpath(1),
                    dest: test_vpath(2),
                    size: 10,
                    conflict: None,
                },
            ],
            PlanOptions::default(),
        );
        let mut journal = Journal::open(JobId(4), dir.path()).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id: JobId(4),
                started_at: Timestamp::EPOCH,
                plan: plan.clone(),
                kind: JobKind::Copy,
            })
            .unwrap();
        // Step 0: fully completed -- crucially, its own Intent also
        // carried a partial_name (a completed CopyFile still stages
        // through a partial while it's running), which must NOT leak into
        // `orphaned_partials` once the step is known complete.
        journal
            .append(&JournalRecord::Intent {
                step_index: 0,
                step: plan.steps[0].clone(),
                partial_name: Some(".duet-partial-zzz999-dest0".to_string()),
            })
            .unwrap();
        journal
            .append(&JournalRecord::Completion {
                step_index: 0,
                outcome: StepOutcome::Succeeded,
            })
            .unwrap();
        // Step 1: Intent only -- the crash landed mid-copy, with a
        // partial staged.
        journal
            .append(&JournalRecord::Intent {
                step_index: 1,
                step: plan.steps[1].clone(),
                partial_name: Some(".duet-partial-abc123-dest".to_string()),
            })
            .unwrap();
        // No JobFinished -- the job never reached a terminal state.

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.incomplete_steps, vec![1]);
        assert_eq!(
            report.orphaned_partials,
            vec![(1, ".duet-partial-abc123-dest".to_string())]
        );
        assert_eq!(report.last_outcome, None);
    }

    /// T-5.2.5: `JournalRecord::JobStarted`'s new `kind` field must survive
    /// a real serde round-trip, not just compile -- the wire format is
    /// newline-delimited JSON (see the module doc comment's "Wire format"
    /// section), so a field that's present in the Rust struct but silently
    /// dropped or misnamed in the `Serialize`/`Deserialize` derive would
    /// still compile fine and only fail at runtime.
    #[test]
    fn job_started_kind_round_trips_through_serde() {
        let record = JournalRecord::JobStarted {
            job_id: JobId(1),
            started_at: Timestamp::EPOCH,
            plan: Plan::new(Vec::new(), PlanOptions::default()),
            kind: JobKind::Delete { permanent: true },
        };
        let line = serde_json::to_string(&record).unwrap();
        let back: JournalRecord = serde_json::from_str(&line).unwrap();
        match back {
            JournalRecord::JobStarted { kind, .. } => {
                assert_eq!(kind, JobKind::Delete { permanent: true });
            }
            other => panic!("expected JobStarted, got {other:?}"),
        }
    }

    /// A `RecoveryReport` built by a real `JournalReader::scan`/`fold` must
    /// carry the same `kind` its `JobStarted` record was written with --
    /// this is what lets T-5.2.5's startup recovery UI label each
    /// interrupted operation ("2 copies, 1 move") without re-deriving it
    /// from the plan's step shapes.
    #[test]
    fn scan_carries_the_jobs_kind_into_the_recovery_report() {
        let dir = TempDir::new().unwrap();
        let plan = Plan::new(
            vec![Step::CreateDir {
                dest: test_vpath(0),
                mode: None,
            }],
            PlanOptions::default(),
        );
        let mut journal = Journal::open(JobId(5), dir.path()).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id: JobId(5),
                started_at: Timestamp::EPOCH,
                plan: plan.clone(),
                kind: JobKind::Move,
            })
            .unwrap();
        // A dangling Intent -- otherwise irrelevant to this test, just
        // needed to keep the journal a realistic mid-job shape.
        journal
            .append(&JournalRecord::Intent {
                step_index: 0,
                step: plan.steps[0].clone(),
                partial_name: None,
            })
            .unwrap();

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].kind, JobKind::Move);
    }

    /// `Journal::resolve` must not just return `Ok(())` -- it must actually
    /// close the loop: after resolving, re-scanning the exact same
    /// `state_dir` must show the job as fully accounted for (no more
    /// incomplete steps) and terminally `Cancelled`, proving the appended
    /// `Completion`/`JobFinished` records are both durable and correctly
    /// paired by a fresh `JournalReader::fold`, not just self-consistent
    /// from `resolve`'s own point of view.
    #[test]
    fn resolve_closes_out_a_dangling_job_so_a_rescan_shows_it_fully_accounted_for() {
        let dir = TempDir::new().unwrap();
        let plan = Plan::new(
            vec![
                Step::CreateDir {
                    dest: test_vpath(0),
                    mode: None,
                },
                Step::CreateDir {
                    dest: test_vpath(1),
                    mode: None,
                },
            ],
            PlanOptions::default(),
        );
        let job_id = JobId(6);
        let mut journal = Journal::open(job_id, dir.path()).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id,
                started_at: Timestamp::EPOCH,
                plan: plan.clone(),
                kind: JobKind::CreateDir,
            })
            .unwrap();
        // Step 0: genuinely completed.
        journal
            .append(&JournalRecord::Intent {
                step_index: 0,
                step: plan.steps[0].clone(),
                partial_name: None,
            })
            .unwrap();
        journal
            .append(&JournalRecord::Completion {
                step_index: 0,
                outcome: StepOutcome::Succeeded,
            })
            .unwrap();
        // Step 1: Intent only -- the one genuinely dangling step.
        journal
            .append(&JournalRecord::Intent {
                step_index: 1,
                step: plan.steps[1].clone(),
                partial_name: None,
            })
            .unwrap();

        let reports_before = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports_before.len(), 1);
        assert_eq!(reports_before[0].incomplete_steps, vec![1]);
        assert_eq!(reports_before[0].last_outcome, None);

        let journal = Journal::open(job_id, dir.path()).unwrap();
        journal.resolve(&reports_before[0], "test reason").unwrap();

        let reports_after = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports_after.len(), 1);
        assert!(
            reports_after[0].incomplete_steps.is_empty(),
            "resolve must leave no incomplete steps behind on a fresh rescan"
        );
        assert_eq!(reports_after[0].last_outcome, Some(JobOutcome::Cancelled));
    }

    /// Group commit (2026-09-06): a batch is one `write` + one `fsync`, and
    /// every record in it is recovered, in order, exactly as if appended
    /// one at a time.
    #[test]
    fn append_batch_records_are_all_recovered_in_order() {
        let dir = TempDir::new().unwrap();
        let mut journal = Journal::open(JobId(3), dir.path()).unwrap();
        let mut batch = vec![JournalRecord::JobStarted {
            job_id: JobId(3),
            started_at: Timestamp::EPOCH,
            plan: Plan::new(Vec::new(), PlanOptions::default()),
            kind: JobKind::Copy,
        }];
        for i in 0..200u32 {
            batch.push(JournalRecord::Intent {
                step_index: i,
                step: Step::CreateDir {
                    dest: test_vpath(i),
                    mode: None,
                },
                partial_name: None,
            });
        }
        // Complete all but the last three, in a second batch.
        journal.append_batch(&batch).unwrap();
        let completions: Vec<JournalRecord> = (0..197u32)
            .map(|i| JournalRecord::Completion {
                step_index: i,
                outcome: StepOutcome::Succeeded,
            })
            .collect();
        journal.append_batch(&completions).unwrap();

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].incomplete_steps, vec![197, 198, 199]);
        assert_eq!(reports[0].last_outcome, None);
    }

    /// The batch crash artifact: a power loss mid-writeback can leave a
    /// *prefix* of the last batch ending in one or more unparseable lines.
    /// Everything before the first bad line is recovered; the bad tail is
    /// dropped, not treated as corruption.
    #[test]
    fn scan_drops_a_multi_line_torn_tail() {
        let dir = TempDir::new().unwrap();
        let mut journal = Journal::open(JobId(4), dir.path()).unwrap();
        journal
            .append_batch(&[
                JournalRecord::JobStarted {
                    job_id: JobId(4),
                    started_at: Timestamp::EPOCH,
                    plan: Plan::new(Vec::new(), PlanOptions::default()),
                    kind: JobKind::Copy,
                },
                JournalRecord::Intent {
                    step_index: 0,
                    step: Step::CreateDir {
                        dest: test_vpath(0),
                        mode: None,
                    },
                    partial_name: None,
                },
            ])
            .unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"Completion\":{\"step_in\n\0\0\0\0\n{\"Intent\":{\"ste")
            .unwrap();
        file.sync_all().unwrap();

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].incomplete_steps,
            vec![0],
            "the intact prefix is recovered; the torn tail counts for nothing"
        );
    }

    /// What is never tolerated: an unparseable line *followed by a valid
    /// record*. An append-only file cannot produce that shape by crashing,
    /// so it is real corruption and a hard error, exactly as before batching.
    #[test]
    fn scan_rejects_garbage_followed_by_a_valid_record() {
        let dir = TempDir::new().unwrap();
        let mut journal = Journal::open(JobId(5), dir.path()).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id: JobId(5),
                started_at: Timestamp::EPOCH,
                plan: Plan::new(Vec::new(), PlanOptions::default()),
                kind: JobKind::Copy,
            })
            .unwrap();
        let path = journal.path().to_path_buf();
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"not json at all\n").unwrap();
            file.sync_all().unwrap();
        }
        journal
            .append(&JournalRecord::Completion {
                step_index: 0,
                outcome: StepOutcome::Succeeded,
            })
            .unwrap();

        let err = JournalReader::scan(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("followed by a valid one"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scan_treats_a_truncated_trailing_record_as_absent_not_an_error() {
        let dir = TempDir::new().unwrap();
        let job_id = JobId(7);
        {
            let mut journal = Journal::open(job_id, dir.path()).unwrap();
            journal
                .append(&JournalRecord::JobStarted {
                    job_id,
                    started_at: Timestamp::EPOCH,
                    plan: Plan::new(
                        vec![Step::CreateDir {
                            dest: test_vpath(0),
                            mode: None,
                        }],
                        PlanOptions::default(),
                    ),
                    kind: JobKind::Copy,
                })
                .unwrap();
            journal
                .append(&JournalRecord::Intent {
                    step_index: 0,
                    step: Step::CreateDir {
                        dest: test_vpath(0),
                        mode: None,
                    },
                    partial_name: None,
                })
                .unwrap();
        }

        // Simulate a SIGKILL landing mid-`write()` of a third record:
        // append half of a well-formed Completion record's bytes, with no
        // trailing newline -- exactly what an interrupted `write()` leaves
        // behind (see the module doc comment's "Wire format" section for
        // why a torn record can only ever be the last one).
        let completion = JournalRecord::Completion {
            step_index: 0,
            outcome: StepOutcome::Succeeded,
        };
        let full_line = serde_json::to_string(&completion).unwrap();
        let torn = &full_line[..full_line.len() / 2];
        let path = dir
            .path()
            .join("jobs")
            .join(format!("{}.journal", job_id.0));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(torn.as_bytes()).unwrap();
        file.sync_all().unwrap();

        let reports = JournalReader::scan(dir.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].incomplete_steps,
            vec![0],
            "the torn Completion must not count -- step 0 must still read as incomplete"
        );
    }

    /// Real, not simulated: forks this test binary as a child process that
    /// writes real `Intent` records through [`Journal::append`] (with a
    /// real `fsync` per call) in a tight loop, sends it a genuine
    /// `SIGKILL` after a short delay, then confirms [`JournalReader::scan`]
    /// recovers a well-formed [`RecoveryReport`] from whatever the child
    /// got through before it died.
    ///
    /// The child writes a huge number of `Intent`-only records (no
    /// `Completion`s at all) before it could plausibly finish, rather than
    /// interleaved `Intent`+`Completion` pairs -- that makes the outcome
    /// deterministic: the kill is guaranteed to land during the
    /// intents-only phase, so `incomplete_steps` is guaranteed non-empty
    /// regardless of how many records actually got through before death,
    /// rather than being sensitive to exactly which of a pair survived.
    ///
    /// Re-execs the test binary itself (`std::env::current_exe()`) filtered
    /// to just this one test, rather than pulling in a process-forking
    /// crate — a plain `std::process::Command`/`Child::kill()` (which sends
    /// a real `SIGKILL` on Unix) is all this needs. Fragile in one specific
    /// way: `CHILD_TEST_NAME` below must be kept in sync with this
    /// function's own fully-qualified name if it's ever renamed.
    #[test]
    fn real_sigkill_produces_a_journal_scan_can_recover() {
        const CHILD_DIR_ENV: &str = "DUET_JOURNAL_CRASH_CHILD_DIR";
        const CHILD_TEST_NAME: &str =
            "journal::tests::real_sigkill_produces_a_journal_scan_can_recover";

        if let Ok(dir) = std::env::var(CHILD_DIR_ENV) {
            // Child branch: write as fast as possible until SIGKILLed.
            // The loop bound (10 million) is chosen only to be "clearly
            // unreachable within the parent's kill delay below" -- since
            // the process is killed long before it could finish, the bound
            // itself costs nothing in wall-clock time.
            let mut journal = Journal::open(JobId(99), Path::new(&dir)).unwrap();
            journal
                .append(&JournalRecord::JobStarted {
                    job_id: JobId(99),
                    started_at: Timestamp::EPOCH,
                    plan: Plan::new(Vec::new(), PlanOptions::default()),
                    kind: JobKind::Copy,
                })
                .unwrap();
            for step_index in 0..10_000_000u32 {
                journal
                    .append(&JournalRecord::Intent {
                        step_index,
                        step: Step::CreateDir {
                            dest: test_vpath(step_index),
                            mode: None,
                        },
                        partial_name: None,
                    })
                    .unwrap();
            }
            return;
        }

        // Parent branch.
        let dir = TempDir::new().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .args([CHILD_TEST_NAME, "--exact", "--nocapture"])
            .env(CHILD_DIR_ENV, dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn crash-test child");

        // Kill only once the child has demonstrably written *something*:
        // a fixed delay is a race against process start-up plus one
        // fsync per record, which on a CI runner's ext4 root disk can
        // exceed 30ms before the first Intent lands (observed 2026-09-04:
        // an empty journal, `reports.len() == 0`). Waiting for the file
        // to pass a few KiB guarantees the kill hits the intents-only
        // phase -- 10 million records lie ahead of it -- on any machine.
        let journal_path = dir.path().join("jobs").join("99.journal");
        let wait_start = Instant::now();
        while std::fs::metadata(&journal_path).map_or(0, |m| m.len()) < 4096 {
            assert!(
                wait_start.elapsed() < Duration::from_secs(10),
                "the crash-test child wrote fewer than 4 KiB of journal in 10s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child
            .kill()
            .expect("failed to SIGKILL the crash-test child");
        let status = child.wait().expect("failed to reap the crash-test child");
        assert!(
            !status.success(),
            "child should have been killed, not exited normally -- \
             the kill delay may need to be shortened for this machine"
        );

        let reports =
            JournalReader::scan(dir.path()).expect("scan must succeed on a SIGKILLed journal");
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.job_id, JobId(99));
        assert!(
            !report.incomplete_steps.is_empty(),
            "the intents-only phase should have been interrupted mid-flight, \
             leaving at least one Intent with no Completion"
        );
        assert!(report.orphaned_partials.is_empty());
        assert_eq!(report.last_outcome, None);
    }
}
