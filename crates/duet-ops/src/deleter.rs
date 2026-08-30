// SPDX-License-Identifier: MIT
//! [`plan_delete`] — T-5.1.8's delete planner: `JobKind::Delete`'s two
//! modes ([`DeleteMode::Permanent`]/[`DeleteMode::Trash`]) as a
//! materialised [`Plan`].
//!
//! # Permanent delete needs no walk of its own
//!
//! Unlike [`crate::planner::plan_copy`]/[`crate::mover::plan_move`], this
//! module never descends into a target directory itself: one
//! [`Step::Remove`] with [`RemoveMode::Recursive`] per top-level directory
//! target is the *whole* plan for it, because `RemoveKind::Recursive`
//! already walks and removes the subtree safely at the `duet-vfs` layer
//! (T-3.1.3 — see `duet_vfs::local::traverse`'s own module doc comment:
//! every subdirectory is opened `O_NOFOLLOW` off an already-open parent fd,
//! never by re-resolving a path string, so a concurrent rename or a
//! symlink substituted mid-walk can neither escape the subtree nor get
//! followed). This is exactly what T-5.1.8's own AC asks for — "recursive
//! delete never follows a symlink out of the tree" — already true by
//! construction one layer down; this module's job is to *plan* the right
//! `Step`, not to re-implement tree-walking safety that already exists.
//!
//! A target that no longer exists by the time it's planned (a concurrent
//! delete elsewhere, e.g.) is silently skipped rather than erroring —
//! "already gone" is success, not failure, matching `remove_recursive`'s
//! own `NOENT`-is-not-an-error convention one layer down.
//!
//! # Trash (T-5.3.1): the full freedesktop spec, not `plan_move` in disguise
//!
//! Earlier (T-5.1.8/T-5.2.6), "trash" here meant nothing more than
//! `crate::mover::plan_move` into a caller-supplied directory — a
//! deliberate placeholder, disclosed as such in this module's own prior
//! doc comment. T-5.3.1 replaces that with design.md §9.10/FR-CFG-07's
//! real implementation: `duet_platform::trash::resolve_trash_destination`
//! (the pure decision layer — topdir discovery, the two-method per-mount
//! `$topdir/.Trash{,-$uid}` resolution, `.trashinfo` formatting) is called
//! once per target, and this module turns its answer into two steps:
//! [`Step::WriteTrashInfo`] (the `.trashinfo` sidecar, written first, as a
//! barrier) then a dependency-gated [`Step::Rename`] (the actual content
//! move). See `duet_platform::trash`'s own module doc comment for why the
//! sidecar goes first (crash safety: an orphaned `.trashinfo` pointing at
//! still-there content is recoverable; trashed content with no metadata at
//! all is not) and for the plan-time-not-execution-time name resolution
//! that makes the two steps' names guaranteed to match.
//!
//! Every resolved trash destination is on the *same filesystem* as its
//! target, by construction (home trash only applies when the target
//! already shares `$XDG_DATA_HOME`'s device; a target on another
//! filesystem gets a trash rooted on *that* filesystem instead) — so the
//! content move is always a same-device [`Step::Rename`], never a
//! cross-device copy+verify+remove sequence the way an ordinary
//! `plan_move` across mounts needs. This is exactly what T-5.3.1's own AC
//! ("trashing on a second mount works") is asking for, achieved by
//! construction rather than by falling back to `plan_move`'s cross-device
//! machinery.
//!
//! [`DeleteMode::Trash`] carries only `data_home` (`duet_config::paths::
//! xdg_data_home()`'s result) — not a pre-resolved trash directory, and
//! not per-target routing, both of which are now this module's own job via
//! `duet_platform`. Environment/`$HOME` resolution itself stays the
//! caller's concern (`duet_ui::delete_dialog`), matching this crate's
//! existing "backend-agnostic, environment-agnostic" boundary with
//! `duet-config` everywhere else.

use std::path::Path;

