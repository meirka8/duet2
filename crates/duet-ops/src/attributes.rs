// SPDX-License-Identifier: MIT
//! [`plan_attributes`] — T-5.2.8's attributes/permissions planner
//! (FR-OPS-12, `docs/commands.md`'s `file.attributes` /
//! `file.chmod_recursive` rows, `docs/keymap-tc.csv` row 16's `Ctrl+A`
//! "Change Attributes" dialog).
//!
//! # The whole job is "one `SetMeta` per path"
//!
//! [`duet_types::MetaPatch`], [`Step::SetMeta`] and the executor's own
//! `set_meta_step` dispatch have all been real and tested since
//! T-3.1.5/T-5.1.6 — this module adds no `Step` variant, no
//! [`FileSystem`] trait method and no executor arm. It is purely the walk
//! that decides *which paths* the user's one patch applies to, which is
//! what makes "recursive apply runs through the operation queue, not
//! synchronously" (this task's own AC) true by construction: recursion is
//! a bigger `Plan`, executed by the same journaled, pausable, cancellable
//! job machinery a copy uses, not a `for` loop in a dialog's confirm
//! handler.
//!
//! Every emitted step carries `depends_on: None`. Unlike a copy job's own
//! `SetMeta` steps — which are gated on the `CreateDir`/`CopyFile` that
//! produced the destination they describe (see
//! [`crate::planner::plan_copy`]'s `DeferredMeta`) — nothing in *this*
//! plan creates anything. Every target already exists on disk at plan
//! time, so there is no producing step to gate on.
//!
//! # Symlinks: skipped during the walk, followed when named explicitly
//!
//! This is the one genuinely load-bearing decision here, and it exists
//! because of a concrete, verified property of the layer below:
//! `duet_vfs::local::meta::set_meta` applies `MetaPatch::mode` with
//! `chmodat(..., AtFlags::empty())`, which **follows** a symlink in the
//! last component (Linux has no `lchmod`; `AT_SYMLINK_NOFOLLOW` is
//! rejected with `ENOTSUP` for `chmod` specifically, since permission bits
//! on a symlink itself are not a thing). A naive "emit a `SetMeta` for
//! every path under the target" walk would therefore reach *out of the
//! tree* the moment it met a symlink pointing outside it — exactly what
//! design.md §13's symlink row forbids ("never followed implicitly during
//! recursive delete or recursive chmod").
//!
//! So: a [`EntryKind::Symlink`] entry **encountered during the recursive
//! walk is skipped entirely** — no step is emitted for it, and it is never
//! descended into (it isn't a directory, so the work queue never sees it
//! either way). This is also precisely what `chmod -R` itself does, so it
//! is the behaviour a user reaching for a recursive chmod already expects.
//!
//! A symlink named *directly* as one of `targets` still gets its
//! `SetMeta`, and therefore still resolves through to its referent for the
//! mode half. Also `chmod`'s own documented behaviour ("for each symbolic
//! link listed on the command line, chmod changes the permissions of the
//! pointed-to file"), and the user did after all put the cursor on that
//! exact entry and ask for it. The timestamp half is safe in both cases
//! regardless: `set_meta` applies those with
//! `utimensat(..., AtFlags::SYMLINK_NOFOLLOW)`.
//!
//! Fifos, sockets and device nodes are *not* skipped: `chmod` on one of
//! those follows nothing and changes exactly the entry named, so there is
//! no escape-the-tree hazard and no reason to treat them specially.
//!
//! # No `chmod -R u+X`-style special-casing
//!
//! The same patch applies uniformly to files and directories alike. There
//! is deliberately no "only add the execute bit to directories" nuance:
//! that is `chmod`'s *symbolic-operator* mode (`u+X`), a different feature
//! from the absolute `644`/`rw-r--r--` spec this dialog collects, and
//! nothing in this task's AC ("recursive apply") asks for it.

use std::collections::VecDeque;

use duet_types::{EntryKind, ErrorKind, MetaPatch, VPath};
use duet_vfs::{FileSystem, ListOpts};
use futures_util::StreamExt;

use crate::plan::{Plan, PlanOptions};
use crate::planner::PlannerError;
use crate::step::Step;

