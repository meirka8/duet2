// SPDX-License-Identifier: MIT
//! [`plan_move`] — T-5.1.5's async, cancellable move planner: same-device
//! entries become a single zero-cost [`Step::Rename`]; cross-device entries
//! become the `CopyFile`(+`Verify`)+`Remove` sequence [`Step::Rename`]'s own
//! doc comment already describes design.md §9.3 as requiring ("Never
//! unlink before the destination is durable").
//!
//! # Where the same-device-vs-cross-device decision is made, and why
//!
//! At plan time, not execution time — comparing [`duet_types::Metadata::
//! dev`] between each top-level source and `dest_dir` (already populated by
//! every `FileSystem::stat` call; no new trait method, nothing backend
//! -specific). Two reasons this beats "always emit `Rename`, let the
//! executor catch `EXDEV` and fall back dynamically":
//!
//! - `Plan::compute_totals` gives `Step::Rename` zero weight in
//!   `PlanTotals.bytes`/`files` (it's genuinely free — no content moves).
//!   If cross-device-ness weren't known until execution, a `Plan` full of
//!   `Rename` steps that actually turn into real copies at run time would
//!   report **zero planned bytes** for exactly the slow, large-file moves
//!   FR-OPS-03's honest-progress reporting matters most for.
//! - A `renameat2` call that's going to fail `EXDEV` has no partial-work
//!   cost worth saving by trying anyway (unlike, say, `copy_file_range`,
//!   which `duet_vfs::local::probe::accelerated_copy` tries reactively
//!   because there's no cheaper way to know in advance) — an up-front
//!   `stat`-based check (already needed for sizing/existence) is free.
//!
//! # The dependency-gating problem this module's output depends on
//!
//! A cross-device move's `Remove` of the source must never run if its
//! `CopyFile` (or, when `PlanOptions::verify` is set, the `Verify` after
//! it) failed or never ran — the executor's own step loop previously had
//! no mechanism to make one step's execution contingent on another's
//! outcome at all (it advanced through `Failed`/`Skipped` outcomes
//! unconditionally). [`Step::Remove`]/[`Step::Verify`]'s `depends_on`
//! field and the executor's own dependency-gating logic (see
//! `executor::dependency_block_reason`) are what this module's emitted
//! steps rely on for that guarantee — every `Remove`/`Verify` this
//! function emits for a cross-device entry sets `depends_on` to the exact
//! step index it needs to have succeeded (or at least not definitely
//! failed — see that function's own doc comment for why "not `Failed`"
//! rather than "`Succeeded`" is the correct gate).
//!
//! # Why this doesn't reuse [`crate::planner::plan_copy`] directly
//!
//! `plan_copy`'s `Step::CreateDir` steps carry only a `dest` path, not the
//! source directory they came from — there is no way to reconstruct "which
//! source directory does this `CreateDir` correspond to" from `plan_copy`'s
//! output alone, which this module needs in order to know which source
//! subdirectories to eventually `Remove` (deepest first, once everything
//! inside them is gone). Rather than changing `plan_copy`'s already-shipped
//! step shape to carry data only a move ever needs, this module walks the
//! cross-device subtree itself, tracking both sides of every directory
//! pair — a small, deliberate amount of duplication with `plan_copy`'s own
//! BFS structure, not an oversight.
//!
//! # Scope cuts made here, disclosed rather than silently assumed
//!
//! Mirroring [`crate::planner`]'s own precedent:
//!
//! - **Symlinks (and fifos/sockets/device nodes) are excluded**, exactly as
//!   `plan_copy` already excludes them and for the same reason — no `Step`
//!   variant exists for "recreate a symlink pointing at X," so a symlink
//!   inside a cross-device-moved directory is silently left behind in the
//!   source rather than mishandled.
//! - **`VerifyAlgorithm::SizeOnly`, never `Blake3`**, when `PlanOptions::
//!   verify` is set — `Blake3` execution is unimplemented in the executor
//!   (T-5.1.12's own scope) and this task's own AC only requires "never
//!   unlinks before the destination is fsync'd," which a successful
//!   `CopyFile` `Completion` already satisfies without any verification at
//!   all when `verify` is `false` (design.md §9.3: "verify (if enabled)").
//! - **No destination conflict pre-check** (every step's `conflict` field
//!   is `None`) and **no hardlink-graph interaction** — same reasoning
//!   `plan_copy`'s own module doc comment already gives for both.
//! - **A subtree is assumed to stay on one device throughout its own
//!   walk** — the same-device-vs-cross-device decision is made once per
//!   top-level source (from its own `stat`), not re-checked per descendant.
//!   A submount appearing partway down a source tree (rare) would be
//!   walked as if it were still on the top-level source's own device;
//!   `duet-vfs`'s own `FsProps`/`probe` module documents this same class of
//!   rare-edge-case simplification elsewhere (per-mount caching, not
//!   per-path).

