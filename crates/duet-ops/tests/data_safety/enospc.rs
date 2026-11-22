// SPDX-License-Identifier: MIT
//! `ENOSPC`, from a real filesystem that is really full.
//!
//! The cases in [`cases`] take a directory on a small, dedicated
//! filesystem and fill it. Two ways of providing that filesystem:
//!
//! - **here**: a size-limited `tmpfs`, mounted inside a private user and
//!   mount namespace (`unshare --user --map-root-user --mount`), which
//!   needs no privilege. Each test below re-executes this test binary
//!   inside such a namespace and runs one case there.
//! - **`privileged::`**: an `ext4` filesystem on a loop device -- the
//!   setup design.md §14.4 names -- which needs root to create and so
//!   runs in CI's privileged job. Same cases, same assertions.
//!
//! If unprivileged user namespaces are disabled on the machine, the tests
//! here **fail**, saying how to enable them; they do not skip.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use duet_ops::{
    ControlState, ExecutionControl, JobEvent, JobId, JobKind, JobReport, Journal, Plan, execute,
};
use duet_vfs::{FileSystem, LocalFs};

use crate::harness::vp;

const CASE_ENV: &str = "DUET_DATA_SAFETY_ENOSPC_CASE";
const DONE_MARKER: &str = "DUET_DATA_SAFETY_ENOSPC_CASE_PASSED";

/// A job running on its own runtime, with its controls in reach.
pub struct Running {
    runtime: tokio::runtime::Runtime,
    pub control: ExecutionControl,
    events: tokio::sync::mpsc::UnboundedReceiver<JobEvent>,
    handle: tokio::task::JoinHandle<JobReport>,
}

impl Running {
    pub fn start(
        state_dir: &Path,
        job_id: u64,
        kind: JobKind,
        plan: Plan,
    ) -> Result<Running, String> {
        let journal = Journal::open(JobId(job_id), state_dir).map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let control = ExecutionControl::new();
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let handle = runtime.spawn(execute(
            fs,
            JobId(job_id),
            kind,
            plan,
            journal,
            1, // one copy at a time: when it stops for space, nothing else is mid-flight
            tx,
            control.clone(),
            None,
        ));
        Ok(Running {
            runtime,
            control,
            events,
            handle,
        })
    }

    /// Blocks until the job has paused itself for lack of space. A job
    /// that finishes without ever doing so fails the case: the fixture did
    /// not fill the filesystem.
    pub fn wait_until_paused_for_space(&mut self) {
        let events = &mut self.events;
        let seen = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match events.recv().await {
                        Some(JobEvent::QueuePausedForSpace { .. }) => return Ok(()),
                        Some(JobEvent::Finished { report, .. }) => return Err(report),
                        Some(_) => {}
                        None => panic!("the job's event stream ended"),
                    }
                }
            })
            .await
        });
        match seen {
            Ok(Ok(())) => {}
            Ok(Err(report)) => panic!("the job finished without running out of space: {report:?}"),
            Err(_) => panic!("the job neither paused for space nor finished within 60 s"),
        }
        // The pause is requested before the event is sent; let the step
        // loop settle into it.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.control.state() != ControlState::Paused {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    /// Lets a job that paused for space run on, now that space has been
    /// freed -- resuming it again each time it pauses, because a
    /// filesystem may hand freed blocks back only at its next commit
    /// (`ext4` does), so the first retry can still find the disk full.
    pub fn finish_once_space_is_back(mut self, filesystem: &Path) -> JobReport {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            // Force the commit that makes freed space allocatable.
            let _ = Command::new("sync").arg("-f").arg(filesystem).status();
            self.control.resume();
            let events = &mut self.events;
            let finished = self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(120), async {
                    loop {
                        match events.recv().await {
                            Some(JobEvent::QueuePausedForSpace { .. }) => return false,
                            Some(JobEvent::Finished { .. }) | None => return true,
                            Some(_) => {}
                        }
                    }
                })
                .await
                .expect("the job neither finished nor paused again within 120 s")
            });
            if finished {
                return self.finish();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the job kept running out of space after space was freed"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn finish(self) -> JobReport {
        self.runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(120), self.handle).await })
            .expect("the job did not finish within 120 s")
            .expect("the job task panicked")
    }
}

