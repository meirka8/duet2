// SPDX-License-Identifier: MIT
//! [`plan_copy`] — T-5.1.1's async, cancellable source walk (design.md
//! §9.3: "A job is a `Plan`... produced by walking the source set. Planning
//! is itself async and cancellable and produces the totals (files, bytes)
//! that make honest progress possible.").
//!
//! T-2.3.1 already delivered [`Plan`]/[`Step`]'s data shape (see
//! [`crate::plan`]'s own doc comment); this module is the walk that
//! actually produces one, by descending through [`duet_vfs::FileSystem`]
//! (never `std::fs` directly -- see this crate's `Cargo.toml` for why).
//!
//! # Scope cuts made here, disclosed rather than silently assumed
//!
//! T-5.1.1's own AC ("planning 100k files completes in ≤ 2s... a plan is
//! serialisable") doesn't require everything design.md §9.3's "Planning"
//! paragraph mentions in the same breath -- some of that prose maps to
//! later, separately-scoped tasks:
//!
//! - **Always [`Step::CopyFile`], never [`Step::Reflink`].** Deciding
//!   *whether* a reflink applies (same filesystem, backend advertises
//!   `Caps::REFLINK`) is the copy-strategy ladder's job (T-5.1.4, design.md
//!   §9.3's "Copy strategy ladder" section, its own dedicated task) --
//!   folding that probe into the walk here would mean a second `stat`-class
//!   call per destination purely to pick a `Step` variant the executor
//!   could just as easily reinterpret at execution time from `Caps` it
//!   already has to consult anyway.
//! - **No hardlink graph.** Design.md §9.3 describes `HashMap<(dev, ino),
//!   VPath>` deduplication in the same "Planning" paragraph, but
//!   `documentation/task.md` gives it its own task, T-5.1.7 ("Hardlink
//!   graph preservation within a job"), depending on T-5.1.4 rather than
//!   this one -- so it's out of scope here, not silently dropped.
//! - **No destination conflict pre-check**, so every emitted step's
//!   `conflict` field is `None`. [`Step`]'s own doc comment establishes
//!   this is a safe, well-defined state ("the executor still has to
//!   re-check immediately before acting" regardless of what the planner
//!   found), and T-5.1.9 ("Conflict resolution engine") is the task that
//!   actually owns interactive conflict handling. Adding a `stat` per
//!   destination here would double this walk's I/O for a signal the
//!   executor already has to reverify at execution time anyway -- exactly
//!   the kind of budget risk that would jeopardize the ≤2s/100k-files AC
//!   for a result nothing downstream can trust unconditionally.
//! - **Symlinks (and fifos/sockets/device nodes) are excluded from the
//!   plan.** [`Step`] has no "recreate a symlink pointing at X" variant --
//!   folding a symlink into `Step::CopyFile` would silently copy the
//!   *target's content* instead of recreating the link, which is a
//!   behaviour change worth its own task and Step variant, not something
//!   to smuggle in here.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use duet_types::{EntryKind, MetaPatch, Metadata, VPath, VfsError};
use duet_vfs::{FileSystem, ListFields, ListOpts};
use futures_util::StreamExt;

use crate::plan::{Plan, PlanOptions};
use crate::step::Step;

