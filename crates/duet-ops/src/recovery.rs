// SPDX-License-Identifier: MIT
//! T-5.2.5's "finish what an interrupted job was doing, or clean up after
//! it" planner: rebuild a [`Plan`] from a [`RecoveryReport`]'s dangling
//! steps, and locate an orphaned partial file's real on-disk path so a
//! "discard" action can actually delete it.
//!
//! # Why this mirrors [`crate::rerun`] so closely
//!
//! `rerun::plan_from_report` (T-5.2.4, "re-run the failed/skipped items of
//! a *finished* job") and [`plan_from_recovery`] (T-5.2.5, "resume the
//! remaining steps of a job a crash *interrupted*") are the same
//! transformation -- "keep only these step indices out of the original
//! plan, remapping `depends_on` onto the smaller list" -- applied to two
//! different sources of "which indices." A finished job's `JobReport`
//! names them via `errors`/`skipped`; a crash-recovered `RecoveryReport`
//! names them via `incomplete_steps` (every `Intent` that never got a
//! matching `Completion` -- see [`crate::journal`]'s module doc comment).
//! [`rerun::rebuild_subset`] is the one place that actual remap logic
//! lives; this module is a thin wrapper over it, not a second
//! implementation that could drift from the first.
//!
//! # Why `orphaned_partial_path` lives here, not in `executor.rs`
//!
//! The `.duet-partial-<rand>-<name>` sibling path a `CopyFile`/`Reflink`
//! step stages through is derived once, at execution time, by
//! `executor.rs`'s own `copy_file_step`. Recovery needs to derive that
//! *same* path again, after the fact, purely from a `RecoveryReport`'s
//! already-durable `plan` and `orphaned_partials` -- no live `FileSystem`,
//! no re-running any step. That is a planning-time concern (turning
//! recorded intent into a path a caller can act on), not an execution-time
//! one, which is why it sits alongside [`plan_from_recovery`] here rather
//! than as a second copy bolted onto `executor.rs`.

//!
//! # Resuming is more than "redo the dangling intents" (T-10.2.1)
//!
//! [`plan_from_recovery`] is the mechanical subset rebuild. On its own it
//! is not a safe resume, for four reasons the data-safety suite turned
//! from theory into failing tests; [`prepare_resume`] is the entry point
//! that handles all four and is what a caller should use:
//!
//! 1. **A dangling intent does not mean the step did not happen.** The
//!    crash window between a step's side effect and its `Completion`
//!    reaching the disk is real for every kind (`docs/crash-safety.md`,
//!    the "after ..., before `Completion` fsync'd" rows). Redoing a rename
//!    whose source is already at the destination fails with a phantom
//!    "not found"; redoing a `WriteTrashInfo` collides with the sidecar it
//!    already wrote and blocks the move gated on it. [`prepare_resume`]
//!    looks at the filesystem and drops the steps that have demonstrably
//!    taken effect.
//! 2. **A dependency with a `Completion` is not a dependency that
//!    succeeded.** `rebuild_subset` clears a `depends_on` that points
//!    outside the redo set, which is right when that step succeeded and
//!    catastrophically wrong when its durable outcome is `Failed` or
//!    `Skipped`: the cross-device move whose copy failed, then crashed
//!    after journaling the source `Remove`'s intent, would resume into an
//!    ungated delete of the only copy. [`prepare_resume`] pulls such
//!    dependencies back into the redo set so the gate is re-evaluated.
//! 3. **The dangling intents are not the remaining work.** A job killed
//!    at file 300 of 10 000 has a window of dangling intents and nine
//!    thousand steps with no record at all. Resuming only the former
//!    finishes a few dozen files and reports the job done.
//!    [`prepare_resume`] resumes [`RecoveryReport::remaining_steps`].
//! 4. **The orphaned partials are superseded, not continued.** A resumed
//!    copy stages through a fresh `.duet-partial-*` name; the interrupted
//!    one's file would otherwise stay behind forever, since the journal
//!    that names it is closed out by the resume. [`prepare_resume`]
//!    removes them first, through the same [`discard_orphaned_partials`]
//!    the "discard" action uses.

use std::collections::BTreeSet;

use duet_types::{ErrorKind, Result, VPath};
use duet_vfs::{FileSystem, ListOpts, RemoveKind};
use futures_util::StreamExt;
use tokio::io::AsyncReadExt;

