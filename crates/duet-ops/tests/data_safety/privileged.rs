// SPDX-License-Identifier: MIT
//! The cases that need root: a real block device that fills up, fails,
//! or loses power.
//!
//! design.md §14.4 names them -- "ENOSPC via a small loop device ...
//! `dm-flakey` for I/O errors" -- and they are the only cases in the suite
//! where the fault comes from below the filesystem:
//!
//! - **`ENOSPC` on a loop device**: the same cases as `enospc::`, on a
//!   16 MiB `ext4` instead of a `tmpfs`.
//! - **A device that goes away mid-transfer** (`dm` `error` target): every
//!   read and write fails with `EIO` from the block layer up, the job
//!   ends in errors, the device comes back and is remounted.
//! - **Power loss** (`dm-flakey` `drop_writes`): the process is killed and,
//!   before anything it left in the page cache can be written back, the
//!   device starts discarding every write; then the filesystem is
//!   remounted. This is the one fault `SIGKILL` cannot stand in for,
//!   because it also loses whatever was written and not flushed -- it is
//!   what tests that a source is never unlinked before its copy is
//!   *durable*, and that the journal survives losing its unflushed tail.
//!
//!   The order matters. A power cut stops the process and the disk at the
//!   same instant. Switching the device to `drop_writes` while the process
//!   is still running models something else -- a disk that acknowledges
//!   `fsync` and keeps nothing -- which no program can defend against, and
//!   which the first CI run of this file duly "failed" (every `fsync`
//!   returned success in microseconds, the job finished, all forty sources
//!   were unlinked, and the remount showed an empty destination).
//!
//! # Why these are `#[ignore]`d, and why that is not a loophole
//!
//! Creating a loop device and a device-mapper target needs root, which a
//! developer's `cargo nextest run` does not have and should not need. So
//! the tests are ignored by default and run by CI's
//! `data-safety (privileged)` job on every pull request, with
//! `--run-ignored ignored-only` and no retries. They are part of the
//! release gate; "ignored" only says where they run. Run them locally
//! with passwordless `sudo`:
//!
//! ```text
//! cargo nextest run -p duet-ops --test data_safety \
//!     --run-ignored ignored-only -j 1
//! ```
//!
//! The test process itself stays unprivileged (Duet is an unprivileged
//! program writing to a filesystem somebody else mounted) and calls
//! `scripts/data-safety-device.sh` through `sudo -n` for the device
//! plumbing only. Without passwordless `sudo` they fail, saying so.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use duet_ops::{CancelToken, JobKind, JournalReader, PlanOptions, plan_move};
use duet_vfs::LocalFs;
use tempfile::TempDir;

use crate::harness::{
    Backend, Child, ChildEnd, ChildSpec, JOB, World, block_on, pattern, resume_in, vp, write_file,
};
use crate::model::{Tree, check_no_content_lost};
use crate::scenarios::Scenario;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/data-safety-device.sh"
);