/// A cheaply cloneable flag a caller uses to ask an in-progress
/// [`plan_copy`] walk to stop.
///
/// Mirrors `duet-index`'s `DirSizeService`/`SizeHandle` cancellation shape
/// (`Arc<AtomicBool>`, polled between syscalls, "stops within a handful of
/// syscalls of the request" -- see `duet_index::size_service`'s own doc
/// comment) rather than introducing `tokio-util`'s `CancellationToken`: no
/// crate in this workspace depends on `tokio-util` directly today (it's
/// present only transitively), and a plain atomic flag is simpler and
/// already-precedented for exactly this "cooperative, checked-between-
/// -syscalls" cancellation style. [`plan_copy`] checks it before every
/// `read_dir` chunk and before every top-level source, not just once at
/// the start -- the same promptness contract, adapted from a dedicated
/// walker thread to an `.await`-driven walk that shares whatever
/// executor/worker the caller runs it on.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Why [`plan_copy`] didn't produce a [`Plan`].
#[derive(Debug)]
pub enum PlannerError {
    /// `cancel`'s [`CancelToken::cancel`] was called before the walk
    /// finished. Not a `Vfs` error: the walk didn't fail, the caller asked
    /// it to stop.
    Cancelled,
    /// A `stat`/`read_dir` call against `fs` failed.
    Vfs(Box<VfsError>),
    /// One of `sources` has no file name component (e.g. a mount root) --
    /// there's no basename to join onto `dest_dir`, so this source can't
    /// be placed in the destination tree at all.
    NoFileName(VPath),
}

/// One directory still to descend into: `source` is where to read entries
/// from, `dest` is where the `CreateDir` step for `source` already pointed
/// -- new entries found under `source` are joined onto `dest`, mirroring
/// the source tree's shape at the destination.
struct PendingDir {
    source: VPath,
    dest: VPath,
}

/// A `CreateDir`/`CopyFile` step's index, destination, and source metadata,
/// recorded so its own `Step::SetMeta` follow-up can be deferred to the
/// very end of the plan (T-5.1.6) instead of interleaved right after it.
/// Two independent reasons this needs to be *every* entry, not just
/// directories:
/// - A directory's own timestamps would otherwise risk getting bumped
///   right back up the moment a child entry gets created inside it (every
///   `CopyFile`/`CreateDir` publish is itself a new directory entry, which
///   touches the parent directory's own mtime) -- deferring to the end,
///   after the whole subtree is populated, avoids that entirely.
/// - Interleaving a file's `SetMeta` immediately after its own `CopyFile`
///   would insert a barrier step between every pair of otherwise-
///   consecutive `CopyFile`s, destroying `execute`'s own copy-class
///   batching (T-5.1.3's "Concurrency model") -- a plan with `N` files
///   would run at effective concurrency 1 instead of `execute`'s actual
///   `concurrency` bound, since `SetMeta` is a barrier step, not
///   copy-class. Keeping every `CopyFile`/`CreateDir` step run
///   uninterrupted, with all `SetMeta` steps appended after the walk
///   finishes, preserves the exact batching shape this crate already had
///   before T-5.1.6 (order among the deferred `SetMeta` steps themselves
///   doesn't matter -- see [`metadata_to_patch`]'s own doc comment).
struct DeferredMeta {
    step_index: u32,
    dest: VPath,
    meta: Metadata,
}

/// The well-known xattr names POSIX ACLs and the SELinux label live under
/// on Linux -- duplicated from `duet_vfs::local::meta`'s own (private)
/// constants of the same name, per this codebase's existing convention of
/// duplicating small, load-bearing naming primitives per-crate rather than
/// exporting a backend implementation detail across the VFS abstraction
/// boundary (see e.g. `executor::partial_file_name`'s own precedent).
const ACL_ACCESS_XATTR: &str = "system.posix_acl_access";
const SELINUX_XATTR: &str = "security.selinux";

