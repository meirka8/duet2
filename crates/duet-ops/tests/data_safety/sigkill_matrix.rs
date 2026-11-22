// SPDX-License-Identifier: MIT
//! `docs/crash-safety.md`, row by row.
//!
//! Every test below is named exactly as the table names it and kills a
//! real child process at exactly the interruption point its row describes
//! (see `harness`). Each then checks, in order:
//!
//! 1. the general invariant ([`inspect`]): the tree is in a state the
//!    journal's durable records allow, and no content is lost;
//! 2. what the row itself claims about the journal and the tree at that
//!    point (the closure each test passes);
//! 3. that **resume** turns the crash state into exactly the finished
//!    job, with nothing left over ([`finish_by_resume`]);
//! 4. in a second, identical crash, that **discard** leaves a legal state
//!    with no scratch files ([`finish_by_discard`]).
//!
//! Rows the table labels "mid-`<syscall>`" describe a state that cannot
//! be reached from user space (the kernel exposes no half-applied rename).
//! They exist so that claim is tested rather than assumed: those tests
//! kill the child at an arbitrary instant while it runs thousands of such
//! steps ([`storm_rounds`]) and require the invariant to hold wherever the
//! kill happened to land.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use duet_ops::{RecoveryReport, Step};
use duet_vfs::LocalFs;

use crate::harness::{
    Backend, Child, ChildEnd, JOB, Resumed, World, assert_nothing_left_to_recover, crash_at,
    discard, resume, vp,
};
use crate::model::{
    Node, StepStatus, Tree, check_finished, check_interrupted, check_no_content_lost, compare,
    read_journal, real,
};
use crate::scenarios::{self, CopyOpts, Scenario, StormKind};

/// A crashed job, captured.
pub struct Crashed<'a> {
    pub world: &'a World,
    pub scenario: &'a Scenario,
    pub before: Tree,
    pub after: Tree,
    pub statuses: Vec<StepStatus>,
    /// The product's own view of the journal; `None` when not even
    /// `JobStarted` was durable.
    pub report: Option<RecoveryReport>,
}

impl Crashed<'_> {
    pub fn path(&self, relative: &str) -> PathBuf {
        self.world.root.join(relative)
    }

    pub fn node(&self, relative: &str) -> Option<&Node> {
        self.after.get(&self.path(relative))
    }

    #[track_caller]
    pub fn assert_absent(&self, relative: &str) {
        assert!(
            self.node(relative).is_none(),
            "{relative} must not exist yet, found {:?}",
            self.node(relative)
        );
    }

    /// `relative` is byte-for-byte and attribute-for-attribute what it was
    /// before the job started.
    #[track_caller]
    pub fn assert_unchanged(&self, relative: &str) {
        let path = self.path(relative);
        assert_eq!(
            self.after.get(&path),
            self.before.get(&path),
            "{relative} must be exactly as it was before the job"
        );
        assert!(self.before.get(&path).is_some(), "{relative} never existed");
    }

    /// `relative` holds exactly the bytes `source` held before the job.
    #[track_caller]
    pub fn assert_content_of(&self, relative: &str, source: &str) {
        let content = |tree: &Tree, rel: &str| match tree.get(&self.path(rel)) {
            Some(Node::File { len, hash, .. }) => Some((*len, *hash)),
            _ => None,
        };
        let want = content(&self.before, source).expect("source existed before the job");
        assert_eq!(
            content(&self.after, relative),
            Some(want),
            "{relative} must hold exactly the content of {source}"
        );
    }

    #[track_caller]
    pub fn assert_status(&self, step: usize, want: &str) {
        let got = match &self.statuses[step] {
            StepStatus::Untouched => "untouched",
            StepStatus::InFlight { .. } => "in-flight",
            StepStatus::Succeeded => "succeeded",
            StepStatus::Skipped => "skipped",
            StepStatus::Failed { .. } => "failed",
        };
        assert_eq!(got, want, "journal state of step {step}");
    }

    pub fn partials(&self) -> Vec<PathBuf> {
        self.after.partials()
    }

    #[track_caller]
    pub fn assert_no_partials(&self) {
        assert!(
            self.partials().is_empty(),
            "no scratch file may exist at this point: {:?}",
            self.partials()
        );
    }

    #[track_caller]
    pub fn report(&self) -> &RecoveryReport {
        self.report
            .as_ref()
            .expect("the journal must hold at least JobStarted here")
    }
}

/// Captures the crash state and checks the general invariant against it.
#[track_caller]
pub fn inspect<'a>(world: &'a World, scenario: &'a Scenario, before: Tree) -> Crashed<'a> {
    let after = world.capture();
    let statuses = world.statuses(&scenario.plan, JOB);
    if let Err(why) = check_interrupted(&before, &scenario.plan, &statuses, &after) {
        panic!("the crash left an illegal state:\n{why}\njournal: {statuses:?}");
    }
    if let Err(why) = check_no_content_lost(&before, &after, &scenario.deletable) {
        panic!("{why}");
    }
    Crashed {
        world,
        scenario,
        before,
        after,
        statuses,
        report: world.scan(JOB),
    }
}

