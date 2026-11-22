// SPDX-License-Identifier: MIT
//! `SIGKILL` at every step boundary of real copy, move and delete jobs
//! (design.md §14.4: "crash injection at every step boundary of
//! copy/move/delete").
//!
//! The matrix proves each `Step` kind in isolation. This module proves the
//! plans the product actually builds: a directory tree run through
//! `plan_copy`, `plan_move` (same device and across devices, with and
//! without verification) and `plan_delete` (permanent and to trash), then
//! killed once at each of four boundaries of **every** step --
//!
//! - before the step's `Intent` reaches the journal,
//! - after the `Intent` is durable, before the step does anything,
//! - after the step's effect, before its `Completion` is journaled,
//! - after the `Completion` is durable --
//!
//! plus the four boundaries around the job's own `JobStarted` and
//! `JobFinished` records. Each kill is followed by the general invariant
//! check and a resume that must produce exactly the finished job.
//!
//! [`crash_during_recovery`] then kills the *resumed* job as well: a crash
//! while recovering from a crash must be as recoverable as the first.

use duet_ops::Journal;

use crate::harness::{
    Backend, ChildSpec, JOB, RESUME_JOB, World, assert_nothing_left_to_recover, block_on, crash_at,
    run_job, run_to_end,
};
use crate::model::{check_finished, check_interrupted, check_no_content_lost};
use crate::scenarios::{self, Scenario};
use crate::sigkill_matrix::{finish_by_resume, inspect};

const STEP_BOUNDARIES: [&str; 4] = [
    "journal.intent.before_write",
    "step.before_effect",
    "step.after_effect",
    "journal.completion.after_fsync",
];

const JOB_BOUNDARIES: [&str; 4] = [
    "journal.job_started.before_write",
    "journal.job_started.after_fsync",
    "journal.job_finished.before_write",
    "journal.job_finished.after_fsync",
];

/// First, the model itself: an uninterrupted run of the scenario must
/// produce exactly what the model predicts. Without this, a sweep that
/// passes could simply mean the model and the engine are wrong together.
fn model_matches_an_uninterrupted_run(
    new_world: fn() -> World,
    setup: &dyn Fn(&World) -> Scenario,
) {
    let world = new_world();
    let scenario = setup(&world);
    let before = world.capture();
    let report = run_to_end(&world, &scenario.spec(&world, Backend::Local));
    assert!(
        report.errors.is_empty() && report.skipped.is_empty(),
        "the fixture must run cleanly when nothing is injected: {report:?}"
    );
    let after = world.capture();
    if let Err(why) = check_finished(&before, &scenario.plan, &after) {
        panic!("the model disagrees with an uninterrupted run:\n{why}");
    }
    assert!(after.partials().is_empty());
}

fn sweep(new_world: fn() -> World, setup: impl Fn(&World) -> Scenario + Sync) {
    model_matches_an_uninterrupted_run(new_world, &setup);

    let steps = {
        let world = new_world();
        setup(&world).plan.steps.len()
    };
    let mut failpoints: Vec<String> = JOB_BOUNDARIES.iter().map(|b| format!("{b}@0")).collect();
    for step in 0..steps {
        for boundary in STEP_BOUNDARIES {
            failpoints.push(format!("{boundary}@{step}"));
        }
    }

    // Each crash is an independent world; run them on a few threads.
    let failpoints = std::sync::Mutex::new(failpoints.into_iter());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let Some(failpoint) = failpoints.lock().unwrap().next() else {
                        break;
                    };
                    let world = new_world();
                    let scenario = setup(&world);
                    let before = world.capture();
                    crash_at(&world, &scenario.spec(&world, Backend::Local), &failpoint);
                    // `inspect` and `finish_by_resume` panic with the
                    // details; name the crash point on the way out.
                    let guard = NameOnPanic(&failpoint);
                    let crashed = inspect(&world, &scenario, before);
                    finish_by_resume(&crashed, Backend::Local);
                    std::mem::forget(guard);
                }
            });
        }
    });
}

/// Prints which crash point a failing sweep iteration was on.
struct NameOnPanic<'a>(&'a str);

impl Drop for NameOnPanic<'_> {
    fn drop(&mut self) {
        eprintln!("^^^ while sweeping crash point {:?}", self.0);
    }
}

#[test]
fn copy_tree_survives_sigkill_at_every_step_boundary() {
    sweep(World::new, |w| scenarios::copy_tree(w, false));
}

#[test]
fn copy_tree_with_verification_survives_sigkill_at_every_step_boundary() {
    sweep(World::new, |w| scenarios::copy_tree(w, true));
}

#[test]
fn cross_device_move_survives_sigkill_at_every_step_boundary() {
    sweep(World::with_second_device, |w| {
        scenarios::move_tree_across_devices(w, false)
    });
}

#[test]
fn cross_device_move_with_verification_survives_sigkill_at_every_step_boundary() {
    sweep(World::with_second_device, |w| {
        scenarios::move_tree_across_devices(w, true)
    });
}

#[test]
fn same_device_move_survives_sigkill_at_every_step_boundary() {
    sweep(World::new, scenarios::move_tree_same_device);
}

#[test]
fn permanent_delete_survives_sigkill_at_every_step_boundary() {
    sweep(World::new, scenarios::delete_tree);
}

#[test]
fn delete_to_trash_survives_sigkill_at_every_step_boundary() {
    sweep(World::new, scenarios::trash_tree);
}