use std::collections::VecDeque;

use duet_types::{EntryKind, Metadata, VPath};
use duet_vfs::{FileSystem, ListFields, ListOpts};
use futures_util::StreamExt;

use crate::plan::{Plan, PlanOptions};
use crate::planner::{CancelToken, PlannerError, metadata_to_patch};
use crate::step::{RemoveMode, Step, VerifyAlgorithm};

/// A cross-device top-level source: its own `dest` path (already resolved
/// via `dest_dir.join(basename)`) and `stat`-ed metadata, carried forward
/// from the initial same-device-vs-cross-device pass so the subtree walk
/// doesn't have to `stat` it a second time.
struct CrossDeviceEntry {
    source: VPath,
    dest: VPath,
    meta: Metadata,
}

/// One directory still to descend into during the cross-device subtree
/// walk — both sides of the pair, since (unlike [`crate::planner::
/// plan_copy`]) this module needs the source side back later to build
/// `Remove` steps for it. See the module doc comment's "Why this doesn't
/// reuse `plan_copy`" section.
struct PendingDir {
    source: VPath,
    dest: VPath,
}

/// A `CreateDir`/`CopyFile` step's index, destination, and source metadata,
/// recorded so its own `Step::SetMeta` follow-up can be deferred to the
/// very end of the plan (T-5.1.6) instead of interleaved right after it --
/// see `planner::DeferredMeta`'s own doc comment for the full reasoning
/// (directory mtimes, and keeping `execute`'s copy-class batching intact).
/// Duplicated here rather than shared with `planner`'s own private type
/// since the two walks otherwise share nothing else structurally.
struct DeferredMeta {
    step_index: u32,
    dest: VPath,
    meta: Metadata,
}

