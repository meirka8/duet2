//! [`DirectoryModel`]: the per-tab panel model (design.md §9.2).

use duet_types::EntryId;
use roaring::RoaringBitmap;

use crate::diff::DirDiffBatch;
use crate::entry_store::EntryStore;
use crate::filter::FilterSpec;
use crate::sort::{self, SortKeys, SortOptions, Sorter};

pub use crate::sort::SortColumn;

/// Cheap-to-read summary of the current selection (T-3.2.4's AC). Both
/// fields are maintained incrementally by [`DirectoryModel`]'s selection
/// methods, so reading this is an `O(1)` struct copy, never a scan over
/// the selected set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionStats {
    pub count: u64,
    pub total_bytes: u64,
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
    /// The active filter (T-3.2.3), or `None` for "everything visible"
    /// (equivalent to `FilterSpec { show_hidden: true, .. }` but avoids
    /// allocating/evaluating a no-op filter on the hot path).
    filter: Option<FilterSpec>,
    /// Indices into `entries` (equivalently, `EntryId::index()` values),
    /// in the current sorted-and-filtered display order. This is what the
    /// virtualized table renders through; row `r` on screen is
    /// `entries.name(EntryId::new(order[r]))`.
    order: Vec<u32>,
    /// Selected entries, keyed by stable `EntryId` rather than display
    /// position, so a re-sort or a filter change doesn't perturb the
    /// selected set (T-3.2.4's AC). `RoaringBitmap` over `EntryId::index()`
    /// values: compressed, so a large contiguous or sparse selection (a
    /// shift-click range, "select all") costs far less than one bit per
    /// live entry.
    selection: RoaringBitmap,
    /// Sum of `entries.size(id)` for every `id` in `selection`, maintained
    /// incrementally by every method that mutates `selection` (never
    /// recomputed by scanning the whole selection). This is what makes
    /// [`Self::selection_stats`] an `O(1)` read regardless of selection
    /// size -- T-3.2.4's AC ("selection statistics... update in <= 5ms
    /// after a selection change") is trivial once the running total is
    /// already right there instead of needing a fresh reduction over
    /// (potentially) 500k+ ids on every call.
    selected_bytes: u64,
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
            filter: None,
            order: Vec::new(),
            selection: RoaringBitmap::new(),
            selected_bytes: 0,
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

    /// Every live entry in sort order, ignoring the active filter --
    /// exposed mainly so tests (and any future "clear filter" UI path) can
    /// observe that filtering never perturbs it.
    pub fn full_order(&self) -> &[u32] {
        &self.full_order
    }

    pub fn filter(&self) -> Option<&FilterSpec> {
        self.filter.as_ref()
    }

    pub fn selection(&self) -> &RoaringBitmap {
        &self.selection
    }

    /// `O(1)`: count and total byte size of the current selection. See
    /// `selected_bytes`'s field doc comment for why this never scans.
    pub fn selection_stats(&self) -> SelectionStats {
        SelectionStats {
            count: self.selection.len(),
            total_bytes: self.selected_bytes,
        }
    }

    pub fn is_selected(&self, id: EntryId) -> bool {
        self.selection.contains(id.index())
    }

    /// Adds `id` to the selection. A no-op (including for `selected_bytes`)
    /// if `id` was already selected.
    pub fn select(&mut self, id: EntryId) {
        if self.selection.insert(id.index()) {
            self.selected_bytes += self.entries.size(id);
        }
    }

    /// Removes `id` from the selection. A no-op if it wasn't selected.
    pub fn deselect(&mut self, id: EntryId) {
        if self.selection.remove(id.index()) {
            self.selected_bytes -= self.entries.size(id);
        }
    }

    pub fn toggle_selection(&mut self, id: EntryId) {
        if self.is_selected(id) {
            self.deselect(id);
        } else {
            self.select(id);
        }
    }

    /// Selects every id in `ids` (e.g. a shift-click range, or "select
    /// all" passing `self.order().iter().copied().map(EntryId::new)`).
    /// `O(k)` in the number of ids given, same as `k` individual `select`
    /// calls but without `k` separate method-call overheads.
    pub fn select_many(&mut self, ids: impl IntoIterator<Item = EntryId>) {
        for id in ids {
            self.select(id);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected_bytes = 0;
    }

    pub fn cursor(&self) -> Option<EntryId> {
        self.cursor
    }

    /// Moves the cursor to `id` directly, or clears it (`None`). This
    /// method has no opinion about display/row position -- like
    /// `select`/`deselect`, it operates purely on the stable `EntryId`; a
    /// caller that thinks in terms of rows (T-4.2.2's keyboard movement)
    /// translates through `order()` itself. Doesn't bump `generation`,
    /// same as every other selection method -- see `selected_bytes`'s doc
    /// comment for why cursor/selection changes are cheap, non-generation
    /// events distinct from population/sort/filter.
    pub fn set_cursor(&mut self, id: Option<EntryId>) {
        self.cursor = id;
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
    /// stable sort over the precomputed keys, then one `O(n)` filter scan
    /// (`filtered_order`) to derive `order` -- the expensive parts (key
    /// generation, sorting) never re-run just because the filter changes;
    /// see [`Self::set_filter`].
    fn resort(&mut self) {
        let keys = self
            .sorter
            .build_keys(&self.entries, self.sort_options.column);
        let mut full_order: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&ix| !self.entries.is_tombstoned(EntryId::new(ix)))
            .collect();
        sort::sort_order(&mut full_order, &keys, self.sort_options);

        self.full_order = full_order;
        self.sort_keys = Some(keys);
        self.order = self.filtered_order();
        self.prune_selection_to_live();
        self.generation += 1;
    }

    /// Drops any selected id that has since been tombstoned (e.g. a watch-
    /// driven delete, once T-3.2.7 wires diffs through here), keeping
    /// `selected_bytes` from silently drifting away from the truth. `O(k)`
    /// in selection size (`RoaringBitmap` iteration), not `O(n)` in entry
    /// count -- a resort already touches every live entry once for the
    /// sort itself, but selection is typically far smaller than the full
    /// listing, so scanning *it* instead is the cheaper direction.
    fn prune_selection_to_live(&mut self) {
        let stale: Vec<u32> = self
            .selection
            .iter()
            .filter(|&ix| self.entries.is_tombstoned(EntryId::new(ix)))
            .collect();
        for ix in stale {
            self.deselect(EntryId::new(ix));
        }
    }

    /// Replaces the active filter and re-derives `order` from the
    /// already-sorted `full_order` with a single `O(n)` scan through
    /// [`FilterSpec::matches`]. Does **not** touch `sort_keys`/`full_order`
    /// or call `Sorter::build_keys`/`sort::sort_order` -- T-3.2.3's AC:
    /// changing the filter must not force a re-sort.
    pub fn set_filter(&mut self, filter: Option<FilterSpec>) {
        self.filter = filter;
        self.order = self.filtered_order();
        self.generation += 1;
    }

    /// Scans `full_order` through the active filter (or passes everything
    /// through, if `self.filter` is `None`). Pulled out as its own method
    /// so both `resort` (sort changed, filter reapplied) and `set_filter`
    /// (filter changed, sort reused) share one implementation of "what does
    /// visible mean right now" rather than two copies that could drift.
    fn filtered_order(&self) -> Vec<u32> {
        match &self.filter {
            None => self.full_order.clone(),
            Some(spec) => self
                .full_order
                .iter()
                .copied()
                .filter(|&ix| {
                    let id = EntryId::new(ix);
                    spec.matches(
                        self.entries.name(id),
                        self.entries.flags(id).hidden(),
                        self.entries.kind(id),
                    )
                })
                .collect(),
        }
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
    fn set_cursor_moves_and_clears_without_bumping_generation() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("a", &Metadata::minimal(EntryKind::File));
        let gen_before = model.generation();

        model.set_cursor(Some(EntryId::new(0)));
        assert_eq!(model.cursor(), Some(EntryId::new(0)));
        assert_eq!(model.generation(), gen_before);

        model.set_cursor(None);
        assert!(model.cursor().is_none());
        assert_eq!(model.generation(), gen_before);
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
        model.select(b);

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

    #[test]
    fn filter_narrows_order_without_touching_full_order() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("main.rs", &Metadata::minimal(EntryKind::File));
        model
            .entries_mut()
            .push("readme.md", &Metadata::minimal(EntryKind::File));
        model
            .entries_mut()
            .push(".gitignore", &Metadata::minimal(EntryKind::File));
        model.sort_by(SortColumn::Name, true);

        let full_before = model.full_order().to_vec();
        // `filter: None` means "everything visible" (see its field doc
        // comment), so the dotfile is included here too.
        assert_eq!(full_before.len(), 3);

        model.set_filter(Some(crate::filter::FilterSpec {
            show_hidden: false,
            quick_filter: None,
            mask: Some(crate::filter::Mask::new("*.rs")),
        }));

        // full_order is untouched by a filter change -- same 3 entries,
        // same sort order -- only `order` narrows.
        assert_eq!(model.full_order(), full_before.as_slice());
        let visible: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(visible, vec!["main.rs"]);

        // Clearing the filter restores full visibility without a resort.
        model.set_filter(None);
        let visible: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(visible, vec![".gitignore", "main.rs", "readme.md"]);
    }

    #[test]
    fn changing_sort_reapplies_the_active_filter() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("b.rs", &Metadata::minimal(EntryKind::File));
        model
            .entries_mut()
            .push("a.txt", &Metadata::minimal(EntryKind::File));
        model
            .entries_mut()
            .push("a.rs", &Metadata::minimal(EntryKind::File));
        model.sort_by(SortColumn::Name, true);
        model.set_filter(Some(crate::filter::FilterSpec {
            show_hidden: true,
            quick_filter: None,
            mask: Some(crate::filter::Mask::new("*.rs")),
        }));
        let visible: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(visible, vec!["a.rs", "b.rs"]);

        // Re-sorting descending: filter (still "*.rs") stays applied.
        model.sort_by(SortColumn::Name, false);
        let visible: Vec<_> = model.ordered_names().map(|(_, n)| n.to_string()).collect();
        assert_eq!(visible, vec!["b.rs", "a.rs"]);
        assert!(model.filter().is_some());
    }

    #[test]
    fn filter_over_1m_entries_completes_within_budget() {
        use std::time::Instant;

        const N: usize = 1_000_000;
        let mut model = DirectoryModel::new();
        for i in 0..N {
            let kind = if i % 5 == 0 {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            let ext = if i % 3 == 0 { "rs" } else { "txt" };
            model
                .entries_mut()
                .push(&format!("entry_{i:07}.{ext}"), &Metadata::minimal(kind));
        }
        model.sort_by(SortColumn::Name, true);

        let start = Instant::now();
        model.set_filter(Some(crate::filter::FilterSpec {
            show_hidden: true,
            quick_filter: None,
            mask: Some(crate::filter::Mask::new("*.rs")),
        }));
        let elapsed = start.elapsed();

        assert!(!model.order().is_empty());
        // T-3.2.3's AC: "filter over 1M entries <= 80ms". Only enforced
        // under --release for the same reason sort.rs's 1M-entry test is:
        // an unoptimized debug build isn't what the AC's number is about.
        if !cfg!(debug_assertions) {
            assert!(
                elapsed.as_millis() <= 80,
                "filtering 1M entries took {elapsed:?}, over the 80ms T-3.2.3 budget"
            );
        }
    }

    #[test]
    fn selection_stats_track_count_and_bytes_incrementally() {
        let mut model = DirectoryModel::new();
        let mut meta = Metadata::minimal(EntryKind::File);
        meta.size = 100;
        let a = model.entries_mut().push("a", &meta);
        meta.size = 250;
        let b = model.entries_mut().push("b", &meta);

        assert_eq!(model.selection_stats(), SelectionStats::default());

        model.select(a);
        assert_eq!(
            model.selection_stats(),
            SelectionStats {
                count: 1,
                total_bytes: 100
            }
        );
        model.select(b);
        assert_eq!(
            model.selection_stats(),
            SelectionStats {
                count: 2,
                total_bytes: 350
            }
        );
        // Selecting an already-selected id is a no-op, not double-counted.
        model.select(a);
        assert_eq!(model.selection_stats().total_bytes, 350);

        model.deselect(a);
        assert_eq!(
            model.selection_stats(),
            SelectionStats {
                count: 1,
                total_bytes: 250
            }
        );

        model.toggle_selection(a);
        model.toggle_selection(b);
        assert_eq!(
            model.selection_stats(),
            SelectionStats {
                count: 1,
                total_bytes: 100
            }
        );

        model.clear_selection();
        assert_eq!(model.selection_stats(), SelectionStats::default());
    }

    #[test]
    fn selecting_500k_then_resorting_preserves_the_exact_set_by_id() {
        use std::collections::HashSet;

        const N: usize = 500_000;
        let mut model = DirectoryModel::new();
        let mut ids = Vec::with_capacity(N);
        for i in 0..N {
            let mut meta = Metadata::minimal(EntryKind::File);
            meta.size = i as u64;
            ids.push(
                model
                    .entries_mut()
                    .push(&format!("entry_{i:07}.txt"), &meta),
            );
        }
        model.sort_by(SortColumn::Name, true);

        // Select every other entry -- 250k of the 500k, by EntryId, before
        // any sort has happened relative to this selection pass.
        model.select_many(ids.iter().copied().step_by(2).collect::<Vec<_>>());
        let expected: HashSet<u32> = ids.iter().step_by(2).map(|id| id.index()).collect();
        assert_eq!(model.selection_stats().count as usize, expected.len());

        let bytes_before = model.selection_stats().total_bytes;

        // Re-sort by a different column, then a different direction --
        // this permutes `order`/`full_order` completely.
        model.sort_by(SortColumn::Size, true);
        let selected_after_size_sort: HashSet<u32> = model.selection().iter().collect();
        assert_eq!(selected_after_size_sort, expected);
        assert_eq!(model.selection_stats().total_bytes, bytes_before);

        model.sort_by(SortColumn::Name, false);
        let selected_after_name_desc: HashSet<u32> = model.selection().iter().collect();
        assert_eq!(selected_after_name_desc, expected);
        assert_eq!(model.selection_stats().total_bytes, bytes_before);
    }

    #[test]
    fn selection_stats_update_within_budget_after_a_selection_change() {
        use std::time::Instant;

        const N: usize = 500_000;
        let mut model = DirectoryModel::new();
        let mut ids = Vec::with_capacity(N);
        for i in 0..N {
            let mut meta = Metadata::minimal(EntryKind::File);
            meta.size = 1;
            ids.push(
                model
                    .entries_mut()
                    .push(&format!("entry_{i:07}.txt"), &meta),
            );
        }
        model.sort_by(SortColumn::Name, true);

        let start = Instant::now();
        model.select_many(ids);
        let select_elapsed = start.elapsed();

        let start = Instant::now();
        let stats = model.selection_stats();
        let read_elapsed = start.elapsed();

        assert_eq!(stats.count as usize, N);
        assert_eq!(stats.total_bytes as usize, N);
        // T-3.2.4's AC: "selection statistics... update in <= 5ms after a
        // selection change". `selection_stats()` itself is an O(1) struct
        // copy (see its doc comment) so it's trivially inside budget; what
        // this actually exercises is that reading stats right after a
        // large selection change doesn't require any deferred/batched
        // recomputation to catch up first.
        if !cfg!(debug_assertions) {
            assert!(
                read_elapsed.as_millis() <= 5,
                "selection_stats() took {read_elapsed:?} after selecting {N} entries \
                 ({select_elapsed:?} to select) -- expected an O(1) read"
            );
        }
    }

    #[test]
    fn tombstoning_a_selected_entry_prunes_it_from_selection_on_resort() {
        let mut model = DirectoryModel::new();
        let mut meta = Metadata::minimal(EntryKind::File);
        meta.size = 42;
        let a = model.entries_mut().push("a", &meta);
        let b = model.entries_mut().push("b", &meta);
        model.select(a);
        model.select(b);
        assert_eq!(model.selection_stats().total_bytes, 84);

        model.entries_mut().remove(a);
        model.sort_by(SortColumn::Name, true);

        assert!(!model.is_selected(a));
        assert!(model.is_selected(b));
        assert_eq!(model.selection_stats().total_bytes, 42);
    }
}