/// Writes ballast into `dir` until exactly about `leave_free` bytes remain
/// available on its filesystem. Returns the ballast file.
pub fn fill_leaving(dir: &Path, leave_free: u64) -> PathBuf {
    use std::io::Write;
    let ballast = dir.join("ballast.bin");
    let mut file = std::fs::File::create(&ballast).unwrap();
    let block = vec![0xA5u8; 64 * 1024];
    loop {
        let available = crate::harness::block_on(LocalFs.volume_stats(&vp(dir)))
            .expect("statvfs the small filesystem")
            .available_bytes;
        if available <= leave_free {
            break;
        }
        let want = (available - leave_free).min(block.len() as u64) as usize;
        match file.write(&block[..want]) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(28) => break, // ENOSPC
            Err(e) => panic!("writing ballast: {e}"),
        }
    }
    file.sync_all().ok();
    ballast
}

/// The cases themselves, independent of how the small filesystem was
/// provided. `small` is an empty, writable directory at its root.
pub mod cases {
    use std::path::Path;

    use duet_ops::{
        CancelToken, JobKind, JobOutcome, JournalReader, Plan, PlanOptions, Step, plan_copy,
        plan_move,
    };
    use duet_types::ErrorKind;
    use duet_vfs::LocalFs;

    use super::{Running, fill_leaving};
    use crate::harness::{Backend, JOB, World, block_on, pattern, resume_in, vp, write_file};
    use crate::model::{
        Tree, check_finished, check_interrupted, check_no_content_lost, read_journal, step_statuses,
    };

    /// Six files of 600 KiB each.
    fn six_files(src: &Path) {
        for i in 0..6u64 {
            write_file(&src.join(format!("f{i}.bin")), &pattern(50 + i, 600 * 1024));
        }
    }

    fn roots(world: &World, small: &Path) -> Vec<std::path::PathBuf> {
        let mut roots = world.roots();
        roots.push(small.join("dst"));
        roots
    }

    /// A copy onto a filesystem with room for only part of it: the job
    /// pauses instead of failing, leaves no truncated file under a real
    /// name and no scratch file eating the space it is waiting for, and
    /// completes once space is freed.
    pub fn copy_pauses_then_completes(small: &Path) {
        let world = World::new();
        let src = world.root.join("src");
        six_files(&src);
        let dst = small.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        let plan = block_on(plan_copy(
            &LocalFs,
            &[vp(&src)],
            &vp(&dst),
            PlanOptions::default(),
            &CancelToken::new(),
        ))
        .unwrap();
        let ballast = fill_leaving(small, 1_600 * 1024); // room for two of the six
        let before = Tree::capture(&roots(&world, small));

        let mut job = Running::start(&world.state, JOB, JobKind::Copy, plan.clone()).unwrap();
        job.wait_until_paused_for_space();

        let paused = Tree::capture(&roots(&world, small));
        let statuses = step_statuses(&plan, &read_journal(&world.state, JOB));
        check_interrupted(&before, &plan, &statuses, &paused)
            .unwrap_or_else(|why| panic!("while paused for space:\n{why}"));
        check_no_content_lost(&before, &paused, &[]).unwrap();
        assert!(
            paused.partials().is_empty(),
            "a job waiting for space must not hold space in scratch files: {:?}",
            paused.partials()
        );
        let copied = plan
            .steps
            .iter()
            .filter(|step| match step {
                Step::CopyFile { dest, .. } | Step::Reflink { dest, .. } => {
                    paused.get(&crate::model::real(dest)).is_some()
                }
                _ => false,
            })
            .count();
        assert!(
            (1..6).contains(&copied),
            "the fixture should have room for some files but not all, copied {copied}"
        );

        std::fs::remove_file(&ballast).unwrap();
        let report = job.finish_once_space_is_back(small);
        assert!(
            report.errors.is_empty() && report.skipped.is_empty(),
            "once space is back the copy must simply complete: {report:?}"
        );
        let finished = Tree::capture(&roots(&world, small));
        check_finished(&before, &plan, &finished).unwrap();
        assert!(finished.partials().is_empty());
    }