/// Resumes the crashed job the way the product does and requires the
/// result to be exactly the finished job: every destination complete,
/// every hardlink a hardlink, no scratch file, no journal left dangling,
/// and no error or skip invented along the way.
#[track_caller]
pub fn finish_by_resume(crashed: &Crashed<'_>, backend: Backend) -> Option<Resumed> {
    let world = crashed.world;
    let plan = &crashed.scenario.plan;
    let resumed = match &crashed.report {
        Some(report) if report.needs_attention() => Some(resume(world, backend, report)),
        _ => None,
    };
    if let Some(Resumed {
        report: Some(report),
        ..
    }) = &resumed
    {
        assert!(
            report.errors.is_empty() && report.skipped.is_empty(),
            "a resume must finish the job cleanly, not report phantom failures: {report:?}"
        );
    }
    let finished = world.capture();
    let journal_has_the_job = !read_journal(&world.state, JOB).is_empty();
    let verdict = if journal_has_the_job {
        check_finished(&crashed.before, plan, &finished)
    } else {
        // Killed before `JobStarted` was durable: the job never existed,
        // and nothing about the tree may say otherwise.
        compare(&crashed.before, &finished)
    };
    if let Err(why) = verdict {
        panic!("after resume the tree is not the finished job:\n{why}");
    }
    if journal_has_the_job {
        for step in &plan.steps {
            if let Step::Link { source, dest, .. } = step {
                assert_eq!(
                    finished.inodes.get(&real(source)),
                    finished.inodes.get(&real(dest)),
                    "{dest} must be a hardlink of {source}, not a copy"
                );
            }
        }
    }
    assert_nothing_left_to_recover(world);
    resumed
}

/// Discards the crashed job the way the product does: afterwards no
/// scratch file exists, the state is still a legal interrupted state, and
/// nothing is offered for recovery again.
#[track_caller]
pub fn finish_by_discard(crashed: &Crashed<'_>, backend: Backend) {
    let world = crashed.world;
    if let Some(report) = &crashed.report
        && report.needs_attention()
    {
        discard(world, backend, report);
    }
    let finished = world.capture();
    assert!(
        finished.partials().is_empty(),
        "discard left scratch files behind: {:?}",
        finished.partials()
    );
    if let Err(why) = check_interrupted(
        &crashed.before,
        &crashed.scenario.plan,
        &crashed.statuses,
        &finished,
    ) {
        panic!("after discard the tree is in an illegal state:\n{why}");
    }
    if let Err(why) = check_no_content_lost(&crashed.before, &finished, &crashed.scenario.deletable)
    {
        panic!("{why}");
    }
    assert_nothing_left_to_recover(world);
}

