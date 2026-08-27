// SPDX-License-Identifier: MIT
//! T-5.2.4's "re-run the failed/skipped items" planner: rebuild a [`Plan`]
//! containing only the steps a finished job's [`JobReport`] says still need
//! doing.
//!
//! `docs/commands.md`'s own title for the command this backs --
//! `ops.queue.retry_failed`, "Re-run the failed/skipped items of a job" --
//! names *both* categories, so both [`JobReport::errors`] and
//! [`JobReport::skipped`] feed the redo set. A skip is not a success: a
//! `ConflictPolicy::Skip` that left the destination untouched is exactly
//! the kind of thing a user re-runs after deciding differently, which is
//! also why `duet_ui`'s retry path hands the live
//! `ConflictResolver` to the new job rather than letting
//! `PlanOptions::default_conflict` silently re-skip it.
//!
//! # Why this is a pure function over `Plan` + `JobReport`
//!
//! Everything needed to rebuild the work is already in those two values:
//! the original `Plan` holds the steps themselves (a `Job` keeps its whole
//! plan for exactly this kind of after-the-fact inspection), and the report
//! holds the `step_index`es that went wrong. No `FileSystem`, no re-walk,
//! no `async` -- re-planning from scratch would risk producing a
//! *different* set of steps than the one the user is looking at in the
//! report, which is the opposite of what "re-run these items" means.
//!
//! # `depends_on` remapping
//!
//! A `Step`'s [`depends_on`](Step::depends_on) is an index into
//! `Plan::steps`, so a filtered subset invalidates every one of them. Two
//! cases, both handled here:
//!
//! - **The dependency is *not* in the redo set.** It succeeded in the
//!   original run, so its physical prerequisite genuinely exists on disk
//!   now (the directory was created, the file was copied). There is
//!   nothing left to gate on, so the dependency is cleared to `None`.
//!   Leaving a stale index in place would be actively wrong: it would
//!   point at whatever unrelated step now occupies that position in the
//!   smaller plan.
//! - **The dependency *is* in the redo set.** It is being redone
//!   alongside its dependent, so the gate still means something -- remap
//!   it to that step's new index within the rebuilt subset.
//!
//! Steps that carry no `depends_on` field at all (`CreateDir`,
//! `CopyFile`, `Reflink`, `Rename`) need nothing done to them, because
//! their ordering is positional -- which is also why the redo set is
//! rebuilt in ascending `step_index` order rather than in whatever order
//! the report happened to record the failures. See [`Step::depends_on`]'s
//! own doc comment.
//!
//! # Options are reused verbatim
//!
//! The new `Plan` carries the original's [`PlanOptions`](crate::PlanOptions)
//! unchanged -- same conflict default, same verify flag. There is no UI for
//! choosing different options on a retry (none of T-5.2.1/T-5.2.6/T-5.2.7's
//! own dialogs offers a "different options this time" control either), and
//! inventing one here would mean the rebuilt plan no longer describes the
//! same operation the user asked to re-run.

use std::collections::{BTreeSet, HashMap};

use crate::job::JobReport;
use crate::plan::Plan;
use crate::step::Step;