/// Walks `sources` (each moved as a child of `dest_dir`, keeping its own
/// basename — mirroring `plan_copy`'s own convention) and returns the
/// materialised, totalled [`Plan`] a `JobKind::Move` job would run. Async
/// and cancellable per design.md §9.3, via the same [`CancelToken`]
/// `plan_copy` uses.
///
/// See the module doc comment for the full design: same-device entries
/// become a single [`Step::Rename`]; cross-device entries become
/// `CreateDir`/`CopyFile` steps (batched together, for `execute`'s own
/// concurrency to parallelise exactly as it would for a plain copy) followed
/// by dependency-gated `Verify` (if `options.verify`) and `Remove` steps.
pub async fn plan_move(
    fs: &dyn FileSystem,
    sources: &[VPath],
    dest_dir: &VPath,
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let mut steps: Vec<Step> = Vec::new();
    let dest_dev = fs
        .stat(dest_dir, false)
        .await
        .map_err(PlannerError::Vfs)?
        .dev;

    // Phase 1: same-device vs cross-device, per top-level source.
    let mut cross_device: Vec<CrossDeviceEntry> = Vec::new();
    for source in sources {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }
        let name = source
            .inner()
            .file_name()
            .ok_or_else(|| PlannerError::NoFileName(source.clone()))?;
        let dest = dest_dir
            .join(name)
            .map_err(|_| PlannerError::NoFileName(source.clone()))?;
        let meta = fs.stat(source, false).await.map_err(PlannerError::Vfs)?;

        if !matches!(meta.kind, EntryKind::Directory | EntryKind::File) {
            continue; // symlinks etc -- see the module doc comment's scope cuts
        }

        let same_device = matches!((meta.dev, dest_dev), (Some(a), Some(b)) if a == b);
        if same_device {
            steps.push(Step::Rename {
                source: source.clone(),
                dest,
                conflict: None,
            });
        } else {
            cross_device.push(CrossDeviceEntry {
                source: source.clone(),
                dest,
                meta,
            });
        }
    }

    // Phase 2: walk every cross-device entry's subtree (BFS, mirroring
    // `plan_copy`), collecting `CopyFile` step indices (for phase 3),
    // source-side directory paths in discovery order (for phase 4), and
    // every entry's own metadata for a deferred `SetMeta` (T-5.1.6, phase
    // 5 below).
    let mut queue: VecDeque<PendingDir> = VecDeque::new();
    let mut copy_step_indices: Vec<usize> = Vec::new();
    let mut source_dirs: Vec<VPath> = Vec::new();
    let mut deferred_meta: Vec<DeferredMeta> = Vec::new();

    for entry in &cross_device {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }
        match entry.meta.kind {
            EntryKind::Directory => {
                steps.push(Step::CreateDir {
                    dest: entry.dest.clone(),
                    mode: entry.meta.mode,
                });
                deferred_meta.push(DeferredMeta {
                    step_index: (steps.len() - 1) as u32,
                    dest: entry.dest.clone(),
                    meta: entry.meta.clone(),
                });
                source_dirs.push(entry.source.clone());
                queue.push_back(PendingDir {
                    source: entry.source.clone(),
                    dest: entry.dest.clone(),
                });
            }
            EntryKind::File => {
                steps.push(Step::CopyFile {
                    source: entry.source.clone(),
                    dest: entry.dest.clone(),
                    size: entry.meta.size,
                    conflict: None,
                });
                copy_step_indices.push(steps.len() - 1);
                deferred_meta.push(DeferredMeta {
                    step_index: (steps.len() - 1) as u32,
                    dest: entry.dest.clone(),
                    meta: entry.meta.clone(),
                });
            }
            _ => unreachable!("filtered to Directory | File in phase 1"),
        }
    }

    // T-5.1.6: full metadata (xattrs/ACL/SELinux label included), not just
    // mode -- mirrors `plan_copy`'s own upgrade from `MODE`-only.
    let list_opts = ListOpts {
        fields: ListFields::all(),
        follow_symlinks: false,
    };
    while let Some(dir) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Err(PlannerError::Cancelled);
        }
        let mut chunks = fs.read_dir(&dir.source, list_opts);
        while let Some(chunk) = chunks.next().await {
            if cancel.is_cancelled() {
                return Err(PlannerError::Cancelled);
            }
            let entries = chunk.map_err(PlannerError::Vfs)?;
            for entry in entries {
                let source = dir
                    .source
                    .join(&entry.name)
                    .map_err(|_| PlannerError::NoFileName(dir.source.clone()))?;
                let dest = dir
                    .dest
                    .join(&entry.name)
                    .map_err(|_| PlannerError::NoFileName(dir.dest.clone()))?;
                match entry.metadata.kind {
                    EntryKind::Directory => {
                        steps.push(Step::CreateDir {
                            dest: dest.clone(),
                            mode: entry.metadata.mode,
                        });
                        deferred_meta.push(DeferredMeta {
                            step_index: (steps.len() - 1) as u32,
                            dest: dest.clone(),
                            meta: entry.metadata.clone(),
                        });
                        source_dirs.push(source.clone());
                        queue.push_back(PendingDir { source, dest });
                    }
                    EntryKind::File => {
                        steps.push(Step::CopyFile {
                            source,
                            dest: dest.clone(),
                            size: entry.metadata.size,
                            conflict: None,
                        });
                        copy_step_indices.push(steps.len() - 1);
                        deferred_meta.push(DeferredMeta {
                            step_index: (steps.len() - 1) as u32,
                            dest,
                            meta: entry.metadata,
                        });
                    }
                    _ => {} // symlinks etc -- see the module doc comment's scope cuts
                }
            }
        }
    }

    // Phase 3: for every CopyFile, append its (optional Verify, then)
    // Remove of the source -- each depends_on the step directly before it
    // in its own chain, per the module doc comment's dependency-gating
    // explanation.
    for &copy_idx in &copy_step_indices {
        let (source, dest) = match &steps[copy_idx] {
            Step::CopyFile { source, dest, .. } => (source.clone(), dest.clone()),
            _ => unreachable!("copy_step_indices only ever indexes CopyFile steps"),
        };
        let remove_depends_on = if options.verify {
            steps.push(Step::Verify {
                source: source.clone(),
                dest,
                algorithm: VerifyAlgorithm::SizeOnly,
                depends_on: Some(copy_idx as u32),
            });
            (steps.len() - 1) as u32
        } else {
            copy_idx as u32
        };
        steps.push(Step::Remove {
            target: source,
            mode: RemoveMode::File,
            depends_on: Some(remove_depends_on),
        });
    }

    // Phase 4: remove now-hopefully-empty source directories, deepest
    // first -- reversing BFS discovery order puts every depth-N directory
    // before any depth-(N-1) one, which is exactly "children before
    // parents." No `depends_on` needed: `executor::remove_step` already
    // treats `ENOTEMPTY` (a directory that still has something left
    // inside it, because a contained copy failed or was skipped) as a
    // self-explanatory `Skipped`, not an error.
    for dir_source in source_dirs.into_iter().rev() {
        steps.push(Step::Remove {
            target: dir_source,
            mode: RemoveMode::EmptyDir,
            depends_on: None,
        });
    }

    // Phase 5 (T-5.1.6): every cross-device entry's own `SetMeta` follow-up
    // lands last, after everything above -- see `planner::DeferredMeta`'s
    // own doc comment (directory mtimes, and keeping every `CopyFile` run
    // in phase 2 unbroken for `execute`'s copy-class batching). Order
    // relative to phase 3/4's `Verify`/`Remove` steps doesn't matter:
    // `SetMeta` only ever targets `dest`, never the source paths phase 3/4
    // touch.
    for entry in deferred_meta {
        steps.push(Step::SetMeta {
            target: entry.dest,
            patch: metadata_to_patch(&entry.meta),
            depends_on: Some(entry.step_index),
        });
    }

    Ok(Plan::new(steps, options))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use duet_types::{Caps, ErrorKind, MetaPatch, Result, UnixPathBuf, VfsError};
    use duet_vfs::{
        AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CopyOutcome, DirEntry, LocalFs, Mode,
        RemoveKind, RenameFlags, VolumeStats, WriteOpts,
    };
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::executor::{ExecutionControl, execute};
    use crate::job::{JobId, JobReport};
    use crate::journal::Journal;
    use crate::step::StepKind;

    fn vpath_for(dir: &Path) -> VPath {
        VPath::local(UnixPathBuf::new(dir.to_str().unwrap()).unwrap())
    }

    async fn run(fs: Arc<dyn FileSystem>, job_id: u64, plan: Plan, state_dir: &Path) -> JobReport {
        let journal = Journal::open(JobId(job_id), state_dir).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let control = ExecutionControl::new();
        execute(fs, JobId(job_id), plan, journal, 2, tx, control, None).await
    }

    /// A `FileSystem` test double wrapping a real [`LocalFs`], whose `stat`
    /// reports a fake, deliberately-different `dev` for any path under
    /// `foreign_prefix` -- lets `plan_move`'s tests exercise genuine
    /// cross-device planning/execution without needing a second real
    /// mounted filesystem available in the test environment. Every other
    /// method delegates straight through to the real backend, so the
    /// actual copy/remove/rename operations are entirely real.
    struct FakeDeviceFs {
        inner: LocalFs,
        foreign_prefix: String,
        /// Optional: `open_read` fails with this classification for any
        /// path whose name contains this substring -- used to inject a
        /// genuine copy failure for the "never removes an unverified
        /// source" test, mirroring T-5.1.4's own injection-test precedent.
        fail_read_containing: Option<(String, ErrorKind)>,
    }

    impl FakeDeviceFs {
        fn new(foreign_prefix: &Path) -> Self {
            FakeDeviceFs {
                inner: LocalFs,
                foreign_prefix: foreign_prefix.to_str().unwrap().to_string(),
                fail_read_containing: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl FileSystem for FakeDeviceFs {
        fn scheme(&self) -> &'static str {
            self.inner.scheme()
        }
        fn caps(&self) -> Caps {
            self.inner.caps()
        }
        fn read_dir(
            &self,
            p: &VPath,
            opts: ListOpts,
        ) -> futures_util::stream::BoxStream<'_, Result<Vec<DirEntry>>> {
            self.inner.read_dir(p, opts)
        }
        async fn stat(&self, p: &VPath, follow: bool) -> Result<Metadata> {
            let mut meta = self.inner.stat(p, follow).await?;
            if p.inner().as_str().starts_with(&self.foreign_prefix) {
                meta.dev = Some(u64::MAX);
            }
            Ok(meta)
        }
        async fn volume_stats(&self, p: &VPath) -> Result<VolumeStats> {
            self.inner.volume_stats(p).await
        }
        async fn open_read(&self, p: &VPath) -> Result<Box<dyn AsyncReadSeek>> {
            if let Some((needle, kind)) = &self.fail_read_containing
                && p.inner().as_str().contains(needle.as_str())
            {
                return Err(Box::new(
                    VfsError::new(*kind, "injected failure for testing").with_path(p.clone()),
                ));
            }
            self.inner.open_read(p).await
        }
        async fn open_write(&self, p: &VPath, o: WriteOpts) -> Result<Box<dyn AsyncWriteCommit>> {
            self.inner.open_write(p, o).await
        }
        async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()> {
            self.inner.create_dir(p, mode).await
        }
        async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()> {
            self.inner.remove(p, kind).await
        }
        async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()> {
            self.inner.rename(from, to, flags).await
        }
        async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()> {
            self.inner.set_meta(p, m).await
        }
        fn watch(&self, p: &VPath) -> Result<futures_util::stream::BoxStream<'_, ChangeEvent>> {
            self.inner.watch(p)
        }
        async fn server_side_copy(
            &self,
            _from: &VPath,
            _to: &VPath,
            _should_cancel: &(dyn Fn() -> bool + Send + Sync),
        ) -> Result<CopyOutcome> {
            // Force the naive fallback loop for every file in these tests
            // -- keeps behaviour deterministic regardless of whether the
            // real backend happens to support FICLONE/copy_file_range on
            // this machine, and is what makes `fail_read_containing`
            // actually reachable (server_side_copy never calls open_read).
            Ok(CopyOutcome::Unsupported)
        }
    }

    #[tokio::test]
    async fn same_device_directory_move_produces_a_single_rename_step_and_moves_everything() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), b"world").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let cancel = CancelToken::new();
        let plan = plan_move(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        assert_eq!(
            plan.steps.len(),
            1,
            "a same-device move must be exactly one step"
        );
        assert_eq!(plan.steps[0].kind(), StepKind::Rename);
        assert_eq!(
            plan.totals.bytes, 0,
            "Rename is documented as contributing 0 to totals"
        );

        let report = run(fs, 1, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!src.path().exists());
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("sub/b.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn cross_device_move_copies_then_removes_the_entire_source_tree() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), b"world").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(FakeDeviceFs::new(dst.path()));
        let cancel = CancelToken::new();
        let plan = plan_move(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        // 1 CreateDir (src itself) + 1 CreateDir (sub) + 2 CopyFile + 2
        // Remove(File) + 2 Remove(EmptyDir).
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind()).collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == StepKind::CopyFile).count(),
            2
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == StepKind::Remove).count(),
            4,
            "2 file removes + 2 directory removes"
        );
        assert!(
            !kinds.contains(&StepKind::Verify),
            "PlanOptions::default().verify is false -- no Verify steps expected"
        );

        let report = run(fs, 2, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            !src.path().exists(),
            "the whole source tree must be gone after a fully-successful cross-device move"
        );
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join(src_name).join("sub/b.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn cross_device_move_with_verify_uses_size_only_and_still_completes() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(FakeDeviceFs::new(dst.path()));
        let cancel = CancelToken::new();
        let plan = plan_move(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions {
                verify: true,
                ..PlanOptions::default()
            },
            &cancel,
        )
        .await
        .unwrap();

        let verify_steps: Vec<_> = plan
            .steps
            .iter()
            .filter(|s| s.kind() == StepKind::Verify)
            .collect();
        assert_eq!(verify_steps.len(), 1);
        assert!(matches!(
            verify_steps[0],
            Step::Verify {
                algorithm: VerifyAlgorithm::SizeOnly,
                ..
            }
        ));

        let report = run(fs, 3, plan, state.path()).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!src.path().join("a.txt").exists());
    }

    /// T-5.1.5's own AC, verbatim: "Cross-device move never unlinks before
    /// the destination is fsync'd; verified by injection test." Injects a
    /// real `CopyFile` failure (a read error on the source) and confirms
    /// the dependency-gated `Remove` never ran -- the source must survive.
    #[tokio::test]
    async fn cross_device_move_never_removes_source_when_copy_fails() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let mut fake = FakeDeviceFs::new(dst.path());
        fake.fail_read_containing = Some(("a.txt".to_string(), ErrorKind::Permission));
        let fs: Arc<dyn FileSystem> = Arc::new(fake);
        let cancel = CancelToken::new();
        let plan = plan_move(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let report = run(fs, 4, plan, state.path()).await;
        assert_eq!(
            report.errors.len(),
            1,
            "the injected read failure must surface"
        );
        assert_eq!(
            report.skipped.len(),
            3,
            "the dependency-gated file Remove, the dependency-gated file SetMeta \
             (T-5.1.6), AND the now-self-gated (still non-empty, ENOTEMPTY) parent \
             directory Remove must all be skipped"
        );
        assert!(
            src.path().join("a.txt").exists(),
            "the source must survive a failed copy -- never unlinked before the \
             destination is durable"
        );
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        assert!(!dst.path().join(src_name).join("a.txt").exists());
    }

    /// Simulates "the first run got interrupted after the copy phase but
    /// before the remove phase" deterministically (by executing only the
    /// plan's copy-phase prefix first), then re-runs the *full* plan --
    /// proving a resumed move still removes sources whose copy already
    /// durably succeeded, even though the second run's own re-attempted
    /// `CopyFile` step is `Skipped` (destination already exists), not
    /// `Succeeded`. This is exactly the scenario
    /// `dependency_block_reason`'s own doc comment explains gating on
    /// "not `Failed`" (rather than "`Succeeded`") is for.
    #[tokio::test]
    async fn resuming_after_a_partial_run_still_removes_sources_whose_copy_already_succeeded() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        let dst = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(FakeDeviceFs::new(dst.path()));
        let cancel = CancelToken::new();
        let plan = plan_move(
            &*fs,
            &[vpath_for(src.path())],
            &vpath_for(dst.path()),
            PlanOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        let copy_phase_end = plan
            .steps
            .iter()
            .position(|s| matches!(s.kind(), StepKind::Remove | StepKind::Verify))
            .expect("this plan has at least one Remove step");

        // Run 1: only the copy phase -- simulates a crash/cancel right
        // after the copy durably succeeded but before any Remove ran.
        let copy_only_plan = Plan::new(plan.steps[..copy_phase_end].to_vec(), plan.options);
        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        let first = run(Arc::clone(&fs), 5, copy_only_plan, state.path()).await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(
            src.path().join("a.txt").exists(),
            "sanity check: the source must still be present after only the copy phase ran"
        );
        assert!(dst.path().join(src_name).join("a.txt").exists());

        // Run 2: the full plan, re-attempting everything from scratch --
        // the CreateDir merges silently into the already-existing
        // directory (T-5.1.9's own fix: that's normal, not a conflict),
        // while the CopyFile step hits a real Conflict (already exists)
        // and is Skipped, not Succeeded.
        let second = run(Arc::clone(&fs), 6, plan, state.path()).await;
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert_eq!(
            second.skipped.len(),
            1,
            "the re-attempted CopyFile must be Skipped (Conflict), not Succeeded"
        );
        assert!(
            !src.path().join("a.txt").exists(),
            "the Remove must still have proceeded on the second run, even though its \
             dependency's outcome this time was Skipped rather than Succeeded"
        );
    }
}
