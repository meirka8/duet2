// SPDX-License-Identifier: MIT
//! `FileTable`: the real, virtualised directory-listing view (T-4.2.1),
//! replacing one of `workspace.rs`'s two placeholder panels.
//!
//! This is "S-1's spike, for real" (`spikes/s1-virtualised-table/`, Phase
//! 0): a `duet_widgets::table::TableDelegate` implementation that reads a
//! real `duet_index::DirectoryModel`/`EntryStore` -- the Phase 3 SoA panel
//! model, not a parallel/duplicate structure built for this view -- by
//! index, directly from its columns, with no per-row heap allocation
//! during render/scroll (design.md §9.5: "FileTable ... reads directly
//! from the EntryStore columns by index. No per-frame allocation, no
//! per-row `String` formatting: size and date strings are formatted into a
//! per-frame arena and cached by value").
//!
//! # The text-cache strategy, and how it maps onto design.md §9.5's wording
//!
//! `EntryStore` stores raw `u64` sizes and `i64` mtimes, not display text --
//! formatting has to happen somewhere. This delegate ([`FileTableDelegate`])
//! formats every row's size/modified text (and, for the same
//! zero-per-frame-allocation reason, wraps its name) exactly once per
//! [`DirectoryModel`] generation (population/sort/filter), not once per
//! frame: [`FileTableDelegate::rebuild_row_text`] does one linear pass
//! writing through a single reused scratch buffer (the "arena" -- sized
//! once, at most a handful of bytes, and never freed for the delegate's
//! lifetime) and commits each result as an owned [`gpui::SharedString`]
//! (one `Arc<str>` allocation) into a per-row cache. Every subsequent
//! frame's `render_td` reads that cache and clones the `SharedString` --
//! an `Arc` refcount bump, not a heap allocation -- so scrolling among
//! already-cached rows is genuinely allocation-free, which is what the
//! AC's counting-allocator check (`examples/bench_file_table.rs`) verifies.
//!
//! The name column gets the same treatment even though it needs no
//! *formatting*: `EntryStore::name` returns a `&str` borrowed from the
//! store's internal `NameArena`, and GPUI's `IntoElement` bound needs
//! either a `String`/`SharedString` or a genuinely `'static` `&str` --
//! S-1's spike had the latter (its arena was `Box::leak`ed, safe there
//! because that spike's data was static for the process's whole life).
//! Duet's model is not: a directory can be re-listed, replacing the whole
//! `EntryStore`, so leaking would leak unboundedly across repopulations.
//! Caching an owned `SharedString` per name once, alongside size/date,
//! avoids both the leak and any unsafe lifetime extension.

use std::fmt::Write as _;
use std::path::PathBuf;

use duet_index::{DirectoryModel, SortColumn};
use duet_types::{EntryId, EntryKind, UnixPathBuf, VPath};
use duet_vfs::{DirEntry, FileSystem, ListFields, ListOpts, LocalFs};
use duet_widgets::table::{
    Column, ColumnSort, Table, TableDelegate, TableEvent, TableRow, TableState,
};
use duet_widgets::theme::TokenPalette;
use futures_util::StreamExt;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString, Styled as _, Window,
    actions, div, px,
};

/// Column indices this delegate ships with -- a reasonable subset
/// (name/size/modified). The full column set (permissions, owner,
/// extension, git status, ...) is T-4.2.4's job; see `Column::new` calls
/// in [`FileTableDelegate::new`] for the exact three.
const COL_NAME: usize = 0;
const COL_SIZE: usize = 1;
const COL_MODIFIED: usize = 2;

/// `duet_widgets::table::TableDelegate::render_last_empty_col`'s default
/// (`gpui-component-0.5.1/src/table/delegate.rs`) unconditionally appends
/// a `w_3()` (12px) filler column after the real ones, plus the table's
/// own `.bordered(true)` border -- so a column-width sum that exactly
/// matches the measured panel width still overflows by a few pixels and
/// trips the built-in horizontal scrollbar. Reserved out of every
/// [`FileTableDelegate::apply_responsive_widths`] call so "fits exactly"
/// really means fits, with a little slack for rounding.
const TABLE_CHROME_RESERVE: f32 = 20.0;

/// Responsive column sizing: Size and Modified stay at their fixed ideal
/// widths always -- they're short, roughly constant-width content (a size
/// string, a date), nothing is gained by shrinking them. Name is the only
/// elastic column: it grows to fill whatever space Size+Modified leave
/// behind, and shrinks as the panel narrows, down to [`NAME_MIN`] --
/// narrower than that, filenames just get ellipsized (`render_td` applies
/// `.truncate()` to the Name cell), never truncated by hiding a column or
/// growing the panel. Below `NAME_MIN` even with Size+Modified included,
/// this falls back to fixed minimum widths for all three and lets
/// `duet_widgets::table::Table`'s built-in horizontal scrollbar take over
/// -- "impossibly narrow", not worth squeezing further.
///
/// Full per-column configuration (more columns, user-chosen order/widths)
/// is T-4.2.4's job.
mod responsive {
    pub const NAME_MIN: f32 = 60.0;
    pub const NAME_IDEAL: f32 = 360.0;
    pub const SIZE_WIDTH: f32 = 110.0;
    pub const MODIFIED_WIDTH: f32 = 170.0;

    /// Computes column widths (Name, Size, Modified) for `available`
    /// pixels of panel width. Never changes the *number* of columns --
    /// see the module doc comment.
    pub fn column_widths(available: f32) -> [f32; 3] {
        let name = available - SIZE_WIDTH - MODIFIED_WIDTH;
        if name >= NAME_MIN {
            [name, SIZE_WIDTH, MODIFIED_WIDTH]
        } else {
            [NAME_MIN, SIZE_WIDTH, MODIFIED_WIDTH]
        }
    }
}

/// Clones `base` (Name/Size/Modified, in that order, carrying their
/// sortable/alignment flags) and overwrites each clone's width from
/// `widths`. Shared by [`FileTableDelegate::new`] (the narrow first-frame
/// widths) and [`FileTableDelegate::apply_responsive_widths`] (every
/// subsequent recomputation) so the two never drift out of sync on column
/// order/count.
fn columns_with_widths(base: &[Column], widths: [f32; 3]) -> Vec<Column> {
    base.iter()
        .cloned()
        .zip(widths)
        .map(|(mut col, w)| {
            col.width = px(w);
            col
        })
        .collect()
}

/// One row's pre-formatted display text -- see the module doc comment for
/// why every field is cached by value rather than recomputed per frame.
#[derive(Clone, Default)]
struct RowText {
    name: SharedString,
    size: SharedString,
    modified: SharedString,
}

