// SPDX-License-Identifier: MIT
//! `Step` — the atomic unit of work a [`Plan`](crate::Plan) is made of.
//!
//! design.md §9.3: "Steps are concrete: `CreateDir`, `CopyFile`, `Reflink`,
//! `Rename`, `Link`, `SetMeta`, `Remove`, `Verify`." Every field here is
//! plain, owned, serializable data — a `Step` describes *what* to do, never
//! *how* (no `FileSystem` handles, no open file descriptors), so a `Plan`
//! full of them can be journaled, replayed, and inspected by a recovery
//! reader that has no live VFS mount table at all (see [`crate::journal`]).

use duet_types::{MetaPatch, VPath};
use serde::{Deserialize, Serialize};

use crate::conflict::ConflictPolicy;

/// One step in a [`Plan`](crate::Plan)'s ordered step list.
///
/// Every variant that produces or replaces a destination path carries an
/// already-resolved `conflict: Option<ConflictPolicy>` — resolution happens
/// once, at plan-execution time (job-level default, then per-conflict user
/// answer, then "apply to all", per design.md §9.3's Conflicts section), not
/// re-decided per attempt. `None` means the planner found no pre-existing
/// destination at plan time (the common case); the executor still has to
/// re-check immediately before acting, since the world can change between
/// planning and execution — but it re-checks *whether* a conflict now
/// exists, not *how* to resolve one, which the plan already answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Step {
    /// Create a directory at `dest` (non-recursive — mirrors
    /// `FileSystem::create_dir`; the planner is responsible for ordering
    /// ancestor `CreateDir` steps before anything that writes inside them).
    CreateDir {
        dest: VPath,
        /// Permission bits for the new directory. `None` means "backend
        /// default" (see `duet_vfs::Mode::DEFAULT_DIR`); kept as a bare
        /// `u32` rather than depending on `duet-vfs`'s `Mode` type so this
        /// crate's plan data stays serializable and VFS-implementation
        /// agnostic, matching `duet_types::MetaPatch::mode`'s precedent.
        mode: Option<u32>,
    },
    /// Copy `source`'s content to `dest` via the general copy-strategy
    /// ladder (design.md §9.3: `copy_file_range` → sparse-aware buffered
    /// copy with `fadvise`). The planner emits [`Step::Reflink`] instead
    /// when it already knows a same-filesystem reflink applies; `CopyFile`
    /// is the fallback/general path, including cross-device copies.
    CopyFile {
        source: VPath,
        dest: VPath,
        /// Size at planning time, feeding `Plan`'s honest-progress totals
        /// (FR-OPS-03). The actual byte count copied may differ if the
        /// source changes underneath the job; the executor reports the
        /// discrepancy rather than silently trusting this number.
        size: u64,
        conflict: Option<ConflictPolicy>,
    },
    /// Copy-on-write reflink (`ioctl(FICLONE)`) — same filesystem, backend
    /// advertises `Caps::REFLINK`. If the reflink attempt itself fails at
    /// execution time (e.g. cross-subvolume on btrfs), the executor steps
    /// down the copy-strategy ladder for this one step rather than
    /// re-planning the whole job.
    Reflink {
        source: VPath,
        dest: VPath,
        size: u64,
        conflict: Option<ConflictPolicy>,
    },
    /// Move `source` to `dest`. Same-filesystem → `renameat2`; the terminal
    /// step of a cross-device move (copy → verify → fsync → unlink source)
    /// is represented as a `CopyFile`/`Reflink`/`Verify` sequence followed
    /// by a `Remove` of the source, *not* as a `Rename` step — `Rename`
    /// exists only for the case the backend can actually satisfy atomically
    /// (design.md §9.3: "Never unlink before the destination is durable").
    Rename {
        source: VPath,
        dest: VPath,
        conflict: Option<ConflictPolicy>,
    },
    /// Hardlink `dest` to `source` — either an explicit "create hardlink"
    /// user operation, or the second-and-later occurrence of an
    /// already-copied inode in this job's hardlink graph (design.md §9.3:
    /// `HashMap<(dev, ino), VPath>`), preserving link structure instead of
    /// duplicating the data.
    Link { source: VPath, dest: VPath },
    /// Apply a metadata patch to `target`. The *order* fields within `patch`
    /// are applied in (mode → xattrs/ACL/SELinux label → timestamps last,
    /// per design.md §9.3 — writing xattrs perturbs ctime) is an executor
    /// concern, not encoded in the data here.
    SetMeta { target: VPath, patch: MetaPatch },
    /// Remove `target`. Deletes are journaled *before* execution (design.md
    /// §9.3: "so the undo stack has something to work from for trash
    /// operations"), which is exactly what an `Intent` journal record
    /// already guarantees for every step kind — nothing `Remove`-specific
    /// is needed beyond picking [`RemoveMode`] correctly.
    ///
    /// `depends_on`: for the terminal `Remove` of a cross-device move (the
    /// source, once its copy is durable), the `step_index` of the
    /// `CopyFile`/`Reflink`/`Verify` step this removal is contingent on —
    /// `None` for a plain delete with no such prerequisite. See the
    /// executor's own dependency-gating doc comment (T-5.1.5) for exactly
    /// what "contingent on" means operationally: this is what keeps a
    /// cross-device move from ever unlinking a source whose destination
    /// copy failed, which nothing enforced before this field existed (the
    /// step loop previously advanced through `Failed`/`Skipped` outcomes
    /// unconditionally).
    Remove {
        target: VPath,
        mode: RemoveMode,
        depends_on: Option<u32>,
    },
    /// Verify `dest` against `source` (or against a recorded expected hash)
    /// by content hash — FR-OPS-08's optional post-copy verification, or
    /// the optional (`PlanOptions::verify`-gated — design.md §9.3: "verify
    /// (if enabled)") verify step of a cross-device move before the source
    /// is unlinked. Read-only: never mutates either path.
    ///
    /// `depends_on`: the `step_index` of the `CopyFile`/`Reflink` step
    /// that produced `dest`, so a copy that failed or was skipped never
    /// gets "verified" against stale/nonexistent content — same
    /// dependency-gating mechanism `Remove`'s own field documents.
    /// `None` for a verify with no such prerequisite (not currently
    /// emitted by any planner, but the field stays generally useful, not
    /// move-specific).
    Verify {
        source: VPath,
        dest: VPath,
        algorithm: VerifyAlgorithm,
        depends_on: Option<u32>,
    },
}

