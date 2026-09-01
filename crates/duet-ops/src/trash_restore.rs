// SPDX-License-Identifier: MIT
//! T-5.3.2 phase 1's two planners on top of T-5.3.1's freedesktop trash
//! implementation: [`plan_trash_restore`] (`trash.restore`) and
//! [`plan_trash_purge`] (`trash.empty`/`trash.delete_selected` alike — see
//! that function's own doc comment for why one planner serves both).
//!
//! Both take a slice of [`duet_platform::trash::TrashEntry`] —
//! [`duet_platform::trash::list_trash_entries`]'s own output — rather than
//! re-deriving trash locations themselves; this module's whole job is
//! turning an already-discovered entry into the right `Step`s, mirroring
//! `deleter.rs`'s own "the pure decision lives in `duet-platform`, this
//! crate only turns the answer into steps" boundary.
//!
//! # Restore: the mirror image of `deleter.rs`'s trash-write ordering, and
//! why it needs no `depends_on` between the recreated parent and the move
//!
//! Trashing something (T-5.3.1) writes `.trashinfo` *before* moving the
//! content, specifically so a crash between the two leaves recoverable
//! evidence (an orphaned sidecar pointing at content still at its original
//! path) rather than functionally-lost content (moved, but with no
//! metadata pointing back to where it came from). Restoring is the same
//! principle in reverse: the content move happens first, and the
//! `.trashinfo` sidecar is only removed *after* it durably succeeds. A
//! crash in between leaves the sidecar still present — the entry just
//! reappears in a future [`duet_platform::trash::list_trash_entries`] scan,
//! re-drivable by hitting restore again — rather than the opposite
//! ordering's failure mode, which would permanently orphan already-moved
//! content the instant a crash landed between the two.
//!
//! This is expressed as a plain [`crate::step::Step::Remove`] on the
//! `.trashinfo` path, `depends_on: Some(rename_step_index)` — the same
//! dependency-gating mechanism `mover.rs`'s cross-device `Remove(source)`
//! already uses, gated on the `Rename` that must land first.
//!
//! The recreated-parent-directory `Step::CreateDir`s (when
//! `original_path`'s parent no longer exists — the AC's own "recreates it"
//! clause) are **not** wired through `depends_on` at all, deliberately:
//! [`crate::creators::plan_mkdir`]'s own doc comment already establishes
//! that `CreateDir` needs no dependency field because `execute`'s own
//! concurrency model runs every barrier step (which `CreateDir`, `Rename`,
//! and `Remove` all are) strictly one at a time, in plan order, each fully
//! finished before the next one starts — so plain step position already
//! guarantees the parent exists (or definitively failed to) before the
//! `Rename` right after it even begins. Explicitly *not* chaining
//! `Rename`'s `depends_on` onto the `CreateDir`(s) is the load-bearing part
//! of that choice, not an oversight: [`crate::executor`]'s own
//! `dependency_block_reason` gates on "the named dependency's outcome was
//! `Failed`", and a step whose *own* dependency was unmet is recorded
//! `Skipped`, not `Failed` — so if `Rename` depended on a failed
//! `CreateDir`, `Rename` itself would come back `Skipped`, and the
//! `.trashinfo` `Remove` gated on *that* would see "not `Failed`" and
//! wrongly proceed, deleting the sidecar for content that never actually
//! moved. Leaving `Rename` ungated instead means: if the parent really
//! couldn't be created, `Rename`'s own attempt then genuinely fails too (a
//! real `NotFound` from trying to rename into a still-missing directory) —
//! a *direct* `Failed`, not a dependency-blocked `Skipped` — which
//! correctly blocks the dependent `.trashinfo` removal. The AC's "or
//! reports clearly" clause is satisfied by the `CreateDir` step's own
//! `StepFailure` landing in the job report exactly as-is (noisier than a
//! single combined error would be, but this is the exact same trade-off
//! [`crate::creators::plan_mkdir`]'s own doc comment already discloses for
//! a multi-level F7 whose middle level fails).
//!
//! # Restore's `Rename` conflict policy: live resolution, not `Abort`
//!
//! Unlike `deleter.rs`'s trash-mode `Rename` (whose destination is
//! guaranteed collision-free by `duet_platform::trash`'s own plan-time
//! probe, so a live collision would mean a genuine race worth aborting
//! over), restoring into `original_path` is landing on a path this module
//! has no special knowledge about — something else may legitimately occupy
//! it by now (a new file created with the same name after the original was
//! trashed, e.g.). `conflict: None` here, exactly the same as
//! [`crate::mover::plan_move`]'s own same-device `Rename` and
//! [`crate::creators::plan_rename_in_place`]'s, leaves resolution to the
//! usual tiers: a live [`crate::ConflictResolver`], then
//! `PlanOptions::default_conflict`.
//!
//! # Empty trash and delete-selected: one planner, one caller-chosen scope
//!
//! [`plan_trash_purge`] permanently removes every given entry's content and
//! `.trashinfo` sidecar — the same `Step::Remove` shape
//! [`crate::deleter::plan_delete`]'s own `DeleteMode::Permanent` already
//! uses, just applied to trash entries instead of live filesystem targets.
//! `trash.empty` and `trash.delete_selected` (`docs/commands.md`) differ
//! only in *which* `entries` slice the caller passes — every entry
//! [`duet_platform::trash::list_trash_entries`] found, or a user-chosen
//! subset — not in what planning logic runs, mirroring
//! [`crate::job::JobKind::ChangeAttributes`]'s own "one variant, several
//! closely-related shapes" precedent, which [`crate::job::JobKind::PurgeTrash`]
//! follows for the same reason.