/// `duet_widgets::table::TableDelegate` (gpui-component's `TableDelegate`,
/// reached only through the façade -- R-G7) over a real
/// `duet_index::DirectoryModel`. Reads `order`/`EntryStore` columns by
/// index (never scans, never rebuilds a parallel row list) and only ever
/// allocates when [`Self::rebuild_row_text`] runs -- once per model
/// generation, never per frame. See the module doc comment.
pub struct FileTableDelegate {
    model: DirectoryModel,
    columns: Vec<Column>,
    /// The Name/Size/Modified column definitions (sortable flags,
    /// alignment, ...) at their initial widths -- `columns` is rebuilt from
    /// this base (cloned, re-widthed) every time
    /// [`Self::apply_responsive_widths`] runs, so those flags never need to
    /// be re-specified by hand there.
    base_columns: Vec<Column>,
    /// The panel width `columns` was last computed for -- lets
    /// `apply_responsive_widths` skip recomputation (and the `cx.notify()`
    /// that would follow) when the measured width hasn't materially
    /// changed since the last frame.
    last_available_width: Option<f32>,
    /// `row_text[row_ix]` corresponds to `model.order()[row_ix]` as of
    /// `cached_generation`. See the module doc comment.
    row_text: Vec<RowText>,
    cached_generation: u64,
    /// Sum of `entries.size(id)` over every non-directory entry currently
    /// in view (T-4.2.3, `workspace::status_bar_row`'s "N of M items
    /// selected, X of Y bytes" -- FR-SEL-05). Directories are excluded,
    /// matching `write_size`'s own "a directory's raw stored size isn't a
    /// meaningful byte count, that's what the deferred Space-key
    /// recursive-size feature is for" convention. Computed inside
    /// `rebuild_row_text`'s existing per-generation pass over `order()`
    /// rather than a separate scan -- selection changes (which don't bump
    /// `generation()`) never re-trigger it, so reading this on every
    /// footer render (i.e. on every selection change) is genuinely O(1).
    total_bytes_in_view: u64,
    /// Reused scratch buffer for every `write!`-based size/date format in
    /// [`Self::rebuild_row_text`] -- cleared and reused per entry rather
    /// than allocating a fresh `String` each time. Never shrinks once
    /// grown, so after the first few rows it costs nothing further.
    scratch: String,
    /// `true` until the first real model arrives ([`Self::set_model`]) --
    /// drives `gpui_component`'s built-in loading-skeleton view
    /// (`TableDelegate::loading`) while a background directory listing is
    /// still in flight.
    loading: bool,
    /// The cursor's current row position within `model.order()` (T-4.2.2),
    /// cached alongside `model`'s own `EntryId`-based cursor rather than
    /// recomputed from it. Keyboard movement (`move_cursor_by`/
    /// `move_cursor_to`) reads and writes this directly -- an `O(1)` row
    /// computation, not a scan over `order()` -- which is what keeps
    /// holding an arrow key smooth even over a near-1M-row listing (the
    /// same reasoning `selected_bytes` documents for selection stats).
    /// Only ever goes stale across a sort (`order()` itself changes), and
    /// [`Self::perform_sort`] -- called by the user sorting, not per
    /// frame -- re-derives it there with the one `O(n)` scan that's
    /// actually necessary ([`Self::sync_cursor_row_from_model`]).
    cursor_row: Option<usize>,
    /// The fixed end of an in-progress Shift+movement range-selection
    /// (T-4.2.3), or `None` when no such session is active. Set on the
    /// first `extend_selection_to` after a reset, held fixed across
    /// repeated Shift+movements (so extending then shrinking back removes
    /// exactly what the same session added), and cleared by any *plain*
    /// cursor movement ([`Self::move_cursor_by`]/[`Self::move_cursor_to`])
    /// -- TC keeps cursor and selection independent (FR-SEL-01), so a
    /// plain arrow key never touches selection itself, but it does end
    /// the current range-select session, matching Explorer/TC's own
    /// behaviour of starting a fresh anchor at wherever the cursor lands
    /// next.
    range_anchor: Option<usize>,
}

impl FileTableDelegate {
    /// Builds a delegate over `model` (which may be empty -- see
    /// `loading`'s doc comment for the "still populating" case).
    pub fn new(model: DirectoryModel) -> Self {
        let base_columns = vec![
            Column::new("name", "Name")
                .width(px(responsive::NAME_IDEAL))
                .sortable(),
            // Not `.text_right()` -- see `render_td`'s doc comment: that
            // builder sets `Column::align`, which nothing in
            // `gpui-component-0.5.1`'s table rendering ever reads.
            Column::new("size", "Size")
                .width(px(responsive::SIZE_WIDTH))
                .sortable(),
            Column::new("modified", "Modified")
                .width(px(responsive::MODIFIED_WIDTH))
                .sortable(),
        ];
        // Start at NAME_MIN, not NAME_IDEAL: `duet_widgets::resizable`'s
        // `ResizablePanel` treats a panel with no stored size yet as
        // non-shrinkable on its very first render (`flex_none()` in
        // `gpui-component-0.5.1/src/resizable/panel.rs`), and an
        // automatic min-content size still applies underneath that --
        // wide enough first-frame content could in principle inflate a
        // heavily off-center splitter ratio's panel width beyond what the
        // ratio itself asks for. Cheap insurance against that: start
        // narrow, and let `FileTable::render`'s measuring canvas correct
        // `columns` to the real available width within a frame or two,
        // same as any other resize.
        let columns = columns_with_widths(
            &base_columns,
            [
                responsive::NAME_MIN,
                responsive::SIZE_WIDTH,
                responsive::MODIFIED_WIDTH,
            ],
        );
        let mut delegate = Self {
            model,
            columns,
            base_columns,
            last_available_width: None,
            row_text: Vec::new(),
            cached_generation: u64::MAX, // guarantees the first rebuild runs
            total_bytes_in_view: 0,
            scratch: String::new(),
            loading: true,
            cursor_row: None,
            range_anchor: None,
        };
        delegate.rebuild_row_text();
        delegate.set_cursor_row(Some(0));
        delegate
    }

    /// Recomputes `columns` for `available` pixels of real panel width
    /// (see `FileTable::render`'s measuring `canvas`) -- see the
    /// `responsive` module doc comment. Returns whether anything actually
    /// changed, so the caller only `cx.notify()`s on a real change rather
    /// than every frame.
    fn apply_responsive_widths(&mut self, available: f32) -> bool {
        if let Some(last) = self.last_available_width
            && (last - available).abs() < 1.0
        {
            return false;
        }
        self.last_available_width = Some(available);

        let usable = (available - TABLE_CHROME_RESERVE).max(0.0);
        self.columns = columns_with_widths(&self.base_columns, responsive::column_widths(usable));
        true
    }

    pub fn model(&self) -> &DirectoryModel {
        &self.model
    }

    /// Mutable access to the backing model -- e.g. for
    /// `examples/bench_file_table.rs` to pre-select a stride of rows before
    /// timing a scroll, exercising the multi-selection highlight path
    /// `render_tr` reads on every frame. Selection changes don't bump
    /// `DirectoryModel::generation()`, so callers that only touch selection
    /// through this don't need to worry about invalidating `row_text`
    /// (which caches name/size/date text, unrelated to selection).
    pub fn model_mut(&mut self) -> &mut DirectoryModel {
        &mut self.model
    }

