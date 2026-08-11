//! [`DirectoryModel`]: the per-tab panel model (design.md §9.2).

use duet_types::EntryId;
use roaring::RoaringBitmap;

use crate::diff::DirDiffBatch;
use crate::entry_store::EntryStore;

/// Which column drives the current sort, for [`DirectoryModel::sort_by`]
/// (T-3.2.2's actual comparator/collation work is out of scope here --
/// this is just the selector shape the Phase 2 API needs to exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Size,
    Modified,
    Kind,
}

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
pub struct DirectoryModel {
    entries: EntryStore,
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
            order: Vec::new(),
            selection: RoaringBitmap::new(),
            cursor: None,
            generation: 0,
        }
    }

    pub fn entries(&self) -> &EntryStore {
        &self.entries
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

    /// Re-sorts (and re-filters, if a filter is active) `order` by the
    /// given column. This is T-3.2.2's comparator/collation work
    /// (natural-numeric mode, locale collation, directories-first) --
    /// deliberately unimplemented here since T-2.5.1 owes the shape, not
    /// the sorting algorithm.
    pub fn sort_by(&mut self, _column: SortColumn, _ascending: bool) {
        todo!("T-3.2.2: locale-aware column sort with precomputed sort keys")
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
        let a = model
            .entries
            .push("b_second", &Metadata::minimal(EntryKind::File));
        let b = model
            .entries
            .push("a_first", &Metadata::minimal(EntryKind::File));
        // Simulate a completed sort by hand (sort_by itself is todo!()):
        // display order is [b, a] even though push order was [a, b].
        model.order = vec![b.index(), a.index()];

        let names: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(names, vec!["a_first", "b_second"]);
    }

    #[test]
    fn selection_keyed_by_entry_id_survives_reorder() {
        let mut model = DirectoryModel::new();
        let a = model.entries.push("a", &Metadata::minimal(EntryKind::File));
        let b = model.entries.push("b", &Metadata::minimal(EntryKind::File));
        model.order = vec![a.index(), b.index()];
        model.selection.insert(b.index());

        // Reorder happens (hand-simulated, since sort_by is todo!()).
        model.order = vec![b.index(), a.index()];

        assert!(model.selection().contains(b.index()));
        assert!(!model.selection().contains(a.index()));
    }
}