use std::path::Path;

use duet_platform::trash::TrashEntry;
use duet_types::{ErrorKind, UnixPathBuf, VPath};
use duet_vfs::FileSystem;

use crate::plan::{Plan, PlanOptions};
use crate::planner::{CancelToken, PlannerError};
use crate::step::{RemoveMode, Step};

/// Builds the [`Plan`] `trash.restore` runs: for each of `entries`, recreate
/// its `original_path`'s parent directory if it no longer exists, move the
/// trashed content back, then remove the now-stale `.trashinfo` sidecar
/// once the move has durably succeeded. See the module doc comment for the
/// full ordering rationale.
///
/// # Errors
/// - [`PlannerError::Cancelled`] — `cancel` was triggered mid-plan.
/// - [`PlannerError::Vfs`] — a `stat` call (part of
///   [`crate::creators::plan_mkdir`]'s own ancestor walk) failed for a
///   reason other than "not found".
/// - [`PlannerError::Trash`] — one of `entry`'s own paths (from
///   [`duet_platform::trash::list_trash_entries`]) isn't valid UTF-8 or
///   isn't a well-formed absolute path — essentially unreachable in
///   practice, since every path a `TrashEntry` carries was itself built
///   from already-UTF-8 components (see `duet_platform::trash`'s own
///   "always UTF-8" note), but handled honestly rather than assumed away
///   with an `.unwrap()`.
pub async fn plan_trash_restore(
    fs: &dyn FileSystem,
    entries: &[TrashEntry],
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let mut steps: Vec<Step> = Vec::new();

    for entry in entries {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }

        let content = path_to_vpath(&entry.content_path)?;
        let info = path_to_vpath(&entry.info_path)?;
        let original = path_to_vpath(&entry.original_path)?;

        // AC: "restore into a deleted parent recreates it." Purely
        // positional ordering (no `depends_on`) -- see the module doc
        // comment for why that's the load-bearing choice here, not an
        // omission.
        if let Some(parent) = original.parent() {
            let mkdir_plan = crate::creators::plan_mkdir(fs, &parent).await?;
            steps.extend(mkdir_plan.steps);
        }

        let rename_index = steps.len() as u32;
        steps.push(Step::Rename {
            source: content,
            dest: original,
            // Live conflict resolution -- see the module doc comment's
            // "Restore's `Rename` conflict policy" section.
            conflict: None,
            depends_on: None,
        });
        steps.push(Step::Remove {
            target: info,
            mode: RemoveMode::File,
            // Only removed once the content move above is durable -- the
            // mirror image of `deleter.rs`'s "WriteTrashInfo before Rename"
            // barrier, in reverse. See the module doc comment.
            depends_on: Some(rename_index),
        });
    }

    Ok(Plan::new(steps, options))
}