    /// Replaces the backing model wholesale (a fresh directory listing
    /// finished loading, or the 1M-row synthetic benchmark corpus) and
    /// forces the text cache to rebuild for it. Cursor resets to the first
    /// row (TC's own behaviour on entering/refreshing a listing) --
    /// there's no previous row position that would still mean anything
    /// against an entirely new entry set.
    pub fn set_model(&mut self, model: DirectoryModel) {
        self.model = model;
        self.cached_generation = u64::MAX;
        self.rebuild_row_text();
        self.set_cursor_row(Some(0));
        self.range_anchor = None;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// The cursor's current row within `model.order()`, if any (an empty
    /// listing has none).
    pub fn cursor_row(&self) -> Option<usize> {
        self.cursor_row
    }

    /// Sets the cursor to `row` (clamped into range) and keeps
    /// `model`'s own `EntryId`-based cursor in lock-step -- the two must
    /// never disagree, so every write to either goes through this one
    /// method rather than setting `self.cursor_row` or calling
    /// `model.set_cursor` directly.
    fn set_cursor_row(&mut self, row: Option<usize>) {
        let row = row.filter(|_| !self.model.order().is_empty());
        self.cursor_row = row;
        let id = row
            .and_then(|r| self.model.order().get(r))
            .copied()
            .map(EntryId::new);
        self.model.set_cursor(id);
    }

    /// Moves the cursor by `delta` rows (negative for up), clamped to
    /// `[0, order().len() - 1]`. Returns the resulting row so the caller
    /// (`FileTable`'s action handlers) can decide whether to scroll it
    /// into view -- a no-op (empty listing) returns `None`. Ends any
    /// in-progress range-select session -- see the `range_anchor` field's
    /// doc comment.
    fn move_cursor_by(&mut self, delta: i64) -> Option<usize> {
        let len = self.model.order().len();
        if len == 0 {
            return None;
        }
        self.range_anchor = None;
        let current = self.cursor_row.unwrap_or(0) as i64;
        let target = (current + delta).clamp(0, len as i64 - 1) as usize;
        self.set_cursor_row(Some(target));
        self.cursor_row
    }

    /// Moves the cursor directly to `row`, clamped into range (so
    /// `usize::MAX` is a convenient "last row" for End/Ctrl+End). Ends any
    /// in-progress range-select session, same as `move_cursor_by`.
    fn move_cursor_to(&mut self, row: usize) -> Option<usize> {
        let len = self.model.order().len();
        if len == 0 {
            return None;
        }
        self.range_anchor = None;
        self.set_cursor_row(Some(row.min(len - 1)));
        self.cursor_row
    }

    /// Shift+movement's range-select: extends/shrinks the selection
    /// between a fixed anchor (established the first time this runs after
    /// a reset, held fixed across repeated calls) and `row`, leaving any
    /// selection made by other means (Ins, mask, ...) outside that range
    /// untouched. See the `range_anchor` field's doc comment for the full
    /// algorithm walkthrough. A no-op on an empty listing.
    fn extend_selection_to(&mut self, row: usize) {
        let len = self.model.order().len();
        if len == 0 {
            return;
        }
        let row = row.min(len - 1);
        let old_row = self.cursor_row.unwrap_or(row);
        let anchor = *self.range_anchor.get_or_insert(old_row);

        let prev_lo = anchor.min(old_row);
        let prev_hi = anchor.max(old_row);
        let new_lo = anchor.min(row);
        let new_hi = anchor.max(row);

        for r in prev_lo..=prev_hi {
            if (r < new_lo || r > new_hi)
                && let Some(&ix) = self.model.order().get(r)
            {
                self.model.deselect(EntryId::new(ix));
            }
        }
        for r in new_lo..=new_hi {
            if let Some(&ix) = self.model.order().get(r) {
                self.model.select(EntryId::new(ix));
            }
        }
        self.set_cursor_row(Some(row));
    }

    /// Ins/Space (T-4.2.3): toggles selection of the entry at the cursor.
    /// Ins additionally advances the cursor afterward
    /// (`FileTable::on_action`'s job, via `move_cursor_by`); Space's
    /// TC behaviour of also computing and showing a directory's size is
    /// deferred -- it needs `duet_index::size_service::DirSizeService`
    /// wired up asynchronously (design.md §8.2: never block the UI
    /// thread) and a place to display the computed value, both bigger
    /// than this task's "selection rendering + commands" scope.
    fn toggle_cursor_selection(&mut self) {
        if let Some(row) = self.cursor_row
            && let Some(&ix) = self.model.order().get(row)
        {
            self.model.toggle_selection(EntryId::new(ix));
        }
    }

    /// Num* (T-4.2.3): flips every currently-visible entry's selection
    /// state. `O(n)` in the visible entry count -- fine here the same way
    /// `sync_cursor_row_from_model`'s scan is: this runs once per
    /// keypress, not per frame.
    fn invert_selection(&mut self) {
        let ids: Vec<EntryId> = self
            .model
            .order()
            .iter()
            .copied()
            .map(EntryId::new)
            .collect();
        for id in ids {
            self.model.toggle_selection(id);
        }
    }

    /// Ctrl+Num+ (T-4.2.3): selects every currently-visible entry.
    fn select_all(&mut self) {
        let ids: Vec<EntryId> = self
            .model
            .order()
            .iter()
            .copied()
            .map(EntryId::new)
            .collect();
        self.model.select_many(ids);
    }

    /// Ctrl+Num- (T-4.2.3): unconditionally clears the selection.
    fn deselect_all(&mut self) {
        self.model.clear_selection();
    }

    /// Shift+Num+ (T-4.2.3, per `docs/keymap-tc.csv`'s `sel.by_same_ext`
    /// row -- registered in `docs/commands.md` as `sel.same_extension`;
    /// the two names disagreeing is the same keymap-CSV/catalogue
    /// inconsistency T-3.3.2 already found and documented, not a new
    /// gap): selects every entry sharing the cursor entry's extension.
    /// A no-op if the cursor entry has none.
    fn select_same_extension(&mut self) {
        let Some(cursor_name) = self
            .cursor_row
            .and_then(|row| self.model.order().get(row))
            .map(|&ix| self.model.entries().name(EntryId::new(ix)).to_string())
        else {
            return;
        };
        let Some(ext) = std::path::Path::new(&cursor_name)
            .extension()
            .and_then(|e| e.to_str())
        else {
            return;
        };

        let ids: Vec<EntryId> = self
            .model
            .order()
            .iter()
            .copied()
            .map(EntryId::new)
            .filter(|&id| {
                std::path::Path::new(self.model.entries().name(id))
                    .extension()
                    .and_then(|e| e.to_str())
                    == Some(ext)
            })
            .collect();
        self.model.select_many(ids);
    }

    // FR-SEL-03 also lists "select all with same name" (matching by base
    // name/stem, not extension -- `photo.jpg`/`photo.png`). Not
    // implemented here: `docs/keymap-tc.csv` has no row for it at all
    // (unlike `sel.same_extension`'s at least approximate "Shift+Num +"),
    // and there's nothing to invoke it -- no command palette (T-4.3.6) or
    // keymap-CSV-to-action bridge exists yet for a command with no direct
    // key. An unreachable method is scope creep dressed up as progress;
    // left for whichever task actually gives it a way to run.

    /// Re-derives `cursor_row` from `model`'s `EntryId`-based cursor after
    /// `order()` itself changes (a resort) -- the one place an `O(n)` scan
    /// over `order()` is actually necessary, and it's fine here because
    /// sorting is already `O(n log n)` and user-triggered, not a per-frame
    /// cost. See the `cursor_row` field's doc comment.
    fn sync_cursor_row_from_model(&mut self) {
        self.cursor_row = self
            .model
            .cursor()
            .and_then(|id| self.model.order().iter().position(|&ix| ix == id.index()));
        // A resort invalidates row positions out from under any
        // in-progress range-select session just as much as it does the
        // cursor -- see the `range_anchor` field's doc comment.
        self.range_anchor = None;
    }

    /// Rebuilds `row_text` for the current `model.order()`/`generation()`
    /// if (and only if) it's stale. `O(n)` in the visible entry count, run
    /// from population/sort/filter call sites -- never from `render_td`/
    /// `render_tr`, which is what keeps this off the per-frame path.
    fn rebuild_row_text(&mut self) {
        if self.cached_generation == self.model.generation()
            && self.row_text.len() == self.model.order().len()
        {
            return;
        }

        self.row_text.clear();
        self.row_text.reserve(self.model.order().len());
        self.total_bytes_in_view = 0;
        for &ix in self.model.order() {
            let id = EntryId::new(ix);
            let entries = self.model.entries();
            let kind = entries.kind(id);

            let name = SharedString::new(entries.name(id));

            self.scratch.clear();
            write_size(&mut self.scratch, kind, entries.size(id));
            let size = SharedString::new(self.scratch.as_str());
            if kind != EntryKind::Directory {
                self.total_bytes_in_view += entries.size(id);
            }

            self.scratch.clear();
            write_date(&mut self.scratch, entries.mtime_secs(id));
            let modified = SharedString::new(self.scratch.as_str());

            self.row_text.push(RowText {
                name,
                size,
                modified,
            });
        }
        self.cached_generation = self.model.generation();
    }

    /// See the `total_bytes_in_view` field's doc comment.
    pub fn total_bytes_in_view(&self) -> u64 {
        self.total_bytes_in_view
    }
}

impl TableDelegate for FileTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.model.order().len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        let column = match col_ix {
            COL_SIZE => SortColumn::Size,
            COL_MODIFIED => SortColumn::Modified,
            _ => SortColumn::Name,
        };
        let ascending = !matches!(sort, ColumnSort::Descending);
        self.model.sort_by(column, ascending);
        self.rebuild_row_text();
        self.sync_cursor_row_from_model();
    }

    /// Row container: the only per-row work is an `order`/selection-bitmap
    /// lookup and a conditional background color -- no string formatting,
    /// no allocation (matching S-1 spike's `render_tr`).
    /// Cursor and selection are visually distinct (TC convention, and
    /// `docs/config-schema.md` §4 gives them separate tokens for exactly
    /// this): selection is a subtle background tint that can span many
    /// rows, the cursor is a single, strongly-highlighted row showing
    /// where keyboard commands (T-4.2.2 movement now, T-4.2.3 selection
    /// commands next) actually act. A row can be both -- the cursor's
    /// stronger fill wins in that case, same as TC's own rendering.
    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> TableRow {
        let is_cursor = self.cursor_row == Some(row_ix);
        let selected = self
            .model
            .order()
            .get(row_ix)
            .copied()
            .is_some_and(|ix| self.model.is_selected(EntryId::new(ix)));

        let row = div().id(("file-row", row_ix));
        let tokens = TokenPalette::current(cx);
        if is_cursor {
            row.bg(tokens.color.cursor_bg)
                .text_color(tokens.color.cursor_fg)
        } else if selected {
            row.bg(tokens.color.selection_bg)
        } else {
            row
        }
    }

    /// Cell content: a `SharedString` clone out of `row_text` -- an `Arc`
    /// refcount bump, never a heap allocation. See the module doc comment.
    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self
            .row_text
            .get(row_ix)
            .map(|row| match col_ix {
                COL_NAME => row.name.clone(),
                COL_SIZE => row.size.clone(),
                COL_MODIFIED => row.modified.clone(),
                _ => SharedString::default(),
            })
            .unwrap_or_default();

        // `.truncate()` (overflow-hidden + nowrap + ellipsis) matters most
        // for Name, the one column that shrinks (see the `responsive`
        // module doc comment) -- a long filename in a narrow panel gets a
        // "…" instead of being clipped mid-glyph or forcing overflow.
        // Size/Modified are short, fixed-width strings this never
        // actually triggers for, so applying it unconditionally is simpler
        // than a per-column special case.
        //
        // Right-alignment can't come from `Column::text_right()` --
        // `duet_widgets::table::Column::align` is set by that call but
        // never read anywhere in `gpui-component-0.5.1`'s table rendering
        // (confirmed by reading the crate source: no `.align` reference
        // outside the setter itself), so it's a dead field. Applying
        // `gpui::Styled::text_right()` directly to this cell -- a
        // different, unrelated `text_right` that genuinely sets the
        // element's own text-alignment style -- is what actually works.
        // `w_full()` first so there's room within the cell to align into
        // (this div would otherwise shrink-wrap to the text's own width).
        let mut cell = div()
            .id(("file-cell", row_ix as u64 * 8 + col_ix as u64))
            .w_full()
            .px_2()
            .truncate();
        if col_ix == COL_SIZE || col_ix == COL_MODIFIED {
            cell = cell.text_right();
        }
        cell.child(text)
    }

    fn loading(&self, _cx: &App) -> bool {
        self.loading
    }
}

