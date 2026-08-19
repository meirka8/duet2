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
//! trivial: a line that fails to parse as JSON, or a final line with no
//! trailing `\n` at all, just gets dropped. A record's bytes are written by
//! a single `write_all` call immediately followed by `fsync`, so a crash
//! can only ever truncate the *last* record, never corrupt one in the
//! middle — [`JournalReader::scan`] treats a parse failure anywhere before
//! the last line as real corruption (a hard error), not a recoverable gap.
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
    /// Sorted ascending (natural resume order).
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
        let mut line = serde_json::to_string(record)
            .map_err(|e| corrupt(&self.path, format!("failed to serialize record: {e}")))?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).map_err(io_error)?;
        self.file.sync_all().map_err(io_error)?;
        Ok(())
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

        // Every complete record's bytes end in '\n' (see the module doc
        // comment's "Wire format" section) -- so only when the file does
        // *not* end in '\n' can its last non-empty line possibly be a torn
        // record. When it does, every non-empty line is a complete record
        // and a parse failure anywhere is real corruption, not a crash
        // artifact.
        let ends_with_newline = contents.ends_with('\n');
        let mut lines: Vec<&str> = contents.split('\n').filter(|l| !l.is_empty()).collect();
        let torn_tail = if ends_with_newline { None } else { lines.pop() };

        let mut records = Vec::with_capacity(lines.len() + 1);
        for line in lines {
            let record: JournalRecord = serde_json::from_str(line)
                .map_err(|e| corrupt(path, format!("unparseable record {line:?}: {e}")))?;
            records.push(record);
        }
        if let Some(tail) = torn_tail {
            // A parse failure here is the expected SIGKILL artifact --
            // silently dropped, not an error.
            if let Ok(record) = serde_json::from_str::<JournalRecord>(tail) {
                records.push(record);
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
                JournalRecord::Completion { step_index, .. } => {
                    completed.insert(*step_index);
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

        Ok(Some(RecoveryReport {
            job_id: *job_id,
            plan: plan.clone(),
            incomplete_steps,
            orphaned_partials,
            last_outcome,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

        std::thread::sleep(Duration::from_millis(30));
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
