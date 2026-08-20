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
//! # Trash reuses [`crate::mover::plan_move`] wholesale
//!
//! design.md §9.10/FR-CFG-07's full freedesktop trash-spec implementation
//! (multi-mount `$topdir/.Trash-$uid`, `.trashinfo` metadata, a restore
//! browser) is T-5.3.1/T-5.3.2's own, later scope — not duplicated here.
//! What T-5.1.8 needs from "trash" is much narrower: move the target into
//! *some* directory instead of removing its content, which is exactly
//! [`crate::mover::plan_move`]'s job (same-device: one `Rename`;
//! cross-device: copy+verify+remove, metadata- and hardlink-graph-
//! preserving since T-5.1.6/T-5.1.7) — trashing a file across a mount
//! boundary needs precisely the same fallback a same-filesystem-assuming
//! `rename(2)` can't provide on its own. [`DeleteMode::Trash`] takes the
//! trash directory as an already-resolved [`VPath`] rather than computing
//! one — XDG data-directory resolution is a `duet-config`/`duet-platform`
//! concern (environment/HOME-dependent), not something this
//! backend-agnostic, environment-agnostic crate does anywhere else either.
//!
//! One thing this module *does* still own on top of a plain `plan_move`
//! call: forcing [`ConflictPolicy::AutoRename`] as the effective
//! `default_conflict`, regardless of what the caller's own `options`
//! requested. The freedesktop trash spec requires a colliding name to be
//! disambiguated automatically, every time — that's not a user preference
//! `PlanOptions::default_conflict` should be able to override, the way it
//! legitimately can for an ordinary copy/move.

use duet_types::VPath;
use duet_vfs::FileSystem;

use crate::plan::{Plan, PlanOptions};
use crate::planner::{CancelToken, PlannerError};
use crate::step::{RemoveMode, Step};

/// Which of `JobKind::Delete`'s two behaviours (`permanent: bool`) a
/// [`plan_delete`] call should produce a [`Plan`] for.
pub enum DeleteMode {
    /// Content is actually removed. Bypasses trash (Shift+Del in TC/FR-OPS
    /// terms).
    Permanent,
    /// Content is moved into `trash_dir` instead of removed — see the
    /// module doc comment for why this is "reuse `plan_move`," not a
    /// separate implementation.
    Trash { trash_dir: VPath },
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
        DeleteMode::Trash { trash_dir } => {
            let trash_options = PlanOptions {
                default_conflict: crate::conflict::ConflictPolicy::AutoRename,
                ..options
            };
            crate::mover::plan_move(fs, targets, &trash_dir, trash_options, cancel).await
        }
    }
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
    use crate::step::StepKind;

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

    #[tokio::test]
    async fn trash_mode_moves_a_file_into_the_trash_directory() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, b"trash me").unwrap();
        let trash_dir = dir.path().join("trash");
        std::fs::create_dir(&trash_dir).unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src)],
            DeleteMode::Trash {
                trash_dir: vpath_for(&trash_dir),
            },
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            plan.steps
                .iter()
                .any(|s| matches!(s.kind(), StepKind::Rename)),
            "same-device trash move should be a single zero-cost Rename, exactly like plan_move"
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!src.exists());
        assert_eq!(
            std::fs::read_to_string(trash_dir.join("a.txt")).unwrap(),
            "trash me"
        );
    }

    /// The freedesktop-spec-mandated collision behaviour: trashing two
    /// different files that happen to share a basename must not silently
    /// overwrite (or skip) the second one -- `PlanOptions::default()`'s own
    /// `default_conflict` (`Skip`) would do exactly that if `plan_delete`
    /// didn't override it for `Trash` mode specifically.
    #[tokio::test]
    async fn trash_mode_auto_renames_on_a_name_collision_regardless_of_caller_options() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, b"second").unwrap();
        let trash_dir = dir.path().join("trash");
        std::fs::create_dir(&trash_dir).unwrap();
        std::fs::write(trash_dir.join("a.txt"), b"first").unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        // Caller explicitly asks for Skip -- plan_delete must still force
        // AutoRename for trash mode.
        let options = PlanOptions {
            default_conflict: crate::conflict::ConflictPolicy::Skip,
            ..PlanOptions::default()
        };
        let plan = plan_delete(
            &*fs,
            &[vpath_for(&src)],
            DeleteMode::Trash {
                trash_dir: vpath_for(&trash_dir),
            },
            options,
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report.skipped.is_empty(),
            "must not skip -- must auto-rename instead"
        );
        assert!(!src.exists());
        assert_eq!(
            std::fs::read_to_string(trash_dir.join("a.txt")).unwrap(),
            "first",
            "the pre-existing trashed file must be untouched"
        );
        assert_eq!(
            std::fs::read_to_string(trash_dir.join("a (2).txt")).unwrap(),
            "second"
        );
    }
}