use duet_types::{ErrorKind, UnixPathBuf, VPath};
use duet_vfs::FileSystem;

use crate::conflict::ConflictPolicy;
use crate::plan::{Plan, PlanOptions};
use crate::planner::{CancelToken, PlannerError};
use crate::step::{RemoveMode, Step};

/// Which of `JobKind::Delete`'s two behaviours (`permanent: bool`) a
/// [`plan_delete`] call should produce a [`Plan`] for.
pub enum DeleteMode {
    /// Content is actually removed. Bypasses trash (Shift+Del in TC/FR-OPS
    /// terms).
    Permanent,
    /// Content is trashed per the freedesktop spec — see the module doc
    /// comment. `data_home` is `duet_config::paths::xdg_data_home()`'s
    /// result; each target's actual trash destination (home trash, or the
    /// correct per-mount trash for a target elsewhere) is resolved
    /// individually inside [`plan_delete`], since different targets in one
    /// job may live on different filesystems.
    Trash { data_home: VPath },
}

/// Builds the [`Plan`] a `JobKind::Delete { permanent }` job runs. Async
/// and cancellable per design.md §9.3, via the same [`CancelToken`]
/// `plan_copy`/`plan_move` use — see the module doc comment for why
/// `Permanent`'s own cancellation surface is much smaller than either of
/// those (no walk of its own to interrupt mid-way).
pub async fn plan_delete(
    fs: &dyn FileSystem,
    targets: &[VPath],
    mode: DeleteMode,
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    match mode {
        DeleteMode::Permanent => plan_permanent_delete(fs, targets, options, cancel).await,
        DeleteMode::Trash { data_home } => {
            plan_trash_delete(fs, targets, &data_home, options, cancel).await
        }
    }
}

/// T-5.3.1's real trash planner — see the module doc comment for the full
/// design. `data_home` is stat'd only once per resolution inside
/// `duet_platform::trash::resolve_trash_destination`, not walked or
/// cached here; this function's own job is purely the `VPath` <->
/// `std::path::Path` boundary and turning one target's [`ResolvedTrash`]
/// answer into its `Step` pair.
///
/// One [`TrashReservations`] is shared across every target in this call --
/// planning and execution are separate phases (see [`crate::plan_delete`]'s
/// callers, which always plan a whole job's worth of steps before running
/// any of them), so two same-named targets in this same job would
/// otherwise both resolve to the identical destination: `duet_platform::
/// trash`'s own on-disk collision probe has nothing to see yet for a
/// sibling target that hasn't actually been renamed. See
/// [`TrashReservations`]'s own doc comment.
///
/// [`ResolvedTrash`]: duet_platform::trash::ResolvedTrash
/// [`TrashReservations`]: duet_platform::trash::TrashReservations
async fn plan_trash_delete(
    fs: &dyn FileSystem,
    targets: &[VPath],
    data_home: &VPath,
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let data_home_path = Path::new(data_home.inner().as_str());
    let mut steps = Vec::new();
    let mut reservations = duet_platform::trash::TrashReservations::new();

    for target in targets {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }
        // A target that's already gone (a concurrent delete elsewhere) is
        // silently skipped, matching `plan_permanent_delete`'s own
        // "already gone is success" convention one function up.
        match fs.stat(target, false).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(PlannerError::Vfs(e)),
        }

        let target_path = Path::new(target.inner().as_str());
        let resolved = duet_platform::trash::resolve_trash_destination(
            target_path,
            data_home_path,
            &mut reservations,
        )
        .map_err(|e| PlannerError::Trash {
            target: target.clone(),
            message: e.to_string(),
        })?;

        let info_path = path_to_vpath(&resolved.info_path, target)?;
        let content_path = path_to_vpath(&resolved.content_path, target)?;

        let write_trash_info_index = steps.len() as u32;
        steps.push(Step::WriteTrashInfo {
            info_path,
            content: resolved.trashinfo,
        });
        steps.push(Step::Rename {
            source: target.clone(),
            dest: content_path,
            // `duet_platform::trash` already guarantees this destination
            // name is collision-free as of its own probe -- a real
            // conflict here would mean something else claimed the exact
            // same name in the narrow plan-time-to-execution-time window
            // (the same TOCTOU window every other `AutoRename` use already
            // has, see that module's own doc comment). `Abort` -- not
            // `AutoRename` -- deliberately: silently picking a *different*
            // name at this point would leave the `.trashinfo` sidecar
            // already written for the name above pointing at content that
            // never lands there, which is exactly the mismatch this whole
            // plan-time-resolution design exists to prevent. `Abort`
            // cancels the rest of this delete job too (not just this one
            // step) -- heavier than a single-step failure, but the
            // alternative (a per-step `Failed` via a nonsensical
            // `RenameTarget`-needs-an-alternate message) reads far more
            // confusingly for what is already an exceedingly rare race,
            // and this codebase's own precedent elsewhere (`ENOSPC`
            // pausing the whole job, not just the offending step) already
            // favours a clear, job-wide stop over a partial, silently-
            // divergent success in cases this narrow.
            conflict: Some(ConflictPolicy::Abort),
            depends_on: Some(write_trash_info_index),
        });
    }
    Ok(Plan::new(steps, options))
}