    /// A cross-device move onto a filesystem that fills up, then cancelled
    /// by the user while it waits: every file is either still at the
    /// source or complete at the destination.
    pub fn move_onto_full_filesystem_then_cancel(small: &Path) {
        let world = World::new();
        let src = world.root.join("src");
        six_files(&src);
        let dst = small.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        let plan = block_on(plan_move(
            &LocalFs,
            &[vp(&src)],
            &vp(&dst),
            PlanOptions {
                verify: true,
                ..PlanOptions::default()
            },
            &CancelToken::new(),
        ))
        .unwrap();
        assert!(
            plan.steps
                .iter()
                .any(|s| matches!(s, Step::CopyFile { .. })),
            "the small filesystem must be a different device"
        );
        fill_leaving(small, 1_600 * 1024);
        let before = Tree::capture(&roots(&world, small));

        let mut job = Running::start(&world.state, JOB, JobKind::Move, plan.clone()).unwrap();
        job.wait_until_paused_for_space();
        let paused = Tree::capture(&roots(&world, small));
        let statuses = step_statuses(&plan, &read_journal(&world.state, JOB));
        check_interrupted(&before, &plan, &statuses, &paused)
            .unwrap_or_else(|why| panic!("while paused for space:\n{why}"));
        check_no_content_lost(&before, &paused, &[]).unwrap();

        job.control.cancel();
        let report = job.finish();
        assert!(
            report.errors.is_empty(),
            "a cancel is not an error: {report:?}"
        );

        let after = Tree::capture(&roots(&world, small));
        let statuses = step_statuses(&plan, &read_journal(&world.state, JOB));
        check_interrupted(&before, &plan, &statuses, &after)
            .unwrap_or_else(|why| panic!("after cancelling:\n{why}"));
        check_no_content_lost(&before, &after, &[]).unwrap();
        assert!(after.partials().is_empty(), "{:?}", after.partials());
        let reports = JournalReader::scan(&world.state).unwrap();
        assert_eq!(reports[0].last_outcome, Some(JobOutcome::Cancelled));
    }

    /// The journal's own filesystem is full before the job starts: the job
    /// must fail up front and touch nothing, not run unjournaled.
    pub fn journal_on_full_filesystem_touches_nothing(small: &Path) {
        let world = World::new();
        let src = world.root.join("src");
        six_files(&src);
        let dst = world.root.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        let plan = block_on(plan_copy(
            &LocalFs,
            &[vp(&src)],
            &vp(&dst),
            PlanOptions::default(),
            &CancelToken::new(),
        ))
        .unwrap();
        let state = small.join("state");
        std::fs::create_dir_all(&state).unwrap();
        fill_leaving(small, 0);
        let before = world.capture();

        match Running::start(&state, JOB, JobKind::Copy, plan) {
            // Not even the journal file could be created.
            Err(why) => assert!(why.contains("space"), "{why}"),
            Ok(job) => {
                let report = job.finish();
                assert_eq!(report.errors.len(), 1, "{report:?}");
                assert_eq!(report.errors[0].kind, ErrorKind::Space, "{report:?}");
                assert_eq!(report.files_completed, 0);
            }
        }
        crate::model::compare(&before, &world.capture())
            .unwrap_or_else(|why| panic!("a job that could not be journaled ran anyway:\n{why}"));
    }

    /// The journal's filesystem fills up *during* the job: from that
    /// moment no step may run (a step with no durable intent is a step
    /// recovery cannot see), what did run is all accounted for, the torn
    /// journal still scans, and once space is back a resume finishes the
    /// job.
    pub fn journal_filling_up_mid_job_stops_cleanly_and_resumes(small: &Path) {
        let world = World::new();
        let dst = world.root.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        let steps: Vec<Step> = (0..3000)
            .map(|i| Step::CreateDir {
                dest: vp(&dst.join(format!("d{i:05}"))),
                mode: None,
            })
            .collect();
        let plan = Plan::new(steps, PlanOptions::default());
        let state = small.join("state");
        std::fs::create_dir_all(&state).unwrap();
        // Room for `JobStarted` (which carries the whole plan) and for only
        // part of the 6000 intent/completion records that follow.
        let started_record = serde_json::to_vec(&plan).unwrap().len() as u64;
        let ballast = fill_leaving(small, started_record + 300 * 1024);
        let before = world.capture();

        let job = Running::start(&state, JOB, JobKind::CreateDir, plan.clone()).unwrap();
        let report = job.finish();
        assert!(
            report
                .errors
                .iter()
                .any(|failure| failure.kind == ErrorKind::Space),
            "the job must report that its journal ran out of space: {:?}",
            report.errors.first()
        );

        let after = world.capture();
        let statuses = step_statuses(&plan, &read_journal(&state, JOB));
        check_interrupted(&before, &plan, &statuses, &after).unwrap_or_else(|why| {
            panic!("a step ran without a durable intent after the journal filled up:\n{why}")
        });
        let made = after.nodes.len() - before.nodes.len();
        assert!(
            made > 0 && made < 3000,
            "the fixture should let the job get part of the way, made {made} of 3000"
        );

        // The journal ends wherever the disk did. It must still be
        // readable -- one unreadable journal fails the whole startup scan.
        let reports = JournalReader::scan(&state).expect("a journal cut short by ENOSPC must scan");
        let interrupted = reports
            .into_iter()
            .find(|r| r.job_id == duet_ops::JobId(JOB))
            .expect("the job's journal");
        assert!(interrupted.needs_attention());

        std::fs::remove_file(&ballast).unwrap();
        let _ = std::process::Command::new("sync")
            .arg("-f")
            .arg(small)
            .status();
        let resumed = resume_in(&state, Backend::Local, &interrupted);
        let job = resumed.report.expect("there was work left");
        assert!(
            job.errors.is_empty() && job.skipped.is_empty(),
            "{:?}",
            job.errors.first()
        );
        check_finished(&before, &plan, &world.capture()).unwrap();
    }
}