/// One table row: crash at `failpoint`, check the row's own claims, then
/// recover -- once by resume, once (from a fresh identical crash) by
/// discard.
#[track_caller]
fn row(
    setup: impl Fn(&World) -> Scenario,
    backend: Backend,
    failpoint: &str,
    at_crash: impl Fn(&Crashed<'_>),
) {
    row_then(setup, backend, failpoint, at_crash, |_| {});
}

#[track_caller]
fn row_then(
    setup: impl Fn(&World) -> Scenario,
    backend: Backend,
    failpoint: &str,
    at_crash: impl Fn(&Crashed<'_>),
    after_resume: impl Fn(&Option<Resumed>),
) {
    for resume_it in [true, false] {
        let world = World::new();
        let scenario = setup(&world);
        let before = world.capture();
        crash_at(&world, &scenario.spec(&world, backend), failpoint);
        let crashed = inspect(&world, &scenario, before);
        at_crash(&crashed);
        if resume_it {
            after_resume(&finish_by_resume(&crashed, backend));
        } else {
            finish_by_discard(&crashed, backend);
        }
    }
}

/// The row's resume must have concluded "step 0 already happened" and run
/// nothing -- the "recovery infers success" half of the after-effect rows.
#[track_caller]
fn inferred_done(resumed: &Option<Resumed>) {
    let resumed = resumed.as_ref().expect("a dangling intent needs recovery");
    assert_eq!(
        resumed.decision.already_applied,
        vec![0],
        "recovery must recognise the step as already done"
    );
    assert!(
        resumed.report.is_none(),
        "nothing was left to run, so no job may have been started"
    );
}

/// Whether the filesystem the world lives on really reflinks. Probed in
/// the world's *state* directory -- same filesystem, but outside the
/// snapshotted tree, because the probe creates and removes files.
fn reflink_supported(world: &World) -> bool {
    LocalFs
        .probe_fs_properties(&vp(&world.state))
        .expect("probe the test filesystem")
        .reflink_supported
}

const STORM_STEPS: usize = 3000;
const STORM_ROUNDS: usize = 5;

/// Kills a child at an arbitrary instant while it runs [`STORM_STEPS`]
/// steps of one kind, [`STORM_ROUNDS`] times at different depths into the
/// job, and requires the invariant to hold every time -- whichever
/// instruction, or syscall, the kill interrupted.
///
/// What makes this deterministic despite the racing kill: the assertion
/// never depends on *where* the kill landed, only on the state it left.
/// The child lingers after finishing, so there is always a process to
/// kill; and at least one round must have caught the job mid-plan, or the
/// test has not tested anything and fails.
pub fn storm_rounds(
    kind: impl Fn() -> StormKind,
    backend: Backend,
    at_crash: impl Fn(&Crashed<'_>),
) {
    let mut caught_mid_plan = 0;
    for round in 0..STORM_ROUNDS {
        let world = World::new();
        let scenario = scenarios::storm(&world, kind(), STORM_STEPS);
        let before = world.capture();
        let mut spec = scenario.spec(&world, backend);
        spec.linger = true;
        let mut child = Child::spawn(&world, &spec, None);
        // Kill once the journal shows the job this deep into the plan.
        let records_wanted = 2 + STORM_STEPS * (round + 1) / (STORM_ROUNDS + 2);
        let journal = world.state.join("jobs").join(format!("{JOB}.journal"));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let records = std::fs::read(&journal)
                .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count())
                .unwrap_or(0);
            if records >= records_wanted {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the storm child never got {records_wanted} records into its journal"
            );
            std::thread::yield_now();
        }
        child.kill();
        assert!(matches!(child.wait(), ChildEnd::Killed));

        let crashed = inspect(&world, &scenario, before);
        if crashed
            .statuses
            .iter()
            .any(|status| *status != StepStatus::Succeeded)
        {
            caught_mid_plan += 1;
        }
        at_crash(&crashed);
        finish_by_resume(&crashed, backend);
    }
    assert!(
        caught_mid_plan >= 1,
        "no kill landed while the job was still running; the storm proved nothing"
    );
}

// ---------------------------------------------------------------- CopyFile

fn copy_rows_before_intent(opts: fn() -> CopyOpts) {
    row(
        |w| scenarios::copy_file(w, opts()),
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            c.assert_absent("dst/a.bin");
            c.assert_unchanged("src/a.bin");
            c.assert_no_partials();
        },
    );
}

fn copy_rows_after_intent(opts: fn() -> CopyOpts) {
    for replacing in [false, true] {
        row(
            |w| {
                let opts = opts();
                scenarios::copy_file(w, if replacing { opts.replacing() } else { opts })
            },
            Backend::Local,
            "step.before_effect@0",
            |c| {
                c.assert_status(0, "in-flight");
                if replacing {
                    c.assert_unchanged("dst/a.bin");
                } else {
                    c.assert_absent("dst/a.bin");
                }
                c.assert_unchanged("src/a.bin");
                c.assert_no_partials();
                assert_eq!(c.report().incomplete_steps, vec![0]);
            },
        );
    }
}

fn copy_rows_before_publish(opts: fn() -> CopyOpts) {
    for backend in [Backend::Local, Backend::Buffered] {
        for replacing in [false, true] {
            row(
                |w| {
                    let opts = opts();
                    scenarios::copy_file(w, if replacing { opts.replacing() } else { opts })
                },
                backend,
                "copy.before_publish@0",
                |c| {
                    c.assert_status(0, "in-flight");
                    if replacing {
                        c.assert_unchanged("dst/a.bin");
                    } else {
                        c.assert_absent("dst/a.bin");
                    }
                    c.assert_unchanged("src/a.bin");
                    // Exactly one scratch file: the one the journal names,
                    // holding the complete content.
                    let report = c.report();
                    assert_eq!(report.orphaned_partials.len(), 1);
                    let (step, name) = &report.orphaned_partials[0];
                    assert_eq!(*step, 0);
                    let partial = c.path("dst").join(name);
                    assert_eq!(c.partials(), vec![partial.clone()]);
                    assert_eq!(
                        std::fs::read(&partial).unwrap(),
                        std::fs::read(c.path("src/a.bin")).unwrap(),
                        "the partial must already hold the whole file"
                    );
                },
            );
        }
    }
}

fn copy_rows_after_publish(opts: fn() -> CopyOpts) {
    for replacing in [false, true] {
        row_then(
            |w| {
                let opts = opts();
                scenarios::copy_file(w, if replacing { opts.replacing() } else { opts })
            },
            Backend::Local,
            "step.after_effect@0",
            |c| {
                c.assert_status(0, "in-flight");
                c.assert_content_of("dst/a.bin", "src/a.bin");
                c.assert_unchanged("src/a.bin");
                c.assert_no_partials();
            },
            inferred_done,
        );
    }
}

