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

use duet_types::VPath;

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