/// A crash while recovering from a crash. The first kill lands mid-job;
/// the resume is prepared exactly as the product prepares it (old journal
/// closed out, remainder handed to a fresh job) and then *that* job is
/// killed at every step boundary in turn. The second recovery must still
/// arrive at exactly the finished original job.
fn crash_during_recovery(new_world: fn() -> World, setup: impl Fn(&World) -> Scenario) {
    let (steps, first_crash) = {
        let world = new_world();
        let steps = setup(&world).plan.steps.len();
        (steps, format!("step.before_effect@{}", steps / 3))
    };
    // Upper bound on the remainder's length; a crash point past the end of
    // the resumed plan is simply never reached and the iteration stops.
    for second_step in 0..steps {
        for boundary in ["step.before_effect", "step.after_effect"] {
            let world = new_world();
            let scenario = setup(&world);
            let before = world.capture();
            crash_at(&world, &scenario.spec(&world, Backend::Local), &first_crash);
            let first = inspect(&world, &scenario, before);

            // The product's resume, with the remainder run in a child so
            // it can be killed: prepare, close the old journal, start the
            // new job.
            let report = first.report().clone();
            let decision = block_on(duet_ops::prepare_resume(&duet_vfs::LocalFs, &report)).unwrap();
            Journal::open(report.job_id, &world.state)
                .and_then(|journal| journal.resolve(&report, "resumed"))
                .unwrap();
            if second_step >= decision.plan.steps.len() {
                return;
            }
            let remainder = Scenario {
                plan: decision.plan.clone(),
                kind: scenario.kind,
                deletable: scenario.deletable.clone(),
            };
            let before_second = world.capture();
            let spec = ChildSpec {
                job_id: RESUME_JOB,
                ..remainder.spec(&world, Backend::Local)
            };
            crash_at(&world, &spec, &format!("{boundary}@{second_step}"));

            let after = world.capture();
            let statuses = world.statuses(&remainder.plan, RESUME_JOB);
            let context = format!("first {first_crash}, then {boundary}@{second_step}");
            check_interrupted(&before_second, &remainder.plan, &statuses, &after)
                .unwrap_or_else(|why| panic!("{context}: illegal state:\n{why}"));
            check_no_content_lost(&first.before, &after, &scenario.deletable)
                .unwrap_or_else(|why| panic!("{context}: {why}"));

            // Recover again, in-process this time.
            let second_report = world
                .scan(RESUME_JOB)
                .expect("the resumed job journaled JobStarted");
            if second_report.needs_attention() {
                let again =
                    block_on(duet_ops::prepare_resume(&duet_vfs::LocalFs, &second_report)).unwrap();
                if !again.plan.steps.is_empty() {
                    let job = run_job(
                        Backend::Local.fs(),
                        &world.state,
                        RESUME_JOB + 1,
                        scenario.kind,
                        again.plan,
                        4,
                    );
                    assert!(
                        job.errors.is_empty() && job.skipped.is_empty(),
                        "{context}: second recovery reported {job:?}"
                    );
                }
                Journal::open(second_report.job_id, &world.state)
                    .and_then(|journal| journal.resolve(&second_report, "resumed again"))
                    .unwrap();
            }
            let finished = world.capture();
            check_finished(&first.before, &scenario.plan, &finished)
                .unwrap_or_else(|why| panic!("{context}: not the finished job:\n{why}"));
            assert_nothing_left_to_recover(&world);
            let _ = JOB;
        }
    }
}

#[test]
fn copy_tree_survives_a_second_sigkill_during_recovery() {
    crash_during_recovery(World::new, |w| scenarios::copy_tree(w, true));
}

#[test]
fn cross_device_move_survives_a_second_sigkill_during_recovery() {
    crash_during_recovery(World::with_second_device, |w| {
        scenarios::move_tree_across_devices(w, true)
    });
}

#[test]
fn delete_to_trash_survives_a_second_sigkill_during_recovery() {
    crash_during_recovery(World::new, scenarios::trash_tree);
}

/// A kill can land *inside* the journal's own `write(2)`, leaving the file
/// ending mid-record (the stress runs of this suite produced exactly
/// that). Reproduced here deterministically: the process dies just before
/// a batch is written, and the first half of a record is appended by hand
/// -- byte for byte what a `write` cut short leaves. The job must still
/// be inspectable, resumable and discardable, and closing its journal out
/// must not glue a new record onto the fragment: that turned the journal
/// into one the reader rejects as corrupt, failing every later startup
/// scan for every job.
#[test]
fn a_journal_torn_mid_record_by_the_kill_still_scans_resumes_and_closes() {
    use std::io::Write;
    for resume_it in [true, false] {
        for mid_step in [0usize, 9, 20] {
            let world = World::new();
            let scenario = scenarios::copy_tree(&world, true);
            let before = world.capture();
            crash_at(
                &world,
                &scenario.spec(&world, Backend::Local),
                &format!("journal.intent.before_write@{mid_step}"),
            );
            let journal = world.state.join("jobs").join(format!("{JOB}.journal"));
            let whole = serde_json::to_string(&duet_ops::JournalRecord::Intent {
                step_index: mid_step as u32,
                step: scenario.plan.steps[mid_step].clone(),
                partial_name: None,
            })
            .unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&journal)
                .unwrap()
                .write_all(&whole.as_bytes()[..whole.len() / 2])
                .unwrap();

            let crashed = inspect(&world, &scenario, before);
            if resume_it {
                finish_by_resume(&crashed, Backend::Local);
            } else {
                crate::sigkill_matrix::finish_by_discard(&crashed, Backend::Local);
            }
            // And the journal is well-formed again, line for line.
            let text = std::fs::read_to_string(&journal).unwrap();
            assert!(text.ends_with('\n'));
            for line in text.lines() {
                serde_json::from_str::<duet_ops::JournalRecord>(line)
                    .unwrap_or_else(|e| panic!("journal line does not parse ({e}): {line}"));
            }
        }
    }
}
