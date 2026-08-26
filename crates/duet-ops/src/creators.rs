// SPDX-License-Identifier: MIT
//! T-5.2.7's four small "create one thing" planners: [`plan_mkdir`] (F7),
//! [`plan_rename_in_place`] (Shift+F6), [`plan_symlink`], and
//! [`plan_hardlink`].
//!
//! # Why these are a module and not four one-liners scattered around
//!
//! Everything here produces a [`Plan`] of one or a handful of steps, with
//! no walk, no totals worth computing, and no conflict pre-check —
//! deliberately the smallest planners in the crate. They still go through
//! the plan → journal → execute pipeline rather than calling
//! [`duet_vfs::FileSystem`] directly from the UI, for three reasons that
//! apply just as much to a single `mkdir` as to a 100k-file copy:
//!
//! - **The UI thread never blocks on a syscall** (NFR-01…NFR-06, and
//!   `duet_vfs::local::guard`'s hard assertion). A job runs on the ops
//!   executor's own workers; a direct call from a click handler would not.
//! - **Every mutation is journaled** (FR-OPS-07). "Create a directory" is
//!   a real, undo-relevant change to the user's filesystem, and a crash
//!   between intent and completion should leave the same evidence trail a
//!   copy does.
//! - **Conflicts resolve through one engine.** A name collision on rename
//!   or symlink creation gets the same seven-policy FR-OPS-04 treatment as
//!   a colliding copy, including the interactive prompt, rather than a
//!   second, parallel, subtly-different implementation living in the
//!   dialog code.
//!
//! # Conventions borrowed rather than invented
//!
//! - **"Already there" is success, not failure.** [`plan_mkdir`] returns a
//!   valid, empty-steps `Plan` when its target already exists as a
//!   directory — the mirror image of [`crate::plan_delete`]'s own
//!   "already gone is success" short-circuit for a missing delete target.
//! - **Collisions are the executor's problem, not the planner's.**
//!   [`plan_rename_in_place`] emits `conflict: None` exactly as
//!   [`crate::plan_move`] does for an ordinary same-device rename; see
//!   [`Step`]'s own doc comment for why a plan-time pre-check would be
//!   re-verified at execution time anyway.
//! - **Fail fast when the answer is already knowable.**
//!   [`plan_hardlink`] rejects a directory source at plan time instead of
//!   letting a doomed job reach the executor, since no mainstream Linux
//!   filesystem permits it (see `duet_vfs::FileSystem::link`'s own doc
//!   comment).
//!
//! # `PlannerError`: reused variants, no new ones
//!
//! A disclosed judgment call, since two of these functions reject inputs
//! that no existing planner had to: both rejections fit an existing
//! [`PlannerError`] variant honestly, so none was added.
//!
//! - An unusable `new_name` in [`plan_rename_in_place`] becomes
//!   [`PlannerError::NoFileName`]. This looks like a stretch from the
//!   variant's name alone, but it is exactly the precedent
//!   [`crate::plan_copy`]/[`crate::plan_move`] already set: both map a
//!   failed `VPath::join` (i.e. `PathParseError::InvalidComponent`) onto
//!   `NoFileName`, which is precisely the failure an empty or
//!   separator-bearing name produces here. The variant is in practice the
//!   crate's "this path component is unusable" error, not narrowly "the
//!   source was a mount root".
//! - A directory source in [`plan_hardlink`] becomes
//!   [`PlannerError::Vfs`] carrying an `ErrorKind::Permission` error at
//!   `source` — not a fabrication, but the same classification the VFS
//!   layer itself documents for this case (`FileSystem::link`'s error
//!   table: "`Permission` — ... the backend refuses to hardlink `source`'s
//!   kind of entry (e.g. a directory)"). Pre-checking here changes *when*
//!   the caller learns this, not *what* they learn.
//!
//! The alternative — new `InvalidName`/`NotLinkable` variants — would read
//! marginally better at the call site but would break every existing
//! exhaustive match on `PlannerError` outside this crate for no behavioural
//! gain.

use duet_types::{ErrorKind, VPath, VfsError};
use duet_vfs::FileSystem;

use crate::plan::{Plan, PlanOptions};
use crate::planner::PlannerError;
use crate::step::Step;