/// Builds the `MetaPatch` a follow-up [`Step::SetMeta`] step uses to bring
/// a freshly-created destination's metadata in line with `source_meta`
/// (T-5.1.6, design.md §9.3's metadata list: mode, xattrs, POSIX ACLs,
/// SELinux label, timestamps, ownership -- content itself is already
/// handled by whichever `CopyFile`/`Reflink`/`CreateDir` step this
/// follows). Ownership is always included when known (`uid`/`gid`), even
/// though it will commonly no-op under `EPERM` for an unprivileged
/// process -- `duet_vfs::local::meta::set_meta` already degrades that
/// gracefully (T-5.1.6's own fix there), which is exactly design.md's
/// "then ownership if privileged" framing: attempt, don't pre-check.
///
/// Disclosed scope cut: only adds `source_meta`'s own xattrs/ACL/SELinux
/// label to the destination's, never removes one already present on the
/// destination that source doesn't have (e.g. a POSIX default ACL a
/// freshly-created child inherited from its parent directory). Diffing
/// against the destination's own xattr set would need an extra `listxattr`
/// per entry on top of everything else this walk already does -- a real,
/// rare-in-practice gap, not silently assumed away.
pub(crate) fn metadata_to_patch(source_meta: &Metadata) -> MetaPatch {
    let mut set_xattrs = source_meta.xattrs.clone().unwrap_or_default();
    if let Some(acl) = &source_meta.acl {
        set_xattrs.insert(ACL_ACCESS_XATTR.to_string(), acl.clone());
    }
    if let Some(label) = &source_meta.selinux_label {
        set_xattrs.insert(SELINUX_XATTR.to_string(), label.clone().into_bytes());
    }
    MetaPatch {
        mode: source_meta.mode,
        uid: source_meta.uid,
        gid: source_meta.gid,
        modified: source_meta.modified,
        accessed: source_meta.accessed,
        set_xattrs,
        remove_xattrs: Vec::new(),
    }
}

