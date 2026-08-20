// SPDX-License-Identifier: MIT
//! `Plan` — a materialised, ordered list of [`Step`]s (design.md §9.3).
//!
//! "A job is a `Plan`: a materialised, ordered list of `Step`s produced by
//! walking the source set. Planning is itself async and cancellable and
//! produces the totals (files, bytes) that make honest progress possible."
//! The actual async/cancellable walk is T-5.1.1's job; this task's scope is
//! the `Plan` *data shape* the walk produces — required to be serializable
//! (T-2.3.1's AC: "a hand-written plan for 'copy a 3-file directory'
//! serialises and deserialises") since it is also exactly what a journal's
//! `JournalRecord::JobStarted` record and a resume-after-crash reader
//! persist and reload (see [`crate::journal`]).

use serde::{Deserialize, Serialize};

use crate::conflict::ConflictPolicy;
use crate::step::Step;

/// Options in effect when a [`Plan`] was produced, carried alongside the
/// step list so a serialized plan is self-describing — a recovery reader
/// replaying a journal doesn't need a live `Job`/queue to reinterpret what
/// "the default" meant at plan time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Falls back to this policy for any conflict the user hasn't already
    /// answered (design.md §9.3: "job-level default → per-conflict user
    /// answer → apply to all answers" — this is the first tier).
    pub default_conflict: ConflictPolicy,
    /// FR-OPS-08: run post-copy verification (BLAKE3) as part of this job.
    pub verify: bool,
}

impl Default for PlanOptions {
    /// The conservative default: never clobber silently, no extra
    /// verification pass unless the caller opts in.
    fn default() -> Self {
        PlanOptions {
            default_conflict: ConflictPolicy::Skip,
            verify: false,
        }
    }
}

/// Totals a `Plan` reports up front, so `duet-ops`'s progress reporting is
/// honest (FR-OPS-03) rather than inferred from steps executed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanTotals {
    /// Count of `CreateDir` steps.
    pub dirs: u64,
    /// Count of steps that produce a file-shaped destination entry
    /// (`CopyFile`, `Reflink`, `Link`) — what "N files remaining" (FR-OPS-03)
    /// counts against.
    pub files: u64,
    /// Sum of `Step::planned_bytes()` across the whole plan.
    pub bytes: u64,
    /// Count of `Link` steps specifically — T-5.1.7's hardlink-graph dedup
    /// "reports" itself through this field (a subset of `files`, which
    /// already counts every `Link` step too): how many of this job's files
    /// were preserved as a hardlink to an already-copied inode instead of
    /// a second physical copy.
    pub hardlinks_preserved: u64,
}

/// A materialised, ordered list of [`Step`]s, with the totals that make
/// honest two-level progress (FR-OPS-03) possible from the moment a job
/// starts running, before a single byte has moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub totals: PlanTotals,
    pub options: PlanOptions,
}

impl Plan {
    /// Builds a `Plan` from an already-produced step list, computing
    /// `totals` from the steps themselves so callers (including this
    /// crate's own tests) never have to keep a hand-maintained total in
    /// sync with the step list by hand.
    pub fn new(steps: Vec<Step>, options: PlanOptions) -> Self {
        let totals = Self::compute_totals(&steps);
        Plan {
            steps,
            totals,
            options,
        }
    }

    /// Computes [`PlanTotals`] from a step list. Exposed separately from
    /// [`Plan::new`] so a planner that streams steps incrementally (T-5.1.1:
    /// "planning is itself async and cancellable") can recompute totals as
    /// it goes without constructing a throwaway `Plan` each time.
    pub fn compute_totals(steps: &[Step]) -> PlanTotals {
        use crate::step::StepKind;
        let mut totals = PlanTotals::default();
        for step in steps {
            match step.kind() {
                StepKind::CreateDir => totals.dirs += 1,
                StepKind::CopyFile | StepKind::Reflink => totals.files += 1,
                StepKind::Link => {
                    totals.files += 1;
                    totals.hardlinks_preserved += 1;
                }
                StepKind::Rename | StepKind::SetMeta | StepKind::Remove | StepKind::Verify => {}
            }
            totals.bytes += step.planned_bytes();
        }
        totals
    }
}

#[cfg(test)]
mod tests {
    use duet_types::{MountId, UnixPathBuf, VPath};

    use super::*;
    use crate::step::Step;

    fn vpath(p: &str) -> VPath {
        VPath::new(MountId::local(), UnixPathBuf::new(p).unwrap())
    }

    /// Hand-writes the plan for "copy a 3-file directory" (`src/` containing
    /// `a.txt`, `b.txt`, `c.txt`, copied into a freshly created `dst/`) and
    /// round-trips it through JSON. This is T-2.3.1's AC verbatim: "a
    /// hand-written plan for 'copy a 3-file directory' serialises and
    /// deserialises" — not merely asserting the derive macros exist, but
    /// actually serializing, actually deserializing, and comparing the
    /// result to the original.
    fn copy_three_file_directory_plan() -> Plan {
        let steps = vec![
            Step::CreateDir {
                dest: vpath("/dst"),
                mode: Some(0o755),
            },
            Step::CopyFile {
                source: vpath("/src/a.txt"),
                dest: vpath("/dst/a.txt"),
                size: 100,
                conflict: None,
            },
            Step::CopyFile {
                source: vpath("/src/b.txt"),
                dest: vpath("/dst/b.txt"),
                size: 250,
                conflict: None,
            },
            Step::CopyFile {
                source: vpath("/src/c.txt"),
                dest: vpath("/dst/c.txt"),
                size: 0,
                conflict: Some(ConflictPolicy::AutoRename),
            },
        ];
        Plan::new(steps, PlanOptions::default())
    }

    #[test]
    fn totals_reflect_hand_written_steps() {
        let plan = copy_three_file_directory_plan();
        assert_eq!(plan.totals.dirs, 1);
        assert_eq!(plan.totals.files, 3);
        assert_eq!(plan.totals.bytes, 350);
    }

    #[test]
    fn three_file_directory_plan_serialises_and_deserialises() {
        let plan = copy_three_file_directory_plan();

        let json = serde_json::to_string(&plan).expect("serialize");
        let round_tripped: Plan = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(round_tripped, plan);
        // Not just structural equality -- confirm the wire form actually
        // carries the step data a recovery reader would need, not e.g. an
        // accidentally-opaque blob.
        assert!(json.contains("a.txt"));
        assert!(json.contains("CopyFile"));
        assert!(json.contains("AutoRename"));
    }

    #[test]
    fn empty_plan_serialises_and_deserialises() {
        let plan = Plan::new(Vec::new(), PlanOptions::default());
        let json = serde_json::to_string(&plan).expect("serialize");
        let round_tripped: Plan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, plan);
        assert_eq!(plan.totals, PlanTotals::default());
    }
}