use crate::journal::RecoveryReport;
use crate::plan::Plan;
use crate::step::Step;

/// Builds the "finish what was interrupted" plan for a `RecoveryReport`:
/// every step named by `report.incomplete_steps`, cloned out of
/// `report.plan` in ascending order, with `depends_on` remapped exactly as
/// [`crate::rerun::rebuild_subset`] documents (this *is* that same
/// transformation, applied to a crash-recovered redo set instead of a
/// re-run-failed one).
///
/// An empty `incomplete_steps` produces a valid, empty-steps `Plan` --
/// callers should not call this for a report with nothing incomplete
/// (`report.incomplete_steps.is_empty() && report.orphaned_partials.
/// is_empty()` means there's nothing to resume, only maybe partials to
/// discard), but it's a safe no-op rather than a panic if they do.
pub fn plan_from_recovery(report: &RecoveryReport) -> Plan {
    let redo: std::collections::BTreeSet<u32> = report.incomplete_steps.iter().copied().collect();
    crate::rerun::rebuild_subset(&report.plan, redo)
}

/// What [`prepare_resume`] decided about an interrupted job.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumePlan {
    /// The steps that still have to run, as a self-contained plan
    /// (`depends_on` remapped onto it). Empty when every dangling step
    /// turned out to have taken effect already -- there is then nothing to
    /// enqueue, only the old journal to close out.
    pub plan: Plan,
    /// Original step indices `plan` was built from, ascending; `plan.
    /// steps[i]` is the original plan's step `redo[i]`.
    pub redo: Vec<u32>,
    /// Dangling steps found to have already taken effect on disk, and so
    /// left out of `plan`. Ascending.
    pub already_applied: Vec<u32>,
}

/// Turns a crash-recovered [`RecoveryReport`] into the plan that finishes
/// the job, doing the three things the module doc comment's "Resuming is
/// more than..." section lists: removes the orphaned partials, drops the
/// dangling steps whose effect is already on disk, and re-gates anything
/// that depended on a step the journal records as failed or skipped.
///
/// This touches the filesystem (it removes the orphaned partials and
/// `stat`s/reads the paths the dangling steps name), so it runs wherever
/// the executor would -- never on a UI thread.
///
/// The caller still owns the two steps around it: enqueueing
/// [`ResumePlan::plan`] as a fresh job (when it has any steps) and then
/// closing the interrupted job's journal with
/// [`crate::Journal::resolve`].
///
/// # Errors
/// Only from removing an orphaned partial (see
/// [`discard_orphaned_partials`]). Inspection failures while deciding
/// whether a step already took effect are never errors: an unanswerable
/// question means "redo it", and the executor reports whatever is really
/// wrong when it tries.
pub async fn prepare_resume(fs: &dyn FileSystem, report: &RecoveryReport) -> Result<ResumePlan> {
    discard_orphaned_partials(fs, report).await?;

    let mut redo: BTreeSet<u32> = BTreeSet::new();
    let mut already_applied: Vec<u32> = Vec::new();
    for &index in &report.incomplete_steps {
        let Some(step) = report.plan.steps.get(index as usize) else {
            continue;
        };
        if already_took_effect(fs, step).await {
            already_applied.push(index);
        } else {
            redo.insert(index);
        }
    }
    // The steps the job never reached are remaining work too -- for a
    // killed job, usually most of it. They have no intent, so by the
    // journal's own contract they have had no effect and need no
    // inspection; and a job that reached `JobFinished` has none
    // (`remaining_steps` leaves them out: it stopped on purpose).
    if report.was_interrupted() {
        redo.extend(
            report
                .not_started_steps
                .iter()
                .copied()
                .filter(|&index| (index as usize) < report.plan.steps.len()),
        );
        // So are the steps that finished *unsuccessfully* before the
        // process died. Their failures and skips lived in the job's
        // in-memory report, which died with it: nobody was ever told. A
        // resume that left them out would end "completed" over files
        // that were never copied. Redoing them either succeeds this time
        // or puts the failure in front of the user at last.
        redo.extend(
            report
                .unsuccessful_steps
                .iter()
                .copied()
                .filter(|&index| (index as usize) < report.plan.steps.len()),
        );
    }

    // Re-gate: any step about to be redone whose dependency chain reaches
    // a step that completed *unsuccessfully* drags that step back in, so
    // the executor re-attempts it and re-evaluates the gate, instead of
    // `rebuild_subset` clearing the dependency as if it had been met.
    // (For an interrupted job every unsuccessful step is already in; this
    // is what covers a job that reached `JobFinished` -- cancelled, say --
    // with intents still dangling.)
    let unsuccessful: BTreeSet<u32> = report.unsuccessful_steps.iter().copied().collect();
    let mut frontier: Vec<u32> = redo.iter().copied().collect();
    while let Some(index) = frontier.pop() {
        let dependency = report
            .plan
            .steps
            .get(index as usize)
            .and_then(Step::depends_on);
        if let Some(dependency) = dependency
            && unsuccessful.contains(&dependency)
            && redo.insert(dependency)
        {
            frontier.push(dependency);
        }
    }

    // And what redoing those entails: the steps gated on them, and the
    // deferred metadata of every directory they create something in --
    // never, though, a step already found to have taken effect.
    crate::rerun::include_consequences(&report.plan, &mut redo);
    for index in &already_applied {
        redo.remove(index);
    }

    let redo_list: Vec<u32> = redo.iter().copied().collect();
    Ok(ResumePlan {
        plan: crate::rerun::rebuild_subset(&report.plan, redo),
        redo: redo_list,
        already_applied,
    })
}