fn copy_rows_after_completion(opts: fn() -> CopyOpts) {
    row(
        |w| scenarios::copy_file(w, opts()),
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            c.assert_content_of("dst/a.bin", "src/a.bin");
            c.assert_unchanged("src/a.bin");
            c.assert_no_partials();
            assert!(
                !c.report().needs_attention(),
                "a completed step leaves nothing to recover"
            );
        },
    );
}

#[test]
fn copyfile_sigkill_before_intent_leaves_no_trace() {
    copy_rows_before_intent(CopyOpts::small);
}

#[test]
fn copyfile_sigkill_after_intent_before_partial_created_dest_untouched() {
    copy_rows_after_intent(CopyOpts::small);
}

#[test]
fn copyfile_sigkill_mid_write_leaves_source_intact_and_partial_marked() {
    // The buffered path: one 1 MiB buffer of three is on disk.
    for replacing in [false, true] {
        row(
            |w| {
                let opts = CopyOpts::multi_chunk();
                scenarios::copy_file(w, if replacing { opts.replacing() } else { opts })
            },
            Backend::Buffered,
            "copy.mid_write@0",
            |c| {
                c.assert_status(0, "in-flight");
                if replacing {
                    c.assert_unchanged("dst/a.bin");
                } else {
                    c.assert_absent("dst/a.bin");
                }
                c.assert_unchanged("src/a.bin");
                let partials = c.partials();
                assert_eq!(
                    partials.len(),
                    1,
                    "exactly one marked partial: {partials:?}"
                );
                let len = std::fs::metadata(&partials[0]).unwrap().len();
                let full = std::fs::metadata(c.path("src/a.bin")).unwrap().len();
                assert!(
                    len > 0 && len < full,
                    "the partial must be a non-empty, incomplete prefix ({len} of {full} bytes)"
                );
            },
        );
    }
    // The accelerated path has a mid-write only where the kernel copies in
    // more than one `copy_file_range` call: a file larger than one 64 MiB
    // chunk, on a filesystem that cannot reflink it in one step. (Where it
    // can, there is no such state -- the Reflink table's row 3.)
    let probe = World::new();
    if !reflink_supported(&probe) {
        row(
            |w| {
                scenarios::copy_file(
                    w,
                    CopyOpts {
                        size: 70 * 1024 * 1024,
                        ..CopyOpts::small()
                    },
                )
            },
            Backend::Local,
            "copy.mid_write@0",
            |c| {
                c.assert_status(0, "in-flight");
                c.assert_absent("dst/a.bin");
                c.assert_unchanged("src/a.bin");
                let report = c.report();
                assert_eq!(report.orphaned_partials.len(), 1);
                let partial = c.path("dst").join(&report.orphaned_partials[0].1);
                assert_eq!(c.partials(), vec![partial.clone()]);
                let len = std::fs::metadata(&partial).unwrap().len();
                assert!(len > 0 && len < 70 * 1024 * 1024);
            },
        );
    }
}

#[test]
fn copyfile_sigkill_after_write_before_rename_partial_is_complete_and_resumable() {
    copy_rows_before_publish(CopyOpts::small);
}

#[test]
fn copyfile_sigkill_mid_rename_dest_is_old_or_new_never_mixed() {
    storm_rounds(
        || StormKind::Copy { reflink: false },
        Backend::Local,
        |_| {},
    );
}

#[test]
fn copyfile_sigkill_after_rename_before_completion_record_dest_correct_journal_catches_up() {
    copy_rows_after_publish(CopyOpts::small);
}

#[test]
fn copyfile_sigkill_after_completion_record_step_is_stable() {
    copy_rows_after_completion(CopyOpts::small);
}

// ----------------------------------------------------------------- Reflink

fn reflink_opts() -> CopyOpts {
    CopyOpts::small().reflink()
}

#[test]
fn reflink_sigkill_before_intent_leaves_no_trace() {
    copy_rows_before_intent(reflink_opts);
}

#[test]
fn reflink_sigkill_after_intent_before_partial_created_dest_untouched() {
    copy_rows_after_intent(reflink_opts);
}

#[test]
fn reflink_sigkill_mid_ficlone_partial_is_absent_or_complete_never_truncated() {
    storm_rounds(
        || StormKind::Copy { reflink: true },
        Backend::Local,
        |c| {
            // `FICLONE` has no streaming phase: where the filesystem
            // really clones, a staged file is empty (clone not applied)
            // or whole (applied), never a prefix. Elsewhere the step
            // falls down the ladder and the general prefix rule
            // (`inspect`) is what applies.
            if !reflink_supported(c.world) {
                return;
            }
            for partial in c.partials() {
                let len = std::fs::metadata(&partial).unwrap().len();
                let name = partial.file_name().unwrap().to_str().unwrap();
                let source = c.world.root.join("src").join(&name[name.len() - 6..]);
                let full = std::fs::metadata(&source).unwrap().len();
                assert!(
                    len == 0 || len == full,
                    "{}: a reflinked partial is empty or whole, found {len} of {full}",
                    partial.display()
                );
            }
        },
    );
}