/// Builds the [`Plan`] for F7 "create directory", accepting nested path
/// segments so a whole tree (`a/b/c`) is created in one job, as Total
/// Commander does.
///
/// Walks up from `dest` through its ancestors, `stat`-ing each level until
/// it finds one that already exists, then emits one [`Step::CreateDir`] per
/// missing level in creation order (shallowest first) — necessary because
/// `FileSystem::create_dir` is non-recursive by design (it mirrors
/// `mkdirat`, not `mkdir -p`; see its own doc comment on why recursion is
/// the caller's decision to make).
///
/// # Ordering is the whole dependency mechanism here
///
/// [`Step::CreateDir`] has no `depends_on` field, and doesn't need one:
/// every `CreateDir` is a *barrier* step in [`crate::execute`]'s
/// concurrency model — it runs alone, after everything before it has fully
/// completed, and nothing after it starts until it finishes. Position in
/// `Plan::steps` therefore already guarantees `a` exists before `a/b` is
/// attempted. This matches how [`crate::plan_copy`] orders its own chain of
/// ancestor `CreateDir` steps for a deeply-nested destination (discovery
/// order, no gating field), rather than inventing a second convention.
///
/// If a level's creation does fail, the levels below it fail too, each with
/// its own honest `NotFound` [`crate::StepFailure`] — noisier than a single
/// gated skip would be, but it is the behaviour `plan_copy` already
/// produces for the identical situation.
///
/// # Errors
/// - [`PlannerError::Vfs`] — a `stat` other than "not found" failed (a
///   permission-denied ancestor, say). "Not found" is not an error: it is
///   the signal that this level needs creating.
///
/// A `dest` that already exists *as a directory* is not an error either —
/// it produces a valid `Plan` with zero steps (see the module doc
/// comment). A `dest` that exists as a *non*-directory does emit a
/// `CreateDir` step, so the executor's conflict engine (not this planner)
/// decides what to do about the entry in the way.
pub async fn plan_mkdir(fs: &dyn FileSystem, dest: &VPath) -> Result<Plan, PlannerError> {
    let mut missing: Vec<VPath> = Vec::new();
    let mut cursor = dest.clone();
    loop {
        match fs.stat(&cursor, false).await {
            Ok(meta) => {
                // `missing.is_empty()` means this first existing level is
                // `dest` itself. A directory there is a no-op; anything
                // else is a genuine conflict for the executor to resolve.
                if !meta.is_dir() && missing.is_empty() {
                    missing.push(cursor.clone());
                }
                break;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                missing.push(cursor.clone());
                match cursor.parent() {
                    Some(parent) => cursor = parent,
                    // Walked to the mount root without finding anything
                    // that exists. Nothing sane is left to check.
                    None => break,
                }
            }
            Err(e) => return Err(PlannerError::Vfs(e)),
        }
    }

    // Collected deepest-first; the executor needs shallowest-first.
    missing.reverse();
    let steps = missing
        .into_iter()
        .map(|dest| Step::CreateDir {
            dest,
            // `None` = the backend's own default mode
            // (`duet_vfs::Mode::DEFAULT_DIR`, umask-respecting). F7 has no
            // UI for picking permission bits, and inventing an explicit
            // mode here would bypass the user's umask.
            mode: None,
        })
        .collect();
    Ok(Plan::new(steps, PlanOptions::default()))
}

/// Builds the [`Plan`] for Shift+F6 "rename in place": `source` keeps its
/// parent directory and takes `new_name` as its final path component.
///
/// Pure and synchronous — nothing here needs a [`FileSystem`] or an
/// `.await`. There is deliberately no existence pre-check on either side:
/// [`Step::Rename`] carries its own `conflict` field and the executor
/// re-checks and resolves a collision at execution time, exactly as
/// [`crate::plan_move`] leaves it to. `conflict` is emitted as `None`
/// (matching `plan_move`'s own same-device rename), so resolution falls
/// through the usual tiers, starting with `options.default_conflict`.
///
/// # Errors
/// [`PlannerError::NoFileName`] (see the module doc comment for why that
/// variant) when the rename target is not a single usable path component:
/// - `new_name` is empty, `.`, `..`, or contains a NUL byte;
/// - `new_name` contains `/` — a rename may change an entry's *name*, not
///   its *location*; moving it elsewhere is [`crate::plan_move`]'s job;
/// - `source` is a mount root and so has no parent to rename within.
pub fn plan_rename_in_place(
    source: &VPath,
    new_name: &str,
    options: PlanOptions,
) -> Result<Plan, PlannerError> {
    let reject = || PlannerError::NoFileName(source.clone());
    // `VPath::join` already rejects "", ".", "/"-bearing, and NUL-bearing
    // components; ".." is the one it accepts (`UnixPathBuf` keeps `..`
    // literally rather than resolving it — see its own doc comment) and
    // that this operation must not.
    if new_name == ".." {
        return Err(reject());
    }
    let parent = source.parent().ok_or_else(reject)?;
    let dest = parent.join(new_name).map_err(|_| reject())?;
    Ok(Plan::new(
        vec![Step::Rename {
            source: source.clone(),
            dest,
            conflict: None,
        }],
        options,
    ))
}

