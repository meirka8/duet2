// SPDX-License-Identifier: MIT
//! `ConflictPolicy` and the conflict-resolution prompt data (FR-OPS-04).
//!
//! design.md §9.3: "Policy is a resolved enum per step, decided by:
//! job-level default → per-conflict user answer → 'apply to all' answers.
//! The prompt shows both sides (size, mtime, and a hash on request) and
//! offers the TC set: skip / overwrite / overwrite-if-older / overwrite-all
//! / rename / auto-rename / abort." FR-OPS-04 (design.md §6.3) gives the
//! canonical P0 requirement text with slightly different naming:
//! "skip, overwrite, overwrite-if-older, overwrite-if-different-size,
//! rename-target, auto-rename, and *apply to all*".
//!
//! [`ConflictPolicy`] below reconciles the two: it uses FR-OPS-04's more
//! precise variant names (`OverwriteIfDifferentSize` rather than the
//! ambiguous "overwrite-all", `RenameTarget` rather than bare "rename"),
//! and adds `Abort` from §9.3's TC-set enumeration, which FR-OPS-04's prose
//! doesn't list explicitly but which a job clearly needs (the user has to
//! be able to say "stop the whole job here"). "Apply to all" is modelled
//! separately as [`ConflictScope`] rather than as its own `ConflictPolicy`
//! variant, since it is orthogonal to *which* policy is chosen — any of the
//! seven policies can be scoped to "this conflict only" or "all remaining
//! conflicts in this job".

use duet_types::{Metadata, VPath};
use serde::{Deserialize, Serialize};

/// A conflict resolution policy — what to do when a `Step`'s destination
/// already exists. See the module doc comment for how this reconciles
/// design.md §9.3's TC-set prose with FR-OPS-04's canonical requirement
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// Leave the existing destination untouched; the source entry for this
    /// step is not copied/moved/linked.
    Skip,
    /// Replace the destination unconditionally.
    Overwrite,
    /// Replace the destination only if its mtime is older than the
    /// source's; otherwise behaves like `Skip`.
    OverwriteIfOlder,
    /// Replace the destination only if its size differs from the source's;
    /// otherwise behaves like `Skip`. FR-OPS-04's canonical name for what
    /// design.md §9.3's prose loosely calls "overwrite-all".
    OverwriteIfDifferentSize,
    /// Write to a new, non-colliding name chosen by the user (prompted
    /// separately, not encoded here) instead of the original destination
    /// path. FR-OPS-04 calls this "rename-target".
    RenameTarget,
    /// Write to a new, non-colliding name chosen automatically by the
    /// engine (e.g. `file (2).txt`), with no prompt.
    AutoRename,
    /// Stop the job at this conflict. Ends the job `Cancelled` (not
    /// `Failed` — the user chose to stop, this isn't an error) with
    /// whatever progress had already completed intact.
    Abort,
}

/// How broadly a [`ConflictPolicy`] answer applies, once decided.
///
/// design.md §9.3: "job-level default → per-conflict user answer →
/// 'apply to all' answers." A `Plan`'s `PlanOptions::default_conflict` is
/// the first tier and needs no `ConflictScope` (it's implicitly
/// "everything, unless overridden"); this type covers the second and third
/// tiers, both of which originate from a live user answer to a
/// [`ConflictPrompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictScope {
    /// Applies to the single conflicting step that was prompted.
    ThisOnly,
    /// Applies to this conflict and every subsequent conflict in the same
    /// job, without prompting again ("apply to all").
    AllRemaining,
}

/// A resolved conflict answer: the chosen policy plus its scope. What the
/// executor records after either consulting `PlanOptions::default_conflict`
/// or receiving a user's answer to a [`ConflictPrompt`].
///
/// Not `Copy` (unlike most small enums/structs in this crate) because
/// `alternate` carries an owned `VPath` when present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictResolution {
    pub policy: ConflictPolicy,
    pub scope: ConflictScope,
    /// The destination `RenameTarget` should use instead of the original,
    /// conflicting one. Only meaningful when `policy == RenameTarget` --
    /// that policy's whole definition is "a new name chosen by the user"
    /// (see its own doc comment), which nothing inside `duet-ops` can
    /// invent on its own; a resolver that returns `RenameTarget` without
    /// supplying one produces a `StepFailure` rather than guessing.
    /// `ConflictPolicy::AutoRename` computes its own name internally
    /// instead (see the executor's `auto_rename_target`) and ignores this
    /// field entirely.
    pub alternate: Option<VPath>,
}

impl ConflictResolution {
    /// A resolution that applies once, to a single step.
    pub fn once(policy: ConflictPolicy) -> Self {
        ConflictResolution {
            policy,
            scope: ConflictScope::ThisOnly,
            alternate: None,
        }
    }

    /// A resolution that applies to this and every remaining conflict in
    /// the job ("apply to all").
    pub fn apply_to_all(policy: ConflictPolicy) -> Self {
        ConflictResolution {
            policy,
            scope: ConflictScope::AllRemaining,
            alternate: None,
        }
    }

    /// A one-off `RenameTarget` answer: use `alternate` instead of the
    /// conflicting destination for this single step.
    pub fn rename_to(alternate: VPath) -> Self {
        ConflictResolution {
            policy: ConflictPolicy::RenameTarget,
            scope: ConflictScope::ThisOnly,
            alternate: Some(alternate),
        }
    }
}

/// A live decision-maker for a conflict that neither a pre-resolved
/// `Step`'s own `conflict` field nor an already-established per-job
/// "apply to all" answer already covers.
///
/// design.md §9.3 is explicit that the actual UI round trip -- showing a
/// human the prompt and waiting for their click -- is "out-of-band...
/// not modelled in this crate." This trait is the seam a caller (a queue
/// manager, a test harness, eventually T-5.2.3's conflict dialog) plugs
/// into, not that round trip itself: `resolve` is a plain synchronous
/// callback, not `async`, because nothing in this crate needs it to block
/// on I/O. A future interactive implementation that genuinely has to wait
/// on a human is expected to bridge that itself (e.g. blocking on a
/// channel fed by the UI thread) -- this trait only has to be able to
/// *represent* the answer once it arrives.
///
/// `execute()` accepts an `Option<Arc<dyn ConflictResolver>>`; passing
/// `None` means every conflict falls back to `PlanOptions::default_conflict`
/// with no live consultation at all -- the fully-automatic/background case.
pub trait ConflictResolver: Send + Sync {
    fn resolve(&self, prompt: &ConflictPrompt) -> ConflictResolution;
}

/// Data surfaced to the UI for an interactive conflict prompt (FR-OPS-04:
/// "per-conflict interactive prompt with side-by-side metadata"; design.md
/// §9.3: "The prompt shows both sides (size, mtime, and a hash on
/// request)"). `source_meta`/`dest_meta` carry whatever `Metadata` fields
/// the executor already fetched; a hash-on-request is a follow-up
/// `FileSystem::stat`-adjacent call the UI triggers separately, not
/// something this struct pre-computes eagerly (design.md §9.1's
/// capability-driven, request-only-what's-needed philosophy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictPrompt {
    /// Index of the conflicting step within its `Plan`.
    pub step_index: u32,
    pub source: VPath,
    pub dest: VPath,
    pub source_meta: Metadata,
    pub dest_meta: Metadata,
}