#[test]
fn reflink_sigkill_after_ficlone_before_rename_partial_is_complete_and_resumable() {
    copy_rows_before_publish(reflink_opts);
}

#[test]
fn reflink_sigkill_mid_rename_dest_is_old_or_new_never_mixed() {
    storm_rounds(|| StormKind::Copy { reflink: true }, Backend::Local, |_| {});
}

#[test]
fn reflink_sigkill_after_rename_before_completion_record_dest_correct_journal_catches_up() {
    copy_rows_after_publish(reflink_opts);
}

#[test]
fn reflink_sigkill_after_completion_record_step_is_stable_and_still_reflinked() {
    copy_rows_after_completion(reflink_opts);
    // Where the filesystem reflinks, the finished copy must really share
    // its source's extents: a clone that silently degraded to a byte copy
    // is a defect even though the content is right.
    let world = World::new();
    if reflink_supported(&world) {
        let scenario = scenarios::copy_file(&world, reflink_opts());
        crate::harness::run_to_end(&world, &scenario.spec(&world, Backend::Local));
        let physical = |path: &Path| -> Option<String> {
            let out = std::process::Command::new("filefrag")
                .arg("-v")
                .arg(path)
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find(|line| line.trim_start().starts_with("0:"))
                .and_then(|line| line.split_whitespace().nth(3).map(str::to_string))
        };
        let source = physical(&world.root.join("src/a.bin"));
        let dest = physical(&world.root.join("dst/a.bin"));
        assert!(
            source.is_some() && source == dest,
            "a reflinked copy must share its source's first extent (filefrag: {source:?} vs \
             {dest:?})"
        );
    }
}

// --------------------------------------------------------------- CreateDir

#[test]
fn createdir_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::create_dir,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            c.assert_absent("dst/new");
        },
    );
}

#[test]
fn createdir_sigkill_after_intent_before_mkdir_dest_absent() {
    row(
        scenarios::create_dir,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_absent("dst/new");
        },
    );
}

#[test]
fn createdir_sigkill_mid_mkdir_dest_absent_or_fully_present() {
    storm_rounds(|| StormKind::CreateDir, Backend::Local, |_| {});
}

#[test]
fn createdir_sigkill_after_mkdir_before_completion_record_recovery_treats_as_done() {
    row(
        scenarios::create_dir,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert!(matches!(c.node("dst/new"), Some(Node::Dir { .. })));
            // `finish_by_resume` then requires the redo to merge into the
            // existing directory with no error and no skip.
        },
    );
}

#[test]
fn createdir_sigkill_after_completion_record_step_is_stable() {
    row(
        scenarios::create_dir,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert!(matches!(c.node("dst/new"), Some(Node::Dir { .. })));
            assert!(!c.report().needs_attention());
        },
    );
}

// ------------------------------------------------------------------ Rename

#[test]
fn rename_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::rename,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            c.assert_unchanged("src/a.bin");
            c.assert_absent("dst/a.bin");
        },
    );
}

#[test]
fn rename_sigkill_after_intent_before_renameat2_source_untouched() {
    row(
        scenarios::rename,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_unchanged("src/a.bin");
            c.assert_absent("dst/a.bin");
        },
    );
}

#[test]
fn rename_sigkill_mid_renameat2_entry_at_exactly_one_of_source_or_dest() {
    // `inspect` enforces "exactly one of the two names" for every rename
    // caught in flight.
    storm_rounds(|| StormKind::Rename, Backend::Local, |_| {});
}

#[test]
fn rename_sigkill_after_renameat2_before_completion_record_recovery_infers_success() {
    row_then(
        scenarios::rename,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_absent("src/a.bin");
            c.assert_content_of("dst/a.bin", "src/a.bin");
        },
        inferred_done,
    );
}

#[test]
fn rename_sigkill_after_completion_record_step_is_stable() {
    row(
        scenarios::rename,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            c.assert_absent("src/a.bin");
            c.assert_content_of("dst/a.bin", "src/a.bin");
            assert!(!c.report().needs_attention());
        },
    );
}

// -------------------------------------------------------------------- Link

#[track_caller]
fn assert_linked(c: &Crashed<'_>) {
    assert_eq!(
        c.after.inodes.get(&c.path("src/a.bin")),
        c.after.inodes.get(&c.path("dst/a.link")),
        "the link must share the source's inode"
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(c.path("src/a.bin")).unwrap().nlink(), 2);
}

#[track_caller]
fn assert_not_linked(c: &Crashed<'_>) {
    use std::os::unix::fs::MetadataExt;
    c.assert_absent("dst/a.link");
    c.assert_unchanged("src/a.bin");
    assert_eq!(std::fs::metadata(c.path("src/a.bin")).unwrap().nlink(), 1);
}