/// Builds the [`Plan`] for "create a symlink at `link_path` pointing at
/// `target`" — a single [`Step::Symlink`].
///
/// Pure, synchronous, and infallible. There is nothing meaningful to
/// validate at plan time: `target` is stored verbatim in the link and is
/// never resolved, so a relative target, a dangling one, or one naming a
/// path outside this backend entirely are all legitimate (see
/// `duet_vfs::FileSystem::symlink`'s own doc comment). `link_path`'s
/// availability is likewise left to the executor's conflict engine, same
/// as every other creating step.
///
/// The returned `Plan` carries [`PlanOptions::default`]
/// (`default_conflict: Skip` — never clobber silently). A caller wanting a
/// different policy for a `link_path` collision sets `plan.options` before
/// enqueuing; the field is public precisely so a single-step plan doesn't
/// need a builder.
pub fn plan_symlink(target: impl Into<String>, link_path: VPath) -> Plan {
    Plan::new(
        vec![Step::Symlink {
            target: target.into(),
            link_path,
            depends_on: None,
        }],
        PlanOptions::default(),
    )
}

/// Builds the [`Plan`] for "create a hardlink at `dest` pointing at
/// `source`'s inode" — a single [`Step::Link`], the standalone
/// user-invoked counterpart to T-5.1.7's hardlink-graph dedup use of the
/// same step.
///
/// `depends_on` is `None`: unlike the dedup case, this links against an
/// already-existing, unrelated file rather than a destination this same job
/// is responsible for producing first.
///
/// # Errors
/// - [`PlannerError::Vfs`] — `source` could not be `stat`ed (it doesn't
///   exist, its parent isn't searchable, ...), or `source` is a directory.
///   The directory rejection is this function's one real piece of work:
///   hardlinking a directory is disallowed on every mainstream Linux
///   filesystem, so there is no point journaling a job that cannot
///   possibly succeed. See the module doc comment for why this reuses
///   `Vfs`/`ErrorKind::Permission` rather than adding a variant.
pub async fn plan_hardlink(
    fs: &dyn FileSystem,
    source: &VPath,
    dest: &VPath,
) -> Result<Plan, PlannerError> {
    let meta = fs.stat(source, false).await.map_err(PlannerError::Vfs)?;
    if meta.kind.is_dir() {
        return Err(PlannerError::Vfs(Box::new(
            VfsError::new(
                ErrorKind::Permission,
                "cannot create a hardlink to a directory -- no mainstream Linux filesystem \
                 permits it; use a symlink instead",
            )
            .with_path(source.clone()),
        )));
    }
    Ok(Plan::new(
        vec![Step::Link {
            source: source.clone(),
            dest: dest.clone(),
            depends_on: None,
        }],
        PlanOptions::default(),
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Arc;

    use duet_types::UnixPathBuf;
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::conflict::ConflictPolicy;
    use crate::step::StepKind;

    fn vpath_for(p: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(p.to_str().unwrap()).unwrap())
    }

    /// Runs a `Plan` all the way through the real executor against a real
    /// `LocalFs` and a real on-disk journal -- mirrors `deleter.rs`'s own
    /// `run` helper, so every test below asserts the actual filesystem
    /// outcome, not merely the shape of the emitted steps.
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

    // -- plan_mkdir ----------------------------------------------------

    #[tokio::test]
    async fn mkdir_creates_a_single_missing_directory() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let target = dir.path().join("newdir");

        let fs = local_fs();
        let plan = plan_mkdir(&*fs, &vpath_for(&target)).await.unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.totals.dirs, 1);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(target.is_dir());
    }

    /// T-5.2.7's own AC clause: F7 "accepts nested path segments to create
    /// a tree in one go, as TC does."
    #[tokio::test]
    async fn mkdir_creates_a_three_level_nested_path_in_one_job() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let target = dir.path().join("a/b/c");

        let fs = local_fs();
        let plan = plan_mkdir(&*fs, &vpath_for(&target)).await.unwrap();

        assert_eq!(
            plan.steps.len(),
            3,
            "one CreateDir per missing level: a, a/b, a/b/c"
        );
        assert!(plan.steps.iter().all(|s| s.kind() == StepKind::CreateDir));
        // Shallowest first -- the executor runs CreateDir steps in order
        // as barrier steps, which is the whole ordering guarantee here.
        let dests: Vec<String> = plan
            .steps
            .iter()
            .map(|s| match s {
                Step::CreateDir { dest, .. } => dest.inner().as_str().to_string(),
                other => panic!("expected CreateDir, got {other:?}"),
            })
            .collect();
        assert_eq!(
            dests,
            vec![
                dir.path().join("a").to_str().unwrap().to_string(),
                dir.path().join("a/b").to_str().unwrap().to_string(),
                dir.path().join("a/b/c").to_str().unwrap().to_string(),
            ]
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(target.is_dir());
    }

    #[tokio::test]
    async fn mkdir_is_an_empty_plan_when_the_target_already_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("already")).unwrap();

        let fs = local_fs();
        let plan = plan_mkdir(&*fs, &vpath_for(&dir.path().join("already")))
            .await
            .unwrap();

        assert!(
            plan.steps.is_empty(),
            "an already-existing directory is success with nothing to do, not an error"
        );
    }

    #[tokio::test]
    async fn mkdir_creates_only_the_missing_suffix() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let target = dir.path().join("a/b/c");

        let fs = local_fs();
        let plan = plan_mkdir(&*fs, &vpath_for(&target)).await.unwrap();
        assert_eq!(
            plan.steps.len(),
            2,
            "`a` already exists -- only `a/b` and `a/b/c` need creating"
        );

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(target.is_dir());
    }

    // -- plan_rename_in_place ------------------------------------------

    #[tokio::test]
    async fn rename_in_place_renames_a_file() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.txt"), b"contents").unwrap();

        let fs = local_fs();
        let plan = plan_rename_in_place(
            &vpath_for(&dir.path().join("old.txt")),
            "new.txt",
            PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind(), StepKind::Rename);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "contents"
        );
    }

    #[tokio::test]
    async fn rename_in_place_renames_a_directory() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("olddir")).unwrap();
        std::fs::write(dir.path().join("olddir/inside.txt"), b"x").unwrap();

        let fs = local_fs();
        let plan = plan_rename_in_place(
            &vpath_for(&dir.path().join("olddir")),
            "newdir",
            PlanOptions::default(),
        )
        .unwrap();

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!dir.path().join("olddir").exists());
        assert!(
            dir.path().join("newdir/inside.txt").exists(),
            "renaming a directory must carry its contents with it"
        );
    }

    #[test]
    fn rename_in_place_rejects_an_empty_name() {
        let err = plan_rename_in_place(
            &VPath::local(UnixPathBuf::new("/a/b.txt").unwrap()),
            "",
            PlanOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PlannerError::NoFileName(_)), "{err:?}");
    }

    #[test]
    fn rename_in_place_rejects_a_name_containing_a_separator() {
        // A rename changes a name, never a location -- `plan_move` is what
        // relocates an entry.
        let err = plan_rename_in_place(
            &VPath::local(UnixPathBuf::new("/a/b.txt").unwrap()),
            "sub/c.txt",
            PlanOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PlannerError::NoFileName(_)), "{err:?}");
    }

    #[test]
    fn rename_in_place_rejects_dot_and_dotdot() {
        for name in [".", ".."] {
            let err = plan_rename_in_place(
                &VPath::local(UnixPathBuf::new("/a/b.txt").unwrap()),
                name,
                PlanOptions::default(),
            )
            .unwrap_err();
            assert!(
                matches!(err, PlannerError::NoFileName(_)),
                "{name}: {err:?}"
            );
        }
    }

    // -- plan_symlink --------------------------------------------------

    #[tokio::test]
    async fn symlink_creates_a_dangling_relative_link() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let link = dir.path().join("pointer");

        let fs = local_fs();
        let plan = plan_symlink("../nonexistent-sibling", vpath_for(&link));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind(), StepKind::Symlink);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../nonexistent-sibling"),
            "the target string must survive plan -> journal -> execute untouched"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink_metadata (lstat) must report a symlink; metadata() would follow it"
        );
        assert!(
            std::fs::metadata(&link).is_err(),
            "sanity check: following this deliberately dangling link must fail"
        );
    }

    /// A `Plan` that can't round-trip through serde would silently break
    /// crash recovery for this one step kind, since the journal persists
    /// plans verbatim (`JournalRecord::JobStarted`).
    #[test]
    fn a_symlink_plan_round_trips_through_the_journal_wire_format() {
        let plan = plan_symlink(
            "../elsewhere/target",
            VPath::local(UnixPathBuf::new("/a/link").unwrap()),
        );
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
        assert!(json.contains("Symlink"));
        assert!(json.contains("../elsewhere/target"));
    }

    #[tokio::test]
    async fn symlink_skips_an_occupied_link_path_under_the_skip_policy() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let link = dir.path().join("taken");
        std::fs::write(&link, b"i was here first").unwrap();

        let fs = local_fs();
        let plan = plan_symlink("somewhere", vpath_for(&link));
        assert_eq!(plan.options.default_conflict, ConflictPolicy::Skip);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "i was here first",
            "Skip must leave the occupying entry completely untouched"
        );
    }

    #[tokio::test]
    async fn symlink_replaces_an_occupied_link_path_under_the_overwrite_policy() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let link = dir.path().join("taken");
        std::fs::write(&link, b"i was here first").unwrap();

        let fs = local_fs();
        let mut plan = plan_symlink("somewhere", vpath_for(&link));
        plan.options.default_conflict = ConflictPolicy::Overwrite;

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("somewhere"));
    }

    // -- plan_hardlink -------------------------------------------------

    #[tokio::test]
    async fn hardlink_creates_a_second_name_for_the_same_inode() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = dir.path().join("a.txt");
        let dest = dir.path().join("b.txt");
        std::fs::write(&source, b"shared content").unwrap();

        let fs = local_fs();
        let plan = plan_hardlink(&*fs, &vpath_for(&source), &vpath_for(&dest))
            .await
            .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind(), StepKind::Link);

        let report = run(fs, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let source_meta = std::fs::metadata(&source).unwrap();
        let dest_meta = std::fs::metadata(&dest).unwrap();
        assert_eq!(
            source_meta.ino(),
            dest_meta.ino(),
            "both names must resolve to the same inode"
        );
        assert_eq!(source_meta.nlink(), 2);

        // Writing through either name must be visible through the other.
        std::fs::write(&source, b"changed").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "changed");
    }

    #[tokio::test]
    async fn hardlink_rejects_a_directory_source_at_plan_time() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("adir");
        std::fs::create_dir(&source).unwrap();
        let dest = dir.path().join("alias");

        let fs = local_fs();
        let err = plan_hardlink(&*fs, &vpath_for(&source), &vpath_for(&dest))
            .await
            .unwrap_err();

        match err {
            PlannerError::Vfs(e) => {
                assert_eq!(e.kind(), ErrorKind::Permission);
                assert_eq!(e.path(), Some(&vpath_for(&source)));
            }
            other => panic!("expected a Vfs rejection, got {other:?}"),
        }
        assert!(
            !dest.exists(),
            "the job must never have reached the executor"
        );
    }

    #[tokio::test]
    async fn hardlink_reports_a_missing_source_at_plan_time() {
        let dir = TempDir::new().unwrap();
        let fs = local_fs();
        let err = plan_hardlink(
            &*fs,
            &vpath_for(&dir.path().join("missing.txt")),
            &vpath_for(&dir.path().join("alias")),
        )
        .await
        .unwrap_err();
        match err {
            PlannerError::Vfs(e) => assert_eq!(e.kind(), ErrorKind::NotFound),
            other => panic!("expected a Vfs rejection, got {other:?}"),
        }
    }
}