/// `std::path::Path` -> `VPath` for a path `duet_platform::trash` produced
/// itself (always UTF-8: built from `Path::join`/`format!` over an
/// already-UTF-8 `target`, never from raw OS bytes) -- `target` is only
/// carried through for a well-attributed [`PlannerError`] in the
/// essentially-impossible case this ever fails.
fn path_to_vpath(path: &Path, target: &VPath) -> Result<VPath, PlannerError> {
    let s = path.to_str().ok_or_else(|| PlannerError::Trash {
        target: target.clone(),
        message: format!("{} is not valid UTF-8", path.display()),
    })?;
    let inner = UnixPathBuf::new(s).map_err(|e| PlannerError::Trash {
        target: target.clone(),
        message: e.to_string(),
    })?;
    Ok(VPath::local(inner))
}

async fn plan_permanent_delete(
    fs: &dyn FileSystem,
    targets: &[VPath],
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let mut steps = Vec::new();
    for target in targets {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }
        let meta = match fs.stat(target, false).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == duet_types::ErrorKind::NotFound => continue,
            Err(e) => return Err(PlannerError::Vfs(e)),
        };
        let mode = if meta.kind.is_dir() {
            RemoveMode::Recursive
        } else {
            RemoveMode::File
        };
        steps.push(Step::Remove {
            target: target.clone(),
            mode,
            depends_on: None,
        });
    }
    Ok(Plan::new(steps, options))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    use duet_types::UnixPathBuf;
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;

    fn vpath_for(p: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(p.to_str().unwrap()).unwrap())
    }

    async fn run(fs: Arc<dyn FileSystem>, plan: Plan, state_dir: &Path) -> crate::job::JobReport {
        let journal = crate::journal::Journal::open(crate::job::JobId(1), state_dir).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = crate::executor::ExecutionControl::new();
        crate::executor::execute(
            fs,
            crate::job::JobId(1),
            crate::job::JobKind::Delete { permanent: true },
            plan,
            journal,
            1,
            tx,
            control,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn permanent_delete_removes_a_file_and_a_directory_tree() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"y").unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[
                vpath_for(&dir.path().join("a.txt")),
                vpath_for(&dir.path().join("sub")),
            ],
            DeleteMode::Permanent,
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        assert_eq!(plan.steps.len(), 2);
        assert!(matches!(
            plan.steps[0],
            Step::Remove {
                mode: RemoveMode::File,
                ..
            }
        ));
        assert!(matches!(
            plan.steps[1],
            Step::Remove {
                mode: RemoveMode::Recursive,
                ..
            }
        ));

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("sub").exists());
    }

    /// T-5.1.8's own AC, verbatim: "recursive delete never follows a
    /// symlink out of the tree (explicit test with a symlink to `/`)."
    #[tokio::test]
    async fn recursive_delete_unlinks_a_symlink_to_root_without_following_it() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("victim")).unwrap();
        std::fs::write(dir.path().join("victim/keep.txt"), b"keep me").unwrap();
        std::os::unix::fs::symlink("/", dir.path().join("victim/escape")).unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&dir.path().join("victim"))],
            DeleteMode::Permanent,
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            !dir.path().join("victim").exists(),
            "the whole victim tree, including the dangling symlink, must be gone"
        );
        assert!(
            Path::new("/etc").exists(),
            "sanity check: the real filesystem root must be completely untouched"
        );
    }

    /// T-5.1.8's other AC clause: "read-only files prompt rather than
    /// silently failing." A parent directory without write permission
    /// makes `unlinkat` fail with `EACCES` regardless of the target file's
    /// own mode -- this must surface as a genuine, correctly-classified
    /// `StepFailure` (so the UI *can* prompt/offer elevation, T-5.1.10's
    /// own already-shipped machinery), never a silently-dropped success.
    #[tokio::test]
    async fn a_permission_denied_removal_surfaces_as_a_real_failure() {
        let dir = TempDir::new().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("a.txt"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&locked.join("a.txt"))],
            DeleteMode::Permanent,
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;

        // Restore write permission so TempDir's own Drop can clean up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.errors[0].kind, duet_types::ErrorKind::Permission);
        assert!(
            locked.join("a.txt").exists(),
            "the file must survive a denied removal"
        );
    }

    #[tokio::test]
    async fn a_missing_target_is_silently_skipped_at_plan_time() {
        let dir = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&dir.path().join("never-existed.txt"))],
            DeleteMode::Permanent,
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        assert!(plan.steps.is_empty());
    }

    fn data_home_vpath(dir: &TempDir) -> VPath {
        vpath_for(dir.path())
    }

    /// A real, end-to-end trash job: home trash (the target and
    /// `data_home` share a filesystem), producing a genuine `.trashinfo`
    /// file at the right path with the right content, and content landing
    /// at the matching `$trash/files/<name>` -- T-5.3.1's own AC in
    /// miniature.
    #[tokio::test]
    async fn trash_mode_moves_a_file_into_home_trash_and_writes_a_matching_trashinfo() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, b"trash me").unwrap();
        let data_home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src)],
            DeleteMode::Trash {
                data_home: data_home_vpath(&data_home),
            },
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        // Exactly a WriteTrashInfo/Rename pair, in that order, with the
        // Rename gated on the WriteTrashInfo step -- the "barrier" shape
        // the module doc comment describes.
        assert_eq!(plan.steps.len(), 2);
        assert!(matches!(plan.steps[0], Step::WriteTrashInfo { .. }));
        assert!(matches!(plan.steps[1], Step::Rename { .. }));
        assert_eq!(plan.steps[1].depends_on(), Some(0));

        let Step::WriteTrashInfo { info_path, .. } = &plan.steps[0] else {
            unreachable!()
        };
        let Step::Rename { dest, .. } = &plan.steps[1] else {
            unreachable!()
        };
        let info_path = Path::new(info_path.inner().as_str()).to_path_buf();
        let content_path = Path::new(dest.inner().as_str()).to_path_buf();
        assert_eq!(
            info_path,
            data_home.path().join("Trash/info/a.txt.trashinfo")
        );
        assert_eq!(content_path, data_home.path().join("Trash/files/a.txt"));

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&content_path).unwrap(), "trash me");

        let trashinfo = std::fs::read_to_string(&info_path).unwrap();
        assert!(trashinfo.starts_with("[Trash Info]\n"), "{trashinfo}");
        assert!(
            trashinfo.contains(&format!("Path={}", src.display())),
            "{trashinfo}"
        );
        assert!(trashinfo.contains("DeletionDate="), "{trashinfo}");
        let mode = std::fs::metadata(info_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// The freedesktop-spec-mandated collision behaviour: trashing two
    /// different files that happen to share a basename must not silently
    /// overwrite (or skip) the second one, and each one's `.trashinfo`
    /// must be paired with the *same* disambiguated name its content
    /// actually landed at -- the exact correctness property plan-time
    /// (not execution-time) name resolution exists to guarantee (see the
    /// module doc comment's "Why plan-time name resolution" cross-
    /// reference to `duet_platform::trash`).
    #[tokio::test]
    async fn two_same_named_targets_get_paired_disambiguated_trashinfo_and_content() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let src_a = dir_a.path().join("dup.txt");
        let src_b = dir_b.path().join("dup.txt");
        std::fs::write(&src_a, b"first").unwrap();
        std::fs::write(&src_b, b"second").unwrap();
        let data_home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src_a), vpath_for(&src_b)],
            DeleteMode::Trash {
                data_home: data_home_vpath(&data_home),
            },
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 4);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty());

        let files_dir = data_home.path().join("Trash/files");
        let info_dir = data_home.path().join("Trash/info");
        assert_eq!(
            std::fs::read_to_string(files_dir.join("dup.txt")).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(files_dir.join("dup (2).txt")).unwrap(),
            "second"
        );
        let info_first = std::fs::read_to_string(info_dir.join("dup.txt.trashinfo")).unwrap();
        let info_second = std::fs::read_to_string(info_dir.join("dup (2).txt.trashinfo")).unwrap();
        assert!(
            info_first.contains(&format!("Path={}", src_a.display())),
            "{info_first}"
        );
        assert!(
            info_second.contains(&format!("Path={}", src_b.display())),
            "{info_second}"
        );
    }

    /// The `WriteTrashInfo` -> `Rename` `depends_on` gate actually blocks
    /// the content move when the sidecar write fails -- mirroring
    /// T-5.1.7's own hardlink-graph dependency-gating test pattern (a
    /// dependent step must never run past a prerequisite that didn't
    /// succeed).
    #[tokio::test]
    async fn a_failed_write_trash_info_step_blocks_its_dependent_rename() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, b"keep me").unwrap();
        let data_home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src)],
            DeleteMode::Trash {
                data_home: data_home_vpath(&data_home),
            },
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let Step::WriteTrashInfo { info_path, .. } = &plan.steps[0] else {
            unreachable!()
        };
        let info_dir = Path::new(info_path.inner().as_str())
            .parent()
            .unwrap()
            .to_path_buf();
        // `plan_delete` already created `info_dir` (via `duet_platform::
        // trash`'s own directory-creation side effect) -- strip write
        // permission from it after the fact so the step's own `open_write`
        // genuinely fails at execution time.
        std::fs::set_permissions(&info_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let report = run(fs, plan, state.path()).await;

        // Restore write permission so TempDir's own Drop can clean up.
        std::fs::set_permissions(&info_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.errors[0].kind, duet_types::ErrorKind::Permission);
        assert!(
            src.exists(),
            "the dependency-gated Rename must never have run -- the source must survive"
        );
        assert!(
            !data_home.path().join("Trash/files/a.txt").exists(),
            "no content should have moved when its own .trashinfo write failed"
        );
    }

    /// A crash landing between `WriteTrashInfo`'s own `Completion` and the
    /// paired `Rename`'s own `Completion` (`Intent` recorded -- the
    /// executor always durably declares intent before attempting a step's
    /// side effect -- but the rename itself, or its `Completion` record,
    /// never landed; the exact window the module doc comment -- and
    /// `duet_platform::trash`'s own -- both call out as safe: recoverable,
    /// not silently lost, mirroring `journal.rs`'s own established "step 0
    /// fully completed, step 1 has an `Intent` with no `Completion`" crash-
    /// window shape) is resumable via the *real*
    /// `journal::JournalReader::scan` + `recovery::plan_from_recovery` +
    /// `executor::execute` path already merged by T-5.2.5, not a
    /// hand-simulated stand-in -- this is the proof the new
    /// `Step::WriteTrashInfo` variant is genuinely wired into every
    /// exhaustive match it needed to join (`Step::depends_on`,
    /// `rerun::depends_on_mut`, `executor::dispatch`), not merely
    /// compiling in isolation.
    #[tokio::test]
    async fn a_crash_between_write_trash_info_and_rename_recovers_via_the_real_resume_path() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, b"trash me").unwrap();
        let data_home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let job_id = crate::job::JobId(42);

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src)],
            DeleteMode::Trash {
                data_home: data_home_vpath(&data_home),
            },
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 2);

        // Simulate the crash by hand-driving the journal to exactly the
        // "WriteTrashInfo completed, Rename never even started" state --
        // and *actually performing* WriteTrashInfo's own effect first, so
        // on-disk state matches what the journal claims (otherwise this
        // would be testing "resume re-attempts a step whose Completion was
        // journaled but whose effect never happened," a different and
        // less realistic scenario than the crash window this test targets).
        {
            let mut journal = crate::journal::Journal::open(job_id, state.path()).unwrap();
            journal
                .append(&crate::journal::JournalRecord::JobStarted {
                    job_id,
                    started_at: duet_types::Timestamp::EPOCH,
                    plan: plan.clone(),
                    kind: crate::job::JobKind::Delete { permanent: false },
                })
                .unwrap();
            journal
                .append(&crate::journal::JournalRecord::Intent {
                    step_index: 0,
                    step: plan.steps[0].clone(),
                    partial_name: None,
                })
                .unwrap();
            let Step::WriteTrashInfo { info_path, content } = &plan.steps[0] else {
                unreachable!()
            };
            std::fs::write(Path::new(info_path.inner().as_str()), content).unwrap();
            journal
                .append(&crate::journal::JournalRecord::Completion {
                    step_index: 0,
                    outcome: crate::journal::StepOutcome::Succeeded,
                })
                .unwrap();
            // Step 1 (the Rename): its own Intent is recorded -- the
            // executor always journals Intent before attempting a step's
            // side effect -- but the crash lands before the rename itself
            // (or its Completion) ever happens, so the source is still at
            // its original path.
            journal
                .append(&crate::journal::JournalRecord::Intent {
                    step_index: 1,
                    step: plan.steps[1].clone(),
                    partial_name: None,
                })
                .unwrap();
        }

        let reports = crate::journal::JournalReader::scan(state.path()).unwrap();
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.incomplete_steps, vec![1]);
        assert!(src.exists(), "content must still be at its original path");
        assert!(
            data_home.path().join("Trash/info/a.txt.trashinfo").exists(),
            "the .trashinfo sidecar must already be durable"
        );

        // Resume via the real recovery machinery -- not a hand-rolled
        // stand-in.
        let resume_plan = crate::recovery::plan_from_recovery(report);
        assert_eq!(resume_plan.steps.len(), 1);
        assert!(matches!(resume_plan.steps[0], Step::Rename { .. }));
        assert_eq!(
            resume_plan.steps[0].depends_on(),
            None,
            "WriteTrashInfo wasn't in the redo set (it already succeeded), so rebuild_subset \
             must clear the now-dangling dependency rather than leave a stale index"
        );

        let journal = crate::journal::Journal::open(job_id, state.path()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = crate::executor::ExecutionControl::new();
        let resume_report = crate::executor::execute(
            fs,
            job_id,
            crate::job::JobKind::Delete { permanent: false },
            resume_plan,
            journal,
            1,
            tx,
            control,
            None,
        )
        .await;

        assert!(
            resume_report.errors.is_empty(),
            "{:?}",
            resume_report.errors
        );
        assert!(
            !src.exists(),
            "resume must have completed the interrupted move"
        );
        assert_eq!(
            std::fs::read_to_string(data_home.path().join("Trash/files/a.txt")).unwrap(),
            "trash me"
        );
    }
}
