//! The diff/event protocol between a [`crate::DirectoryModel`] and the UI
//! (design.md §9.2's "Streaming population"; T-3.2.7's AC).
//!
//! T-2.5.1 (Phase 2) built the [`DirEntryDiff`]/[`DirDiffBatch`] *shapes*.
//! T-3.2.7 (this module's [`compute_diff`]) is the actual algorithm:
//! given the model's current entries (by stable `EntryId`, in their
//! current order) and a freshly observed listing (name/metadata pairs, no
//! ids yet), produce the minimal batch of inserts/removes/updates/reorders
//! needed to bring the model up to date -- never a [`DirEntryDiff::Reset`],
//! which this function never emits on its own (`Reset` is reserved for a
//! caller that already knows incremental tracking isn't trustworthy, e.g.
//! T-3.2.5/T-3.2.6's `WatchUpdate::RescanNeeded` after an `IN_Q_OVERFLOW`
//! or a poll fallback's "can't tell what changed, just that something
//! did").

use std::collections::{HashMap, HashSet};

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
    /// state. [`compute_diff`] derives this from the caller-supplied
    /// `new` sequence's own order -- it does not re-sort `new` itself
    /// (see that function's doc comment), so a caller that cares about
    /// sort-correct positions must hand `compute_diff` an already-sorted
    /// `new` (e.g. via [`crate::Sorter`]).
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

/// One entry as [`compute_diff`]'s `old` side already has it: a stable id
/// plus the name/metadata the model currently has on file for it.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffEntry {
    pub id: EntryId,
    pub name: Box<str>,
    pub metadata: Metadata,
}

/// Computes the minimal [`DirDiffBatch`] that brings a listing from `old`
/// to `new`.
///
/// - `old` is the model's current entries, **in their current display
///   order**, each already carrying the stable [`EntryId`] the model
///   assigned it.
/// - `new` is a freshly observed listing (a `read_dir` chunk, or a
///   watch-triggered re-stat) as bare name/metadata pairs -- it has no
///   ids yet, because ids are the model's to assign, not the
///   filesystem's. `new`'s sequence is treated as authoritative order;
///   this function does not sort it (T-3.2.2's `Sorter` is a separate,
///   composable concern -- a caller that wants sort-correct output hands
///   `compute_diff` an already-sorted `new`, e.g. via `Sorter`/
///   `sort_order` first).
/// - `allocate_id` mints an [`EntryId`] for each entry in `new` that has
///   no match in `old`. Passing something that mirrors `EntryStore::push`'s
///   own sequential assignment (`EntryId::new(current_len)`, incrementing)
///   is what keeps an id `compute_diff` allocates here consistent with the
///   id `EntryStore::push` will actually assign when a caller applies the
///   resulting `Insert` diffs in order.
///
/// **Matching is by name.** A renamed file is observed as a `Remove` +
/// `Insert` pair (a new name has no old entry to match), not a rename
/// diff -- there is no rename variant in [`DirEntryDiff`], and detecting
/// "same inode, different name" would need `(dev, ino)` tracking this
/// function doesn't have inputs for. This is a real, documented scope
/// boundary, not an oversight: it's still *correct* (the end state is
/// right), just not maximally minimal for the rename case specifically.
///
/// **Never emits [`DirEntryDiff::Reset`].** Every difference between
/// `old` and `new` is expressible as some combination of
/// insert/remove/update/reorder, so this function always uses that
/// combination -- `Reset` is for a caller that already knows it can't
/// trust incremental tracking at all (T-3.2.5/T-3.2.6's overflow/error
/// handling), which is a different situation from "I have two listings
/// and want the diff between them."
pub fn compute_diff(
    old: &[DiffEntry],
    new: &[(Box<str>, Metadata)],
    generation_from: u64,
    mut allocate_id: impl FnMut() -> EntryId,
) -> DirDiffBatch {
    let mut by_name: HashMap<&str, usize> = HashMap::with_capacity(old.len());
    for (ix, entry) in old.iter().enumerate() {
        by_name.insert(entry.name.as_ref(), ix);
    }

    let mut matched = vec![false; old.len()];
    let mut diffs = Vec::new();
    let mut new_order: Vec<EntryId> = Vec::with_capacity(new.len());

    for (position, (name, metadata)) in new.iter().enumerate() {
        if let Some(&old_ix) = by_name.get(name.as_ref()) {
            matched[old_ix] = true;
            let old_entry = &old[old_ix];
            new_order.push(old_entry.id);
            if &old_entry.metadata != metadata {
                diffs.push(DirEntryDiff::Update {
                    id: old_entry.id,
                    metadata: metadata.clone(),
                });
            }
        } else {
            let id = allocate_id();
            new_order.push(id);
            diffs.push(DirEntryDiff::Insert {
                id,
                name: name.clone(),
                metadata: metadata.clone(),
                position,
            });
        }
    }

    for (ix, was_matched) in matched.iter().enumerate() {
        if !was_matched {
            diffs.push(DirEntryDiff::Remove { id: old[ix].id });
        }
    }

    // Reorder detection: did the *relative* order of entries present in
    // both `old` and `new` change? (Newly inserted entries don't count --
    // their placement is already carried by `Insert::position`; removed
    // ones trivially can't have "moved".) Comparing just the carried-over
    // subsequence, rather than the full lists, is what keeps a pure
    // insert/remove (no reordering among survivors) from spuriously
    // triggering a `Reorder` too.
    let carried: HashSet<EntryId> = old
        .iter()
        .enumerate()
        .filter(|&(ix, _)| matched[ix])
        .map(|(_, e)| e.id)
        .collect();
    let old_common_order: Vec<EntryId> = old
        .iter()
        .map(|e| e.id)
        .filter(|id| carried.contains(id))
        .collect();
    let new_common_order: Vec<EntryId> = new_order
        .iter()
        .copied()
        .filter(|id| carried.contains(id))
        .collect();
    if old_common_order != new_common_order {
        diffs.push(DirEntryDiff::Reorder {
            order: new_order.iter().map(|id| id.index()).collect(),
        });
    }

    DirDiffBatch {
        generation_from,
        generation_to: generation_from + 1,
        diffs,
    }
}