/// Directories show `<DIR>` (a Total-Commander-style convention) rather
/// than a byte count -- `Metadata::size`'s own doc comment notes it's not
/// meaningful for directories on backends that don't report a real one.
fn write_size(out: &mut String, kind: EntryKind, bytes: u64) {
    if kind == EntryKind::Directory {
        out.push_str("<DIR>");
        return;
    }
    write_byte_count(out, bytes);
}

/// The byte-count half of [`write_size`], split out so
/// `workspace::status_bar_row`'s "N items, X bytes selected" (T-4.2.3)
/// can format a *sum* of bytes the same way without dragging in an
/// `EntryKind` that doesn't apply to a total.
pub(crate) fn write_byte_count(out: &mut String, bytes: u64) {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        out.push_str("0 B");
        return;
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        let _ = write!(out, "{bytes} B");
    } else {
        let _ = write!(out, "{size:.1} {}", UNITS[unit]);
    }
}

/// Unix-seconds -> `YYYY-MM-DD HH:MM`, via Howard Hinnant's `civil_from_days`
/// algorithm -- the same one S-1's spike used
/// (`spikes/s1-virtualised-table/src/store.rs`), reused here rather than
/// pulling in a date/time crate dependency for one display format.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, hour, minute)
}

fn write_date(out: &mut String, mtime_secs: i64) {
    if mtime_secs == 0 {
        out.push('-');
        return;
    }
    let (y, mo, d, hh, mm) = civil_from_unix(mtime_secs);
    let _ = write!(out, "{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}");
}

// T-4.2.2's keyboard cursor movement, per `docs/keymap-tc.csv`'s
// `nav.cursor_up`/`nav.cursor_down`/`nav.cursor_top`/`nav.cursor_bottom`/
// `nav.page_up`/`nav.page_down` rows -- Home and Ctrl+Home (same for
// End/Ctrl+End) both bind to the same action since the CSV itself marks
// the Ctrl variants "uncertain... may be redundant with plain Home/End".
//
// These are plain GPUI actions bound directly to keys, the same shape
// T-4.1.4's `ResizeSplitterLeft`/`ResizeSplitterRight` already established
// in `workspace.rs`, not routed through `duet-commands`' registry/
// predicate/keymap-CSV pipeline. That pipeline exists and the CSV is what
// these bindings are faithful to, but nothing yet turns a loaded keymap
// entry into a live GPUI keybinding at runtime -- building that generic
// bridge is bigger than "cursor rendering, keyboard movement" and belongs
// to whichever task first needs more than a handful of hand-wired
// commands (T-4.3.x territory), not this one.
actions!(
    duet_file_table,
    [
        CursorUp,
        CursorDown,
        CursorHome,
        CursorEnd,
        CursorPageUp,
        CursorPageDown
    ]
);

// T-4.2.3's selection commands, per `docs/keymap-tc.csv`'s `sel.*`/
// `unsel.*` rows -- same "plain GPUI action, not routed through
// duet-commands" shape and rationale as T-4.2.2's cursor actions above.
//
// Deliberately not everything FR-SEL-02/03 lists: `sel.by_mask`/
// `unsel.by_mask` (Numpad +/-) need a wildcard-pattern prompt dialog,
// `sel.mask_history_show` a history dropdown -- both real new UI surface,
// not just another action/keybinding pair, so they're left for a
// follow-up rather than stretching this task to cover them. Space's TC
// behaviour of also computing a directory's size is deferred for the same
// reason (needs `duet_index::size_service` wired up asynchronously plus
// somewhere to show the result) -- `ToggleSelection` only toggles.
// `sel.same_name` has no `FileTableDelegate::select_same_name` binding
// here either: `docs/keymap-tc.csv` has no row for it at all (see that
// method's doc comment).
actions!(
    duet_file_table,
    [
        ToggleSelectionAndAdvance,
        ToggleSelection,
        InvertSelection,
        SelectAll,
        DeselectAll,
        SelectSameExtension,
        ExtendSelectionUp,
        ExtendSelectionDown,
        ExtendSelectionToTop,
        ExtendSelectionToBottom,
        ExtendSelectionPageUp,
        ExtendSelectionPageDown
    ]
);

