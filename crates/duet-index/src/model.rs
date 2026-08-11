//! [`DirectoryModel`]: the per-tab panel model (design.md §9.2).

use duet_types::EntryId;
use roaring::RoaringBitmap;

use crate::diff::DirDiffBatch;
use crate::entry_store::EntryStore;
use crate::sort::{self, SortKeys, SortOptions, Sorter};

pub use crate::sort::SortColumn;

/// The panel model a single tab owns (design.md §9.2):
///
/// ```text
/// DirectoryModel {
///     entries:    EntryStore,      // struct-of-arrays, see EntryStore
///     order:      Vec<u32>,        // sorted+filtered indices -- what the table renders
///     selection:  RoaringBitmap,   // by stable EntryId
///     cursor:     EntryId,
///     generation: u64,             // bumped on every mutation; drives cheap diffing
/// }
/// ```
///
/// One deviation from that sketch: `cursor` is `Option<EntryId>`, not a
/// bare `EntryId`. `EntryId` has no reserved sentinel value that safely
/// means "no cursor" (unlike, say, `u32::MAX` being an obviously-invalid
/// array index, `EntryId(u32::MAX)` is a syntactically valid id an
/// `EntryStore` could legitimately assign at exactly 2^32 entries), and an
/// empty directory is a real, common state (freshly created folder, fully
/// filtered view) that has no entry to point the cursor at. Modeling that
/// as `None` is more honest than picking a magic in-band value and hoping
/// nothing ever collides with it.
///
/// A second deviation, added by T-3.2.2/T-3.2.3: `order` is not stored
/// directly as sketched above. It is derived from `full_order` (every live
/// entry, sorted) by applying the active filter (T-3.2.3). Splitting the
/// two is what lets a filter change recompute `order` (an `O(n)` scan)
/// without re-sorting, and a sort change reuse the same filter without
/// re-deriving it from scratch -- see `sort.rs`'s module doc comment and
/// `filter.rs`'s (T-3.2.3) for the full reasoning.
pub struct DirectoryModel {
    entries: EntryStore,
    sorter: Sorter,
    sort_options: SortOptions,
    /// Precomputed per-column sort keys for `entries`, built for
    /// `sort_options.column`. `None` only before the first sort (a freshly
    /// populated, never-sorted model shows push order via `full_order`
    /// falling back to identity -- see `resort`'s doc comment).
    sort_keys: Option<SortKeys>,
    /// Every live (non-tombstoned) entry, in the active sort order,
    /// filter-independent. T-3.2.3's filtering derives `order` from this
    /// without touching `sort_keys` or re-sorting.
    full_order: Vec<u32>,
    /// Indices into `entries` (equivalently, `EntryId::index()` values),
    /// in the current sorted-and-filtered display order. This is what the
    /// virtualized table renders through; row `r` on screen is
    /// `entries.name(EntryId::new(order[r]))`.
    order: Vec<u32>,
    /// Selected entries, keyed by stable `EntryId` rather than display
    /// position, so a re-sort or a filter change doesn't perturb the
    /// selected set (T-3.2.4's AC).
    selection: RoaringBitmap,
    cursor: Option<EntryId>,
    /// Bumped on every mutation (population, sort, filter, watch-driven
    /// update). Cheap diffing and stale-batch detection (see
    /// `DirDiffBatch`) both hinge on this monotonically increasing.
    generation: u64,
}

impl DirectoryModel {
    /// An empty model at generation 0, ready to receive streamed
    /// population (design.md §9.2's chunked `read_dir` application).
    pub fn new() -> Self {
        DirectoryModel {
            entries: EntryStore::new(),
            sorter: Sorter::new(),
            sort_options: SortOptions::default(),
            sort_keys: None,
            full_order: Vec::new(),
            order: Vec::new(),
            selection: RoaringBitmap::new(),
            cursor: None,
            generation: 0,
        }
    }

    pub fn entries(&self) -> &EntryStore {
        &self.entries
    }

    /// Mutable access to the underlying store, for population/watch-driven
    /// updates. Callers that push/remove entries through this must call
    /// [`Self::sort_by`] (or a future incremental-update path) afterward to
    /// bring `order`/`full_order` back in sync -- this method does not do
    /// so implicitly, since a caller streaming many chunks in (design.md
    /// §9.2's "one flush per frame, max") wants to batch several pushes
    /// before paying for one resort, not resort after every single push.
    pub fn entries_mut(&mut self) -> &mut EntryStore {
        &mut self.entries
    }

    /// The current sorted-and-filtered render order. Length is the
    /// *visible* entry count, which may be less than `entries.len()`
    /// (tombstoned or filtered-out entries are excluded).
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    pub fn selection(&self) -> &RoaringBitmap {
        &self.selection
    }

    pub fn cursor(&self) -> Option<EntryId> {
        self.cursor
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn sort_options(&self) -> SortOptions {
        self.sort_options
    }

    /// Read access to the currently visible names in display order, keyed
    /// by their stable id -- what FR-NAV-13's quick-search regime scores
    /// against on every keystroke (design.md §9.2: "Scoring reuses a
    /// fuzzy-subsequence matcher... run over the current `order` slice's
    /// visible names"). Quick-search's own state (query buffer,
    /// last-keystroke timestamp, active flag) is transient tab/UI state,
    /// not part of this model -- this method is the entire surface
    /// `DirectoryModel` needs to expose for it.
    pub fn ordered_names(&self) -> impl Iterator<Item = (EntryId, &str)> + '_ {
        self.order.iter().map(|&ix| {
            let id = EntryId::new(ix);
            (id, self.entries.name(id))
        })
    }