#[cfg(test)]
mod tests {
    use duet_types::EntryKind;
    use proptest::collection::vec as pvec;
    use proptest::prelude::*;

    use super::*;

    fn meta(size: u64) -> Metadata {
        let mut m = Metadata::minimal(EntryKind::File);
        m.size = size;
        m
    }

    fn entry(id: u32, name: &str, size: u64) -> DiffEntry {
        DiffEntry {
            id: EntryId::new(id),
            name: name.into(),
            metadata: meta(size),
        }
    }

    fn name_meta(name: &str, size: u64) -> (Box<str>, Metadata) {
        (name.into(), meta(size))
    }

    fn sequential_allocator(start: u32) -> impl FnMut() -> EntryId {
        let mut next = start;
        move || {
            let id = EntryId::new(next);
            next += 1;
            id
        }
    }

    #[test]
    fn identical_listing_produces_empty_diff() {
        let old = vec![entry(0, "a", 10), entry(1, "b", 20)];
        let new = vec![name_meta("a", 10), name_meta("b", 20)];
        let batch = compute_diff(&old, &new, 0, sequential_allocator(2));
        assert!(batch.diffs.is_empty());
        assert!(!batch.is_reset());
    }

    /// T-3.2.7's AC, verbatim: "a 1-entry change produces a 1-entry diff,
    /// not a reset." Three variants of "1-entry change" -- a metadata
    /// edit, an addition, a removal -- each checked directly.
    #[test]
    fn single_metadata_change_produces_one_update_diff() {
        let old = vec![entry(0, "a", 10), entry(1, "b", 20), entry(2, "c", 30)];
        let new = vec![name_meta("a", 10), name_meta("b", 99), name_meta("c", 30)];
        let batch = compute_diff(&old, &new, 5, sequential_allocator(3));
        assert_eq!(batch.generation_from, 5);
        assert_eq!(batch.generation_to, 6);
        assert_eq!(batch.diffs.len(), 1, "{:?}", batch.diffs);
        assert!(matches!(
            &batch.diffs[0],
            DirEntryDiff::Update { id, metadata } if id.index() == 1 && metadata.size == 99
        ));
        assert!(!batch.is_reset());
    }

    #[test]
    fn single_new_entry_produces_one_insert_diff() {
        let old = vec![entry(0, "a", 10), entry(1, "b", 20)];
        let new = vec![name_meta("a", 10), name_meta("b", 20), name_meta("c", 30)];
        let batch = compute_diff(&old, &new, 0, sequential_allocator(2));
        assert_eq!(batch.diffs.len(), 1, "{:?}", batch.diffs);
        assert!(matches!(
            &batch.diffs[0],
            DirEntryDiff::Insert { name, position, .. } if &**name == "c" && *position == 2
        ));
        assert!(!batch.is_reset());
    }

    #[test]
    fn single_removed_entry_produces_one_remove_diff() {
        let old = vec![entry(0, "a", 10), entry(1, "b", 20), entry(2, "c", 30)];
        let new = vec![name_meta("a", 10), name_meta("c", 30)];
        let batch = compute_diff(&old, &new, 0, sequential_allocator(3));
        assert_eq!(batch.diffs.len(), 1, "{:?}", batch.diffs);
        assert!(matches!(&batch.diffs[0], DirEntryDiff::Remove { id } if id.index() == 1));
        assert!(!batch.is_reset());
    }