/// Builds the "re-run what didn't work" plan for a finished job: every
/// step named by `report`'s errors *or* skips, cloned out of `original` in
/// their original relative order, with [`Step::depends_on`] remapped onto
/// the rebuilt, smaller step list (see the module doc comment for both
/// cases).
///
/// An empty redo set produces a valid, empty-steps `Plan` -- the same
/// "nothing to do is success, not an error" convention
/// [`crate::plan_mkdir`]/[`crate::plan_delete`] already establish. Callers
/// are nonetheless expected to gate on the report actually having
/// something in it (`docs/commands.md`'s own `job.has_errors` context
/// predicate for `ops.queue.retry_failed`), so an empty plan here is a
/// defensive floor, not a normal path.
///
/// A `step_index` that isn't a valid index into `original.steps` at all is
/// skipped rather than panicking. That should not happen -- the executor
/// populates both lists from the very plan being run -- but a `JobReport`
/// is serializable and could in principle arrive from a journal written by
/// a different build, and "silently do less work" is the only safe
/// interpretation available.
pub fn plan_from_report(original: &Plan, report: &JobReport) -> Plan {
    // A `BTreeSet` does two jobs at once: it dedups an index that shows up
    // in *both* lists (an executor could plausibly record a step as
    // skipped and later as failed), and iterating it yields ascending
    // indices, which is precisely the original relative order the
    // positional-ordering step kinds depend on.
    let mut redo: BTreeSet<u32> = BTreeSet::new();
    redo.extend(report.errors.iter().map(|failure| failure.step_index));
    redo.extend(report.skipped.iter().map(|skip| skip.step_index));

    let mut new_index_of: HashMap<u32, u32> = HashMap::with_capacity(redo.len());
    let mut steps: Vec<Step> = Vec::with_capacity(redo.len());
    for old_index in redo {
        let Some(step) = original.steps.get(old_index as usize) else {
            continue;
        };
        new_index_of.insert(old_index, steps.len() as u32);
        steps.push(step.clone());
    }

    // A second pass, not a fused one: a step's dependency may point
    // *forward* in principle, and in any case the map isn't complete until
    // every redone step has been assigned its new index.
    for step in &mut steps {
        if let Some(dependency) = depends_on_mut(step) {
            *dependency = dependency.and_then(|old| new_index_of.get(&old).copied());
        }
    }

    Plan::new(steps, original.options)
}