/// Removes every scratch file the interrupted job left behind -- the whole
/// of the "discard" action's filesystem work, and the first thing a resume
/// does. Two kinds:
///
/// - the `.duet-partial-*` files `report.orphaned_partials` names (the
///   executor's own staging name for each dangling `CopyFile`/`Reflink`,
///   recorded in the `Intent`);
/// - the *backend's* staging siblings of a dangling step's output.
///   `FileSystem::open_write` stages through a `.duet-partial-<rand>-<name>`
///   sibling of its own that no journal record names, so a kill between
///   its creation and its publishing rename -- the buffered-copy fallback
///   mid-write, or a `WriteTrashInfo` mid-write -- leaves a file only a
///   directory listing can find. Only names of exactly that shape, for
///   exactly the dangling steps' own outputs, are touched.
///
/// A file that is already gone is success (a previous, interrupted discard
/// got that far).
///
/// # Errors
/// The first removal that fails for any reason other than `NotFound`. The
/// caller must then *not* close the job's journal: a partial still on
/// disk, unaccounted for, would never be offered for cleanup again.
pub async fn discard_orphaned_partials(fs: &dyn FileSystem, report: &RecoveryReport) -> Result<()> {
    for (step_index, partial_name) in &report.orphaned_partials {
        // A stale or foreign step index names nothing to delete --
        // `orphaned_partial_path` degrades to `None` for it.
        let Some(path) = orphaned_partial_path(report, *step_index, partial_name) else {
            continue;
        };
        remove_if_present(fs, &path).await?;
        if let Some(parent) = path.parent() {
            remove_staging_siblings(fs, &parent, partial_name).await?;
        }
    }
    for &step_index in &report.incomplete_steps {
        if let Some(Step::WriteTrashInfo { info_path, .. }) =
            report.plan.steps.get(step_index as usize)
            && let (Some(parent), Some(name)) = (info_path.parent(), info_path.inner().file_name())
        {
            remove_staging_siblings(fs, &parent, name).await?;
        }
    }
    Ok(())
}