impl Step {
    /// The step's kind, independent of its field values — what the crash-
    /// safety table (`docs/crash-safety.md`, T-2.3.2) is organised by, and
    /// what `JournalRecord`/progress-reporting code matches on without
    /// having to destructure every variant's full field set.
    pub fn kind(&self) -> StepKind {
        match self {
            Step::CreateDir { .. } => StepKind::CreateDir,
            Step::CopyFile { .. } => StepKind::CopyFile,
            Step::Reflink { .. } => StepKind::Reflink,
            Step::Rename { .. } => StepKind::Rename,
            Step::Link { .. } => StepKind::Link,
            Step::SetMeta { .. } => StepKind::SetMeta,
            Step::Remove { .. } => StepKind::Remove,
            Step::Verify { .. } => StepKind::Verify,
        }
    }

    /// Planned byte count this step contributes to `Plan`'s totals, or `0`
    /// for steps with no content-transfer size of their own (everything
    /// but `CopyFile`/`Reflink`).
    pub fn planned_bytes(&self) -> u64 {
        match self {
            Step::CopyFile { size, .. } | Step::Reflink { size, .. } => *size,
            _ => 0,
        }
    }
}

/// [`Step`] without its field payload — a cheap, `Copy` tag for
/// classification (journal records, progress events, the crash-safety
/// table). See [`Step::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepKind {
    CreateDir,
    CopyFile,
    Reflink,
    Rename,
    Link,
    SetMeta,
    Remove,
    Verify,
}

/// Which removal semantics a [`Step::Remove`] uses.
///
/// A deliberately plain, serializable mirror of `duet_vfs::RemoveKind`
/// rather than a re-export of it: `duet-vfs`'s type isn't
/// `Serialize`/`Deserialize` (it has no reason to be, at the `FileSystem`
/// trait layer), and plan/journal data must be. The executor (T-5.1.x) maps
/// this to `duet_vfs::RemoveKind` at the one call site that actually invokes
/// `FileSystem::remove`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RemoveMode {
    /// A single file, symlink, or other non-directory node.
    File,
    /// A directory that must already be empty.
    EmptyDir,
    /// Recursively remove a directory and everything under it. The only
    /// mode whose interruption mid-way leaves genuinely partial state on
    /// disk (some descendants gone, others not) rather than an atomic
    /// all-or-nothing change — see `docs/crash-safety.md`.
    Recursive,
}

/// Which algorithm a [`Step::Verify`] uses to compare `source` and `dest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerifyAlgorithm {
    /// BLAKE3 content hash (FR-OPS-08's specified algorithm, T-5.1.12).
    Blake3,
    /// Cheap fallback: compare sizes only, no content read. Used where a
    /// full hash isn't warranted (e.g. verifying a reflink, which cannot
    /// have corrupted data in transit — design.md §9.3's `CopyOutcome`
    /// doc comment).
    SizeOnly,
}