#[test]
fn link_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::link,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            assert_not_linked(c);
        },
    );
}

#[test]
fn link_sigkill_after_intent_before_linkat_dest_absent() {
    row(
        scenarios::link,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_not_linked(c);
        },
    );
}

#[test]
fn link_sigkill_mid_linkat_dest_absent_or_fully_linked() {
    storm_rounds(
        || StormKind::Link,
        Backend::Local,
        |c| {
            // Every destination that exists is a true second name for its
            // source -- never a dangling or half-made entry.
            for step in &c.scenario.plan.steps {
                let Step::Link { source, dest, .. } = step else {
                    unreachable!()
                };
                if let Some(inode) = c.after.inodes.get(&real(dest)) {
                    assert_eq!(Some(inode), c.after.inodes.get(&real(source)));
                }
            }
        },
    );
}

#[test]
fn link_sigkill_after_linkat_before_completion_record_recovery_treats_as_done() {
    row_then(
        scenarios::link,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_linked(c);
        },
        inferred_done,
    );
}

#[test]
fn link_sigkill_after_completion_record_step_is_stable() {
    row(
        scenarios::link,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert_linked(c);
            assert!(!c.report().needs_attention());
        },
    );
}

// ----------------------------------------------------------------- Symlink

#[track_caller]
fn assert_symlinked(c: &Crashed<'_>) {
    let target = c.path("src/a.bin").to_str().unwrap().to_string();
    assert_eq!(c.node("dst/a.sym"), Some(&Node::Symlink { target }));
}

#[test]
fn symlink_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::symlink,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            c.assert_absent("dst/a.sym");
        },
    );
}

#[test]
fn symlink_sigkill_after_intent_before_symlinkat_link_absent() {
    row(
        scenarios::symlink,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_absent("dst/a.sym");
        },
    );
}

#[test]
fn symlink_sigkill_after_symlinkat_before_completion_record_recovery_treats_as_done() {
    row_then(
        scenarios::symlink,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_symlinked(c);
        },
        inferred_done,
    );
}

#[test]
fn symlink_sigkill_after_completion_record_step_is_stable() {
    row(
        scenarios::symlink,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert_symlinked(c);
            assert!(!c.report().needs_attention());
        },
    );
}

// ----------------------------------------------------------------- SetMeta

#[track_caller]
fn assert_meta(c: &Crashed<'_>, mode: u32, mtime_secs: i64) {
    match c.node("dst/a.bin") {
        Some(Node::File {
            mode: got_mode,
            mtime,
            ..
        }) => {
            assert_eq!(*got_mode, Some(mode), "mode");
            assert_eq!(mtime.map(|(secs, _)| secs), Some(mtime_secs), "mtime");
        }
        other => panic!("dst/a.bin must be a file, found {other:?}"),
    }
    // Whatever happened to the metadata, the bytes are the original ones.
    c.assert_content_of("dst/a.bin", "dst/a.bin");
}

const SET_META_OLD_MTIME: i64 = 1_600_000_000;

#[test]
fn setmeta_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::set_meta,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            c.assert_unchanged("dst/a.bin");
        },
    );
}

#[test]
fn setmeta_sigkill_after_intent_before_any_field_applied_target_untouched() {
    row(
        scenarios::set_meta,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_unchanged("dst/a.bin");
        },
    );
}

#[test]
fn setmeta_sigkill_mid_apply_content_untouched_and_reapply_is_idempotent() {
    // Mode is applied, timestamps are not: genuinely neither old nor new.
    row(
        scenarios::set_meta,
        Backend::Local,
        "vfs.set_meta.phase_done@1",
        |c| {
            c.assert_status(0, "in-flight");
            assert_meta(c, scenarios::SET_META_NEW_MODE, SET_META_OLD_MTIME);
        },
    );
}

#[test]
fn setmeta_sigkill_after_all_fields_applied_before_completion_record_reapply_is_harmless() {
    row(
        scenarios::set_meta,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_meta(
                c,
                scenarios::SET_META_NEW_MODE,
                scenarios::SET_META_NEW_MTIME,
            );
        },
    );
}

#[test]
fn setmeta_sigkill_after_completion_record_step_is_stable() {
    row(
        scenarios::set_meta,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert_meta(
                c,
                scenarios::SET_META_NEW_MODE,
                scenarios::SET_META_NEW_MTIME,
            );
            assert!(!c.report().needs_attention());
        },
    );
}

// ------------------------------------------------------------------ Remove

#[test]
fn remove_sigkill_before_intent_leaves_no_trace() {
    for setup in [
        scenarios::remove_file as fn(&World) -> Scenario,
        scenarios::remove_tree,
    ] {
        row(
            setup,
            Backend::Local,
            "journal.intent.before_write@0",
            |c| {
                c.assert_status(0, "untouched");
                // Nothing at all has changed, anywhere.
                compare(&c.before, &c.after).unwrap();
            },
        );
    }
}