    #[test]
    fn pure_reorder_with_no_content_change_produces_one_reorder_diff() {
        let old = vec![entry(0, "a", 10), entry(1, "b", 20)];
        let new = vec![name_meta("b", 20), name_meta("a", 10)];
        let batch = compute_diff(&old, &new, 0, sequential_allocator(2));
        assert_eq!(batch.diffs.len(), 1, "{:?}", batch.diffs);
        assert!(matches!(
            &batch.diffs[0],
            DirEntryDiff::Reorder { order } if order == &vec![1, 0]
        ));
    }

    #[test]
    fn insert_and_remove_without_reordering_survivors_has_no_reorder_diff() {
        // "b" is removed and "c" is inserted, but "a" and "d" (which
        // survive) keep their relative order -- no Reorder should appear
        // alongside the Insert/Remove.
        let old = vec![entry(0, "a", 1), entry(1, "b", 2), entry(2, "d", 4)];
        let new = vec![name_meta("a", 1), name_meta("c", 3), name_meta("d", 4)];
        let batch = compute_diff(&old, &new, 0, sequential_allocator(3));
        assert_eq!(batch.diffs.len(), 2, "{:?}", batch.diffs);
        assert!(
            batch
                .diffs
                .iter()
                .any(|d| matches!(d, DirEntryDiff::Insert { name, .. } if &**name == "c"))
        );
        assert!(
            batch
                .diffs
                .iter()
                .any(|d| matches!(d, DirEntryDiff::Remove { id } if id.index() == 1))
        );
        assert!(
            !batch
                .diffs
                .iter()
                .any(|d| matches!(d, DirEntryDiff::Reorder { .. }))
        );
    }

    fn arb_named_entries(
        pool: &'static [&'static str],
    ) -> impl Strategy<Value = Vec<(String, u64)>> {
        pvec((prop::sample::select(pool), 0u64..1_000), 0..12).prop_map(|pairs| {
            let mut seen: HashSet<&'static str> = HashSet::new();
            pairs
                .into_iter()
                .filter(|(n, _)| seen.insert(n))
                .map(|(n, s)| (n.to_string(), s))
                .collect()
        })
    }

    const NAME_POOL: &[&str] = &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];

    proptest! {
        /// T-3.2.7's property test: for arbitrary "old" and "new" listings
        /// (unique names within each, random sizes), compute_diff's output,
        /// applied to a reconstruction seeded from `old`, always yields
        /// exactly `new`'s content -- regardless of how many entries were
        /// inserted, removed, updated, or reordered in between.
        #[test]
        fn diff_always_reconstructs_new_state(
            old_raw in arb_named_entries(NAME_POOL),
            new_raw in arb_named_entries(NAME_POOL),
        ) {
            let mut next_old_id = 0u32;
            let old: Vec<DiffEntry> = old_raw
                .iter()
                .map(|(n, s)| {
                    let id = EntryId::new(next_old_id);
                    next_old_id += 1;
                    DiffEntry { id, name: n.clone().into_boxed_str(), metadata: meta(*s) }
                })
                .collect();
            let new: Vec<(Box<str>, Metadata)> = new_raw
                .iter()
                .map(|(n, s)| (n.clone().into_boxed_str(), meta(*s)))
                .collect();

            let batch = compute_diff(&old, &new, 0, sequential_allocator(next_old_id));

            prop_assert!(!batch.is_reset());

            let mut reconstructed: HashMap<EntryId, (Box<str>, Metadata)> = old
                .iter()
                .map(|e| (e.id, (e.name.clone(), e.metadata.clone())))
                .collect();
            for d in &batch.diffs {
                match d {
                    DirEntryDiff::Insert { id, name, metadata, .. } => {
                        reconstructed.insert(*id, (name.clone(), metadata.clone()));
                    }
                    DirEntryDiff::Remove { id } => {
                        reconstructed.remove(id);
                    }
                    DirEntryDiff::Update { id, metadata } => {
                        if let Some(e) = reconstructed.get_mut(id) {
                            e.1 = metadata.clone();
                        }
                    }
                    DirEntryDiff::Reorder { .. } => {}
                    DirEntryDiff::Reset => prop_assert!(false, "compute_diff must never emit Reset"),
                }
            }

            let mut reconstructed_set: Vec<(String, u64)> = reconstructed
                .values()
                .map(|(n, m)| (n.to_string(), m.size))
                .collect();
            reconstructed_set.sort();
            let mut expected_set = new_raw.clone();
            expected_set.sort();
            prop_assert_eq!(reconstructed_set, expected_set);
        }
    }
}
