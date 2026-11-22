// SPDX-License-Identifier: MIT
//! `EACCES`, from real permission bits.
//!
//! No injection here: the files and directories really are unreadable or
//! unwritable, and the errors come from the kernel. What is asserted is
//! the §14.4 invariant plus the two things specific to a permission
//! failure -- it is reported (never retried into a hang, never swallowed),
//! and it stops at the entry it concerns: the rest of the job proceeds,
//! and nothing is removed on the strength of a copy that did not happen.
//!
//! These cases cannot run as root, which bypasses permission bits. They
//! fail, saying so, rather than pass vacuously.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use duet_ops::{
    CancelToken, DeleteMode, JobKind, PlanOptions, RemoveMode, Step, plan_copy, plan_delete,
    plan_from_report, plan_move,
};
use duet_types::ErrorKind;
use duet_vfs::LocalFs;

use crate::harness::{
    Backend, JOB, RESUME_JOB, World, block_on, crash_at, pattern, resume, run_job, vp, write_file,
};
use crate::model::{check_finished, check_interrupted, check_no_content_lost, compare, real};
use crate::scenarios::{self, Scenario};

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Fails the test, with the reason, when permission bits do not bind this
/// process.
#[track_caller]
fn require_unprivileged(world: &World) {
    let probe = world.state.join("permission-probe");
    std::fs::write(&probe, b"x").unwrap();
    chmod(&probe, 0o000);
    let readable = std::fs::read(&probe).is_ok();
    chmod(&probe, 0o600);
    std::fs::remove_file(&probe).unwrap();
    assert!(
        !readable,
        "this process can read a mode-000 file (root, or CAP_DAC_OVERRIDE): the EACCES cases of \
         the data-safety suite must be run as an ordinary user"
    );
}

/// Restores permissions on drop, so a failing assertion never leaves a
/// temporary directory that cannot be cleaned up or captured.
struct Restore<'a>(Vec<(&'a Path, u32)>);

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        for (path, mode) in &self.0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode));
        }
    }
}

#[test]
fn copy_with_an_unreadable_source_file_reports_it_and_copies_the_rest() {
    let world = World::new();
    require_unprivileged(&world);
    let scenario = scenarios::copy_tree(&world, false);
    let locked = world.root.join("src/docs/big.bin");
    let before = world.capture();

    chmod(&locked, 0o000);
    let restore = Restore(vec![(&locked, 0o644)]);
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        scenario.kind,
        scenario.plan.clone(),
        4,
    );
    drop(restore);

    // Reported once, as a permission problem, on the file it concerns.
    let denied: Vec<_> = report
        .errors
        .iter()
        .filter(|failure| failure.kind == ErrorKind::Permission)
        .collect();
    assert_eq!(denied.len(), 1, "{report:?}");
    assert_eq!(
        denied[0].path.as_ref().map(real),
        Some(world.root.join("dst/src/docs/big.bin"))
    );
    assert_eq!(
        report.errors.len(),
        1,
        "nothing else may have failed: {report:?}"
    );

    let after = world.capture();
    let statuses = world.statuses(&scenario.plan, JOB);
    check_interrupted(&before, &scenario.plan, &statuses, &after).unwrap();
    check_no_content_lost(&before, &after, &[]).unwrap();
    assert!(after.partials().is_empty(), "{:?}", after.partials());
    // The denied file has no destination at all -- not an empty one, not
    // a partial one.
    assert!(
        after
            .get(&world.root.join("dst/src/docs/big.bin"))
            .is_none()
    );
    // Every other file arrived.
    assert!(
        after
            .get(&world.root.join("dst/src/docs/readme.md"))
            .is_some()
    );
    assert!(
        after
            .get(&world.root.join("dst/src/media/clip.raw"))
            .is_some()
    );

    // Fix the permission, re-run the failed part: the job is whole.
    let rerun = plan_from_report(&scenario.plan, &report);
    let second = run_job(
        Backend::Local.fs(),
        &world.state,
        RESUME_JOB,
        scenario.kind,
        rerun,
        4,
    );
    assert!(
        second.errors.is_empty() && second.skipped.is_empty(),
        "{second:?}"
    );
    check_finished(&before, &scenario.plan, &world.capture()).unwrap();
}