#[test]
fn remove_sigkill_after_intent_before_removal_target_untouched_undo_data_present() {
    for setup in [
        scenarios::remove_file as fn(&World) -> Scenario,
        scenarios::remove_tree,
    ] {
        row(setup, Backend::Local, "step.before_effect@0", |c| {
            c.assert_status(0, "in-flight");
            compare(&c.before, &c.after).unwrap();
            // The journal already says, durably, what was about to be
            // deleted -- before a single byte was.
            let report = c.report();
            assert_eq!(report.incomplete_steps, vec![0]);
            assert!(matches!(report.plan.steps[0], Step::Remove { .. }));
        });
    }
}

#[test]
fn remove_sigkill_mid_removal_file_or_emptydir_target_absent_or_fully_present() {
    storm_rounds(|| StormKind::RemoveFile, Backend::Local, |_| {});
    storm_rounds(|| StormKind::RemoveEmptyDir, Backend::Local, |_| {});
}

#[test]
fn remove_sigkill_mid_recursive_removal_partial_is_bounded_and_resumable_no_escape() {
    // Killed after 1, 4 and 8 of the subtree's entries are gone.
    for nth in [1, 4, 8] {
        row(
            scenarios::remove_tree,
            Backend::Local,
            &format!("vfs.remove_recursive.unlinked#{nth}"),
            |c| {
                c.assert_status(0, "in-flight");
                // Bounded: exactly `nth` entries of the subtree are gone,
                // each one a member of it...
                let tree = c.path("src/tree");
                let was = c
                    .before
                    .nodes
                    .keys()
                    .filter(|p| p.starts_with(&tree))
                    .count();
                let is = c
                    .after
                    .nodes
                    .keys()
                    .filter(|p| p.starts_with(&tree))
                    .count();
                assert_eq!(was - is, nth, "entries removed so far");
                // ...and nothing outside it was touched: not the sibling,
                // and not what the symlink inside the tree points at.
                c.assert_unchanged("src/sibling.bin");
                c.assert_unchanged("outside/precious.bin");
                c.assert_unchanged("outside");
            },
        );
    }
}

#[test]
fn remove_sigkill_after_removal_before_completion_record_recovery_infers_success() {
    for setup in [
        scenarios::remove_file as fn(&World) -> Scenario,
        scenarios::remove_empty_dir,
        scenarios::remove_tree,
    ] {
        row_then(
            setup,
            Backend::Local,
            "step.after_effect@0",
            |c| {
                c.assert_status(0, "in-flight");
                let target = real(match &c.scenario.plan.steps[0] {
                    Step::Remove { target, .. } => target,
                    _ => unreachable!(),
                });
                assert!(c.after.get(&target).is_none(), "the target is gone");
            },
            inferred_done,
        );
    }
}

#[test]
fn remove_sigkill_after_completion_record_step_is_stable() {
    for setup in [
        scenarios::remove_file as fn(&World) -> Scenario,
        scenarios::remove_tree,
    ] {
        row(
            setup,
            Backend::Local,
            "journal.completion.after_fsync@0",
            |c| {
                c.assert_status(0, "succeeded");
                assert!(!c.report().needs_attention());
            },
        );
    }
}

// ------------------------------------------------------------------ Verify

fn verify_matching(world: &World) -> Scenario {
    scenarios::verify(world, true)
}

#[track_caller]
fn assert_verify_touched_nothing(c: &Crashed<'_>) {
    compare(&c.before, &c.after).expect("a verify writes nothing, wherever it is killed");
}

#[test]
fn verify_sigkill_before_intent_leaves_no_trace() {
    row(
        verify_matching,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            assert_verify_touched_nothing(c);
        },
    );
}

#[test]
fn verify_sigkill_after_intent_before_hash_no_side_effect() {
    row(
        verify_matching,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_verify_touched_nothing(c);
        },
    );
}

#[test]
fn verify_sigkill_mid_hash_computation_no_side_effect_reruns_cleanly() {
    row(verify_matching, Backend::Local, "verify.mid_hash", |c| {
        c.assert_status(0, "in-flight");
        assert_verify_touched_nothing(c);
    });
}

#[test]
fn verify_sigkill_after_result_computed_before_completion_record_reruns_conservatively() {
    row_then(
        verify_matching,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_verify_touched_nothing(c);
        },
        |resumed| {
            // The lost result is not trusted: the comparison runs again.
            let resumed = resumed.as_ref().unwrap();
            assert!(resumed.decision.already_applied.is_empty());
            assert_eq!(resumed.decision.redo, vec![0]);
            assert!(resumed.report.is_some());
        },
    );
}