/// Mutable access to whichever variants carry a `depends_on` field, or
/// `None` for the four that don't. The write-side mirror of
/// [`Step::depends_on`] -- kept private and local to this module, since
/// rebuilding a filtered plan is the only thing that has any business
/// rewriting an already-materialised step's dependency index.
fn depends_on_mut(step: &mut Step) -> Option<&mut Option<u32>> {
    match step {
        Step::Link { depends_on, .. }
        | Step::Symlink { depends_on, .. }
        | Step::SetMeta { depends_on, .. }
        | Step::Remove { depends_on, .. }
        | Step::Verify { depends_on, .. } => Some(depends_on),
        Step::CreateDir { .. }
        | Step::CopyFile { .. }
        | Step::Reflink { .. }
        | Step::Rename { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use duet_types::{ErrorKind, MetaPatch, MountId, UnixPathBuf, VPath};

    use super::*;
    use crate::conflict::ConflictPolicy;
    use crate::job::{JobReport, SkipEntry, StepFailure};
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

    fn set_meta(target: &str, depends_on: Option<u32>) -> Step {
        Step::SetMeta {
            target: vpath(target),
            patch: MetaPatch::default(),
            depends_on,
        }
    }

    fn remove(target: &str, depends_on: Option<u32>) -> Step {
        Step::Remove {
            target: vpath(target),
            mode: RemoveMode::File,
            depends_on,
        }
    }

    fn failure(step_index: u32) -> StepFailure {
        StepFailure {
            step_index,
            path: None,
            kind: ErrorKind::Permission,
            message: "denied".to_string(),
        }
    }

    fn skip(step_index: u32) -> SkipEntry {
        SkipEntry {
            step_index,
            path: vpath("/dst/whatever"),
            reason: "destination exists".to_string(),
        }
    }

    /// A four-step copy plan: create the destination directory, then copy
    /// three files into it.
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

    #[test]
    fn a_report_with_only_errors_redoes_exactly_those_steps() {
        let original = four_step_plan();
        let report = JobReport {
            errors: vec![failure(1), failure(3)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(
            redo.steps,
            vec![copy_file("a.txt", 100), copy_file("c.txt", 300)]
        );
        assert_eq!(redo.totals.files, 2);
        assert_eq!(
            redo.totals.bytes, 400,
            "totals are recomputed for the subset"
        );
        assert_eq!(redo.totals.dirs, 0, "the succeeded CreateDir is not redone");
    }

    #[test]
    fn a_report_with_only_skips_redoes_exactly_those_steps() {
        let original = four_step_plan();
        let report = JobReport {
            skipped: vec![skip(2)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(redo.steps, vec![copy_file("b.txt", 200)]);
    }

    /// `docs/commands.md`'s own title for this command is "re-run the
    /// failed/skipped items" -- both categories land in one plan, in
    /// original plan order, not errors-then-skips.
    #[test]
    fn errors_and_skips_combine_into_one_plan_in_original_order() {
        let original = four_step_plan();
        let report = JobReport {
            // Deliberately out of order relative to the plan, and with the
            // later index listed as the error: the rebuilt plan must still
            // read 1, 2, 3.
            errors: vec![failure(3)],
            skipped: vec![skip(2), skip(1)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(
            redo.steps,
            vec![
                copy_file("a.txt", 100),
                copy_file("b.txt", 200),
                copy_file("c.txt", 300),
            ]
        );
    }

    #[test]
    fn a_step_reported_both_skipped_and_failed_is_redone_once() {
        let original = four_step_plan();
        let report = JobReport {
            errors: vec![failure(2)],
            skipped: vec![skip(2)],
            ..JobReport::default()
        };

        assert_eq!(plan_from_report(&original, &report).steps.len(), 1);
    }

    #[test]
    fn an_empty_report_produces_an_empty_but_valid_plan() {
        let original = four_step_plan();
        let redo = plan_from_report(&original, &JobReport::default());

        assert!(redo.steps.is_empty());
        assert_eq!(redo.totals, crate::plan::PlanTotals::default());
        assert_eq!(
            redo.options, original.options,
            "even an empty retry keeps the original job's options"
        );
    }

    #[test]
    fn the_original_plan_options_are_reused_verbatim() {
        let original = four_step_plan();
        let report = JobReport {
            errors: vec![failure(1)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(
            redo.options.default_conflict,
            ConflictPolicy::OverwriteIfOlder
        );
        assert!(redo.options.verify);
    }

    /// A dependency on a step that *succeeded* has nothing left to gate on
    /// -- the directory really is there now -- so it is cleared rather
    /// than left pointing at whatever step now occupies index 0.
    #[test]
    fn a_dependency_on_a_step_outside_the_redo_set_is_cleared() {
        let original = Plan::new(
            vec![create_dir("/dst"), set_meta("/dst", Some(0))],
            PlanOptions::default(),
        );
        let report = JobReport {
            errors: vec![failure(1)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(redo.steps.len(), 1);
        assert_eq!(
            redo.steps[0].depends_on(),
            None,
            "step 0 succeeded in the original run, so there is nothing left to wait for"
        );
    }

    /// Both the barrier step and its dependent are being redone, so the
    /// gate still means something -- and must point at the barrier's *new*
    /// index, not its original one.
    #[test]
    fn a_dependency_inside_the_redo_set_is_remapped_to_its_new_index() {
        let original = Plan::new(
            vec![
                create_dir("/dst"),            // 0: succeeded
                copy_file("a.txt", 10),        // 1: failed, redone
                remove("/src/a.txt", Some(1)), // 2: never reached, redone
            ],
            PlanOptions::default(),
        );
        let report = JobReport {
            errors: vec![failure(1)],
            skipped: vec![skip(2)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(redo.steps.len(), 2);
        assert_eq!(
            redo.steps[1].depends_on(),
            Some(0),
            "the cross-device move's Remove must still be gated on its copy, at the copy's \
             new index 0 -- not the stale 1, which is now the Remove itself"
        );
    }

    /// The mixed case in one plan: one dependency remapped, one cleared.
    #[test]
    fn remapping_and_clearing_coexist_within_one_rebuilt_plan() {
        let original = Plan::new(
            vec![
                create_dir("/dst"),              // 0: succeeded
                copy_file("a.txt", 10),          // 1: failed, redone
                set_meta("/dst/a.txt", Some(1)), // 2: redone, dep redone too
                set_meta("/dst", Some(0)),       // 3: redone, dep succeeded
            ],
            PlanOptions::default(),
        );
        let report = JobReport {
            errors: vec![failure(1), failure(2), failure(3)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(redo.steps.len(), 3);
        assert_eq!(redo.steps[1].depends_on(), Some(0));
        assert_eq!(redo.steps[2].depends_on(), None);
    }

    /// Defensive floor: a `step_index` past the end of the original plan
    /// is dropped, not panicked on. See [`plan_from_report`]'s own doc
    /// comment for why a serializable report could carry one at all.
    #[test]
    fn an_out_of_range_step_index_is_ignored_rather_than_panicking() {
        let original = four_step_plan();
        let report = JobReport {
            errors: vec![failure(1), failure(99)],
            ..JobReport::default()
        };

        let redo = plan_from_report(&original, &report);

        assert_eq!(redo.steps, vec![copy_file("a.txt", 100)]);
    }
}