#[test]
fn copy_into_an_unwritable_directory_creates_nothing_and_says_so() {
    let world = World::new();
    require_unprivileged(&world);
    let scenario = scenarios::copy_tree(&world, false);
    let dst = world.root.join("dst");
    let before = world.capture();

    chmod(&dst, 0o555);
    let restore = Restore(vec![(&dst, 0o755)]);
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        scenario.kind,
        scenario.plan.clone(),
        4,
    );
    drop(restore);

    assert!(
        report
            .errors
            .iter()
            .any(|failure| failure.kind == ErrorKind::Permission),
        "{report:?}"
    );
    assert_eq!(report.files_completed, 0);
    // Nothing was created, so nothing differs.
    compare(&before, &world.capture()).unwrap();
}

#[test]
fn cross_device_move_with_an_unreadable_source_file_keeps_that_file() {
    let world = World::with_second_device();
    require_unprivileged(&world);
    let scenario = scenarios::move_tree_across_devices(&world, true);
    let locked = world.root.join("src/docs/big.bin");
    let before = world.capture();

    chmod(&locked, 0o000);
    let restore = Restore(vec![(&locked, 0o644)]);
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        scenario.kind,
        scenario.plan.clone(),
        4,
    );
    drop(restore);

    assert!(
        report
            .errors
            .iter()
            .any(|failure| failure.kind == ErrorKind::Permission),
        "{report:?}"
    );
    let after = world.capture();
    let statuses = world.statuses(&scenario.plan, JOB);
    check_interrupted(&before, &scenario.plan, &statuses, &after).unwrap();
    check_no_content_lost(&before, &after, &[]).unwrap();
    // The file that could not be read is exactly where it was, and so is
    // every directory above it; the files that could be moved were.
    assert_eq!(after.get(&locked), before.get(&locked));
    assert!(after.get(&world.root.join("src/docs")).is_some());
    assert!(after.get(&world.root.join("src/top.txt")).is_none());
    assert!(after.get(&world.far().join("dst/src/top.txt")).is_some());

    let rerun = plan_from_report(&scenario.plan, &report);
    let second = run_job(
        Backend::Local.fs(),
        &world.state,
        RESUME_JOB,
        scenario.kind,
        rerun,
        4,
    );
    assert!(
        second.errors.is_empty() && second.skipped.is_empty(),
        "{second:?}"
    );
    check_finished(&before, &scenario.plan, &world.capture()).unwrap();
}

#[test]
fn cross_device_move_out_of_a_read_only_directory_leaves_both_copies_and_reports_it() {
    let world = World::with_second_device();
    require_unprivileged(&world);
    let scenario = scenarios::move_tree_across_devices(&world, true);
    // Files in `docs/deep` can be read but not unlinked.
    let pinned = world.root.join("src/docs/deep");
    let before = world.capture();

    chmod(&pinned, 0o555);
    let restore = Restore(vec![(&pinned, 0o755)]);
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        scenario.kind,
        scenario.plan.clone(),
        4,
    );
    drop(restore);
    // (restoring the mode does not restore the directory's ctime-derived
    // state the model ignores; its mtime was never touched)

    let denied = report
        .errors
        .iter()
        .filter(|failure| failure.kind == ErrorKind::Permission)
        .count();
    assert_eq!(
        denied, 2,
        "one per file that could not be removed: {report:?}"
    );

    let after = world.capture();
    let statuses = world.statuses(&scenario.plan, JOB);
    check_interrupted(&before, &scenario.plan, &statuses, &after).unwrap();
    check_no_content_lost(&before, &after, &[]).unwrap();
    for name in ["a.bin", "b.bin"] {
        // Copied: the destination is complete...
        assert!(
            after
                .get(&world.far().join("dst/src/docs/deep").join(name))
                .is_some()
        );
        // ...and the source that could not be removed is untouched.
        assert_eq!(
            after.get(&pinned.join(name)),
            before.get(&pinned.join(name))
        );
    }
}