#[track_caller]
fn device(args: &[&str]) {
    let output = Command::new("sudo")
        .arg("-n")
        .arg(SCRIPT)
        .args(args)
        .output()
        .expect("run sudo");
    assert!(
        output.status.success(),
        "`sudo -n {SCRIPT} {}` failed ({:?}). The privileged data-safety cases need passwordless \
         sudo and the dm-flakey kernel module.\n{}{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Where device images and mount points live: on a real disk, not a
/// `tmpfs` (a loop device over a `tmpfs` file has no stable storage to
/// lose).
fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("duet-data-safety-dev-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// A small `ext4` on a loop device, torn down on drop.
struct LoopFs {
    _scratch: TempDir,
    mnt: PathBuf,
}

impl LoopFs {
    fn new(size_mb: u32) -> LoopFs {
        let scratch = scratch();
        let image = scratch.path().join("fs.img");
        let mnt = scratch.path().join("mnt");
        device(&[
            "loop-up",
            image.to_str().unwrap(),
            mnt.to_str().unwrap(),
            &size_mb.to_string(),
        ]);
        LoopFs {
            _scratch: scratch,
            mnt,
        }
    }
}

impl Drop for LoopFs {
    fn drop(&mut self) {
        let _ = Command::new("sudo")
            .args(["-n", SCRIPT, "loop-down", self.mnt.to_str().unwrap()])
            .status();
    }
}

/// An `ext4` on a `dm-flakey` device, torn down on drop.
struct FlakeyFs {
    _scratch: TempDir,
    name: String,
    mnt: PathBuf,
}

impl FlakeyFs {
    fn new(size_mb: u32) -> FlakeyFs {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let scratch = scratch();
        let name = format!(
            "duet-ds-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let image = scratch.path().join("fs.img");
        let mnt = scratch.path().join("mnt");
        device(&[
            "flakey-up",
            &name,
            image.to_str().unwrap(),
            mnt.to_str().unwrap(),
            &size_mb.to_string(),
        ]);
        FlakeyFs {
            _scratch: scratch,
            name,
            mnt,
        }
    }

    fn mode(&self, mode: &str) {
        device(&["flakey-mode", &self.name, mode]);
    }

    /// The machine coming back: unmount, restore the device, replay the
    /// filesystem journal, mount.
    fn remount(&self) {
        device(&["flakey-remount", &self.name, self.mnt.to_str().unwrap()]);
    }
}

impl Drop for FlakeyFs {
    fn drop(&mut self) {
        let _ = Command::new("sudo")
            .args([
                "-n",
                SCRIPT,
                "flakey-down",
                &self.name,
                self.mnt.to_str().unwrap(),
            ])
            .status();
    }
}

// ------------------------------------------------- ENOSPC on a loop device

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn loop_device_copy_onto_a_full_filesystem_pauses_then_completes_when_space_is_freed() {
    let fs = LoopFs::new(16);
    crate::enospc::run_case("copy_pauses_then_completes", &fs.mnt);
}

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn loop_device_cross_device_move_onto_a_full_filesystem_keeps_every_file_it_could_not_copy() {
    let fs = LoopFs::new(16);
    crate::enospc::run_case("move_onto_full_filesystem_then_cancel", &fs.mnt);
}

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn loop_device_a_job_whose_journal_cannot_be_written_touches_nothing() {
    let fs = LoopFs::new(16);
    crate::enospc::run_case("journal_on_full_filesystem_touches_nothing", &fs.mnt);
}

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn loop_device_a_journal_that_fills_up_mid_job_stops_the_job_cleanly_and_resumes_later() {
    let fs = LoopFs::new(16);
    crate::enospc::run_case(
        "journal_filling_up_mid_job_stops_cleanly_and_resumes",
        &fs.mnt,
    );
}

// ------------------------------------------------------------- dm-flakey

/// Forty 256 KiB files in a few directories, to be moved onto the device.
fn move_onto(world: &World, device_root: &Path) -> (Scenario, PathBuf) {
    let src = world.root.join("src");
    for i in 0..40u64 {
        write_file(
            &src.join(format!("d{}/f{i:02}.bin", i % 4)),
            &pattern(700 + i, 256 * 1024),
        );
    }
    let dst = device_root.join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&src)],
        &vp(&dst),
        PlanOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();
    (
        Scenario {
            plan,
            kind: JobKind::Move,
            deletable: Vec::new(),
        },
        dst,
    )
}

/// Blocks until the job's journal holds at least `records` records.
fn wait_for_journal(state: &Path, records: usize) {
    let journal = state.join("jobs").join(format!("{JOB}.journal"));
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let have = std::fs::read(&journal)
            .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count())
            .unwrap_or(0);
        if have >= records {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the job never got {records} records into its journal (has {have})"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The one thing that must hold whatever the device did: every file that
/// existed before the move still exists, whole, under a real name -- at
/// its source or at its destination. (A truncated destination does not
/// count: it is a different content, so its source had better still be
/// there.)
#[track_caller]
fn assert_every_file_survives(before: &Tree, world: &World, dst: &Path, context: &str) {
    let mut roots = world.roots();
    roots.push(dst.to_path_buf());
    let after = Tree::capture(&roots);
    check_no_content_lost(before, &after, &[]).unwrap_or_else(|why| panic!("{context}: {why}"));
}

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn dm_error_device_disappearing_mid_move_loses_no_file() {
    // The device goes away at three different depths into the job.
    for records in [20, 60, 120] {
        let fs = FlakeyFs::new(64);
        let world = World::new();
        let (scenario, dst) = move_onto(&world, &fs.mnt);
        let before = world.capture();
        let spec = ChildSpec {
            concurrency: 2,
            ..scenario.spec(&world, Backend::Local)
        };
        let child = Child::spawn(&world, &spec, None);
        wait_for_journal(&world.state, records);
        fs.mode("error");
        // The job must *end* -- in errors -- not hang on a dead device.
        let report = match child.wait() {
            ChildEnd::Finished(report) => report,
            ChildEnd::Killed => panic!("the child was killed"),
        };
        let context = format!("device gone after {records} journal records");
        // With the device still gone: nothing that was at the source and
        // is no longer there may be missing from the page cache's view.
        fs.remount();
        assert_every_file_survives(&before, &world, &dst, &context);
        // A job that lost its device must not have reported a clean
        // finish unless everything really arrived.
        if report.errors.is_empty() && report.skipped.is_empty() {
            let after = Tree::capture(std::slice::from_ref(&dst));
            assert_eq!(
                after
                    .nodes
                    .values()
                    .filter(|node| matches!(node, crate::model::Node::File { .. }))
                    .count(),
                40,
                "{context}: the job reported success but files are missing after remount"
            );
        }
    }
}

/// What the test itself writes to the device around the power cut, to
/// prove the rig did what the test's name says. Without these a remount
/// that lost *nothing* (the device never dropped a write) or *everything*
/// (the filesystem came back empty) could not be told from a pass or a
/// failure of the engine.
struct Canaries {
    before: PathBuf,
    after: PathBuf,
    content: Vec<u8>,
}

impl Canaries {
    fn new(mnt: &Path) -> Canaries {
        Canaries {
            before: mnt.join("canary-synced-before-the-cut"),
            after: mnt.join("canary-written-after-the-cut"),
            content: pattern(9001, 256 * 1024),
        }
    }

    /// Written and `fsync`'d, file and directory -- the same thing the
    /// engine does for a copy it is about to rely on.
    fn write_durably(path: &Path, content: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(content).unwrap();
        file.sync_all().unwrap();
        std::fs::File::open(path.parent().unwrap())
            .unwrap()
            .sync_all()
            .unwrap();
    }

    /// While the device still works.
    fn before_the_cut(&self) {
        Self::write_durably(&self.before, &self.content);
    }

    /// Once the device is discarding writes. `drop_writes` acknowledges
    /// everything, `fsync` included; the results are ignored all the same,
    /// because what is being established is only that none of it lasts.
    fn after_the_cut(&self) {
        use std::io::Write;
        if let Ok(mut file) = std::fs::File::create(&self.after) {
            let _ = file.write_all(&self.content);
            let _ = file.sync_all();
        }
    }

    #[track_caller]
    fn assert_power_was_cut(&self, context: &str) {
        assert_eq!(
            std::fs::read(&self.before).ok().as_deref(),
            Some(self.content.as_slice()),
            "{context}: a file fsync'd before the cut did not survive the remount -- the rig \
             loses more than a power cut does, so this run proves nothing"
        );
        assert_ne!(
            std::fs::read(&self.after).ok().as_deref(),
            Some(self.content.as_slice()),
            "{context}: a file written after the cut survived the remount -- the device never \
             dropped a write, so this run proves nothing"
        );
    }
}

#[test]
#[ignore = "needs root (loop device / dm-flakey): run by CI's data-safety (privileged) job"]
fn dm_flakey_power_loss_mid_move_loses_no_file_and_leaves_a_recoverable_journal() {
    // Power is cut at five different depths into the job, given as a
    // share of the journal records the whole job writes (an intent and a
    // completion per step). The journal lives on the same device as the
    // destination, so it loses its unflushed tail exactly as the data
    // does.
    for percent in [5, 20, 40, 65, 90] {
        let fs = FlakeyFs::new(64);
        let world = World::new();
        let state = fs.mnt.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let (scenario, dst) = move_onto(&world, &fs.mnt);
        let records = (2 * scenario.plan.steps.len() * percent / 100).max(2);
        let before = world.capture();
        let canaries = Canaries::new(&fs.mnt);
        let spec = ChildSpec {
            state_dir: state.clone(),
            concurrency: 2,
            linger: true,
            ..scenario.spec(&world, Backend::Local)
        };
        let mut child = Child::spawn(&world, &spec, None);
        wait_for_journal(&state, records);
        canaries.before_the_cut();
        // The lights go out: the process stops mid-syscall...
        child.kill();
        assert!(matches!(child.wait(), ChildEnd::Killed));
        // ...and nothing it left in the page cache ever reaches the
        // platter. (Whatever the kernel wrote back on its own between the
        // two lines is what a power cut a few milliseconds later would
        // have kept too.)
        fs.mode("drop");
        canaries.after_the_cut();
        fs.remount();

        let context = format!("power lost {percent}% in, after {records} journal records");
        canaries.assert_power_was_cut(&context);
        assert_every_file_survives(&before, &world, &dst, &context);

        // The journal came back with a torn or missing tail; it must
        // still scan, and a resume from it must lose nothing either.
        let reports = JournalReader::scan(&state).unwrap_or_else(|e| {
            panic!("{context}: the journal does not scan after power loss: {e}")
        });
        if let Some(report) = reports.into_iter().find(|r| r.needs_attention()) {
            let _ = resume_in(&state, Backend::Local, &report);
            assert_every_file_survives(&before, &world, &dst, &format!("{context}, after resume"));
            JournalReader::scan(&state)
                .unwrap_or_else(|e| panic!("{context}: journal unreadable after resume: {e}"));
        }
    }
}