/// Builds the [`Plan`] the `Ctrl+A` attributes dialog enqueues: one
/// [`Step::SetMeta`] carrying `patch` for each of `targets`, plus — when
/// `recursive` — one more for every entry beneath each directory target,
/// at every depth.
///
/// # Empty inputs are valid, not errors
///
/// Two "nothing to do" cases both produce a real, zero-step [`Plan`]
/// rather than an error or a panic, matching [`crate::plan_mkdir`]'s
/// "already there is success" and [`crate::plan_delete`]'s "already gone
/// is success" conventions:
///
/// - an empty `targets` slice;
/// - a `patch` that [`MetaPatch::is_empty`] reports as a no-op (the user
///   left every field in the dialog blank). Guarded here, and *also*
///   short-circuited one layer up by
///   `duet_ui::attributes_dialog::AttributesDialogState::confirm`, which
///   closes without enqueuing anything at all — belt and braces, since an
///   empty patch reaching this far would otherwise journal a job whose
///   every step is a syscall-free no-op.
///
/// # Errors
/// - [`PlannerError::Vfs`] — a `stat`/`read_dir` failed for a reason other
///   than "not found". A target that has vanished between the user
///   selecting it and confirming the dialog is skipped silently, the same
///   way [`crate::plan_delete`] skips an already-deleted target.
/// - [`PlannerError::NoFileName`] — a directory entry's name could not be
///   joined onto its parent (the same `VPath::join` failure
///   [`crate::plan_copy`] maps onto this variant).
///
/// Deliberately *not* cancellable: unlike [`crate::plan_copy`] there is no
/// [`crate::CancelToken`] parameter. A `stat`-free, `size`-free walk that
/// emits one cheap step per entry has no per-entry work worth interrupting,
/// and no caller has anywhere to put a token — the dialog confirms and the
/// plan is done. If a pathological tree ever makes this worth reconsidering,
/// adding a token is an additive change to this one signature.
pub async fn plan_attributes(
    fs: &dyn FileSystem,
    targets: &[VPath],
    patch: MetaPatch,
    recursive: bool,
) -> Result<Plan, PlannerError> {
    if patch.is_empty() {
        return Ok(Plan::new(Vec::new(), PlanOptions::default()));
    }

    let mut steps: Vec<Step> = Vec::new();
    let mut queue: VecDeque<VPath> = VecDeque::new();

    for target in targets {
        let meta = match fs.stat(target, false).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(PlannerError::Vfs(e)),
        };
        steps.push(Step::SetMeta {
            target: target.clone(),
            patch: patch.clone(),
            depends_on: None,
        });
        if recursive && meta.kind.is_dir() {
            queue.push_back(target.clone());
        }
    }

    // Names and kinds only (`ListOpts::names_only`, `ListFields::empty()`).
    // Unlike `plan_copy` -- which needs every field it can get, since its
    // walk is the only place cheap enough to read a source's full metadata
    // once -- this walk needs exactly one thing per entry: is it a
    // directory (descend), a symlink (skip), or anything else (emit)? On a
    // `getdents64`-backed local directory that needs no `stat` calls at
    // all. `follow_symlinks: false` is the default and is load-bearing
    // here: with it `true`, a symlink to a directory would report
    // `EntryKind::Directory` and this walk would descend straight out of
    // the tree.
    let list_opts = ListOpts::names_only();

    while let Some(dir) = queue.pop_front() {
        let mut chunks = fs.read_dir(&dir, list_opts);
        while let Some(chunk) = chunks.next().await {
            let entries = chunk.map_err(PlannerError::Vfs)?;
            for entry in entries {
                // See the module doc comment: `set_meta`'s mode half is a
                // following `chmodat`, so a `SetMeta` on a symlink found
                // during the walk would change whatever it points at,
                // wherever that is.
                if entry.metadata.kind == EntryKind::Symlink {
                    continue;
                }
                let path = dir
                    .join(&entry.name)
                    .map_err(|_| PlannerError::NoFileName(dir.clone()))?;
                steps.push(Step::SetMeta {
                    target: path.clone(),
                    patch: patch.clone(),
                    depends_on: None,
                });
                if entry.metadata.kind.is_dir() {
                    queue.push_back(path);
                }
            }
        }
    }

    Ok(Plan::new(steps, PlanOptions::default()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::Path;
    use std::sync::Arc;

    use duet_types::{Timestamp, UnixPathBuf};
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::step::StepKind;

    fn vpath_for(p: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(p.to_str().unwrap()).unwrap())
    }

    /// Runs a `Plan` all the way through the real executor against a real
    /// `LocalFs` and a real on-disk journal -- the same helper shape
    /// `deleter.rs`/`creators.rs` already use, so every test below asserts
    /// the actual on-disk outcome rather than merely the emitted steps.
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

    fn local_fs() -> Arc<dyn FileSystem> {
        Arc::new(LocalFs)
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn chmod(p: &Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn non_recursive_changes_a_single_files_mode() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        chmod(&file, 0o600);

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&file)],
            MetaPatch::default().with_mode(0o644),
            false,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind(), StepKind::SetMeta);
        assert!(
            matches!(
                plan.steps[0],
                Step::SetMeta {
                    depends_on: None,
                    ..
                }
            ),
            "nothing in an attributes plan creates anything, so no step has a producer to gate on"
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(mode_of(&file), 0o644);
    }

    /// The non-recursive half of the AC: a directory target gets its own
    /// mode changed and nothing else does.
    #[tokio::test]
    async fn non_recursive_on_a_directory_leaves_its_contents_untouched() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let target = dir.path().join("tree");
        std::fs::create_dir(&target).unwrap();
        let inside = target.join("inside.txt");
        std::fs::write(&inside, b"x").unwrap();
        chmod(&target, 0o700);
        chmod(&inside, 0o600);

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&target)],
            MetaPatch::default().with_mode(0o755),
            false,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 1, "only the directory itself");

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(mode_of(&target), 0o755);
        assert_eq!(
            mode_of(&inside),
            0o600,
            "a non-recursive apply must not reach into the directory at all"
        );
    }

    /// The recursive half of the AC, at more than one level of depth.
    #[tokio::test]
    async fn recursive_changes_the_directory_and_everything_beneath_it_at_every_depth() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("top.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/mid.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("sub/deeper/bottom.txt"), b"x").unwrap();

        let everything = [
            root.clone(),
            root.join("top.txt"),
            root.join("sub"),
            root.join("sub/mid.txt"),
            root.join("sub/deeper"),
            root.join("sub/deeper/bottom.txt"),
        ];
        for p in &everything {
            chmod(p, 0o700);
        }

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&root)],
            MetaPatch::default().with_mode(0o755),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            plan.steps.len(),
            everything.len(),
            "one SetMeta per path in the tree, the root included"
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        for p in &everything {
            assert_eq!(mode_of(p), 0o755, "{} kept its old mode", p.display());
        }
    }

    /// This task's AC's other half: "timestamp editing." Real `SetMeta`,
    /// real executor, read back through a real `stat`.
    #[tokio::test]
    async fn a_timestamp_patch_is_applied_and_readable_back() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let file = dir.path().join("dated.txt");
        std::fs::write(&file, b"x").unwrap();

        // 2001-02-03 04:05:00 UTC and one hour earlier, chosen simply for
        // being far from "now" so a passing test can't be an accident.
        let modified = Timestamp::new(981_173_100, 0);
        let accessed = Timestamp::new(981_169_500, 0);

        let fs = local_fs();
        let patch = MetaPatch {
            modified: Some(modified),
            accessed: Some(accessed),
            ..MetaPatch::default()
        };
        let plan = plan_attributes(&*fs, &[vpath_for(&file)], patch, false)
            .await
            .unwrap();

        let report = run(Arc::clone(&fs), plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let meta = fs.stat(&vpath_for(&file), false).await.unwrap();
        assert_eq!(meta.modified.map(|t| t.secs), Some(modified.secs));
        assert_eq!(meta.accessed.map(|t| t.secs), Some(accessed.secs));
        // ... and through plain std, so this isn't just the VFS agreeing
        // with itself.
        assert_eq!(std::fs::metadata(&file).unwrap().mtime(), modified.secs);
    }

    #[tokio::test]
    async fn every_target_in_a_multi_target_call_gets_the_patch() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        for p in [&a, &b, &c] {
            std::fs::write(p, b"x").unwrap();
            chmod(p, 0o600);
        }

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&a), vpath_for(&b), vpath_for(&c)],
            MetaPatch::default().with_mode(0o640),
            false,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 3);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        for p in [&a, &b, &c] {
            assert_eq!(mode_of(p), 0o640);
        }
    }

    /// design.md §13's symlink row, established for this planner the same
    /// way `deleter.rs`'s own
    /// `recursive_delete_unlinks_a_symlink_to_root_without_following_it`
    /// establishes it for delete. The link points at a file in a
    /// completely separate temporary directory with a distinctive mode; a
    /// recursive chmod of the tree must leave that file's mode exactly as
    /// it was. See the module doc comment for why this needs an explicit
    /// skip rather than falling out of the VFS layer for free (unlike
    /// delete, whose `O_NOFOLLOW`-based recursion is safe by construction):
    /// `set_meta`'s mode half is a symlink-following `chmodat`.
    #[tokio::test]
    async fn recursive_never_follows_a_symlink_out_of_the_tree() {
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("do-not-touch.txt");
        std::fs::write(&victim, b"untouchable").unwrap();
        chmod(&victim, 0o400);

        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir(&root).unwrap();
        let inside = root.join("inside.txt");
        std::fs::write(&inside, b"x").unwrap();
        chmod(&root, 0o700);
        chmod(&inside, 0o600);
        std::os::unix::fs::symlink(&victim, root.join("escape")).unwrap();
        // ... and a symlink to a *directory* too, which would additionally
        // let the walk itself descend out of the tree if `read_dir` were
        // ever asked to follow symlinks.
        let outside_dir = outside.path().join("subdir");
        std::fs::create_dir(&outside_dir).unwrap();
        let deep_victim = outside_dir.join("also-do-not-touch.txt");
        std::fs::write(&deep_victim, b"untouchable").unwrap();
        chmod(&deep_victim, 0o400);
        std::os::unix::fs::symlink(&outside_dir, root.join("escape-dir")).unwrap();

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&root)],
            MetaPatch::default().with_mode(0o755),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            plan.steps.len(),
            2,
            "the tree root and inside.txt -- neither symlink may contribute a step"
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(mode_of(&root), 0o755);
        assert_eq!(mode_of(&inside), 0o755);
        assert_eq!(
            mode_of(&victim),
            0o400,
            "the symlink's target lives outside the tree and must be untouched"
        );
        assert_eq!(
            mode_of(&deep_victim),
            0o400,
            "and the walk must not have descended through a symlink to a directory either"
        );
        assert!(
            std::fs::symlink_metadata(root.join("escape")).is_ok(),
            "the links themselves are left in place -- this is a chmod, not a delete"
        );
    }

    #[tokio::test]
    async fn an_empty_patch_produces_a_valid_zero_step_plan() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let fs = local_fs();
        let plan = plan_attributes(&*fs, &[vpath_for(&file)], MetaPatch::default(), true)
            .await
            .unwrap();
        assert!(
            plan.steps.is_empty(),
            "nothing was edited -- a valid plan with nothing to do, not an error"
        );
    }

    #[tokio::test]
    async fn an_empty_target_slice_produces_a_valid_zero_step_plan() {
        let fs = local_fs();
        let plan = plan_attributes(&*fs, &[], MetaPatch::default().with_mode(0o644), true)
            .await
            .unwrap();
        assert!(plan.steps.is_empty());
    }

    #[tokio::test]
    async fn a_target_that_vanished_before_confirm_is_silently_skipped() {
        let dir = TempDir::new().unwrap();
        let present = dir.path().join("here.txt");
        std::fs::write(&present, b"x").unwrap();

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&dir.path().join("gone.txt")), vpath_for(&present)],
            MetaPatch::default().with_mode(0o644),
            false,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 1, "only the target that still exists");
    }

    /// Setuid/setgid/sticky live in the same `0o7777` mask the dialog's own
    /// octal field accepts, so a four-digit spec has to survive the whole
    /// plan -> journal -> execute path intact, not get masked down to nine
    /// `rwx` bits somewhere.
    #[tokio::test]
    async fn a_setgid_and_sticky_mode_survives_to_disk() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let target = dir.path().join("shared");
        std::fs::create_dir(&target).unwrap();
        chmod(&target, 0o755);

        let fs = local_fs();
        let plan = plan_attributes(
            &*fs,
            &[vpath_for(&target)],
            MetaPatch::default().with_mode(0o3775),
            false,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(mode_of(&target), 0o3775);
    }
}