/// Registers [`FileTable`]'s keybindings. Called once from
/// `workspace::run`, before any window opens -- see `bind_workspace_keys`
/// for the identical pattern this mirrors. `Some("FileTable")` scopes
/// every binding to elements tagged with that key context (`FileTable`'s
/// own render root), so they only fire while a file table has focus.
///
/// The Numpad `+`/`-`/`*` bindings below use the plain `"+"`/`"-"`/`"*"`
/// keys rather than a numpad-specific name: `gpui-0.2.2`'s keystroke
/// parser has no distinct `kp_*` identifiers (confirmed by reading
/// `gpui-0.2.2/src/platform/keystroke.rs`), so this covers both the
/// numpad and the top-row keys where the platform doesn't distinguish
/// them -- reasonable on its own, and necessary anyway on numpad-less
/// keyboards. Unverified against a real keypress the same way T-4.2.2's
/// bindings were (see that PR) -- needs UAT.
pub fn bind_file_table_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", CursorUp, Some("FileTable")),
        KeyBinding::new("down", CursorDown, Some("FileTable")),
        KeyBinding::new("home", CursorHome, Some("FileTable")),
        KeyBinding::new("ctrl-home", CursorHome, Some("FileTable")),
        KeyBinding::new("end", CursorEnd, Some("FileTable")),
        KeyBinding::new("ctrl-end", CursorEnd, Some("FileTable")),
        KeyBinding::new("pageup", CursorPageUp, Some("FileTable")),
        KeyBinding::new("pagedown", CursorPageDown, Some("FileTable")),
        KeyBinding::new("insert", ToggleSelectionAndAdvance, Some("FileTable")),
        KeyBinding::new("space", ToggleSelection, Some("FileTable")),
        KeyBinding::new("*", InvertSelection, Some("FileTable")),
        KeyBinding::new("ctrl-+", SelectAll, Some("FileTable")),
        KeyBinding::new("ctrl--", DeselectAll, Some("FileTable")),
        KeyBinding::new("shift-+", SelectSameExtension, Some("FileTable")),
        KeyBinding::new("shift-up", ExtendSelectionUp, Some("FileTable")),
        KeyBinding::new("shift-down", ExtendSelectionDown, Some("FileTable")),
        KeyBinding::new("shift-home", ExtendSelectionToTop, Some("FileTable")),
        KeyBinding::new("shift-end", ExtendSelectionToBottom, Some("FileTable")),
        KeyBinding::new("shift-pageup", ExtendSelectionPageUp, Some("FileTable")),
        KeyBinding::new("shift-pagedown", ExtendSelectionPageDown, Some("FileTable")),
    ]);
}

/// The real virtualised directory-table view: a thin `Render` wrapper
/// around `duet_widgets::table::TableState<FileTableDelegate>`, populated
/// from a real local directory listing via the core Tokio runtime --
/// design.md §8.2's "main thread does no I/O, ever", the same
/// executor-wiring pattern `workspace.rs`'s own T-4.1.1 demo
/// (`count_current_dir_entries`/`spawn_entry_count_demo`) already
/// established.
pub struct FileTable {
    state: Entity<TableState<FileTableDelegate>>,
    focus_handle: FocusHandle,
}

impl FileTable {
    /// Starts listing `dir` in the background and returns immediately with
    /// an empty, `loading` table -- `spawn_directory_load` populates it
    /// once the listing completes.
    pub fn new(
        dir: PathBuf,
        tokio_handle: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = FileTableDelegate::new(DirectoryModel::new());
        let state = cx.new(|cx| TableState::new(delegate, window, cx));
        spawn_directory_load(dir, tokio_handle, state.clone(), cx);

        // `duet_widgets::table::TableState` has its own built-in
        // click-to-select row/column tracking (`selected_row`/
        // `selected_col`, set by an unconditional `on_click` handler
        // baked into the widget itself -- `on_row_left_click` calls
        // `set_selected_row` with no gate, confirmed by reading
        // `gpui-component-0.5.1/src/table/state.rs`), completely separate
        // from `FileTableDelegate::cursor_row`/`model.selection()`. Left
        // alone, a click paints a persistent highlight (`table_active`)
        // that this delegate's own render_tr never reads and nothing
        // clears -- a confusing "second cursor" that doesn't respond to
        // any of T-4.2.2/T-4.2.3's real cursor/selection commands, since
        // mouse support isn't wired up yet (T-4.3.8). Subscribing to
        // `TableEvent` and immediately clearing it back neutralises that
        // stray highlight until real click-driven cursor movement is
        // built and can intentionally use this same mechanism.
        cx.subscribe(&state, |_this, state, event, cx| {
            if matches!(
                event,
                TableEvent::SelectRow(_) | TableEvent::SelectColumn(_)
            ) {
                state.update(cx, |state, cx| state.clear_selection(cx));
            }
        })
        .detach();

        Self {
            state,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Exposes the underlying table state -- e.g. for a future status-bar
    /// selection-stats readout (T-4.2.7) that needs to reach
    /// `TableState::delegate().model()` from outside this view.
    pub fn state(&self) -> &Entity<TableState<FileTableDelegate>> {
        &self.state
    }

    /// Moves the cursor by `delta` rows and scrolls it into view if (and
    /// only if) the move actually took it outside the currently visible
    /// range -- an in-view move (the common case while holding an arrow
    /// key) never forces a scroll, since `TableState::scroll_to_row`'s
    /// only placement option is `ScrollStrategy::Top`, which would
    /// otherwise jump the list on every single keystroke instead of only
    /// when the cursor is actually about to leave the viewport.
    fn move_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if let Some(row) = state.delegate_mut().move_cursor_by(delta) {
                if !state.visible_range().rows().contains(&row) {
                    state.scroll_to_row(row, cx);
                }
                cx.notify();
            }
        });
    }

    /// PgUp/PgDn: same in-view-check as [`Self::move_cursor`], just with
    /// the delta computed from the table's own currently visible row
    /// count instead of a fixed ±1 -- `visible_range()` has to be read
    /// before the move (not after, like the in-view check), so this can't
    /// simply call `move_cursor` with a precomputed delta.
    fn move_cursor_by_page(&mut self, direction: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let page = state.visible_range().rows().len().max(1) as i64;
            if let Some(row) = state.delegate_mut().move_cursor_by(direction * page) {
                if !state.visible_range().rows().contains(&row) {
                    state.scroll_to_row(row, cx);
                }
                cx.notify();
            }
        });
    }

    /// Home/Ctrl+Home (`row = 0`) and End/Ctrl+End (`row = usize::MAX`,
    /// clamped by `move_cursor_to` to the last row) both always scroll --
    /// jumping to either end is definitionally leaving the current view
    /// unless the whole listing already fits on screen, in which case
    /// `scroll_to_row` is a harmless no-op.
    fn move_cursor_to(&mut self, row: usize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if let Some(row) = state.delegate_mut().move_cursor_to(row) {
                state.scroll_to_row(row, cx);
                cx.notify();
            }
        });
    }

    /// Space: toggle the cursor entry's selection in place.
    fn toggle_selection(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.delegate_mut().toggle_cursor_selection();
            cx.notify();
        });
    }

    /// Ins: toggle the cursor entry's selection, then advance -- the
    /// "mark and move on" gesture TC's own Ins key does.
    fn toggle_selection_and_advance(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.delegate_mut().toggle_cursor_selection();
            if let Some(row) = state.delegate_mut().move_cursor_by(1)
                && !state.visible_range().rows().contains(&row)
            {
                state.scroll_to_row(row, cx);
            }
            cx.notify();
        });
    }

    /// Runs `f` against the delegate and notifies -- the shared shape
    /// behind invert/select-all/deselect-all/same-extension, none of
    /// which move the cursor or need a scroll-into-view check.
    fn with_selection(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut FileTableDelegate)) {
        self.state.update(cx, |state, cx| {
            f(state.delegate_mut());
            cx.notify();
        });
    }

    /// Shift+Up/Down: range-select by one row, same in-view scroll check
    /// as plain cursor movement.
    fn extend_selection(&mut self, delta: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            let Some(len) = Some(delegate.model().order().len()).filter(|&len| len > 0) else {
                return;
            };
            let current = delegate.cursor_row().unwrap_or(0) as i64;
            let target = (current + delta).clamp(0, len as i64 - 1) as usize;
            delegate.extend_selection_to(target);
            if !state.visible_range().rows().contains(&target) {
                state.scroll_to_row(target, cx);
            }
            cx.notify();
        });
    }

    /// Shift+PgUp/PgDn: same page-size-aware delta as
    /// [`Self::move_cursor_by_page`], applied to range-select instead of
    /// plain movement.
    fn extend_selection_by_page(&mut self, direction: i64, cx: &mut Context<Self>) {
        // Reads `page` through a non-exclusive borrow that ends before
        // `extend_selection` opens its own `self.state.update` -- calling
        // that from inside an outer `self.state.update` closure here
        // would double-borrow the same entity.
        let page = self.state.read(cx).visible_range().rows().len().max(1) as i64;
        self.extend_selection(direction * page, cx);
    }

    /// Shift+Home/End: range-select to either end, always scrolling --
    /// same reasoning as `move_cursor_to`'s doc comment.
    fn extend_selection_to_edge(&mut self, row: usize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if state.delegate().model().order().is_empty() {
                return;
            }
            state.delegate_mut().extend_selection_to(row);
            let row = state.delegate().cursor_row().expect("just set, non-empty");
            state.scroll_to_row(row, cx);
            cx.notify();
        });
    }
}