#[test]
fn verify_sigkill_after_completion_record_step_is_stable_and_failed_verify_blocks_source_unlink() {
    row(
        verify_matching,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert_verify_touched_nothing(c);
            assert!(!c.report().needs_attention());
        },
    );

    // The other durable outcome: the verify *failed*, and the process died
    // with the source's `Remove` -- gated on that verify -- still ahead.
    // Three places the kill can land; in each the source must survive the
    // crash *and* the resume, and the resume must report the mismatch
    // rather than quietly finish the move.
    for failpoint in [
        // the failed completion is durable, the Remove not yet journaled
        "journal.completion.after_fsync@0",
        // the Remove's intent is durable too (it would be skipped next)
        "journal.intent.after_fsync@1",
        // its skip is decided but not yet journaled
        "step.after_effect@1",
    ] {
        let world = World::new();
        let scenario = scenarios::verify(&world, false);
        let before = world.capture();
        crash_at(&world, &scenario.spec(&world, Backend::Local), failpoint);
        let crashed = inspect(&world, &scenario, before);
        crashed.assert_status(0, "failed");
        crashed.assert_unchanged("src/a.bin");
        crashed.assert_unchanged("dst/a.bin");

        let report = crashed.report();
        assert!(
            report.needs_attention(),
            "{failpoint}: the move is unfinished"
        );
        let resumed = resume(&world, Backend::Local, report);
        let job = resumed.report.expect("the remainder must be re-run");
        assert_eq!(
            job.errors.len(),
            1,
            "{failpoint}: the resume must surface the failed verify: {job:?}"
        );
        assert_eq!(
            job.skipped.len(),
            1,
            "{failpoint}: the gated Remove must be skipped, not run: {job:?}"
        );
        let finished = world.capture();
        compare(&crashed.before, &finished).unwrap_or_else(|why| {
            panic!("{failpoint}: a source whose copy failed verification was touched:\n{why}")
        });
    }
}

// ---------------------------------------------------------- WriteTrashInfo

#[track_caller]
fn assert_not_trashed(c: &Crashed<'_>) {
    c.assert_unchanged("src/a.bin");
    c.assert_absent("data/Trash/files/a.bin");
}

#[test]
fn writetrashinfo_sigkill_before_intent_leaves_no_trace() {
    row(
        scenarios::trash_one,
        Backend::Local,
        "journal.intent.before_write@0",
        |c| {
            c.assert_status(0, "untouched");
            assert_not_trashed(c);
            c.assert_absent("data/Trash/info/a.bin.trashinfo");
            c.assert_no_partials();
        },
    );
}

#[test]
fn writetrashinfo_sigkill_after_intent_before_write_sidecar_absent() {
    row(
        scenarios::trash_one,
        Backend::Local,
        "step.before_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            assert_not_trashed(c);
            c.assert_absent("data/Trash/info/a.bin.trashinfo");
            c.assert_no_partials();
        },
    );
}

#[test]
fn writetrashinfo_sigkill_mid_write_sidecar_absent_and_staging_file_marked() {
    for failpoint in ["trashinfo.before_commit", "vfs.write.before_publish"] {
        row(scenarios::trash_one, Backend::Local, failpoint, |c| {
            c.assert_status(0, "in-flight");
            assert_not_trashed(c);
            c.assert_absent("data/Trash/info/a.bin.trashinfo");
            // The backend's staging sibling: marked, and nothing a trash
            // listing would mistake for a sidecar.
            let partials = c.partials();
            assert_eq!(partials.len(), 1, "{partials:?}");
            assert_eq!(
                partials[0].parent(),
                Some(c.path("data/Trash/info").as_path())
            );
        });
    }
}

#[test]
fn writetrashinfo_sigkill_after_write_before_completion_record_recovery_completes_the_trash() {
    row_then(
        scenarios::trash_one,
        Backend::Local,
        "step.after_effect@0",
        |c| {
            c.assert_status(0, "in-flight");
            c.assert_status(1, "untouched");
            assert_not_trashed(c);
            assert!(c.node("data/Trash/info/a.bin.trashinfo").is_some());
        },
        |resumed| {
            // The sidecar is recognised as written (a redo would collide
            // with it and block the move gated on it); only the move runs.
            let resumed = resumed.as_ref().unwrap();
            assert_eq!(resumed.decision.already_applied, vec![0]);
            assert_eq!(resumed.decision.redo, vec![1]);
        },
    );
}

#[test]
fn writetrashinfo_sigkill_after_completion_record_content_not_yet_moved_is_resumable() {
    row(
        scenarios::trash_one,
        Backend::Local,
        "journal.completion.after_fsync@0",
        |c| {
            c.assert_status(0, "succeeded");
            assert_not_trashed(c);
            // The sidecar-before-content ordering: an orphaned sidecar is
            // harmless, an orphaned trashed file with no sidecar is not.
            assert!(c.node("data/Trash/info/a.bin.trashinfo").is_some());
            assert!(c.report().needs_attention(), "the move is still owed");
        },
    );
}