async fn remove_if_present(fs: &dyn FileSystem, path: &VPath) -> Result<()> {
    match fs.remove(path, RemoveKind::File).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes every entry of `dir` named like a backend staging sibling of
/// `final_name` (`.duet-partial-<rand>-<final_name>`, the shape
/// `duet_vfs::local`'s `open_write` uses). A directory that cannot be
/// listed because it is not there has nothing to sweep.
pub(crate) async fn remove_staging_siblings(
    fs: &dyn FileSystem,
    dir: &VPath,
    final_name: &str,
) -> Result<()> {
    let suffix = format!("-{final_name}");
    let mut stale: Vec<VPath> = Vec::new();
    {
        let mut chunks = fs.read_dir(dir, ListOpts::names_only());
        while let Some(chunk) = chunks.next().await {
            let entries = match chunk {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            for entry in entries {
                if entry.name.starts_with(".duet-partial-")
                    && entry.name.ends_with(&suffix)
                    && entry.name.len() > ".duet-partial-".len() + suffix.len()
                    && let Ok(path) = dir.join(&entry.name)
                {
                    stale.push(path);
                }
            }
        }
    }
    for path in stale {
        remove_if_present(fs, &path).await?;
    }
    Ok(())
}

/// Whether `step`'s effect is already fully on disk -- the "recovery
/// infers success" half of `docs/crash-safety.md`'s "after the side
/// effect, before `Completion` is durable" rows. Conservative by
/// construction: `true` only on positive evidence, `false` for anything
/// uncertain or unreadable, because a wrong `false` costs a redo while a
/// wrong `true` would leave work undone.
///
/// Kinds a redo cannot harm (`CreateDir`, `SetMeta`, `Verify`) are never
/// inferred: re-running them *is* the cheap, exact answer.
async fn already_took_effect(fs: &dyn FileSystem, step: &Step) -> bool {
    match step {
        Step::CreateDir { .. } | Step::SetMeta { .. } | Step::Verify { .. } => false,
        // Published (the rename is the last thing a copy does) with the
        // source's exact bytes. Content, not size or timestamps: a
        // destination that merely looks right is not evidence, and a
        // pre-existing different file at `dest` must still go through the
        // job's conflict policy.
        Step::CopyFile { source, dest, .. } | Step::Reflink { source, dest, .. } => {
            same_file_content(fs, source, dest).await
        }
        // The entry is at `dest` and no longer at `source`.
        Step::Rename { source, dest, .. } => {
            is_absent(fs, source).await && fs.stat(dest, false).await.is_ok()
        }
        // `dest` is another name for `source`'s inode.
        Step::Link { source, dest, .. } => {
            match (fs.stat(source, false).await, fs.stat(dest, false).await) {
                (Ok(a), Ok(b)) => {
                    a.dev.is_some() && a.dev == b.dev && a.ino.is_some() && a.ino == b.ino
                }
                _ => false,
            }
        }
        // A symlink at `link_path` storing exactly `target`.
        Step::Symlink {
            target, link_path, ..
        } => fs.stat(link_path, false).await.is_ok_and(|meta| {
            meta.symlink_target
                .as_ref()
                .is_some_and(|stored| stored.as_str() == target)
        }),
        Step::Remove { target, .. } => is_absent(fs, target).await,
        // The sidecar exists with exactly the planned content.
        Step::WriteTrashInfo { info_path, content } => {
            read_small_file(fs, info_path, content.len() as u64)
                .await
                .is_some_and(|bytes| bytes == content.as_bytes())
        }
    }
}

/// `true` only when `stat` positively reports `NotFound` -- a permission
/// error or an I/O error is not evidence of absence.
async fn is_absent(fs: &dyn FileSystem, path: &VPath) -> bool {
    matches!(fs.stat(path, false).await, Err(e) if e.kind() == ErrorKind::NotFound)
}

/// Reads `path` whole if it is a regular file of exactly `expected_len`
/// bytes; `None` otherwise.
async fn read_small_file(fs: &dyn FileSystem, path: &VPath, expected_len: u64) -> Option<Vec<u8>> {
    let meta = fs.stat(path, false).await.ok()?;
    if !meta.kind.is_file() || meta.size != expected_len {
        return None;
    }
    let mut reader = fs.open_read(path).await.ok()?;
    let mut bytes = Vec::with_capacity(expected_len as usize);
    reader.read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
}

/// `true` when `a` and `b` are both regular files with identical bytes.
/// Sizes are compared first, so the full read only ever happens for the
/// handful of files that were mid-publish when the process died.
async fn same_file_content(fs: &dyn FileSystem, a: &VPath, b: &VPath) -> bool {
    let (Ok(meta_a), Ok(meta_b)) = (fs.stat(a, false).await, fs.stat(b, false).await) else {
        return false;
    };
    if !meta_a.kind.is_file() || !meta_b.kind.is_file() || meta_a.size != meta_b.size {
        return false;
    }
    let (Ok(mut reader_a), Ok(mut reader_b)) = (fs.open_read(a).await, fs.open_read(b).await)
    else {
        return false;
    };
    const CHUNK: usize = 1024 * 1024;
    let mut buf_a = vec![0u8; CHUNK];
    let mut buf_b = vec![0u8; CHUNK];
    loop {
        let Ok(n) = read_full(&mut *reader_a, &mut buf_a).await else {
            return false;
        };
        let Ok(m) = read_full(&mut *reader_b, &mut buf_b).await else {
            return false;
        };
        if n != m || buf_a[..n] != buf_b[..m] {
            return false;
        }
        if n == 0 {
            return true;
        }
    }
}

/// Fills `buf` as far as the stream allows, so two streams that chunk
/// their reads differently still compare block for block. Returns the
/// number of bytes read; less than `buf.len()` only at end of stream.
async fn read_full(
    reader: &mut dyn duet_vfs::AsyncReadSeek,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// The full on-disk path of one orphaned `.duet-partial-*` file named in
/// `report.orphaned_partials`, for the "discard" action to actually
/// delete. Mirrors `executor.rs`'s own `copy_file_step` derivation
/// exactly (`dest.parent()` then `.join(partial_name)`) -- that's the only
/// place a partial's path is ever constructed, so this must match it
/// byte for byte or discard will silently no-op against the wrong path.
///
/// `None` if `step_index` doesn't point at a `CopyFile`/`Reflink` step
/// with a `dest` in `report.plan` (shouldn't happen, but a stale/foreign
/// `step_index` degrades safely rather than panicking), or if `dest` has
/// no parent (a destination at a mount root -- `VPath::parent` returning
/// `None`) or `partial_name` fails to parse as a single path component
/// (`VPath::join`'s own validation).
pub fn orphaned_partial_path(
    report: &RecoveryReport,
    step_index: u32,
    partial_name: &str,
) -> Option<VPath> {
    let step = report.plan.steps.get(step_index as usize)?;
    let dest = match step {
        Step::CopyFile { dest, .. } | Step::Reflink { dest, .. } => dest,
        _ => return None,
    };
    let parent = dest.parent()?;
    parent.join(partial_name).ok()
}

#[cfg(test)]
mod tests {
    use duet_types::{MountId, UnixPathBuf, VPath};

    use super::*;
    use crate::conflict::ConflictPolicy;
    use crate::job::{JobId, JobKind};
    use crate::plan::PlanOptions;
    use crate::step::{RemoveMode, Step};

    fn vpath(p: &str) -> VPath {
        VPath::new(MountId::local(), UnixPathBuf::new(p).unwrap())
    }

    fn create_dir(path: &str) -> Step {
        Step::CreateDir {
            dest: vpath(path),
            mode: Some(0o755),
        }
    }

    fn copy_file(name: &str, size: u64) -> Step {
        Step::CopyFile {
            source: vpath(&format!("/src/{name}")),
            dest: vpath(&format!("/dst/{name}")),
            size,
            conflict: None,
        }
    }

    fn reflink(name: &str, size: u64) -> Step {
        Step::Reflink {
            source: vpath(&format!("/src/{name}")),
            dest: vpath(&format!("/dst/{name}")),
            size,
            conflict: None,
        }
    }

    fn remove(target: &str, depends_on: Option<u32>) -> Step {
        Step::Remove {
            target: vpath(target),
            mode: RemoveMode::File,
            depends_on,
        }
    }

    /// A four-step copy plan: create the destination directory, then copy
    /// three files into it -- the same shape `rerun.rs`'s own
    /// `four_step_plan` uses, so the two modules' tests stay easy to
    /// compare side by side.
    fn four_step_plan() -> Plan {
        Plan::new(
            vec![
                create_dir("/dst"),
                copy_file("a.txt", 100),
                copy_file("b.txt", 200),
                copy_file("c.txt", 300),
            ],
            PlanOptions {
                default_conflict: ConflictPolicy::OverwriteIfOlder,
                verify: true,
            },
        )
    }

    fn report_with(plan: Plan, incomplete_steps: Vec<u32>) -> RecoveryReport {
        RecoveryReport {
            job_id: JobId(1),
            kind: JobKind::Copy,
            plan,
            incomplete_steps,
            orphaned_partials: Vec::new(),
            unsuccessful_steps: Vec::new(),
            not_started_steps: Vec::new(),
            last_outcome: None,
        }
    }

    #[test]
    fn plan_from_recovery_rebuilds_exactly_the_incomplete_steps_in_order() {
        let plan = four_step_plan();
        let report = report_with(plan, vec![1, 3]);

        let redo = plan_from_recovery(&report);

        assert_eq!(
            redo.steps,
            vec![copy_file("a.txt", 100), copy_file("c.txt", 300)]
        );
    }

    #[test]
    fn an_empty_incomplete_steps_list_produces_a_valid_empty_plan() {
        let plan = four_step_plan();
        let report = report_with(plan, Vec::new());

        let redo = plan_from_recovery(&report);

        assert!(redo.steps.is_empty());
        assert_eq!(redo.totals, crate::plan::PlanTotals::default());
    }

    /// A dependency on a step that *isn't* being resumed (it already
    /// completed before the crash) has nothing left to gate on -- cleared,
    /// not left dangling at a stale index. Ported from `rerun.rs`'s own
    /// `a_dependency_on_a_step_outside_the_redo_set_is_cleared`.
    #[test]
    fn a_dependency_outside_the_incomplete_set_is_cleared() {
        let plan = Plan::new(
            vec![
                copy_file("a.txt", 10),        // 0: completed before crash
                remove("/src/a.txt", Some(0)), // 1: interrupted, dep completed
            ],
            PlanOptions::default(),
        );
        let report = report_with(plan, vec![1]);

        let redo = plan_from_recovery(&report);

        assert_eq!(redo.steps.len(), 1);
        assert_eq!(
            redo.steps[0].depends_on(),
            None,
            "step 0 completed before the crash, so there is nothing left to wait for"
        );
    }

    /// Both the barrier step and its dependent are being resumed together,
    /// so the gate still means something -- remapped to the barrier's new
    /// index, not its original one. Ported from `rerun.rs`'s own
    /// `a_dependency_inside_the_redo_set_is_remapped_to_its_new_index`.
    #[test]
    fn a_dependency_inside_the_incomplete_set_is_remapped_to_its_new_index() {
        let plan = Plan::new(
            vec![
                create_dir("/dst"),            // 0: completed before crash
                copy_file("a.txt", 10),        // 1: interrupted, resumed
                remove("/src/a.txt", Some(1)), // 2: never reached, resumed
            ],
            PlanOptions::default(),
        );
        let report = report_with(plan, vec![1, 2]);

        let redo = plan_from_recovery(&report);

        assert_eq!(redo.steps.len(), 2);
        assert_eq!(
            redo.steps[1].depends_on(),
            Some(0),
            "the Remove must be gated on its copy at the copy's new index 0, not the stale 1"
        );
    }

    #[test]
    fn orphaned_partial_path_derives_the_correct_path_for_a_copy_file_step() {
        let plan = Plan::new(vec![copy_file("a.txt", 10)], PlanOptions::default());
        let report = report_with(plan, vec![0]);

        let path = orphaned_partial_path(&report, 0, ".duet-partial-123-a.txt").unwrap();

        assert_eq!(path, vpath("/dst/.duet-partial-123-a.txt"));
    }

    #[test]
    fn orphaned_partial_path_derives_the_correct_path_for_a_reflink_step() {
        let plan = Plan::new(vec![reflink("b.txt", 20)], PlanOptions::default());
        let report = report_with(plan, vec![0]);

        let path = orphaned_partial_path(&report, 0, ".duet-partial-456-b.txt").unwrap();

        assert_eq!(path, vpath("/dst/.duet-partial-456-b.txt"));
    }

    #[test]
    fn orphaned_partial_path_is_none_for_a_non_copy_class_step() {
        let plan = Plan::new(vec![create_dir("/dst")], PlanOptions::default());
        let report = report_with(plan, vec![0]);

        assert_eq!(orphaned_partial_path(&report, 0, ".duet-partial-x"), None);
    }

    #[test]
    fn orphaned_partial_path_is_none_for_an_out_of_range_step_index() {
        let plan = Plan::new(vec![copy_file("a.txt", 10)], PlanOptions::default());
        let report = report_with(plan, vec![0]);

        assert_eq!(orphaned_partial_path(&report, 99, ".duet-partial-x"), None);
    }
}