#[test]
fn permanent_delete_stops_at_a_read_only_directory_and_removes_nothing_inside_it() {
    let world = World::new();
    require_unprivileged(&world);
    let tree = world.root.join("victim");
    write_file(&tree.join("free/a.bin"), &pattern(1, 500));
    write_file(&tree.join("pinned/b.bin"), &pattern(2, 500));
    write_file(&tree.join("pinned/c.bin"), &pattern(3, 500));
    write_file(&world.root.join("bystander.bin"), &pattern(4, 500));
    let plan = block_on(plan_delete(
        &LocalFs,
        &[vp(&tree)],
        DeleteMode::Permanent,
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();
    let before = world.capture();

    let pinned = tree.join("pinned");
    chmod(&pinned, 0o555);
    let restore = Restore(vec![(&pinned, 0o755)]);
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        JobKind::Delete { permanent: true },
        plan.clone(),
        4,
    );
    drop(restore);

    assert!(
        report
            .errors
            .iter()
            .any(|failure| failure.kind == ErrorKind::Permission),
        "{report:?}"
    );
    let after = world.capture();
    // What could not be deleted is intact; nothing outside the target was
    // touched; and the failure is bounded -- everything gone was asked
    // to go.
    assert_eq!(
        after.get(&pinned.join("b.bin")),
        before.get(&pinned.join("b.bin"))
    );
    assert_eq!(
        after.get(&pinned.join("c.bin")),
        before.get(&pinned.join("c.bin"))
    );
    assert_eq!(
        after.get(&world.root.join("bystander.bin")),
        before.get(&world.root.join("bystander.bin"))
    );
    let statuses = world.statuses(&plan, JOB);
    check_interrupted(&before, &plan, &statuses, &after).unwrap();
}

/// The crash that turns a permission failure into data loss if recovery
/// is careless. A cross-device move's copy fails (`EACCES`); the engine
/// journals the `Remove` of the source -- gated on that copy -- and would
/// next record it as skipped. The process dies in between. The journal
/// now holds a `Remove` with a dangling intent and a dependency that has
/// a `Completion`. A resume that reads "has a completion" as "is
/// satisfied" deletes a file that was never copied.
#[test]
fn a_crash_after_a_failed_copy_never_resumes_into_deleting_the_uncopied_source() {
    let world = World::with_second_device();
    require_unprivileged(&world);
    let source = world.root.join("src/secret.bin");
    write_file(&source, &pattern(9, 20_000));
    let dst = world.far().join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&source)],
        &vp(&dst),
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();
    let remove_index = plan
        .steps
        .iter()
        .position(|step| {
            matches!(step, Step::Remove { target, mode: RemoveMode::File, .. } if real(target) == source)
        })
        .expect("the move plans a Remove of its source");
    let scenario = Scenario {
        plan,
        kind: JobKind::Move,
        deletable: Vec::new(),
    };
    let before = world.capture();

    for failpoint in [
        format!("journal.intent.after_fsync@{remove_index}"),
        format!("step.after_effect@{remove_index}"),
    ] {
        chmod(&source, 0o000);
        let restore = Restore(vec![(&source, 0o644)]);
        crash_at(&world, &scenario.spec(&world, Backend::Local), &failpoint);

        let report = world.scan(JOB).expect("the crashed job's journal").clone();
        assert!(report.needs_attention(), "{failpoint}");
        // Resume while the file is still unreadable: the copy fails
        // again, and the source must still be there afterwards.
        let resumed = resume(&world, Backend::Local, &report);
        drop(restore);
        let job = resumed.report.expect("the remainder was re-run");
        assert!(
            job.errors
                .iter()
                .any(|failure| failure.kind == ErrorKind::Permission),
            "{failpoint}: the resume must report the copy it could not make: {job:?}"
        );
        let after = world.capture();
        assert_eq!(
            after.get(&source),
            before.get(&source),
            "{failpoint}: the source of a move whose copy failed must survive the crash and \
             the resume"
        );
        assert!(after.get(&dst.join("secret.bin")).is_none());

        // Reset the journals for the next crash point.
        std::fs::remove_dir_all(world.state.join("jobs")).unwrap();
    }
}