/// Builds the [`Plan`] `trash.empty`/`trash.delete_selected` both run:
/// permanently removes every one of `entries`' own content and
/// `.trashinfo` sidecar. See the module doc comment for why one function
/// serves both commands.
///
/// An entry whose content or sidecar has already vanished by plan time (a
/// concurrent purge elsewhere, e.g.) is silently skipped for that half —
/// the same "already gone is success" convention
/// [`crate::deleter::plan_delete`]'s own permanent-delete mode already
/// established.
///
/// # Errors
/// Same as [`plan_trash_restore`]'s [`PlannerError::Cancelled`]/
/// [`PlannerError::Vfs`]/[`PlannerError::Trash`] cases.
pub async fn plan_trash_purge(
    fs: &dyn FileSystem,
    entries: &[TrashEntry],
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let mut steps: Vec<Step> = Vec::new();

    for entry in entries {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }

        let content = path_to_vpath(&entry.content_path)?;
        match fs.stat(&content, false).await {
            Ok(meta) => {
                let mode = if meta.kind.is_dir() {
                    RemoveMode::Recursive
                } else {
                    RemoveMode::File
                };
                steps.push(Step::Remove {
                    target: content,
                    mode,
                    depends_on: None,
                });
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(PlannerError::Vfs(e)),
        }

        let info = path_to_vpath(&entry.info_path)?;
        match fs.stat(&info, false).await {
            Ok(_) => steps.push(Step::Remove {
                target: info,
                mode: RemoveMode::File,
                depends_on: None,
            }),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(PlannerError::Vfs(e)),
        }
    }

    Ok(Plan::new(steps, options))
}