/// Walks `sources` (each copied as a child of `dest_dir`, keeping its own
/// basename -- exactly what F5 does with a panel's current selection) and
/// returns the materialised, totalled [`Plan`] a `JobKind::Copy` job would
/// run. Async and cancellable per design.md §9.3 -- see [`CancelToken`].
///
/// Iterative (an explicit [`VecDeque`] work queue), not recursive: mirrors
/// `duet-vfs`'s own traversal helpers and `duet-index`'s size walker, both
/// deliberately non-recursive so a pathologically deep tree can't blow the
/// stack -- doubly relevant here since a naively-recursive `async fn`
/// calling itself doesn't even compile without manual boxing at every
/// level (an unbounded-size future), which an explicit queue sidesteps
/// entirely.
pub async fn plan_copy(
    fs: &dyn FileSystem,
    sources: &[VPath],
    dest_dir: &VPath,
    options: PlanOptions,
    cancel: &CancelToken,
) -> Result<Plan, PlannerError> {
    let mut steps = Vec::new();
    let mut queue: VecDeque<PendingDir> = VecDeque::new();
    // T-5.1.6: every entry's own `SetMeta` is deferred to the very end of
    // the plan -- see `DeferredMeta`'s own doc comment for why.
    let mut deferred_meta: Vec<DeferredMeta> = Vec::new();

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
        match meta.kind {
            EntryKind::Directory => {
                steps.push(Step::CreateDir {
                    dest: dest.clone(),
                    mode: meta.mode,
                });
                deferred_meta.push(DeferredMeta {
                    step_index: (steps.len() - 1) as u32,
                    dest: dest.clone(),
                    meta,
                });
                queue.push_back(PendingDir {
                    source: source.clone(),
                    dest,
                });
            }
            EntryKind::File => {
                steps.push(Step::CopyFile {
                    source: source.clone(),
                    dest: dest.clone(),
                    size: meta.size,
                    conflict: None,
                });
                deferred_meta.push(DeferredMeta {
                    step_index: (steps.len() - 1) as u32,
                    dest,
                    meta,
                });
            }
            // Symlinks, fifos, sockets, device nodes: see the module doc
            // comment's "Scope cuts" section.
            _ => {}
        }
    }

    // T-5.1.6: full metadata (xattrs/ACL/SELinux label included), not just
    // mode -- this walk is the only place cheap enough to fetch it once
    // per entry; the executor's own conflict re-check doesn't re-stat the
    // source at all.
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
                            meta: entry.metadata,
                        });
                        queue.push_back(PendingDir { source, dest });
                    }
                    EntryKind::File => {
                        steps.push(Step::CopyFile {
                            source,
                            dest: dest.clone(),
                            size: entry.metadata.size,
                            conflict: None,
                        });
                        deferred_meta.push(DeferredMeta {
                            step_index: (steps.len() - 1) as u32,
                            dest,
                            meta: entry.metadata,
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    // T-5.1.6: every `SetMeta` follow-up lands after the whole walk, once
    // -- see `DeferredMeta`'s own doc comment for why (directory mtimes,
    // and keeping every `CopyFile` run unbroken for `execute`'s batching).
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
    use std::time::{Duration, Instant};

    use duet_types::UnixPathBuf;
    use duet_vfs::LocalFs;
    use tempfile::TempDir;

    use super::*;
    use crate::step::StepKind;

    fn vpath_for(dir: &TempDir) -> VPath {
        VPath::local(UnixPathBuf::new(dir.path().to_str().unwrap()).unwrap())
    }

    #[tokio::test]
    async fn plans_a_three_file_directory_with_a_single_top_level_source() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(src.path().join("b.txt"), b"world!").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/c.txt"), b"x").unwrap();
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();

        let plan = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel)
            .await
            .expect("planning must succeed");

        // 1 CreateDir for `src` itself + 1 for `sub`; 3 CopyFile (a.txt,
        // b.txt, sub/c.txt).
        assert_eq!(plan.totals.dirs, 2);
        assert_eq!(plan.totals.files, 3);
        assert_eq!(plan.totals.bytes, 5 + 6 + 1);

        let src_name = src.path().file_name().unwrap().to_str().unwrap();
        let expected_root_dest = format!("{}/{src_name}", dst.path().to_str().unwrap());
        assert!(plan.steps.iter().any(|s| matches!(
            s,
            Step::CreateDir { dest, .. } if dest.inner().as_str() == expected_root_dest
        )));
        let expected_nested_dest = format!("{expected_root_dest}/sub/c.txt");
        assert!(plan.steps.iter().any(|s| matches!(
            s,
            Step::CopyFile { dest, size: 1, .. } if dest.inner().as_str() == expected_nested_dest
        )));
    }

    #[tokio::test]
    async fn a_single_top_level_file_source_produces_one_copyfile_step() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("only.txt"), b"12345").unwrap();
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src).join("only.txt").unwrap()];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();

        let plan = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel)
            .await
            .expect("planning must succeed");

        assert_eq!(
            plan.steps.len(),
            2,
            "CopyFile plus its own follow-up SetMeta (T-5.1.6)"
        );
        assert!(matches!(plan.steps[0].kind(), StepKind::CopyFile));
        assert!(matches!(plan.steps[1].kind(), StepKind::SetMeta));
        assert_eq!(
            plan.totals,
            crate::plan::PlanTotals {
                dirs: 0,
                files: 1,
                bytes: 5
            }
        );
    }

    #[tokio::test]
    async fn every_emitted_step_leaves_conflict_unresolved() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"x").unwrap();
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();

        let plan = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel)
            .await
            .unwrap();

        for step in &plan.steps {
            if let Step::CopyFile { conflict, .. } = step {
                assert_eq!(
                    *conflict, None,
                    "T-5.1.1's planner never pre-checks destinations for conflicts \
                     -- see the module doc comment's \"Scope cuts\" section"
                );
            }
        }
    }

    #[tokio::test]
    async fn symlinks_are_excluded_from_the_plan() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("real.txt"), b"content").unwrap();
        std::os::unix::fs::symlink(
            src.path().join("real.txt"),
            src.path().join("link_to_real.txt"),
        )
        .unwrap();
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();

        let plan = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel)
            .await
            .unwrap();

        // 1 CreateDir (src itself) + 1 CopyFile (real.txt only).
        assert_eq!(plan.totals.dirs, 1);
        assert_eq!(plan.totals.files, 1);
        assert!(
            !plan
                .steps
                .iter()
                .any(|s| matches!(s, Step::CopyFile { source, .. } if source.inner().as_str().ends_with("link_to_real.txt")))
        );
    }

    #[tokio::test]
    async fn cancelling_before_the_walk_starts_returns_cancelled_without_touching_the_filesystem() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"x").unwrap();
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel).await;
        assert!(matches!(result, Err(PlannerError::Cancelled)));
    }

    /// T-5.1.1's own AC, verbatim: "planning 100k files completes in ≤ 2 s
    /// and reports accurate totals." 100 subdirectories x 1,000 files each
    /// (mirroring `duet_index::size_service`'s own 100k-entry test tree
    /// shape) so the fixture itself is a known-affordable size in this
    /// codebase's existing test suite.
    #[tokio::test]
    async fn plans_100k_files_within_two_seconds_with_accurate_totals() {
        const SUBDIRS: usize = 100;
        const FILES_PER_SUBDIR: usize = 1_000;
        let src = TempDir::new().unwrap();
        for d in 0..SUBDIRS {
            let sub = src.path().join(format!("d{d}"));
            std::fs::create_dir(&sub).unwrap();
            for f in 0..FILES_PER_SUBDIR {
                std::fs::write(sub.join(format!("f{f}")), b"x").unwrap();
            }
        }
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();

        let start = Instant::now();
        let plan = plan_copy(&fs, &sources, &dest_dir, PlanOptions::default(), &cancel)
            .await
            .expect("planning must succeed");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "planning 100k files took {elapsed:?}, expected \u{2264} 2s"
        );
        // +1 for `src` itself, on top of the 100 subdirectories.
        assert_eq!(plan.totals.dirs, (SUBDIRS + 1) as u64);
        assert_eq!(plan.totals.files, (SUBDIRS * FILES_PER_SUBDIR) as u64);
        assert_eq!(plan.totals.bytes, (SUBDIRS * FILES_PER_SUBDIR) as u64);
    }

    /// Races a `plan_copy` walk against a concurrent `cancel()` call on a
    /// genuinely multi-threaded runtime -- see this crate's `Cargo.toml`
    /// comment on why `rt-multi-thread` matters here. Mirrors
    /// `duet_index::size_service`'s own
    /// `cancellation_stops_a_100k_file_walk_promptly` test shape and
    /// promptness bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_stops_a_large_walk_promptly() {
        const SUBDIRS: usize = 200;
        const FILES_PER_SUBDIR: usize = 1_000;
        let src = TempDir::new().unwrap();
        for d in 0..SUBDIRS {
            let sub = src.path().join(format!("d{d}"));
            std::fs::create_dir(&sub).unwrap();
            for f in 0..FILES_PER_SUBDIR {
                std::fs::write(sub.join(format!("f{f}")), b"x").unwrap();
            }
        }
        let dst = TempDir::new().unwrap();

        let fs = LocalFs;
        let sources = vec![vpath_for(&src)];
        let dest_dir = vpath_for(&dst);
        let cancel = CancelToken::new();
        let cancel_for_task = cancel.clone();

        let handle = tokio::spawn(async move {
            plan_copy(
                &fs,
                &sources,
                &dest_dir,
                PlanOptions::default(),
                &cancel_for_task,
            )
            .await
        });

        // A short sleep before cancelling: without it, `cancel()` can win
        // the race against the spawned task's very first poll (nothing
        // guarantees a freshly spawned task has started running yet), which
        // would only prove the walk checks the flag *before starting* --
        // not that it checks it *while a walk is genuinely in progress*.
        // ~900ms is roughly how long the uncancelled 200k-file walk itself
        // takes (measured: a 100k-file walk takes ~450ms), so 50ms lands
        // comfortably mid-walk with no risk of the walk having already
        // finished on its own.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let cancel_issued = Instant::now();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("walk never responded to cancellation")
            .expect("planner task panicked");
        let cancel_to_response = cancel_issued.elapsed();

        assert!(
            matches!(result, Err(PlannerError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert!(
            cancel_to_response < Duration::from_millis(500),
            "took {cancel_to_response:?} to respond to cancel() -- expected a prompt \
             stop, not a stall through the rest of the {SUBDIRS} x {FILES_PER_SUBDIR}-entry tree"
        );
    }
}
