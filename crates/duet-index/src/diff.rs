//! The diff/event protocol between a [`crate::DirectoryModel`] and the UI
//! (design.md §9.2's "Streaming population"; T-3.2.7's AC).
//!
//! T-3.2.7 ("Diff protocol: produce minimal update batches for the UI from
//! model mutations") is Phase 3 work -- the actual algorithm that computes
//! a [`DirDiffBatch`] from two listing generations is `todo!()` here. What
//! this module owes Phase 2 (T-2.5.1's AC: "diff protocol covers
//! insert/remove/update/reorder/reset") is the *shape*: an enum a property
//! test can already be written against ("a 1-entry change produces a
//! 1-entry diff, not a reset") once the computing side lands.

use duet_types::{EntryId, Metadata};

/// One minimal change to a [`crate::DirectoryModel`], as produced by
/// diffing two listing generations. Consumed by the UI to patch its
/// virtualized table incrementally instead of re-rendering from scratch
/// (NFR-05).
///
/// Exactly the five kinds named in T-3.2.7's AC: insert, remove, update,
/// reorder, reset.
#[derive(Debug, Clone, PartialEq)]
pub enum DirEntryDiff {
    /// A new entry appeared: the initial streaming population of a
    /// `read_dir` chunk, or a create picked up by the watcher. Carries
    /// enough to render the row without a synchronous `stat` -- name and
    /// whatever metadata the listing call already had.
    ///
    /// `position` is this entry's index into the *post-diff* `order`
    /// (i.e. where the sorter placed it), so the UI can splice its
    /// virtualization list at the right row instead of resetting scroll
    /// state. Computing that position is exactly the sort-key-comparison
    /// logic that's explicitly out of scope for this task (T-3.2.2).
    Insert {
        id: EntryId,
        name: Box<str>,
        metadata: Metadata,
        position: usize,
    },

    /// An entry disappeared: deleted on disk, or filtered out by a
    /// filter/hidden-files change. The UI looks up `id`'s current display
    /// row itself (it must already track that mapping to have rendered
    /// the row at all) rather than this variant repeating `position`,
    /// which would go stale the instant more than one diff in the same
    /// batch touches ordering.
    Remove { id: EntryId },

    /// An entry's data changed in place (size/mtime/mode edit from a
    /// watch event, or a completed background size computation) without
    /// its sort-relevant position changing. If the changed field *is* the
    /// active sort key, the diffing algorithm must pair this with a
    /// [`DirEntryDiff::Reorder`] rather than silently leaving `order`
    /// stale -- that pairing rule is exactly the kind of property a
    /// T-3.2.7 proptest would check, not something this enum can enforce
    /// on its own.
    Update { id: EntryId, metadata: Metadata },

    /// The sort/filter order changed without any entry being added,
    /// removed, or having its data changed -- a column sort, or a filter
    /// admitting/excluding entries already present. Carries the full new
    /// `order` because a reorder is, by construction, not expressible as
    /// a small set of per-entry edits.
    Reorder { order: Vec<u32> },

    /// Everything changed enough that per-entry diffing isn't worth
    /// computing (design.md §9.2: `IN_Q_OVERFLOW` forcing a full rescan,
    /// or the whole directory changed out from under the tab). The UI
    /// drops its incremental state and re-renders from `DirectoryModel`
    /// directly.
    ///
    /// T-3.2.7's AC ("a 1-entry change produces a 1-entry diff, not a
    /// reset") constrains *when* the algorithm may choose this variant --
    /// it must not be the lazy default -- not that it can never be
    /// correct. When chosen, it must be the only diff in its batch.
    Reset,
}

/// A batch of [`DirEntryDiff`]s produced by one round of model mutation,
/// tagged with the generation transition it represents.
///
/// `generation_from`/`generation_to` let a UI subscriber detect a missed
/// batch (e.g. after being backgrounded) and fall back to a full
/// re-render instead of applying diffs against a generation it never
/// observed -- cheaper than forcing every batch to be self-describing
/// enough to apply blindly.
#[derive(Debug, Clone, PartialEq)]
pub struct DirDiffBatch {
    pub generation_from: u64,
    pub generation_to: u64,
    pub diffs: Vec<DirEntryDiff>,
}

impl DirDiffBatch {
    /// `true` if this batch is a single [`DirEntryDiff::Reset`] (the only
    /// shape a reset batch may take -- see that variant's doc comment).
    pub fn is_reset(&self) -> bool {
        matches!(self.diffs.as_slice(), [DirEntryDiff::Reset])
    }
}