/// `std::path::Path` -> `VPath`, for a path [`duet_platform::trash::
/// TrashEntry`] carries -- mirrors `deleter::path_to_vpath`'s own
/// "essentially impossible in practice" framing, but without an
/// already-known-good sibling `VPath` to attribute a failure to (every one
/// of `content_path`/`info_path`/`original_path` is equally "just a path
/// this module read off disk", not obviously more trustworthy than the
/// others) -- so the failing path's own lossy `Display` form doubles as
/// both the error message and (via [`UnixPathBuf::from_os_lossy`]) the
/// best-effort `target` attribution.
fn path_to_vpath(path: &Path) -> Result<VPath, PlannerError> {
    let fallback = || {
        VPath::local(
            UnixPathBuf::from_os_lossy(path.as_os_str()).unwrap_or_else(|_| UnixPathBuf::root()),
        )
    };
    let s = path.to_str().ok_or_else(|| PlannerError::Trash {
        target: fallback(),
        message: format!("{} is not valid UTF-8", path.display()),
    })?;
    let inner = UnixPathBuf::new(s).map_err(|e| PlannerError::Trash {
        target: fallback(),
        message: format!("{}: {e}", path.display()),
    })?;
    Ok(VPath::local(inner))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::conflict::{ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver};
    use crate::step::StepKind;

    async fn run(fs: Arc<dyn FileSystem>, plan: Plan, state_dir: &Path) -> crate::job::JobReport {
        run_with_resolver(fs, plan, state_dir, None).await
    }

    async fn run_with_resolver(
        fs: Arc<dyn FileSystem>,
        plan: Plan,
        state_dir: &Path,
        resolver: Option<Arc<dyn ConflictResolver>>,
    ) -> crate::job::JobReport {
        let journal = crate::journal::Journal::open(crate::job::JobId(1), state_dir).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = crate::executor::ExecutionControl::new();
        crate::executor::execute(
            fs,
            crate::job::JobId(1),
            crate::job::JobKind::RestoreFromTrash,
            plan,
            journal,
            1,
            tx,
            control,
            resolver,
        )
        .await
    }

    fn local_fs() -> Arc<dyn FileSystem> {
        Arc::new(LocalFs)
    }

    /// Builds a real, on-disk trashed entry (content in `files/`, a real
    /// `.trashinfo` in `info/`) without going through `deleter.rs`'s own
    /// planner -- this module's tests only care about the *read side*
    /// (`TrashEntry`) being restorable/purgeable correctly, not re-proving
    /// T-5.3.1's own trash-write behaviour.
    fn make_trashed_entry(
        trash_root: &Path,
        name: &str,
        original: &Path,
        content: &[u8],
    ) -> TrashEntry {
        std::fs::create_dir_all(trash_root.join("files")).unwrap();
        std::fs::create_dir_all(trash_root.join("info")).unwrap();
        let content_path = trash_root.join("files").join(name);
        let info_path = trash_root.join("info").join(format!("{name}.trashinfo"));
        std::fs::write(&content_path, content).unwrap();
        std::fs::write(
            &info_path,
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-01-02T03:04:05\n",
                original.display()
            ),
        )
        .unwrap();
        TrashEntry {
            content_path,
            info_path,
            original_path: original.to_path_buf(),
            deleted_at: SystemTime::UNIX_EPOCH,
        }
    }

    // -- plan_trash_restore ---------------------------------------------

    /// The core, real-executor, real-disk restore: content moves back to
    /// `original_path`, and the `.trashinfo` sidecar is gone afterward.
    #[tokio::test]
    async fn restore_moves_content_back_and_removes_the_sidecar() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let original = dir.path().join("original/a.txt");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        let entry = make_trashed_entry(&trash_root, "a.txt", &original, b"restored content");

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        // No parent recreation needed here -- exactly Rename + Remove.
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].kind(), StepKind::Rename);
        assert_eq!(plan.steps[1].kind(), StepKind::Remove);
        assert_eq!(plan.steps[1].depends_on(), Some(0));

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!entry.content_path.exists());
        assert!(!entry.info_path.exists());
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "restored content"
        );
    }

    /// The AC's own "recreates it" clause: a restore whose original parent
    /// directory no longer exists recreates it (potentially several levels
    /// deep) and still lands the file at the right place.
    #[tokio::test]
    async fn restore_recreates_a_multi_level_deleted_parent_directory() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        // Neither `deleted` nor `deleted/sub` exist on disk.
        let original = dir.path().join("deleted/sub/a.txt");
        let entry = make_trashed_entry(&trash_root, "a.txt", &original, b"came back");

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        // 2 CreateDir (deleted, deleted/sub) + Rename + Remove.
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::CreateDir,
                StepKind::CreateDir,
                StepKind::Rename,
                StepKind::Remove,
            ]
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(original.parent().unwrap().is_dir());
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "came back");
        assert!(!entry.info_path.exists());
    }

    /// The AC's other clause: "...or reports clearly" -- when the parent
    /// can't actually be recreated (a permission-denied ancestor), the job
    /// must surface a real, classified failure, never panic or silently
    /// "succeed" wrong, and the `.trashinfo` sidecar must survive (nothing
    /// actually moved).
    #[tokio::test]
    async fn restore_reports_clearly_when_parent_recreation_fails() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let original = locked.join("newdir/a.txt");
        let entry = make_trashed_entry(&trash_root, "a.txt", &original, b"stuck");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            !report.errors.is_empty(),
            "a real failure must be reported, not silently swallowed"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == duet_types::ErrorKind::Permission),
            "{:?}",
            report.errors
        );
        assert!(
            entry.content_path.exists(),
            "content must never have moved -- the parent never actually existed"
        );
        assert!(
            entry.info_path.exists(),
            "the .trashinfo sidecar must survive: the Rename never durably succeeded, \
             so its dependent Remove must have been blocked"
        );
    }

    /// The dependency gate's own direct test: a content move that fails
    /// outright (not via a blocked parent -- a genuine `Rename` failure)
    /// must still leave the `.trashinfo` sidecar in place afterward.
    /// Mirrors `deleter.rs`'s own
    /// `a_failed_write_trash_info_step_blocks_its_dependent_rename`, applied
    /// to this reversed ordering.
    #[tokio::test]
    async fn a_failed_rename_blocks_its_dependent_trashinfo_removal() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let dest_parent = dir.path().join("dest");
        std::fs::create_dir(&dest_parent).unwrap();
        let original = dest_parent.join("a.txt");
        let entry = make_trashed_entry(&trash_root, "a.txt", &original, b"never moves");
        // Deny write on the destination's parent so the real `renameat`
        // fails with EACCES -- a genuine, direct Rename failure.
        std::fs::set_permissions(&dest_parent, std::fs::Permissions::from_mode(0o500)).unwrap();

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, plan, state.path()).await;

        std::fs::set_permissions(&dest_parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            entry.content_path.exists(),
            "the source must survive a failed rename"
        );
        assert!(
            entry.info_path.exists(),
            "the dependency-gated Remove must never have run"
        );
    }

    /// Restoring into a path something else now occupies goes through
    /// live, resolvable conflict resolution -- not a fixed `Abort` policy
    /// -- mirroring how `mover.rs`'s own `plan_move` `Rename` steps are
    /// proven conflict-resolvable, just via a live `ConflictResolver`
    /// instead of `PlanOptions::default_conflict` this time, to prove the
    /// *live* resolver path specifically is reachable, not only the
    /// static-default one.
    #[tokio::test]
    async fn restore_into_an_occupied_path_goes_through_live_conflict_resolution() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let original = dir.path().join("a.txt");
        std::fs::write(&original, b"already here").unwrap();
        let entry = make_trashed_entry(&trash_root, "a.txt", &original, b"from trash");

        struct AlwaysOverwrite;
        impl ConflictResolver for AlwaysOverwrite {
            fn resolve(&self, _prompt: &ConflictPrompt) -> ConflictResolution {
                ConflictResolution::once(ConflictPolicy::Overwrite)
            }
        }

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(matches!(plan.steps[0], Step::Rename { conflict: None, .. }));

        let report =
            run_with_resolver(fs, plan, state.path(), Some(Arc::new(AlwaysOverwrite))).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "from trash");
        assert!(!entry.info_path.exists());
    }

    /// Multiple entries in one restore call each get their own
    /// Rename/Remove pair, independently indexed.
    #[tokio::test]
    async fn restoring_multiple_entries_restores_each_independently() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let original_a = dir.path().join("a.txt");
        let original_b = dir.path().join("b.txt");
        let entry_a = make_trashed_entry(&trash_root, "a.txt", &original_a, b"aaa");
        let entry_b = make_trashed_entry(&trash_root, "b.txt", &original_b, b"bbb");

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_restore(
            &*fs,
            &[entry_a.clone(), entry_b.clone()],
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 4);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(std::fs::read_to_string(&original_a).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(&original_b).unwrap(), "bbb");
        assert!(!entry_a.info_path.exists());
        assert!(!entry_b.info_path.exists());
    }

    // -- plan_trash_purge -------------------------------------------------

    async fn run_purge(
        fs: Arc<dyn FileSystem>,
        plan: Plan,
        state_dir: &Path,
    ) -> crate::job::JobReport {
        let journal = crate::journal::Journal::open(crate::job::JobId(2), state_dir).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = crate::executor::ExecutionControl::new();
        crate::executor::execute(
            fs,
            crate::job::JobId(2),
            crate::job::JobKind::PurgeTrash,
            plan,
            journal,
            1,
            tx,
            control,
            None,
        )
        .await
    }

    /// Emptying the whole trash removes every entry's content and sidecar
    /// -- nothing else, and nothing left behind.
    #[tokio::test]
    async fn purge_removes_every_given_entrys_content_and_sidecar() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let entry_a = make_trashed_entry(
            &trash_root,
            "a.txt",
            &PathBuf::from("/wherever/a.txt"),
            b"a",
        );
        let entry_b = make_trashed_entry(
            &trash_root,
            "b.txt",
            &PathBuf::from("/wherever/b.txt"),
            b"b",
        );

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_purge(
            &*fs,
            &[entry_a.clone(), entry_b.clone()],
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(plan.steps.len(), 4);

        let report = run_purge(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        for entry in [&entry_a, &entry_b] {
            assert!(!entry.content_path.exists());
            assert!(!entry.info_path.exists());
        }
    }

    /// `trash.delete_selected`'s own scoping: only the entries actually
    /// passed in are touched, everything else in the same trash root
    /// survives untouched.
    #[tokio::test]
    async fn purge_scoped_to_a_subset_leaves_the_rest_of_the_trash_alone() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let doomed = make_trashed_entry(
            &trash_root,
            "doomed.txt",
            &PathBuf::from("/wherever/doomed.txt"),
            b"bye",
        );
        let survivor = make_trashed_entry(
            &trash_root,
            "survivor.txt",
            &PathBuf::from("/wherever/survivor.txt"),
            b"still here",
        );

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_purge(
            &*fs,
            std::slice::from_ref(&doomed),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run_purge(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!doomed.content_path.exists());
        assert!(!doomed.info_path.exists());
        assert!(
            survivor.content_path.exists(),
            "an entry not passed to plan_trash_purge must survive untouched"
        );
        assert!(survivor.info_path.exists());
    }

    /// A directory trashed as a whole is purged with `RemoveMode::
    /// Recursive`, not `File` (which would fail against a non-empty dir).
    #[tokio::test]
    async fn purge_removes_a_trashed_directorys_entire_subtree() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        std::fs::create_dir_all(trash_root.join("files/adir/sub")).unwrap();
        std::fs::create_dir_all(trash_root.join("info")).unwrap();
        std::fs::write(trash_root.join("files/adir/sub/inside.txt"), b"x").unwrap();
        std::fs::write(
            trash_root.join("info/adir.trashinfo"),
            "[Trash Info]\nPath=/wherever/adir\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        let entry = TrashEntry {
            content_path: trash_root.join("files/adir"),
            info_path: trash_root.join("info/adir.trashinfo"),
            original_path: PathBuf::from("/wherever/adir"),
            deleted_at: SystemTime::UNIX_EPOCH,
        };

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_purge(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(matches!(
            plan.steps[0],
            Step::Remove {
                mode: RemoveMode::Recursive,
                ..
            }
        ));

        let report = run_purge(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!entry.content_path.exists());
        assert!(!entry.info_path.exists());
    }

    /// An entry whose content is already gone by plan time (purged
    /// concurrently, e.g.) is silently skipped for that half rather than
    /// erroring the whole job -- mirrors `plan_permanent_delete`'s own
    /// "already gone is success" convention.
    #[tokio::test]
    async fn purge_silently_skips_an_entry_whose_content_already_vanished() {
        let dir = TempDir::new().unwrap();
        let trash_root = dir.path().join("Trash");
        let state = TempDir::new().unwrap();
        let entry = make_trashed_entry(
            &trash_root,
            "gone.txt",
            &PathBuf::from("/wherever/gone.txt"),
            b"x",
        );
        std::fs::remove_file(&entry.content_path).unwrap();

        let fs = local_fs();
        let cancel = CancelToken::new();
        let plan = plan_trash_purge(
            &*fs,
            std::slice::from_ref(&entry),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        // Only the sidecar Remove -- the content Remove was never emitted.
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind(), StepKind::Remove);

        let report = run_purge(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!entry.info_path.exists());
    }
}