/// Runs `case` against a fresh size-limited tmpfs.
fn run_case_on_tmpfs(case: &str) {
    let base = tempfile::Builder::new()
        .prefix("duet-data-safety-enospc-")
        .tempdir()
        .unwrap();
    let small = base.path().canonicalize().unwrap().join("small");
    std::fs::create_dir_all(&small).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=8m,mode=0777", "tmpfs"])
        .arg(&small)
        .output()
        .expect("run mount(8)");
    assert!(
        mounted.status.success(),
        "mounting an 8 MiB tmpfs inside the namespace failed: {}",
        String::from_utf8_lossy(&mounted.stderr)
    );
    run_case(case, &small);
}

pub fn run_case(case: &str, small: &Path) {
    match case {
        "copy_pauses_then_completes" => cases::copy_pauses_then_completes(small),
        "move_onto_full_filesystem_then_cancel" => {
            cases::move_onto_full_filesystem_then_cancel(small)
        }
        "journal_on_full_filesystem_touches_nothing" => {
            cases::journal_on_full_filesystem_touches_nothing(small)
        }
        "journal_filling_up_mid_job_stops_cleanly_and_resumes" => {
            cases::journal_filling_up_mid_job_stops_cleanly_and_resumes(small)
        }
        other => panic!("unknown ENOSPC case {other:?}"),
    }
}

/// The half that runs inside the namespace. A no-op in an ordinary run.
#[test]
fn namespace_entry() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    run_case_on_tmpfs(&case);
    println!("{DONE_MARKER} {case}");
}

/// Re-executes this binary in a private user + mount namespace and runs
/// one case there.
#[track_caller]
fn in_namespace(case: &str) {
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "enospc::namespace_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CASE_ENV, case)
        .env_remove(crate::harness::CHILD_ENV)
        .output()
        .unwrap_or_else(|e| {
            panic!("cannot run unshare(1) (util-linux), which the ENOSPC cases need: {e}")
        });
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() && stdout.contains(&format!("{DONE_MARKER} {case}")) {
        return;
    }
    panic!(
        "ENOSPC case {case:?} did not pass inside a user+mount namespace ({:?}).\n\
         If the output below says the namespace itself could not be created, unprivileged \
         user namespaces are disabled here; enable them (on Ubuntu 24.04: `sudo sysctl -w \
         kernel.apparmor_restrict_unprivileged_userns=0`) -- this suite does not skip.\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );
}

#[test]
fn copy_onto_a_full_filesystem_pauses_then_completes_when_space_is_freed() {
    in_namespace("copy_pauses_then_completes");
}

#[test]
fn cross_device_move_onto_a_full_filesystem_keeps_every_file_it_could_not_copy() {
    in_namespace("move_onto_full_filesystem_then_cancel");
}

#[test]
fn a_job_whose_journal_cannot_be_written_touches_nothing() {
    in_namespace("journal_on_full_filesystem_touches_nothing");
}

#[test]
fn a_journal_that_fills_up_mid_job_stops_the_job_cleanly_and_resumes_later() {
    in_namespace("journal_filling_up_mid_job_stops_cleanly_and_resumes");
}