    /// Re-sorts (and re-applies the active filter to) `order` by the given
    /// column. T-3.2.2's AC: locale-aware collation with a natural-numeric
    /// mode and a directories-first grouping (see `sort.rs`), driven by
    /// precomputed per-column keys so the comparator never re-derives a
    /// collation key or re-reads a raw name mid-sort.
    pub fn sort_by(&mut self, column: SortColumn, ascending: bool) {
        self.sort_options.column = column;
        self.sort_options.ascending = ascending;
        self.resort();
    }

    /// Toggles the directories-first grouping policy (design.md §9.2:
    /// "directories-first policy" -- T-3.2.2's AC calls this out as
    /// "configurable" explicitly) and re-sorts to apply it.
    pub fn set_dirs_first(&mut self, dirs_first: bool) {
        self.sort_options.dirs_first = dirs_first;
        self.resort();
    }

    /// Recomputes `sort_keys`/`full_order`/`order` from scratch for the
    /// current `sort_options`. `O(n)` key generation (parallelized across
    /// the name column, see `Sorter::build_keys`) plus an `O(n log n)`
    /// stable sort over the precomputed keys.
    ///
    /// Filtering is not yet wired in here (T-3.2.3 lands it): until then,
    /// `order` is always a full clone of `full_order`. T-3.2.3 replaces
    /// that last step with the filter-application scan without touching
    /// anything above it in this function.
    fn resort(&mut self) {
        let keys = self
            .sorter
            .build_keys(&self.entries, self.sort_options.column);
        let mut full_order: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&ix| !self.entries.is_tombstoned(EntryId::new(ix)))
            .collect();
        sort::sort_order(&mut full_order, &keys, self.sort_options);

        self.order = full_order.clone();
        self.full_order = full_order;
        self.sort_keys = Some(keys);
        self.generation += 1;
    }

    /// Applies a diff batch produced elsewhere (e.g. by the T-3.2.7
    /// diffing algorithm reacting to a watch event) to this model:
    /// mutates `entries`/`order`, advances `generation` to
    /// `batch.generation_to`, and leaves `selection`/`cursor` referring to
    /// the same `EntryId`s they did before (ids are stable within a
    /// generation by construction; only a `Reset` batch may invalidate
    /// them).
    ///
    /// Unimplemented: applying `Insert`/`Reorder` correctly requires the
    /// same sort-key machinery as `sort_by` (an inserted entry's `order`
    /// position depends on the active sort), which is Phase 3 scope.
    pub fn apply_diff(&mut self, _batch: &DirDiffBatch) {
        todo!("T-3.2.7: apply a computed diff batch to entries/order/generation")
    }

    /// Computes the minimal `DirDiffBatch` needed to bring an observer
    /// from this model's current generation to its state after `mutate`
    /// runs. This is the diffing algorithm itself (T-3.2.7's actual AC:
    /// "a 1-entry change produces a 1-entry diff, not a reset; property
    /// test over random mutation sequences") -- out of scope for T-2.5.1,
    /// which owes the `DirEntryDiff`/`DirDiffBatch` shapes this will
    /// eventually return.
    pub fn mutate_and_diff(&mut self, _mutate: impl FnOnce(&mut EntryStore)) -> DirDiffBatch {
        todo!("T-3.2.7: minimal-diff computation over an arbitrary mutation")
    }
}

impl Default for DirectoryModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use duet_types::{EntryKind, Metadata};

    use super::*;

    #[test]
    fn new_model_is_empty_with_no_cursor() {
        let model = DirectoryModel::new();
        assert_eq!(model.generation(), 0);
        assert!(model.order().is_empty());
        assert!(model.cursor().is_none());
        assert_eq!(model.selection().len(), 0);
    }

    #[test]
    fn ordered_names_follows_order_not_push_sequence() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("b_second", &Metadata::minimal(EntryKind::File));
        model
            .entries_mut()
            .push("a_first", &Metadata::minimal(EntryKind::File));
        model.sort_by(SortColumn::Name, true);

        let names: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(names, vec!["a_first", "b_second"]);
    }

    #[test]
    fn selection_keyed_by_entry_id_survives_reorder() {
        let mut model = DirectoryModel::new();
        let a = model
            .entries_mut()
            .push("z", &Metadata::minimal(EntryKind::File));
        let b = model
            .entries_mut()
            .push("a", &Metadata::minimal(EntryKind::File));
        model.selection.insert(b.index());

        // Name-ascending puts b ("a") before a ("z").
        model.sort_by(SortColumn::Name, true);
        assert_eq!(model.order(), &[b.index(), a.index()]);
        assert!(model.selection().contains(b.index()));
        assert!(!model.selection().contains(a.index()));

        // Re-sorting descending flips display order; selection (by
        // EntryId, not position) is untouched.
        model.sort_by(SortColumn::Name, false);
        assert_eq!(model.order(), &[a.index(), b.index()]);
        assert!(model.selection().contains(b.index()));
        assert!(!model.selection().contains(a.index()));
    }

    #[test]
    fn sort_by_bumps_generation() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("a", &Metadata::minimal(EntryKind::File));
        let gen0 = model.generation();
        model.sort_by(SortColumn::Name, true);
        assert_eq!(model.generation(), gen0 + 1);
    }

    #[test]
    fn tombstoned_entries_are_excluded_from_order() {
        let mut model = DirectoryModel::new();
        let a = model
            .entries_mut()
            .push("a", &Metadata::minimal(EntryKind::File));
        let b = model
            .entries_mut()
            .push("b", &Metadata::minimal(EntryKind::File));
        model.entries_mut().remove(a);
        model.sort_by(SortColumn::Name, true);
        assert_eq!(model.order(), &[b.index()]);
    }
}