impl Focusable for FileTable {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        div()
            .size_full()
            .key_context("FileTable")
            .track_focus(&self.focus_handle)
            // `.track_focus()` only *registers* this div against
            // `focus_handle` -- it doesn't grab focus on click by itself,
            // and nothing else in the render tree does either (confirmed
            // by reading `gpui-component-0.5.1/src/table/state.rs`:
            // `TableState` has its own unused `focus_handle` field, never
            // `track_focus`ed anywhere, so it was never a focus target to
            // begin with). Without this, clicking the panel after focus
            // had moved elsewhere (Tab to the command line, say) left
            // focus right where it was -- explaining the reported "cursor
            // doesn't move with arrow keys after clicking the panel":
            // every `CursorUp`/`CursorDown`/etc. action above only fires
            // while this div's "FileTable" key context is actually
            // active, which requires focus to be somewhere in this
            // subtree. `on_mouse_down` (not `on_click`, which only fires
            // after a matching mouse-up) so focus moves immediately on
            // press, matching how clicking into any other focusable
            // widget behaves.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _event, window, _cx| {
                    window.focus(&this.focus_handle);
                }),
            )
            .on_action(cx.listener(|this, _: &CursorUp, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorDown, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorHome, _window, cx| {
                this.move_cursor_to(0, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorEnd, _window, cx| {
                this.move_cursor_to(usize::MAX, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorPageUp, _window, cx| {
                this.move_cursor_by_page(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorPageDown, _window, cx| {
                this.move_cursor_by_page(1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSelectionAndAdvance, _window, cx| {
                    this.toggle_selection_and_advance(cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleSelection, _window, cx| {
                this.toggle_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &InvertSelection, _window, cx| {
                this.with_selection(cx, FileTableDelegate::invert_selection);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                this.with_selection(cx, FileTableDelegate::select_all);
            }))
            .on_action(cx.listener(|this, _: &DeselectAll, _window, cx| {
                this.with_selection(cx, FileTableDelegate::deselect_all);
            }))
            .on_action(cx.listener(|this, _: &SelectSameExtension, _window, cx| {
                this.with_selection(cx, FileTableDelegate::select_same_extension);
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionUp, _window, cx| {
                this.extend_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionDown, _window, cx| {
                this.extend_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionToTop, _window, cx| {
                this.extend_selection_to_edge(0, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ExtendSelectionToBottom, _window, cx| {
                    this.extend_selection_to_edge(usize::MAX, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ExtendSelectionPageUp, _window, cx| {
                this.extend_selection_by_page(-1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ExtendSelectionPageDown, _window, cx| {
                    this.extend_selection_by_page(1, cx);
                }),
            )
            // 20% smaller than `gpui-component`'s 16px default, scoped to
            // just this panel (cascades into `Table`'s header/row text) --
            // not a global theme change, so the command line, status bar,
            // and function-key bar keep their own sizes.
            .text_size(px(12.8))
            // Flexbox's default `min-width: auto` would otherwise let
            // `Table`'s declared column widths act as this view's intrinsic
            // minimum content size, which could in principle grow the
            // resizable splitter's panel past what its ratio actually asks
            // for. The splitter should always dictate the available width,
            // never the other way around; pinning `min_w` to zero here
            // rules that out regardless of how wide any column gets.
            .min_w(px(0.))
            .child(Table::new(&self.state).stripe(true).bordered(true))
            .child(
                // Measures the panel's real width every frame (the same
                // canvas-based idiom `gpui-component`'s own
                // `ResizablePanelGroup` uses for
                // `adjust_to_container_size`, see
                // `gpui-component-0.5.1/src/resizable/panel.rs`) and feeds
                // it to `FileTableDelegate::apply_responsive_widths`. A
                // width change takes effect on the next frame, via
                // `TableState::refresh` -- not just `cx.notify()`, which
                // only repaints with whatever `col_groups` already holds.
                // `TableState` computes `col_groups` (the column layout it
                // actually renders from) once at construction and caches
                // it there permanently; `refresh()` is the only thing that
                // re-derives it from `TableDelegate::column()`, per that
                // method's own doc comment ("When we update columns or
                // rows, we need to refresh the table"). Skipping this call
                // silently strands every later width change -- confirmed
                // the hard way: this delegate's own `columns` field was
                // updating correctly all along, but the table kept
                // painting whatever `columns` looked like at construction.
                gpui::canvas(
                    move |bounds, _window, cx| {
                        state.update(cx, |state, cx| {
                            let delegate = state.delegate_mut();
                            if delegate.apply_responsive_widths(f32::from(bounds.size.width)) {
                                state.refresh(cx);
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

/// Lists `dir` on the core's Tokio runtime (never the GPUI/UI thread),
/// then applies the result to `state` through GPUI's foreground executor --
/// see the struct doc comment.
fn spawn_directory_load(
    dir: PathBuf,
    tokio_handle: tokio::runtime::Handle,
    state: Entity<TableState<FileTableDelegate>>,
    cx: &mut Context<FileTable>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio_handle.spawn(async move {
        let result = list_directory(dir).await;
        let _ = tx.send(result);
    });

    cx.spawn(async move |_this, cx| {
        let entries = match rx.await {
            Ok(Ok(entries)) => entries,
            Ok(Err(err)) => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "directory listing failed: {err}"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "directory-load task was dropped before completing"
                );
                return;
            }
        };

        let mut model = DirectoryModel::new();
        for entry in entries {
            model.entries_mut().push(&entry.name, &entry.metadata);
        }
        model.sort_by(SortColumn::Name, true);
        let entry_count = model.order().len();

        let updated = state.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            delegate.set_model(model);
            delegate.set_loading(false);
            cx.notify();
        });

        match updated {
            Ok(()) => {
                tracing::info!(
                    target: "duet_ui::file_table",
                    "directory listing loaded: {entry_count} entries"
                );
            }
            Err(_) => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "directory listing finished ({entry_count} entries) after the table view was dropped"
                );
            }
        }
    })
    .detach();
}

/// Lists `dir` through the real local VFS backend (`duet_vfs::LocalFs`),
/// requesting `MODIFIED` on top of the always-cheap `size`/`kind` (design.md
/// §9.1's `ListFields`) so the Modified column has real data, not a
/// placeholder. Collects the whole (chunked) stream before returning --
/// fine for a single local directory listing of the size this view targets;
/// a panel over a directory with hundreds of thousands of live entries
/// would want to apply chunks incrementally instead (a future refinement,
/// not required by this task's AC).
async fn list_directory(dir: PathBuf) -> Result<Vec<DirEntry>, String> {
    let path_str = dir
        .to_str()
        .ok_or_else(|| "directory path is not valid UTF-8".to_string())?;
    let vpath = VPath::local(
        UnixPathBuf::new(path_str).map_err(|e| format!("invalid path {path_str:?}: {e}"))?,
    );

    let fs = LocalFs;
    let opts = ListOpts {
        fields: ListFields::MODIFIED,
        follow_symlinks: false,
    };
    let mut stream = fs.read_dir(&vpath, opts);
    let mut entries = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read_dir: {e}"))?;
        entries.extend(chunk);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use duet_types::{EntryKind, Metadata, Timestamp};

    use super::*;

    fn meta(kind: EntryKind, size: u64, mtime_secs: i64) -> Metadata {
        let mut m = Metadata::minimal(kind);
        m.size = size;
        m.modified = Some(Timestamp::new(mtime_secs, 0));
        m
    }

    fn sample_model() -> DirectoryModel {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("b_dir", &meta(EntryKind::Directory, 0, 1_700_000_000));
        model
            .entries_mut()
            .push("a_file.txt", &meta(EntryKind::File, 2048, 1_700_000_060));
        model.sort_by(SortColumn::Name, true);
        model
    }

    /// `columns_count`/`rows_count` (the `TableDelegate` trait methods)
    /// just return `self.columns.len()`/`self.model.order().len()` -- see
    /// their impls above -- so this asserts the same facts directly on the
    /// delegate's own state rather than constructing a real `App` (which
    /// needs a running GPUI platform, unavailable under plain `cargo
    /// test`) purely to satisfy those methods' `&App` parameter.
    #[test]
    fn delegate_reads_row_count_and_columns_from_the_real_model() {
        let delegate = FileTableDelegate::new(sample_model());
        assert_eq!(delegate.columns.len(), 3);
        assert_eq!(delegate.model().order().len(), 2);
    }

    /// Five files named so name-ascending order is `f0, f1, f2, f3, f4` --
    /// plain, predictable row positions for the cursor-movement tests.
    fn five_file_model() -> DirectoryModel {
        let mut model = DirectoryModel::new();
        for n in 0..5 {
            model
                .entries_mut()
                .push(&format!("f{n}"), &meta(EntryKind::File, n as u64, 0));
        }
        model.sort_by(SortColumn::Name, true);
        model
    }

    #[test]
    fn new_delegate_starts_cursor_at_row_zero_unless_empty() {
        let delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.cursor_row(), Some(0));
        assert_eq!(
            delegate.model().cursor(),
            Some(EntryId::new(delegate.model().order()[0]))
        );

        let empty = FileTableDelegate::new(DirectoryModel::new());
        assert_eq!(empty.cursor_row(), None);
        assert!(empty.model().cursor().is_none());
    }

    #[test]
    fn move_cursor_by_steps_and_clamps_at_both_ends() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.move_cursor_by(1), Some(1));
        assert_eq!(delegate.move_cursor_by(2), Some(3));
        assert_eq!(
            delegate.move_cursor_by(10),
            Some(4),
            "clamps at the last row rather than going out of bounds"
        );
        assert_eq!(delegate.move_cursor_by(-100), Some(0), "clamps at row 0");
    }

    #[test]
    fn move_cursor_by_keeps_model_cursor_in_lock_step() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_by(2);
        let row = delegate.cursor_row().unwrap();
        assert_eq!(
            delegate.model().cursor(),
            Some(EntryId::new(delegate.model().order()[row])),
            "model's EntryId-based cursor must always match the cached row"
        );
    }

    #[test]
    fn move_cursor_to_jumps_directly_and_clamps_usize_max_to_last_row() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.move_cursor_to(3), Some(3));
        assert_eq!(
            delegate.move_cursor_to(usize::MAX),
            Some(4),
            "End/Ctrl+End pass usize::MAX; must clamp to the last row"
        );
        assert_eq!(delegate.move_cursor_to(0), Some(0));
    }

    #[test]
    fn cursor_movement_on_an_empty_model_is_a_no_op() {
        let mut delegate = FileTableDelegate::new(DirectoryModel::new());
        assert_eq!(delegate.move_cursor_by(1), None);
        assert_eq!(delegate.move_cursor_to(0), None);
        assert!(delegate.cursor_row().is_none());
    }

    #[test]
    fn sort_re_derives_cursor_row_to_follow_the_same_entry() {
        // `TableDelegate::perform_sort` needs a live `Window`/`Context`
        // (unavailable under plain `cargo test`, same reason the module
        // doc comment on `delegate_reads_row_count_and_columns_from_the_
        // real_model` gives) -- this drives the same two calls
        // `perform_sort`'s body makes directly instead, exercising
        // `sync_cursor_row_from_model` without needing the trait method's
        // GPUI-only parameters.
        let mut delegate = FileTableDelegate::new(five_file_model());
        // Name-ascending puts "f4" last; move the cursor onto it, remember
        // which entry that is, then reverse the sort order.
        delegate.move_cursor_to(4);
        let entry_on_cursor = delegate.model().cursor();
        assert_eq!(
            delegate
                .model()
                .entries()
                .name(entry_on_cursor.unwrap())
                .to_string(),
            "f4"
        );

        delegate.model.sort_by(SortColumn::Name, false);
        delegate.sync_cursor_row_from_model();

        assert_eq!(
            delegate.model().cursor(),
            entry_on_cursor,
            "the EntryId under the cursor doesn't change just because order() did"
        );
        assert_eq!(
            delegate.cursor_row(),
            Some(0),
            "f4 is now first in name-descending order, so its row must follow"
        );
    }

    fn selected_names(delegate: &FileTableDelegate) -> Vec<String> {
        delegate
            .model()
            .order()
            .iter()
            .filter(|&&ix| delegate.model().is_selected(EntryId::new(ix)))
            .map(|&ix| {
                delegate
                    .model()
                    .entries()
                    .name(EntryId::new(ix))
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn toggle_cursor_selection_toggles_only_the_cursor_entry() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(2);
        delegate.toggle_cursor_selection();
        assert_eq!(selected_names(&delegate), vec!["f2"]);
        delegate.toggle_cursor_selection();
        assert!(selected_names(&delegate).is_empty());
    }

    #[test]
    fn invert_selection_flips_every_visible_entry() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(1);
        delegate.toggle_cursor_selection(); // f1 selected
        delegate.invert_selection();
        assert_eq!(
            selected_names(&delegate),
            vec!["f0", "f2", "f3", "f4"],
            "everything except the previously-selected f1"
        );
        delegate.invert_selection();
        assert_eq!(selected_names(&delegate), vec!["f1"]);
    }

    #[test]
    fn select_all_and_deselect_all() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.select_all();
        assert_eq!(delegate.model().selection_stats().count, 5);
        delegate.deselect_all();
        assert_eq!(delegate.model().selection_stats().count, 0);
    }

    fn mixed_extension_model() -> DirectoryModel {
        let mut model = DirectoryModel::new();
        for name in ["photo.jpg", "photo.png", "notes.txt", "readme"] {
            model.entries_mut().push(name, &meta(EntryKind::File, 1, 0));
        }
        model.sort_by(SortColumn::Name, true);
        model
    }

    #[test]
    fn select_same_extension_matches_only_the_cursor_entrys_extension() {
        let mut delegate = FileTableDelegate::new(mixed_extension_model());
        let jpg_row = delegate
            .model()
            .order()
            .iter()
            .position(|&ix| delegate.model().entries().name(EntryId::new(ix)) == "photo.jpg")
            .unwrap();
        delegate.move_cursor_to(jpg_row);
        delegate.select_same_extension();
        assert_eq!(selected_names(&delegate), vec!["photo.jpg"]);
    }

    #[test]
    fn select_same_extension_is_a_no_op_when_cursor_entry_has_none() {
        let mut delegate = FileTableDelegate::new(mixed_extension_model());
        let no_ext_row = delegate
            .model()
            .order()
            .iter()
            .position(|&ix| delegate.model().entries().name(EntryId::new(ix)) == "readme")
            .unwrap();
        delegate.move_cursor_to(no_ext_row);
        delegate.select_same_extension();
        assert!(selected_names(&delegate).is_empty());
    }

    #[test]
    fn extend_selection_to_grows_and_shrinks_a_range_from_a_fixed_anchor() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(1); // anchor will be row 1
        delegate.extend_selection_to(3);
        assert_eq!(selected_names(&delegate), vec!["f1", "f2", "f3"]);

        // Shrinking back must deselect exactly what this same session
        // added, not just whatever's now outside the new endpoint.
        delegate.extend_selection_to(2);
        assert_eq!(selected_names(&delegate), vec!["f1", "f2"]);

        delegate.extend_selection_to(1);
        assert_eq!(
            selected_names(&delegate),
            vec!["f1"],
            "shrinking all the way back to the anchor leaves just the anchor row"
        );
    }

    #[test]
    fn extend_selection_to_does_not_touch_unrelated_prior_selection() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(4);
        delegate.toggle_cursor_selection(); // f4 selected by other means

        delegate.move_cursor_to(0);
        delegate.extend_selection_to(1);

        assert_eq!(
            selected_names(&delegate),
            vec!["f0", "f1", "f4"],
            "the range-extend session must not disturb f4's selection"
        );
    }

    #[test]
    fn plain_cursor_movement_ends_a_range_select_session() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(1);
        delegate.extend_selection_to(3); // anchor = 1, selects f1..f3
        assert!(delegate.range_anchor.is_some());

        delegate.move_cursor_by(1); // plain movement, not a Shift+ variant
        assert!(
            delegate.range_anchor.is_none(),
            "plain movement must end the range-select session"
        );
        assert_eq!(
            selected_names(&delegate),
            vec!["f1", "f2", "f3"],
            "but must not itself change the selection made so far"
        );

        // A fresh Shift+extend now starts from the new cursor position
        // (row 4), not the old anchor.
        delegate.extend_selection_to(4);
        assert_eq!(selected_names(&delegate), vec!["f1", "f2", "f3", "f4"]);
    }

    #[test]
    fn total_bytes_in_view_sums_files_and_excludes_directories() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("a", &meta(EntryKind::File, 100, 0));
        model
            .entries_mut()
            .push("dir", &meta(EntryKind::Directory, 4096, 0));
        model
            .entries_mut()
            .push("b", &meta(EntryKind::File, 250, 0));
        model.sort_by(SortColumn::Name, true);

        let delegate = FileTableDelegate::new(model);
        assert_eq!(
            delegate.total_bytes_in_view(),
            350,
            "100 + 250, the directory's stored size must not count"
        );
    }

    #[test]
    fn responsive_widths_keep_size_and_modified_fixed_always() {
        for available in [80.0, 250.0, 500.0, 1000.0] {
            let widths = responsive::column_widths(available);
            assert_eq!(widths[1], responsive::SIZE_WIDTH, "Size never changes");
            assert_eq!(
                widths[2],
                responsive::MODIFIED_WIDTH,
                "Modified never changes"
            );
        }
    }

    #[test]
    fn responsive_widths_grow_name_to_fill_all_leftover_space_when_roomy() {
        let widths = responsive::column_widths(1000.0);
        let expected_name = 1000.0 - responsive::SIZE_WIDTH - responsive::MODIFIED_WIDTH;
        assert_eq!(widths[0], expected_name);
        assert!((widths.iter().sum::<f32>() - 1000.0).abs() < f32::EPSILON);
    }

    #[test]
    fn responsive_widths_shrink_name_as_panel_narrows() {
        let wide = responsive::column_widths(1000.0);
        let narrow = responsive::column_widths(500.0);
        assert!(narrow[0] < wide[0], "Name shrinks as available width drops");
        assert_eq!(
            narrow[0],
            500.0 - responsive::SIZE_WIDTH - responsive::MODIFIED_WIDTH
        );
    }

    #[test]
    fn responsive_widths_fall_back_to_name_min_when_impossibly_narrow() {
        // Below Size+Modified+NAME_MIN, Name can't shrink any further
        // without disappearing -- hold it at its floor and let the
        // Table's own horizontal scrollbar handle the (now unavoidable)
        // overflow, rather than letting Name go to zero or negative.
        let widths = responsive::column_widths(50.0);
        assert_eq!(
            widths,
            [
                responsive::NAME_MIN,
                responsive::SIZE_WIDTH,
                responsive::MODIFIED_WIDTH,
            ]
        );
    }

    #[test]
    fn apply_responsive_widths_updates_columns_and_dedupes_repeat_calls() {
        let mut delegate = FileTableDelegate::new(sample_model());
        assert!(delegate.apply_responsive_widths(500.0));
        assert_eq!(delegate.columns.len(), 3, "column count never changes");
        let narrow_name = delegate.columns[0].width;

        assert!(
            !delegate.apply_responsive_widths(500.3),
            "sub-pixel jitter should not trigger a rebuild"
        );
        assert_eq!(delegate.columns[0].width, narrow_name);

        assert!(
            delegate.apply_responsive_widths(1000.0),
            "a real width change should trigger a rebuild"
        );
        assert!(
            delegate.columns[0].width > narrow_name,
            "Name grows back once the panel widens"
        );
    }

    #[test]
    fn row_text_cache_matches_order_and_is_stable_until_generation_changes() {
        let mut delegate = FileTableDelegate::new(sample_model());
        assert_eq!(delegate.row_text.len(), 2);
        // `SortOptions::default()` is name-ascending with `dirs_first: true`
        // (see `duet_index::sort::SortOptions`), so the directory sorts
        // first despite "b_dir" > "a_file.txt" lexically.
        assert_eq!(delegate.row_text[0].name.as_ref(), "b_dir");
        assert_eq!(delegate.row_text[0].size.as_ref(), "<DIR>");
        assert_eq!(delegate.row_text[1].name.as_ref(), "a_file.txt");
        assert_eq!(delegate.row_text[1].size.as_ref(), "2.0 KB");

        let gen_before = delegate.cached_generation;
        delegate.rebuild_row_text();
        assert_eq!(
            delegate.cached_generation, gen_before,
            "no-op rebuild must not bump the cached generation redundantly"
        );
    }

    #[test]
    fn write_size_formats_bytes_and_directories() {
        let mut buf = String::new();
        write_size(&mut buf, EntryKind::File, 0);
        assert_eq!(buf, "0 B");

        buf.clear();
        write_size(&mut buf, EntryKind::File, 2048);
        assert_eq!(buf, "2.0 KB");

        buf.clear();
        write_size(&mut buf, EntryKind::Directory, 999_999);
        assert_eq!(buf, "<DIR>");
    }

    #[test]
    fn write_date_formats_unix_seconds() {
        let mut buf = String::new();
        write_date(&mut buf, 1_700_000_000);
        assert_eq!(buf, "2023-11-14 22:13");

        buf.clear();
        write_date(&mut buf, 0);
        assert_eq!(buf, "-");
    }
}