#[test]
fn copy_reports_a_permission_error_once_without_retrying_it() {
    // A permission failure is permanent: it must not be put through the
    // transient-error backoff (which would only delay the report), and
    // the report must tell the user what would help.
    let world = World::new();
    require_unprivileged(&world);
    let source = world.root.join("src/a.bin");
    write_file(&source, &pattern(5, 100));
    let dst = world.root.join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_copy(
        &LocalFs,
        &[vp(&source)],
        &vp(&dst),
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();

    chmod(&source, 0o000);
    let restore = Restore(vec![(&source, 0o644)]);
    let started = std::time::Instant::now();
    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        JobKind::Copy,
        plan.clone(),
        1,
    );
    let took = started.elapsed();
    drop(restore);

    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.errors[0].kind, ErrorKind::Permission);
    // One `Intent` for the copy: a retried step journals one per attempt.
    let intents = crate::model::read_journal(&world.state, JOB)
        .iter()
        .filter(|record| {
            matches!(record, duet_ops::JournalRecord::Intent { step, .. }
                if matches!(step, Step::CopyFile { .. } | Step::Reflink { .. }))
        })
        .count();
    assert_eq!(intents, 1, "a permission error must not be retried");
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
}

/// A job that hit an error and was then killed before it could finish has
/// told nobody about the error: the report lived in the dead process. It
/// must come back as an interrupted job, and resuming it must put the
/// failure in front of the user -- not close the job as done over a file
/// that was never copied.
#[test]
fn a_crash_after_a_reported_failure_does_not_bury_the_failure() {
    let world = World::new();
    require_unprivileged(&world);
    let scenario = scenarios::copy_tree(&world, false);
    let locked = world.root.join("src/docs/big.bin");
    let before = world.capture();

    chmod(&locked, 0o000);
    let restore = Restore(vec![(&locked, 0o644)]);
    // Every step has run -- one of them failed -- and the process dies
    // just before the job's last record.
    crash_at(
        &world,
        &scenario.spec(&world, Backend::Local),
        "journal.job_finished.before_write@0",
    );
    let report = world.scan(JOB).expect("the journal");
    assert!(report.was_interrupted());
    assert!(
        report.needs_attention(),
        "a killed job with a failed step must be offered for recovery"
    );

    let resumed = resume(&world, Backend::Local, &report);
    let job = resumed.report.expect("the failed part is re-run");
    assert!(
        job.errors
            .iter()
            .any(|failure| failure.kind == ErrorKind::Permission),
        "the resume must surface the failure the crash swallowed: {job:?}"
    );
    drop(restore);

    // Now that it has been reported, the ordinary "re-run failed" on the
    // fixed permission finishes the copy.
    let rerun = plan_from_report(&resumed.decision.plan, &job);
    let last = run_job(
        Backend::Local.fs(),
        &world.state,
        RESUME_JOB + 1,
        scenario.kind,
        rerun,
        4,
    );
    assert!(
        last.errors.is_empty() && last.skipped.is_empty(),
        "{last:?}"
    );
    check_finished(&before, &scenario.plan, &world.capture()).unwrap();
}
