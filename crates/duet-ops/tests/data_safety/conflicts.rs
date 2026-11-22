// SPDX-License-Identifier: MIT
//! "Skip" must mean *nothing happens to either file*.
//!
//! Not a fault-injection case, but squarely the §14.4 invariant: a step
//! that is skipped because something already occupies its destination has
//! not produced a complete destination, so the source must stay intact --
//! and the file that was there must stay exactly as it was. The steps
//! planned to *follow* a copy (apply the source's metadata to the
//! destination, remove the source of a move, hardlink a second name to
//! it) are all written on the assumption that the copy happened.

use duet_ops::{CancelToken, JobKind, PlanOptions, plan_copy, plan_move};
use duet_vfs::LocalFs;

use crate::harness::{Backend, JOB, World, block_on, pattern, run_job, set_mtime, vp, write_file};
use crate::model::compare;

/// The fixture: `src/a.bin`, and a *different* `a.bin` already sitting in
/// the destination directory.
fn occupied_destination(world: &World, dst: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let source = world.root.join("src/a.bin");
    write_file(&source, &pattern(1, 4000));
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    set_mtime(&source, 1_600_000_000);
    let existing = dst.join("a.bin");
    write_file(&existing, &pattern(2, 2500));
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
    set_mtime(&existing, 1_500_000_000);
}

#[test]
fn cross_device_move_that_skips_a_name_conflict_keeps_the_source() {
    let world = World::with_second_device();
    let dst = world.far().join("dst");
    occupied_destination(&world, &dst);
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&world.root.join("src/a.bin"))],
        &vp(&dst),
        PlanOptions::default(), // default_conflict: Skip
        &CancelToken::new(),
    ))
    .unwrap();
    let before = world.capture();

    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        JobKind::Move,
        plan,
        4,
    );

    assert!(report.errors.is_empty(), "{report:?}");
    assert!(
        !report.skipped.is_empty(),
        "the conflict must be reported as a skip: {report:?}"
    );
    // The whole point: nothing moved, so nothing may be gone, and the file
    // that was already there is byte-for-byte and attribute-for-attribute
    // what it was.
    compare(&before.ignoring_dir_mtimes(), &world.capture()).unwrap_or_else(|why| {
        panic!("a skipped move must change nothing at all:\n{why}");
    });
}

#[test]
fn copy_that_skips_a_name_conflict_leaves_the_existing_file_untouched() {
    let world = World::new();
    let dst = world.root.join("dst");
    occupied_destination(&world, &dst);
    let plan = block_on(plan_copy(
        &LocalFs,
        &[vp(&world.root.join("src/a.bin"))],
        &vp(&dst),
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();
    let before = world.capture();

    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        JobKind::Copy,
        plan,
        4,
    );

    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!report.skipped.is_empty(), "{report:?}");
    // In particular the existing file must not have been given the
    // skipped source's mode and mtime.
    compare(&before.ignoring_dir_mtimes(), &world.capture()).unwrap_or_else(|why| {
        panic!("a skipped copy must change nothing at all:\n{why}");
    });
}

#[test]
fn cross_device_move_of_a_tree_moves_everything_but_the_skipped_conflict() {
    let world = World::with_second_device();
    let src = world.root.join("src");
    write_file(&src.join("keep/a.bin"), &pattern(10, 3000));
    write_file(&src.join("keep/b.bin"), &pattern(11, 3000));
    write_file(&src.join("free.bin"), &pattern(12, 3000));
    let dst = world.far().join("dst");
    // `dst/src/keep/a.bin` already exists and is something else.
    write_file(&dst.join("src/keep/a.bin"), &pattern(13, 1000));
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&src)],
        &vp(&dst),
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();

    let report = run_job(
        Backend::Local.fs(),
        &world.state,
        JOB,
        JobKind::Move,
        plan,
        4,
    );
    assert!(report.errors.is_empty(), "{report:?}");

    // The conflicting file: both sides exactly as they were.
    assert_eq!(
        std::fs::read(src.join("keep/a.bin")).unwrap(),
        pattern(10, 3000)
    );
    assert_eq!(
        std::fs::read(dst.join("src/keep/a.bin")).unwrap(),
        pattern(13, 1000)
    );
    // Everything else: moved.
    assert_eq!(
        std::fs::read(dst.join("src/keep/b.bin")).unwrap(),
        pattern(11, 3000)
    );
    assert_eq!(
        std::fs::read(dst.join("src/free.bin")).unwrap(),
        pattern(12, 3000)
    );
    assert!(!src.join("keep/b.bin").exists());
    assert!(!src.join("free.bin").exists());
    // And the directories still holding the unmoved file are still there.
    assert!(src.join("keep").is_dir());
}
