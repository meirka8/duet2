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
use std::rc::Rc;
use std::time::Duration;

use duet_index::{DirectoryModel, FilterSpec, SortColumn};
use duet_types::{EntryId, EntryKind, UnixPathBuf, VPath};
use duet_vfs::{DirEntry, FileSystem, ListFields, ListOpts, LocalFs};
use duet_widgets::menu::{PopupMenu, PopupMenuItem};
use duet_widgets::table::{
    Column, ColumnSort, Table, TableDelegate, TableEvent, TableRow, TableState,
};
use duet_widgets::theme::TokenPalette;
use futures_util::StreamExt;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    HighlightStyle, InteractiveElement as _, IntoElement, KeyBinding, Modifiers, MouseButton,
    ParentElement as _, Render, SharedString, Styled as _, StyledText, Window, actions, div, px,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

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
    /// Bumped by [`Self::bump_nav_generation`] on every `navigate_to`
    /// (T-4.3.1) call, before its background directory-listing/volume-
    /// stats queries are spawned. Each spawned task captures the
    /// generation it was started for and checks it against this field's
    /// *current* value before applying its result -- if they've since
    /// diverged, a *newer* navigation has started in the meantime, and
    /// the (now-stale) result is silently discarded rather than
    /// overwriting the newer one. Guards against a real, not just
    /// theoretical, race: two navigations issued in quick succession
    /// (e.g. pressing Backspace twice before the first listing finishes)
    /// have no guaranteed completion order, and without this a slower
    /// first load completing after a faster second one would leave the
    /// panel showing the wrong directory's contents under the *right*
    /// directory's path/header.
    nav_generation: u64,
    /// Whether to show a synthetic ".." row above every real entry
    /// (T-4.3.1's parent-directory navigation affordance) -- `false` at
    /// the filesystem root, where there's nowhere to go up to. This
    /// delegate deliberately doesn't know `current_dir` itself (that
    /// lives on `FileTable`, the one thing that changes across
    /// navigation); `FileTable::navigate_to`/`FileTable::new` compute it
    /// and push it in via [`Self::set_has_parent_row`] whenever the
    /// directory changes. Not part of `model`/`EntryStore` at all -- see
    /// [`Self::display_row`]'s doc comment for why.
    has_parent_row: bool,
    /// Whether the *display* cursor is on the synthetic ".." row
    /// (display row 0, whenever `has_parent_row`) rather than pointing
    /// at `cursor_row`. `cursor_row` keeps its ordinary, pseudo-row-
    /// unaware meaning throughout -- every existing selection method
    /// (`toggle_cursor_selection`, `extend_selection_to`, ...) still
    /// reads it directly and stays completely unaware the pseudo-row
    /// exists, which is deliberate: "select/extend from wherever the
    /// cursor last meaningfully was" is a perfectly reasonable answer for
    /// what Ins/Shift+movement do while sitting on "..", and building
    /// every selection method to understand a row with no underlying
    /// entry would roughly double this feature's size for a case nobody
    /// asked about. [`Self::display_row`]/[`Self::set_display_cursor`]
    /// are the translation boundary cursor *movement* and *rendering*
    /// go through instead.
    cursor_on_parent: bool,
    /// FR-SEL-06: which mouse gesture selects a row. See [`MouseMode`]'s
    /// own doc comment. Set once, at construction, by [`FileTable::new`]
    /// -- there is no live-reload path yet.
    mouse_mode: MouseMode,
    /// T-4.3.8's middle-click "open in a new tab" gesture -- see
    /// [`NewTabHandler`]'s doc comment and `render_tr`'s
    /// `with_middle_click_new_tab`. Always `Some` once
    /// `Panel::add_tab_entry` has wired it up (unlike `locked_navigation`,
    /// this one is never conditional) -- `None` only for a bare
    /// `FileTable` built outside a `Panel` (e.g. most tests), where
    /// middle-click is simply inert.
    new_tab_handler: Option<NewTabHandler>,
    /// FR-NAV-07/FR-NAV-13: the active quick-search/quick-filter session,
    /// if any. See [`QuickSearchState`]'s doc comment.
    quick_search: Option<QuickSearchState>,
    /// Which regime plain typing starts a new session in -- read once
    /// from `settings.toml`'s `navigation.quick_search_mode` by
    /// `FileTable::new`, same "no live-reload path yet" story as
    /// `mouse_mode`.
    quick_search_default_mode: QuickSearchMode,
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
            nav_generation: 0,
            has_parent_row: false,
            cursor_on_parent: false,
            mouse_mode: MouseMode::default(),
            new_tab_handler: None,
            quick_search: None,
            quick_search_default_mode: QuickSearchMode::default(),
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

    /// The current `[Name, Size, Modified]` column widths (px) together
    /// with the panel width they were computed for -- `None` until the
    /// measuring canvas has actually run at least once (see
    /// `apply_responsive_widths`). T-4.3.2's `Panel::add_tab_entry` uses
    /// this to seed a freshly-created sibling tab's initial widths
    /// directly from one that's already been measured (every tab in a
    /// `Panel` renders at the same panel width, so a sibling's answer is
    /// exactly right, not an approximation), skipping the one-frame
    /// narrow-then-corrects dance [`Self::new`] otherwise commits to.
    /// Purely cosmetic -- `apply_responsive_widths` would converge to the
    /// same answer regardless -- but without this, a tab opened mid-
    /// session (unlike the very first tab at app startup, which rides
    /// along with several other early re-renders that mask the same
    /// glitch) visibly flashes narrow until some unrelated later action
    /// happens to trigger the next repaint.
    pub(crate) fn responsive_seed(&self) -> Option<([f32; 3], f32)> {
        let available = self.last_available_width?;
        Some((
            [
                f32::from(self.columns[COL_NAME].width),
                f32::from(self.columns[COL_SIZE].width),
                f32::from(self.columns[COL_MODIFIED].width),
            ],
            available,
        ))
    }

    /// The inverse of [`Self::responsive_seed`]: applies an already-known
    /// widths/available-width pair directly, without going through
    /// [`Self::apply_responsive_widths`]'s own recomputation -- there's
    /// nothing to recompute, the caller already has the exact answer that
    /// call would produce.
    fn seed_column_widths(&mut self, widths: [f32; 3], available: f32) {
        self.columns = columns_with_widths(&self.base_columns, widths);
        self.last_available_width = Some(available);
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
        // Every fresh listing starts the cursor on real row 0, not
        // wherever `cursor_on_parent` happened to be left over from
        // whatever directory was showing before -- without this, a plain
        // `Enter`/`nav.enter_dir` into a subdirectory while sitting on the
        // ".." row would silently land the new listing's cursor back on
        // its own ".." row instead of row 0, since `set_has_parent_row`
        // (called right after this, once the new directory's parent-ness
        // is known) only clears it when there's no parent row to be on.
        self.cursor_on_parent = false;
        self.set_cursor_row(Some(0));
        self.range_anchor = None;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// Current navigation generation. See the `nav_generation` field's
    /// doc comment.
    pub fn nav_generation(&self) -> u64 {
        self.nav_generation
    }

    /// Starts a new navigation generation and returns it. See the
    /// `nav_generation` field's doc comment.
    pub fn bump_nav_generation(&mut self) -> u64 {
        self.nav_generation += 1;
        self.nav_generation
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

    /// See the `has_parent_row` field's doc comment. Called by
    /// `FileTable::navigate_to`/`FileTable::new` whenever `current_dir`
    /// changes; leaving the pseudo-row (clearing `cursor_on_parent`) if
    /// it's no longer showing (moving to the filesystem root) rather than
    /// leaving the cursor stuck nowhere real.
    pub fn set_has_parent_row(&mut self, has_parent: bool) {
        self.has_parent_row = has_parent;
        if !has_parent {
            self.cursor_on_parent = false;
        }
    }

    /// See the `mouse_mode` field's doc comment.
    pub(crate) fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    pub(crate) fn set_mouse_mode(&mut self, mode: MouseMode) {
        self.mouse_mode = mode;
    }

    /// See the `quick_search_default_mode` field's doc comment.
    pub(crate) fn set_quick_search_default_mode(&mut self, mode: QuickSearchMode) {
        self.quick_search_default_mode = mode;
    }

    fn quick_search_default_mode(&self) -> QuickSearchMode {
        self.quick_search_default_mode
    }

    /// See the `new_tab_handler` field's doc comment.
    pub(crate) fn set_new_tab_handler(&mut self, handler: Option<NewTabHandler>) {
        self.new_tab_handler = handler;
    }

    /// Test-only: exposes the installed handler so `panel.rs`'s tests can
    /// invoke `Panel::add_tab_entry`'s *real* production closure directly
    /// -- its actual business logic (resolve a name against
    /// `current_dir`, open a new tab, make it active) is what's worth
    /// testing, not which exact pixel `gpui-component`'s `Table` maps to
    /// a given row, which is that crate's own concern to get right.
    #[cfg(test)]
    pub(crate) fn new_tab_handler(&self) -> Option<NewTabHandler> {
        self.new_tab_handler.clone()
    }

    /// T-4.3.8: middle-click a directory row to open it in a new tab --
    /// see [`NewTabHandler`]'s doc comment for why the handler takes just
    /// the entry's name. A no-op for a file row (nothing to open in a new
    /// tab) or while no handler is installed (`new_tab_handler` is `None`
    /// for a bare `FileTable` built outside a `Panel`, e.g. most tests --
    /// see that field's doc comment). `MouseButton::Middle` is one of
    /// `Div::on_mouse_down`'s own button-filtered overloads (confirmed by
    /// reading `gpui-0.2.2/src/elements/div.rs`), so no manual
    /// `event.button ==` check is needed here. Called from `render_tr`
    /// (the `TableDelegate` impl below).
    fn with_middle_click_new_tab(&self, row: TableRow, model_row: usize) -> TableRow {
        let Some(handler) = self.new_tab_handler.clone() else {
            return row;
        };
        let name = self.model.order().get(model_row).copied().and_then(|ix| {
            let id = EntryId::new(ix);
            (self.model.entries().kind(id) == EntryKind::Directory)
                .then(|| self.model.entries().name(id).to_string())
        });
        let Some(name) = name else {
            return row;
        };
        row.on_mouse_down(MouseButton::Middle, move |_event, window, cx| {
            handler(name.clone(), window, cx);
        })
    }

    /// Translates a *display*-row index (what `TableEvent::SelectRow`/
    /// `DoubleClickedRow`/`Table`'s right-click all report) into
    /// `model.order()` space, or `None` if it's the synthetic ".." row --
    /// the inverse of [`Self::display_row`]. T-4.3.8's mouse handlers use
    /// this to turn a click's row index into what the existing
    /// selection methods (`extend_selection_to`, `toggle_cursor_selection`
    /// via a cursor move, ...) already expect; none of them need to learn
    /// about the pseudo-row themselves, same reasoning as
    /// `set_display_cursor`'s doc comment.
    pub(crate) fn model_row(&self, row_ix: usize) -> Option<usize> {
        if self.has_parent_row && row_ix == 0 {
            None
        } else {
            Some(row_ix - self.parent_offset())
        }
    }

    /// FR-NAV-07/FR-NAV-13: clears the active quick-search/quick-filter
    /// session, if any, restoring the model's filter back to `None`. A
    /// no-op if no session is active. Lives here (not solely as a
    /// `FileTable` method) so `context_menu`'s right-click handling
    /// (which only ever gets `&mut self`, never `FileTable`'s own
    /// `Context`) can call it directly too -- `FileTable::exit_quick_search`
    /// is a thin wrapper adding the `cx.notify()` a keyboard/mouse/timer
    /// -driven exit needs.
    fn clear_quick_search(&mut self) {
        let Some(session) = self.quick_search.take() else {
            return;
        };
        if session.mode == QuickSearchMode::Filter {
            self.model.set_filter(None);
            self.rebuild_row_text();
            self.sync_cursor_row_from_model();
        }
    }

    /// The active session's mode, if any -- `None` when no quick-search
    /// /quick-filter regime is active.
    pub(crate) fn quick_search_mode(&self) -> Option<QuickSearchMode> {
        self.quick_search.as_ref().map(|s| s.mode)
    }

    /// The active session's generation counter, if any -- what
    /// `FileTable`'s idle-timeout timer compares against to detect a
    /// stale (superseded by a newer keystroke) timer. See
    /// `QuickSearchState::generation`'s doc comment.
    pub(crate) fn quick_search_generation(&self) -> Option<u64> {
        self.quick_search.as_ref().map(|s| s.generation)
    }

    /// FR-NAV-07/FR-NAV-13's user-facing indicator text -- `None` when no
    /// session is active. `Jump` mode shows the literal query and the
    /// current match's ordinal position among every match (design.md's
    /// own example: "find: rmr (2/5)"); a query matching nothing reads
    /// "(no match)" rather than a bogus "0/0". `Filter` mode shows the
    /// query and how many entries currently pass it -- its own indicator
    /// format, deliberately different from `Jump`'s ordinal (design.md:
    /// filter mode "keeps its own match-count indicator... unaffected"
    /// by the ordinal-position requirement, which is specific to `Jump`).
    pub(crate) fn quick_search_indicator_text(&self) -> Option<String> {
        let session = self.quick_search.as_ref()?;
        Some(match session.mode {
            QuickSearchMode::Jump => match &session.jump_match {
                Some(m) => format!("find: {} ({}/{})", session.query, m.ordinal, m.total),
                None => format!("find: {} (no match)", session.query),
            },
            QuickSearchMode::Filter => {
                let count = session.filter_match_count.unwrap_or(0);
                let noun = if count == 1 { "match" } else { "matches" };
                format!("filter: {} ({count} {noun})", session.query)
            }
        })
    }

    /// Re-scores (`Jump`) or re-filters (`Filter`) against the active
    /// session's current query buffer -- a no-op if no session is
    /// active. The one place every keystroke's effect on the model/
    /// cursor actually happens; `FileTable::push_quick_search_char`/
    /// `toggle_quick_filter` both call this after touching
    /// `self.quick_search` itself.
    /// Returns the display row the caller should scroll into view, if
    /// any -- `apply_quick_search_jump`/`_filter` both move the cursor
    /// (directly, or via `sync_cursor_row_from_model`) but neither has
    /// access to the `TableState` that `scroll_to_row` lives on, only
    /// `FileTable::apply_quick_search` (this delegate method's one
    /// caller) does.
    fn apply_quick_search(&mut self) -> Option<usize> {
        let session = self.quick_search.as_ref()?;
        let mode = session.mode;
        // Cloning just the query string (not the whole session) sidesteps
        // needing `self.quick_search` borrowed immutably (to read it) and
        // mutably (to write `jump_match`/`filter_match_count` back) at
        // the same time -- cheap, since a typed query is always short.
        let query = session.query.clone();
        match mode {
            QuickSearchMode::Jump => self.apply_quick_search_jump(&query),
            QuickSearchMode::Filter => self.apply_quick_search_filter(&query),
        }
    }

    /// `Jump` mode's per-keystroke work (FR-NAV-13): fuzzy-scores every
    /// visible entry against `query` (nucleo's subsequence matcher, same
    /// crate/usage `duet-commands`' command palette already establishes
    /// -- see `duet-commands/src/palette.rs`'s module doc comment),
    /// jumps the cursor to the single highest-scoring match (ties broken
    /// by distance from the cursor's row *before this keystroke* --
    /// FR-NAV-13's literal tiebreak rule), and records that match's
    /// *ordinal position in listing order* among every entry that
    /// matched at all -- not its score rank, which would trivially
    /// always be 1 since the cursor always jumps to the best scorer. (The
    /// `find: rmr (2/5)` example in design.md only makes sense under
    /// this reading: the winner is always the best match, but it isn't
    /// always the first one you'd encounter scrolling down from the
    /// top.) A query matching nothing clears `jump_match` and leaves the
    /// cursor where it was. Returns the winner's display row (UAT: the
    /// cursor was jumping to matches outside the visible scroll range
    /// with nothing bringing the viewport along -- `FileTable::
    /// apply_quick_search` scrolls to whatever this returns).
    fn apply_quick_search_jump(&mut self, query: &str) -> Option<usize> {
        let anchor_row = self.cursor_row.unwrap_or(0);
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();

        let mut matched: Vec<(usize, u32)> = self
            .model
            .ordered_names()
            .enumerate()
            .filter_map(|(row, (_, name))| {
                let score = pattern.score(Utf32Str::new(name, &mut buf), &mut matcher)?;
                Some((row, score))
            })
            .collect();
        matched.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| {
                let da = (a.0 as i64 - anchor_row as i64).unsigned_abs();
                let db = (b.0 as i64 - anchor_row as i64).unsigned_abs();
                da.cmp(&db)
            })
        });

        let winner = matched.first().map(|&(winner_row, _)| {
            let total = matched.len();
            let ordinal = matched.iter().filter(|&&(row, _)| row < winner_row).count() + 1;
            let winner_name = self
                .model
                .entries()
                .name(EntryId::new(self.model.order()[winner_row]))
                .to_string();
            let mut indices = Vec::new();
            pattern.indices(
                Utf32Str::new(&winner_name, &mut buf),
                &mut matcher,
                &mut indices,
            );
            indices.sort_unstable();
            indices.dedup();
            (
                winner_row,
                JumpMatch {
                    ordinal,
                    total,
                    model_row: winner_row,
                    indices,
                },
            )
        });

        if let Some(session) = self.quick_search.as_mut() {
            session.jump_match = winner.as_ref().map(|(_, m)| m.clone());
        }
        winner.map(|(winner_row, _)| {
            let display_row = winner_row + self.parent_offset();
            self.move_cursor_to(display_row);
            display_row
        })
    }

    /// `Filter` mode's per-keystroke work (FR-NAV-07): narrows
    /// `model.order()` to entries whose name contains `query`
    /// (case-insensitive substring -- `FilterSpec::quick_filter`'s
    /// existing behavior, not fuzzy; see [`QuickSearchMode`]'s doc
    /// comment for why reusing it as-is is the right call here).
    /// `show_hidden: true` is load-bearing, not a default left alone: no
    /// filter is ever otherwise applied in this app today (`order()` ==
    /// `full_order()` always, until this method's first call),
    /// `FilterSpec::default()`'s `show_hidden: false` would hide
    /// dotfiles as an unintended side effect of adding quick-filter.
    ///
    /// UAT: narrowing the listing routinely filters out whatever entry
    /// the cursor was previously on -- `sync_cursor_row_from_model`
    /// alone then leaves `cursor_row` at `None` (its own, correct
    /// behavior: the entry it was tracking is no longer visible), which
    /// left the filtered view with *no* cursor highlighted at all and
    /// nothing for Enter/arrow keys to act on, even though the match
    /// count shown was already correct. Falls back to row 0 -- the
    /// first (and typically only, or close to it) remaining visible
    /// entry -- whenever that happens, so a narrowed listing always has
    /// an actionable cursor. Returns the (possibly just-reset) cursor's
    /// display row so `FileTable::apply_quick_search` can scroll it into
    /// view, same as `apply_quick_search_jump`.
    fn apply_quick_search_filter(&mut self, query: &str) -> Option<usize> {
        self.model.set_filter(Some(FilterSpec {
            show_hidden: true,
            quick_filter: Some(query.into()),
            mask: None,
        }));
        self.rebuild_row_text();
        self.sync_cursor_row_from_model();
        if self.cursor_row.is_none() && !self.model.order().is_empty() {
            self.cursor_on_parent = false;
            self.set_cursor_row(Some(0));
        }
        let count = self.model.order().len();
        if let Some(session) = self.quick_search.as_mut() {
            session.filter_match_count = Some(count);
        }
        self.display_row()
    }

    /// The row-preparation half of [`Self::context_menu`] (`TableDelegate`
    /// impl, below) -- moves the cursor to `row_ix` (display-row terms)
    /// and, in `MouseMode::Norton`, also toggles that row's selection
    /// (TC's own right-click convention -- see [`MouseMode`]'s doc
    /// comment). Returns `false` (a no-op) for the synthetic ".." row,
    /// same reasoning as every other mouse handler in this module.
    ///
    /// Split out from `context_menu` itself so it's directly unit
    /// -testable: `context_menu` needs a real `PopupMenu` (buildable only
    /// through `PopupMenu::build`, which itself needs a `Window`) and a
    /// real `Context<TableState<Self>>` (only ever supplied by `Table`'s
    /// own rendering, mid-paint) -- neither is obtainable from a plain
    /// `#[test]`. This method needs neither.
    fn prepare_context_menu_row(&mut self, row_ix: usize) -> bool {
        if self.model_row(row_ix).is_none() {
            return false;
        }
        // A right-click is as much a "mouse click" as a left one --
        // FR-NAV-13's exit list applies the same way.
        self.clear_quick_search();
        self.move_cursor_to(row_ix);
        if self.mouse_mode == MouseMode::Norton {
            self.toggle_cursor_selection();
        }
        true
    }

    /// `0` or `1` -- how many rows the synthetic ".." row, if showing,
    /// adds ahead of every `model.order()`-indexed row. The one number
    /// every display-row/model-row translation in this file is built on.
    fn parent_offset(&self) -> usize {
        self.has_parent_row as usize
    }

    /// `TableDelegate::rows_count`'s real answer -- `model.order().len()`
    /// alone undercounts by one whenever the ".." row is showing.
    fn display_rows_count(&self) -> usize {
        self.model.order().len() + self.parent_offset()
    }

    /// The cursor's position in *display*-row terms: row 0 is the ".."
    /// row when [`Self::set_has_parent_row`] is showing one, matching
    /// exactly what `TableState::scroll_to_row`/`visible_range` index by
    /// (since `TableDelegate::rows_count` reports [`Self::display_rows_count`],
    /// not `model.order().len()`) -- this is deliberately a *different*
    /// number from [`Self::cursor_row`] (model-row terms) whenever a
    /// parent row is showing, and the two are never meant to be compared
    /// directly.
    pub fn display_row(&self) -> Option<usize> {
        if self.cursor_on_parent {
            Some(0)
        } else {
            self.cursor_row.map(|r| r + self.parent_offset())
        }
    }

    /// The inverse of [`Self::display_row`]: moves the cursor to `row`
    /// (display-row terms), translating into `cursor_on_parent`/
    /// `cursor_row` as appropriate. The one place that decides "is this
    /// display row the pseudo-row or a real one" -- `move_cursor_by`/
    /// `move_cursor_to` (the only callers) stay simple, uniform
    /// `0..display_rows_count()` arithmetic because of it.
    fn set_display_cursor(&mut self, row: Option<usize>) {
        match row {
            Some(0) if self.has_parent_row => self.cursor_on_parent = true,
            Some(r) => {
                self.cursor_on_parent = false;
                self.set_cursor_row(Some(r - self.parent_offset()));
            }
            None => {
                self.cursor_on_parent = false;
                self.set_cursor_row(None);
            }
        }
    }

    /// Moves the cursor by `delta` rows (negative for up), clamped to
    /// `[0, display_rows_count() - 1]` -- *display* rows, so the ".." row
    /// (if showing) is a real stop like any other. Returns the resulting
    /// display row so the caller (`FileTable`'s action handlers) can
    /// decide whether to scroll it into view, matching
    /// `TableState`'s own row indexing exactly (see
    /// `display_rows_count`'s doc comment) -- a no-op (empty listing, no
    /// parent row) returns `None`. Ends any in-progress range-select
    /// session -- see the `range_anchor` field's doc comment.
    fn move_cursor_by(&mut self, delta: i64) -> Option<usize> {
        let len = self.display_rows_count();
        if len == 0 {
            return None;
        }
        self.range_anchor = None;
        let current = self.display_row().unwrap_or(0) as i64;
        let target = (current + delta).clamp(0, len as i64 - 1) as usize;
        self.set_display_cursor(Some(target));
        self.display_row()
    }

    /// Moves the cursor directly to `row` (display-row terms, like
    /// `move_cursor_by`), clamped into range (so `usize::MAX` is a
    /// convenient "last row" for End/Ctrl+End). Ends any in-progress
    /// range-select session, same as `move_cursor_by`.
    fn move_cursor_to(&mut self, row: usize) -> Option<usize> {
        let len = self.display_rows_count();
        if len == 0 {
            return None;
        }
        self.range_anchor = None;
        self.set_display_cursor(Some(row.min(len - 1)));
        self.display_row()
    }

    /// T-4.3.1's "cursor restores to the child directory when going up":
    /// after a fresh listing loads, finds `name` in `order()` and moves
    /// the cursor there instead of leaving it at row 0 (`set_model`'s
    /// default). Returns whether `name` was actually found -- it might
    /// not be (the directory was renamed/removed concurrently), in which
    /// case the caller leaves the row-0 default alone. `O(n)`, but this
    /// only ever runs once per completed directory load, never per frame.
    fn select_row_by_name(&mut self, name: &str) -> bool {
        let Some(row) = self
            .model
            .order()
            .iter()
            .position(|&ix| self.model.entries().name(EntryId::new(ix)) == name)
        else {
            return false;
        };
        // `row` is a *model*-space index (into `model.order()`), but
        // `move_cursor_to` takes a *display*-space one (T-4.3.1's ".."
        // row, when showing, occupies display row 0 ahead of every real
        // model row -- see `display_row`'s doc comment). Without
        // `+ parent_offset()` here, restoring the cursor onto anything
        // but the very first real entry lands one row too early in every
        // directory that has a parent (i.e. every directory but the
        // filesystem root) -- caught by T-4.3.7's session-restore test,
        // which is also the only caller that actually exercises this with
        // `has_parent_row` set.
        self.move_cursor_to(row + self.parent_offset());
        true
    }

    /// The cursor entry's name, if (and only if) it's a directory
    /// (T-4.3.1's Enter-to-descend) -- `None` for files, symlinks
    /// (`nav.follow_symlink` isn't implemented), an empty/cursor-less
    /// listing, or -- deliberately -- while [`Self::cursor_on_parent`] is
    /// true: `cursor_row` isn't guaranteed to still point at anything
    /// meaningful once the display cursor has moved onto the pseudo-row,
    /// and returning it anyway would risk `FileTable::enter_cursor_directory`
    /// descending into a stale, wrong directory. `FileTable` checks
    /// `cursor_on_parent` first and handles that case itself (routing to
    /// `navigate_to_parent`), so this never needs to.
    fn cursor_dir_name(&self) -> Option<String> {
        if self.cursor_on_parent {
            return None;
        }
        let row = self.cursor_row?;
        let &ix = self.model.order().get(row)?;
        let id = EntryId::new(ix);
        (self.model.entries().kind(id) == EntryKind::Directory)
            .then(|| self.model.entries().name(id).to_string())
    }

    /// The cursor entry's name regardless of kind (files included, unlike
    /// [`Self::cursor_dir_name`]) -- T-4.3.7's "restore the cursor
    /// position across a restart". Same `None` cases as `cursor_dir_name`
    /// otherwise: an empty/cursor-less listing, or the cursor sitting on
    /// the synthetic ".." row (nothing meaningful to restore there;
    /// landing back on row 0, `set_model`'s own default, is exactly
    /// right).
    pub(crate) fn cursor_entry_name(&self) -> Option<String> {
        if self.cursor_on_parent {
            return None;
        }
        let row = self.cursor_row?;
        let &ix = self.model.order().get(row)?;
        Some(self.model.entries().name(EntryId::new(ix)).to_string())
    }

    /// Whether the display cursor is on the synthetic ".." row. See the
    /// `cursor_on_parent` field's doc comment.
    pub fn cursor_on_parent(&self) -> bool {
        self.cursor_on_parent
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
        self.display_rows_count()
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
        let row = div().id(("file-row", row_ix));
        let tokens = TokenPalette::current(cx);

        // The synthetic ".." row (T-4.3.1) is always display row 0 when
        // showing -- see `has_parent_row`'s doc comment. It can never be
        // selected (there's no underlying entry to select), only ever
        // shown as the cursor.
        if self.has_parent_row && row_ix == 0 {
            return if self.cursor_on_parent {
                row.bg(tokens.color.cursor_bg)
                    .text_color(tokens.color.cursor_fg)
            } else {
                row
            };
        }

        let model_row = row_ix - self.parent_offset();
        let is_cursor = !self.cursor_on_parent && self.cursor_row == Some(model_row);
        let selected = self
            .model
            .order()
            .get(model_row)
            .copied()
            .is_some_and(|ix| self.model.is_selected(EntryId::new(ix)));

        let row = if is_cursor {
            row.bg(tokens.color.cursor_bg)
                .text_color(tokens.color.cursor_fg)
        } else if selected {
            row.bg(tokens.color.selection_bg)
        } else {
            row
        };

        self.with_middle_click_new_tab(row, model_row)
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
        // The synthetic ".." row (T-4.3.1) has no `row_text` entry -- it
        // isn't a model row at all -- so it's rendered directly here
        // instead of going through the `row_text` lookup below.
        let text = if self.has_parent_row && row_ix == 0 {
            match col_ix {
                COL_NAME => SharedString::from(".."),
                _ => SharedString::default(),
            }
        } else {
            let model_row = row_ix - self.parent_offset();
            self.row_text
                .get(model_row)
                .map(|row| match col_ix {
                    COL_NAME => row.name.clone(),
                    COL_SIZE => row.size.clone(),
                    COL_MODIFIED => row.modified.clone(),
                    _ => SharedString::default(),
                })
                .unwrap_or_default()
        };

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

        // T-4.3.3: bold the characters `Jump` mode's fuzzy matcher
        // actually matched, on whichever row is currently the best match
        // -- the same "show your work" convention every mainstream fuzzy
        // finder (fzf, VS Code's Quick Open, ...) uses. Plain `BOLD`
        // weight only, deliberately no color change: this row is also
        // always the cursor row (`Jump` mode moves the cursor to its own
        // match), so a highlight color would have to work against both
        // `cursor_bg`'s light background *and* an ordinary row's dark
        // one -- the exact contrast trap `duet-widgets::theme`'s
        // `table_hover` mapping already got wrong once this same task
        // (see that fix's own commit); weight alone sidesteps it
        // entirely.
        if col_ix == COL_NAME {
            let model_row =
                (!self.has_parent_row || row_ix != 0).then(|| row_ix - self.parent_offset());
            let indices = model_row.and_then(|model_row| {
                let jump_match = self.quick_search.as_ref()?.jump_match.as_ref()?;
                (jump_match.model_row == model_row).then(|| jump_match.indices.clone())
            });
            if let Some(indices) = indices {
                let highlight = HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                };
                let ranges = char_indices_to_byte_ranges(&text, &indices);
                return cell.child(
                    StyledText::new(text)
                        .with_highlights(ranges.into_iter().map(|range| (range, highlight))),
                );
            }
        }
        cell.child(text)
    }

    fn loading(&self, _cx: &App) -> bool {
        self.loading
    }

    /// T-4.3.8's right-click menu (FR-SEL-06). `Table`'s own rendering
    /// already calls this automatically for whichever row was last
    /// right-clicked (`state.rs`'s `right_clicked_row`, set by
    /// `on_row_right_click`) -- confirmed by reading
    /// `gpui-component-0.5.1/src/table/state.rs:1307`'s `context_menu`
    /// call site, so overriding just this one method is the whole
    /// integration, no manual `render_tr` wiring needed.
    ///
    /// `row_ix` is display-row terms, same as every other `TableEvent`;
    /// the synthetic ".." row gets no menu at all (there's no entry to
    /// act on, and TC itself doesn't show one there either).
    ///
    /// Every item is built from the same selection primitives the
    /// keyboard already uses (Ins/Num*/Ctrl+Num+/Ctrl+Num-/Shift+Num+),
    /// via a cloned `Entity<TableState<Self>>` captured into each
    /// `on_click` closure -- `PopupMenuItem::on_click`'s handler only
    /// gets `&mut App`, not this method's own `Context`, so reaching back
    /// into the delegate has to go through `Entity::update` the same way
    /// any other later-invoked callback in this module does (see
    /// `locked_navigation`'s doc comment) -- ordinary `update`, not
    /// `update_in`, since none of these actions need a `Window`.
    ///
    /// Norton mode (FR-SEL-06): right-click both *toggles* the clicked
    /// row's selection and shows this same menu -- TC's own convention,
    /// applied before the menu is built so "Toggle Selection"'s checkmark
    /// already reflects the post-toggle state. `Windows`/`None` leave
    /// selection untouched on right-click, matching their own plain-click
    /// semantics (see [`MouseMode`]'s doc comment) -- only the cursor
    /// moves.
    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        if !self.prepare_context_menu_row(row_ix) {
            return menu;
        }
        let is_selected = self
            .cursor_row
            .and_then(|row| self.model.order().get(row))
            .copied()
            .is_some_and(|ix| self.model.is_selected(EntryId::new(ix)));

        let entity = cx.entity();
        let with_delegate = move |cx: &mut App, f: fn(&mut FileTableDelegate)| {
            let entity = entity.clone();
            entity.update(cx, |state, cx| {
                f(state.delegate_mut());
                cx.notify();
            });
        };

        menu.item(
            PopupMenuItem::new("Toggle Selection")
                .checked(is_selected)
                .on_click({
                    let with_delegate = with_delegate.clone();
                    move |_, _, cx| with_delegate(cx, FileTableDelegate::toggle_cursor_selection)
                }),
        )
        .separator()
        .item(PopupMenuItem::new("Select All").on_click({
            let with_delegate = with_delegate.clone();
            move |_, _, cx| with_delegate(cx, FileTableDelegate::select_all)
        }))
        .item(PopupMenuItem::new("Deselect All").on_click({
            let with_delegate = with_delegate.clone();
            move |_, _, cx| with_delegate(cx, FileTableDelegate::deselect_all)
        }))
        .item(PopupMenuItem::new("Invert Selection").on_click({
            let with_delegate = with_delegate.clone();
            move |_, _, cx| with_delegate(cx, FileTableDelegate::invert_selection)
        }))
        .item(PopupMenuItem::new("Select Same Extension").on_click({
            move |_, _, cx| with_delegate(cx, FileTableDelegate::select_same_extension)
        }))
    }
}

/// Converts `nucleo_matcher`'s per-*character* match indices (positions
/// into the `Utf32Str` it scored, per `Pattern::indices`'s own doc
/// comment -- not byte offsets) into the byte ranges `StyledText::
/// with_highlights` needs, merging adjacent characters into a single
/// range rather than emitting one run per character. `indices` need not
/// be sorted/deduplicated on entry -- this sorts its own copy first,
/// matching `Pattern::indices`'s documented raw-output caveat.
fn char_indices_to_byte_ranges(name: &str, indices: &[u32]) -> Vec<std::ops::Range<usize>> {
    let mut indices = indices.to_vec();
    indices.sort_unstable();
    indices.dedup();

    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut target = indices.iter().copied();
    let mut next_target = target.next();
    for (char_ord, (byte_off, ch)) in name.char_indices().enumerate() {
        if next_target != Some(char_ord as u32) {
            continue;
        }
        let end = byte_off + ch.len_utf8();
        match ranges.last_mut() {
            Some(r) if r.end == byte_off => r.end = end,
            _ => ranges.push(byte_off..end),
        }
        next_target = target.next();
    }
    ranges
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

// T-4.3.1's navigation commands, per `docs/keymap-tc.csv`'s `nav.*` rows
// -- same "plain GPUI action, not routed through duet-commands" shape as
// every other action block above.
//
// `EnterDirectory` covers only `nav.enter_dir`, not plain Enter's real TC
// behaviour (`nav.open_or_enter`, which also opens/executes files under
// the cursor) -- file execution/associations is a separate, larger
// feature, out of scope here. `NavigateHome`'s binding (Alt+Home) is my
// own choice, not a verified TC one -- see that action's handler doc
// comment. `nav.root`'s "root of the active drive" is `/` outright until
// real multi-mount navigation exists. `nav.history_open` (a history
// overlay dialog), `nav.goto_path`/`nav.path_complete` (breadcrumb/path-
// bar editing, T-4.3.4's job), and the directory hotlist/bookmarks
// (T-4.3.5's job, FR-NAV-08's other half) are all deliberately not here.
actions!(
    duet_file_table,
    [
        EnterDirectory,
        NavigateParent,
        NavigateRoot,
        NavigateHome,
        HistoryBack,
        HistoryForward
    ]
);

// T-4.3.3's quick-search (FR-NAV-13) and quick-filter (FR-NAV-07)
// regimes. Plain printable characters are deliberately *not* bound
// actions here (there is no `KeyBinding` for "any letter") -- they're
// captured by a raw `on_key_down` listener on this view's root `div()`
// in `render()` instead, since GPUI's action-binding system is
// chord-based, not a catch-all for arbitrary text input. `QuickSearchCancel`
// (Escape) and `QuickFilterToggle` (`Ctrl+P`, this codebase's own choice
// -- see `QuickSearchMode`'s doc comment) are the only two regime
// -related *actions*.
actions!(duet_file_table, [QuickSearchCancel, QuickFilterToggle]);

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
        KeyBinding::new("enter", EnterDirectory, Some("FileTable")),
        KeyBinding::new("ctrl-pagedown", EnterDirectory, Some("FileTable")),
        KeyBinding::new("backspace", NavigateParent, Some("FileTable")),
        KeyBinding::new("ctrl-pageup", NavigateParent, Some("FileTable")),
        // Not from docs/keymap-tc.csv (no row binds Alt+Up to anything) --
        // added on request as a third, common-convention way to go up,
        // alongside the two TC-verified ones above.
        KeyBinding::new("alt-up", NavigateParent, Some("FileTable")),
        KeyBinding::new("ctrl-\\", NavigateRoot, Some("FileTable")),
        KeyBinding::new("alt-home", NavigateHome, Some("FileTable")),
        KeyBinding::new("alt-left", HistoryBack, Some("FileTable")),
        KeyBinding::new("alt-right", HistoryForward, Some("FileTable")),
        KeyBinding::new("escape", QuickSearchCancel, Some("FileTable")),
        KeyBinding::new("ctrl-p", QuickFilterToggle, Some("FileTable")),
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
    /// The directory this panel is currently showing -- `navigate_to`
    /// (T-4.3.1) is the only thing that changes it after construction.
    current_dir: PathBuf,
    /// The volume's free/total space as of the last successful query
    /// (T-4.2.7's free-space indicator), or `None` until
    /// `spawn_volume_stats_load` finishes at least once. Refreshed
    /// whenever `current_dir` changes -- there's no mount-change *event*
    /// to react to yet (no filesystem-watcher-driven mount table exists),
    /// so "the directory changed" is the practical proxy for "the volume
    /// might have changed" until real multi-mount navigation lands.
    volume_stats: Option<duet_vfs::VolumeStats>,
    /// Retained (rather than only ever borrowed transiently through
    /// `new`) so `navigate_to` (T-4.3.1) can re-spawn a directory listing
    /// and a volume-stats query later, not just once at construction.
    tokio_handle: tokio::runtime::Handle,
    /// Per-panel directory history (FR-NAV-08), browser-style: `navigate_to`
    /// pushes the *previous* directory here and clears `history_forward`
    /// on every ordinary navigation (entering a directory, going to the
    /// parent/root/home); `history_back`/`history_forward` themselves move
    /// between the two stacks without disturbing either past that.
    history_back: Vec<PathBuf>,
    history_forward: Vec<PathBuf>,
    /// T-4.3.2's tab lock (`tab.lock`, *without* `tab.lock_dir_change`):
    /// when `Some`, `navigate_to` hands off to this callback instead of
    /// navigating in place -- see `navigate_to`'s doc comment. `None` for
    /// an unlocked tab (the default for every tab, and the only state a
    /// bare `FileTable` constructed outside a `Panel` -- e.g. in a test --
    /// ever has). Set via [`Self::set_locked_navigation`] by `Panel`,
    /// which owns the actual lock flags this reduces to a single "redirect
    /// or don't" callback; this module deliberately has no `Panel`/tab
    /// concept of its own, matching every other crate-internal boundary in
    /// this codebase (a `FileTable` must remain meaningful standalone).
    locked_navigation: Option<LockedNavigationHandler>,
    /// FR-NAV-13's idle-timeout duration (`settings.toml`'s
    /// `navigation.quick_search_idle_timeout_ms`, default 1200ms) -- read
    /// once from settings, same as `mouse_mode`/`quick_search_default_mode`.
    /// Lives here rather than on `FileTableDelegate` since only the
    /// idle-timer `cx.spawn` (this struct's own job, not rendering) reads
    /// it.
    quick_search_idle_timeout: Duration,
}

/// `dir` is where the locked tab's navigation attempt was headed;
/// `Rc`, not `Box`, because [`FileTable::navigate_to`] clones it out of
/// `self` before calling it (so the call itself doesn't hold `self`
/// borrowed) -- a `Box` can't be cheaply cloned, an `Rc` can.
pub(crate) type LockedNavigationHandler = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

/// T-4.3.8's middle-click "open in a new tab" gesture (this codebase's
/// own reasonable default -- `design.md` gives no guidance on this
/// specific mouse gesture, flagged as such in the PR). Takes the clicked
/// entry's *name*, not a full `PathBuf` like [`LockedNavigationHandler`]
/// -- `FileTableDelegate` (where the click is actually detected, inside
/// `render_tr`) deliberately doesn't know `current_dir` (see
/// `has_parent_row`'s doc comment), so `Panel::add_tab_entry`'s handler
/// resolves the name against the table's *current* `current_dir` itself,
/// read fresh at click time through a captured `WeakEntity<FileTable>`
/// -- not whatever directory the tab happened to be showing when the
/// handler was first installed.
pub(crate) type NewTabHandler = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Fired by [`FileTable::navigate_to`] once `current_dir` actually changes
/// -- deliberately *not* fired by ordinary cursor movement, selection, or
/// sort/loading-state changes, all of which also call `cx.notify()` for
/// their own reasons but don't represent "this tab is now showing a
/// different directory." T-4.3.2's `Panel` (the per-side tab container)
/// subscribes to exactly this event per tab so it knows when to
/// eagerly persist `session.json` -- without a narrowly-scoped event to
/// key off, the only alternative would be re-persisting on every
/// keystroke that moves the cursor, which is wasteful I/O for a change
/// this frequent. Cursor position and sort *are* part of what gets
/// persisted (T-4.3.7), just not through this event -- `workspace.rs`'s
/// periodic save (`SESSION_PERIODIC_SAVE_INTERVAL`) covers those instead,
/// deliberately eventually-consistent rather than instrumenting every
/// cursor-moving/sort-changing call site individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTableEvent {
    DirectoryChanged,
}

impl EventEmitter<FileTableEvent> for FileTable {}

/// What a freshly-created tab seeds its very first directory load with,
/// beyond the directory itself -- T-4.3.7's session-restore inputs (a
/// `Panel` reconstructing tabs from `session.json`) and the closely
/// related "sort persists across ordinary navigation, not just
/// restarts" fix ([`FileTable::navigate_to`] reads the *current* sort
/// back out of this same shape rather than resetting to the default on
/// every directory change). `Default`'s `sort: (SortColumn::Name, true)`
/// matches `duet_index::SortOptions::default()`; `cursor_name: None`
/// simply means "land on row 0", [`FileTableDelegate::set_model`]'s own
/// default when there's no specific entry to restore onto.
#[derive(Clone)]
pub(crate) struct TabRestore {
    pub cursor_name: Option<String>,
    pub sort: (SortColumn, bool),
}

impl Default for TabRestore {
    fn default() -> Self {
        Self {
            cursor_name: None,
            sort: (SortColumn::Name, true),
        }
    }
}

/// FR-SEL-06: which mouse gesture selects a row -- `windows` (plain
/// left-click selects, matching Explorer) or `norton` (plain left-click
/// only moves the cursor; right-click both toggles selection and shows
/// the context menu, TC's own "Norton Commander" convention), or `none`
/// (mouse never changes selection at all, only the keyboard does).
/// Shift+click/Ctrl+click range/toggle-select and double-click-to-enter
/// behave identically in every mode -- only the *plain*, unmodified
/// click's effect on selection depends on this (T-4.3.8).
///
/// Read once from `settings.toml`'s `[selection] mouse_mode`
/// (`duet_config::settings::Selection::mouse_mode`) by
/// `workspace::load_mouse_mode` and threaded down through
/// `Panel`/`FileTable::new` to every `FileTableDelegate` in the
/// workspace -- there is no live-reload path yet (same as
/// `splitter_ratio`'s initial value), so this is fixed for the process's
/// lifetime once read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MouseMode {
    #[default]
    Windows,
    Norton,
    None,
}

impl MouseMode {
    /// Parses `settings.toml`'s raw `mouse_mode` string. An unrecognized
    /// value (a hand-edited file, or one written by a newer build) falls
    /// back to `Windows` -- `Settings::default()`'s own documented
    /// default -- logged once rather than failing startup, same
    /// tolerance-over-strictness policy `duet-config`'s `#[serde(default)]`
    /// fields already apply throughout.
    pub(crate) fn from_settings_str(value: &str) -> Self {
        match value {
            "windows" => Self::Windows,
            "norton" => Self::Norton,
            "none" => Self::None,
            other => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "unknown selection.mouse_mode {other:?}, defaulting to windows"
                );
                Self::Windows
            }
        }
    }
}

/// FR-NAV-07/FR-NAV-13: the two regimes a quick-search session can be in.
/// `Jump` moves the cursor to the best fuzzy match on every keystroke
/// (fuzzy *subsequence*, not prefix, matching per FR-NAV-13); `Filter`
/// hides non-matching rows instead of moving the cursor
/// (`FilterSpec::quick_filter`'s existing case-insensitive substring
/// match, not fuzzy -- neither FR-NAV-07 nor FR-NAV-13 requires filter
/// mode to be fuzzy, and reusing `FilterSpec` as-is needs no
/// `duet_index` changes at all).
///
/// Which one *plain* typing starts is `settings.toml`'s own
/// `navigation.quick_search_mode`; `Ctrl+P` (`QuickFilterToggle`)
/// explicitly switches the *active* session between the two regardless
/// of that default, preserving the query buffer across the switch --
/// this codebase's own resolution (in direct consultation, not a
/// TC-verified convention) of FR-NAV-07's underspecified "a
/// modifier-prefixed mode filters the panel instead of jumping": Shift
/// can't be the modifier (queries need uppercase letters), and
/// `docs/keymap-tc.csv`'s own guess at a chord is tagged "uncertain"
/// confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum QuickSearchMode {
    #[default]
    Jump,
    Filter,
}

impl QuickSearchMode {
    /// Parses `settings.toml`'s raw `navigation.quick_search_mode`
    /// string. Same tolerance-over-strictness policy as
    /// [`MouseMode::from_settings_str`].
    pub(crate) fn from_settings_str(value: &str) -> Self {
        match value {
            "jump" => Self::Jump,
            "filter" => Self::Filter,
            other => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "unknown navigation.quick_search_mode {other:?}, defaulting to jump"
                );
                Self::Jump
            }
        }
    }

    /// `Ctrl+P`'s effect on an already-active (or about-to-start) session.
    fn toggled(self) -> Self {
        match self {
            Self::Jump => Self::Filter,
            Self::Filter => Self::Jump,
        }
    }
}

/// `Jump` mode's current best match -- what the indicator ("find: rmr
/// (2/5)") and the matched row's character-highlighting both read. Only
/// ever `Some` on a [`QuickSearchState`] whose `mode` is `Jump`.
#[derive(Debug, Clone)]
struct JumpMatch {
    /// 1-based rank among every entry that matched at all, ordered by
    /// score (descending), ties broken by distance from the cursor's row
    /// *before this keystroke* (ascending) -- FR-NAV-13's literal
    /// tiebreak rule ("entries physically nearer the current cursor row
    /// breaking ties in favour of the smaller visual jump").
    ordinal: usize,
    total: usize,
    /// The matched entry's *model*-row index (`model.order()` space, not
    /// display-row) -- `render_tr`/`render_td` translate as needed, same
    /// as every other model-row-space field on this delegate.
    model_row: usize,
    /// Character indices into the entry's name that `nucleo_matcher`
    /// scored as part of the match, sorted and deduplicated (raw
    /// `Pattern::indices` output isn't -- see that method's own doc
    /// comment) -- what `render_td` highlights in the Name cell.
    indices: Vec<u32>,
}

/// Transient per-tab quick-search/quick-filter session state (design.md
/// §9.2: "A small piece of transient state on the tab... not part of
/// `DirectoryModel` itself since it's UI-session state, not data").
/// [`FileTableDelegate::quick_search`] is `None` whenever no regime is
/// active -- the common case; the whole struct is dropped (not reset
/// field-by-field) on every exit condition (Escape, idle timeout,
/// jump-mode cursor movement, focus loss), which is also what clears
/// `DirectoryModel`'s filter back to `None` in `Filter` mode -- see
/// `FileTable`'s `exit_quick_search`.
#[derive(Debug, Clone)]
struct QuickSearchState {
    mode: QuickSearchMode,
    /// Characters typed so far this session.
    query: String,
    /// Bumped on every keystroke and on regime start; the idle-timeout
    /// timer captures the generation it was scheduled for and no-ops if
    /// it's since changed, so a stale timer from an earlier keystroke can
    /// never cancel a session a newer keystroke kept alive.
    generation: u64,
    /// `Jump` mode's current best match, `None` if the query currently
    /// matches nothing. Always `None` while `mode` is `Filter`.
    jump_match: Option<JumpMatch>,
    /// `Filter` mode's current visible-row count (`model.order().len()`
    /// after the filter applies). Always `None` while `mode` is `Jump`.
    filter_match_count: Option<usize>,
}

/// `settings.toml` values read once by `Workspace::new` and passed down
/// unchanged to every `FileTable` a `Panel` creates -- bundled together
/// (rather than three more `FileTable::new` parameters, which would push
/// it well past a readable arg count) since they're always read/threaded
/// as a group and share the same "no live-reload path yet" story.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileTableSettings {
    pub(crate) mouse_mode: MouseMode,
    pub(crate) quick_search_default_mode: QuickSearchMode,
    pub(crate) quick_search_idle_timeout: Duration,
}

impl FileTable {
    /// Starts listing `dir` in the background and returns immediately with
    /// an empty, `loading` table -- `spawn_directory_load` populates it
    /// once the listing completes. Also starts a separate, independent
    /// background query for the volume's free/total space
    /// (`spawn_volume_stats_load`) -- kept as two separate background
    /// tasks rather than one combined round-trip so a slow `statvfs` (an
    /// unusual but possible stall on some remote-backed or heavily
    /// loaded filesystems) can never delay the directory listing itself
    /// from appearing.
    ///
    /// `width_seed`: see `FileTableDelegate::responsive_seed`'s doc
    /// comment -- `Some((widths, available))` from an already-measured
    /// sibling tab in the same `Panel` skips this table's first-frame
    /// narrow-column flash; `None` (a brand-new panel, nothing to copy
    /// from yet) falls back to the ordinary narrow-then-corrects default.
    ///
    /// `restore`: see [`TabRestore`] -- applied to this table's very
    /// first directory load only (subsequent navigation reads sort back
    /// out of the live model instead, per `navigate_to`'s doc comment).
    ///
    /// `mouse_mode`: FR-SEL-06, see [`MouseMode`]'s doc comment -- read
    /// once from `settings.toml` by `workspace::load_mouse_mode` and
    /// passed down unchanged from every tab-creating call in `Panel`.
    ///
    /// `pub(crate)`, not `pub`: only `Panel::add_tab_entry` ever
    /// constructs a `FileTable` now (T-4.3.2 made `Panel` the sole owner
    /// of tab lifecycle) -- taking `TabRestore` (itself `pub(crate)`) as
    /// a parameter here would otherwise leak a private type through a
    /// public signature.
    pub(crate) fn new(
        dir: PathBuf,
        tokio_handle: tokio::runtime::Handle,
        width_seed: Option<([f32; 3], f32)>,
        restore: TabRestore,
        settings: FileTableSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut delegate = FileTableDelegate::new(DirectoryModel::new());
        if let Some((widths, available)) = width_seed {
            delegate.seed_column_widths(widths, available);
        }
        delegate.set_mouse_mode(settings.mouse_mode);
        delegate.set_quick_search_default_mode(settings.quick_search_default_mode);
        let state = cx.new(|cx| TableState::new(delegate, window, cx));
        spawn_directory_load(
            dir.clone(),
            tokio_handle.clone(),
            state.clone(),
            restore.cursor_name,
            restore.sort,
            0,
            cx,
        );
        spawn_volume_stats_load(dir.clone(), tokio_handle.clone(), 0, cx);

        // `duet_widgets::table::TableState` has its own built-in
        // click-to-select row/column tracking (`selected_row`/
        // `selected_col`, set by an unconditional `on_click` handler
        // baked into the widget itself -- `on_row_left_click` calls
        // `set_selected_row` with no gate, confirmed by reading
        // `gpui-component-0.5.1/src/table/state.rs`), completely separate
        // from `FileTableDelegate::cursor_row`/`model.selection()`. Left
        // alone, a click paints its own persistent highlight
        // (`table_active`) on top of this delegate's own cursor/selection
        // rendering -- a confusing "second cursor" -- so every event still
        // clears it straight back out, same as before T-4.3.8: real
        // click-driven cursor movement is [`FileTable::handle_left_click`]
        // now, reading `event`/`window.modifiers()` itself rather than
        // anything `TableState` tracked.
        //
        // `subscribe_in`, not `subscribe`: `DoubleClickedRow` needs
        // `window` (`enter_cursor_directory` navigates), and unlike the
        // cross-entity callbacks this module documents elsewhere
        // (`locked_navigation`, stored and invoked far later with no
        // `window` of its own), this one runs synchronously inside
        // `TableState`'s own event emission -- `window` is simply
        // threaded straight through, no `Window::spawn` dance needed.
        cx.subscribe_in(
            &state,
            window,
            |this, state, event, window, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let row_ix = *row_ix;
                    state.update(cx, |state, cx| state.clear_selection(cx));
                    this.handle_left_click(row_ix, window.modifiers(), cx);
                }
                TableEvent::DoubleClickedRow(row_ix) => {
                    this.move_cursor_to(*row_ix, cx);
                    this.enter_cursor_directory(window, cx);
                }
                TableEvent::SelectColumn(_) => {
                    state.update(cx, |state, cx| state.clear_selection(cx));
                }
                _ => {}
            },
        )
        .detach();

        let focus_handle = cx.focus_handle();
        // FR-NAV-13: "panel losing focus" is one of quick-search's exit
        // conditions. `window.on_focus_out` (not a builder method chained
        // onto a `div()` -- it's `Window`'s own subscription API) fires
        // whenever `focus_handle` or a descendant of it loses focus; its
        // listener only gets `&mut Window`/`&mut App`, not this entity's
        // own `Context`, so reaching back in goes through a captured
        // `WeakEntity` + `update`, same as every other cross-entity
        // callback in this module. `.detach()` keeps the `Subscription`
        // alive for the table's whole lifetime, same as every other
        // `cx.subscribe(...).detach()` here.
        let weak_this = cx.entity().downgrade();
        window
            .on_focus_out(&focus_handle, cx, move |_event, _window, cx| {
                let _ = weak_this.update(cx, |this, cx| this.exit_quick_search(cx));
            })
            .detach();

        Self {
            state,
            focus_handle,
            current_dir: dir,
            volume_stats: None,
            tokio_handle,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            locked_navigation: None,
            quick_search_idle_timeout: settings.quick_search_idle_timeout,
        }
    }

    /// See the `locked_navigation` field's doc comment. `Panel` calls this
    /// with `Some(..)` when a tab is locked without `lock_dir_change`, and
    /// with `None` to unlock it (or to allow in-place navigation again
    /// under `lock_dir_change`).
    pub fn set_locked_navigation(&mut self, handler: Option<LockedNavigationHandler>) {
        self.locked_navigation = handler;
    }

    /// See [`NewTabHandler`]'s doc comment. `Panel::add_tab_entry` calls
    /// this once, right after constructing the table -- the handler it
    /// builds captures this same table's own `WeakEntity` (to resolve a
    /// click's target name against `current_dir` at click time), so it
    /// can only be built once the table already exists.
    pub(crate) fn set_new_tab_handler(
        &mut self,
        handler: Option<NewTabHandler>,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, _cx| {
            state.delegate_mut().set_new_tab_handler(handler);
        });
    }

    /// Exposes the underlying table state -- e.g. for
    /// `workspace::panel_footer` (T-4.2.7) to reach
    /// `TableState::delegate().model()` from outside this view.
    pub fn state(&self) -> &Entity<TableState<FileTableDelegate>> {
        &self.state
    }

    /// The directory this panel is currently showing. See the field's doc
    /// comment.
    pub fn current_dir(&self) -> &std::path::Path {
        &self.current_dir
    }

    /// See `FileTableDelegate::responsive_seed`.
    pub(crate) fn responsive_seed(&self, cx: &App) -> Option<([f32; 3], f32)> {
        self.state.read(cx).delegate().responsive_seed()
    }

    /// See `FileTableDelegate::cursor_entry_name` -- T-4.3.7's
    /// `Panel::snapshot`/`ClosedTab` use this to capture what to restore
    /// the cursor onto later.
    pub(crate) fn cursor_entry_name(&self, cx: &App) -> Option<String> {
        self.state.read(cx).delegate().cursor_entry_name()
    }

    /// See `FileTableDelegate::quick_search_indicator_text`'s doc
    /// comment.
    pub(crate) fn quick_search_indicator_text(&self, cx: &App) -> Option<String> {
        self.state.read(cx).delegate().quick_search_indicator_text()
    }

    /// The tab's current sort column + direction -- T-4.3.7's
    /// `Panel::snapshot`/`ClosedTab`/`new_tab` (inherit the active tab's
    /// sort into a freshly created sibling) all read this.
    pub(crate) fn sort_state(&self, cx: &App) -> (SortColumn, bool) {
        let options = self.state.read(cx).delegate().model().sort_options();
        (options.column, options.ascending)
    }

    /// The volume's free/total space as of the last successful query, or
    /// `None` before the first one completes. See the field's doc comment.
    pub fn volume_stats(&self) -> Option<duet_vfs::VolumeStats> {
        self.volume_stats
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

    /// T-4.3.8 (FR-SEL-06): a real left click's full effect, built entirely
    /// out of the same primitives the keyboard already uses -- there's
    /// nothing mouse-specific about *how* selection changes, only about
    /// *when* each existing method fires. `row_ix` is a display-row index
    /// (see [`FileTableDelegate::model_row`]'s doc comment); `modifiers`
    /// is read live from `window.modifiers()` at the call site, since
    /// `TableEvent::SelectRow` itself carries none.
    ///
    /// Shift+click (range-select) and Ctrl+click (toggle) behave the same
    /// under every [`MouseMode`] -- only a *plain*, unmodified click's
    /// effect on selection depends on it: `Windows` selects just this row
    /// (Explorer's convention), `Norton`/`None` move the cursor only,
    /// leaving selection for the right-click menu (Norton) or the
    /// keyboard alone (`None`) to handle -- see `MouseMode`'s own doc
    /// comment. Clicking the synthetic ".." row only ever moves the
    /// cursor onto it, regardless of modifiers: there's no entry there to
    /// select, and (per `cursor_row`/`cursor_on_parent`'s own doc
    /// comments) blindly running the ordinary selection methods while
    /// sitting on it would silently act on whatever real entry the cursor
    /// last meaningfully pointed at instead.
    fn handle_left_click(&mut self, row_ix: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        // FR-NAV-13's exit list explicitly includes "mouse click".
        self.exit_quick_search(cx);
        let Some(model_row) = self.state.read(cx).delegate().model_row(row_ix) else {
            self.move_cursor_to(row_ix, cx);
            return;
        };
        if modifiers.shift {
            self.extend_selection_to_edge(model_row, cx);
            return;
        }
        self.move_cursor_to(row_ix, cx);
        if modifiers.control {
            self.toggle_selection(cx);
            return;
        }
        if self.state.read(cx).delegate().mouse_mode() == MouseMode::Windows {
            self.with_selection(cx, |delegate| delegate.deselect_all());
            self.toggle_selection(cx);
        }
    }

    /// FR-NAV-07/FR-NAV-13: clears the active quick-search/quick-filter
    /// session, if any -- the single exit path every exit condition
    /// (Escape, idle timeout, jump-mode cursor movement, mouse click,
    /// navigation, focus loss) funnels through. A no-op if no session is
    /// active, so every caller can invoke this unconditionally rather
    /// than checking first. See `FileTableDelegate::clear_quick_search`
    /// for the actual state-clearing logic this wraps with `cx.notify()`.
    fn exit_quick_search(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.delegate_mut().clear_quick_search();
            cx.notify();
        });
    }

    /// The "non-search cursor movement" half of FR-NAV-13's exit list
    /// (arrows, Home/End, PgUp/PgDn) -- unlike [`Self::exit_quick_search`],
    /// this only exits an active *jump*-mode session. Filter mode is
    /// deliberately *not* exited by cursor movement -- this codebase's
    /// own judgment call (design.md doesn't say either way): every
    /// mainstream type-ahead filter (Explorer, VS Code's Quick Open, ...)
    /// lets you arrow through the already-filtered results without
    /// cancelling the filter, and cancelling it here would make filter
    /// mode useless for anything beyond a single keystroke.
    fn exit_quick_search_if_jump(&mut self, cx: &mut Context<Self>) {
        let is_jump =
            self.state.read(cx).delegate().quick_search_mode() == Some(QuickSearchMode::Jump);
        if is_jump {
            self.exit_quick_search(cx);
        }
    }

    /// FR-NAV-07/FR-NAV-13: appends `c` to the active session's query
    /// buffer, starting a new one (in `quick_search_default_mode`) if
    /// none is active yet. The `on_key_down` listener in [`Self::render`]
    /// is this method's only caller -- see that listener's own doc
    /// comment for why raw keystroke capture is even safe to add without
    /// double-handling any of this view's already-bound actions.
    fn push_quick_search_char(&mut self, c: char, cx: &mut Context<Self>) {
        let generation = self.state.update(cx, |state, _cx| {
            let default_mode = state.delegate().quick_search_default_mode();
            let delegate = state.delegate_mut();
            let session = delegate
                .quick_search
                .get_or_insert_with(|| QuickSearchState {
                    mode: default_mode,
                    query: String::new(),
                    generation: 0,
                    jump_match: None,
                    filter_match_count: None,
                });
            session.query.push(c);
            session.generation += 1;
            session.generation
        });
        self.apply_quick_search(cx);
        self.schedule_quick_search_idle_timeout(generation, cx);
    }

    /// `Ctrl+P` (`QuickFilterToggle`): starts a fresh session in `Filter`
    /// mode if none is active (this codebase's own choice -- Ctrl+P
    /// always means "filter," regardless of `quick_search_default_mode`,
    /// so its behavior is simple to document even though it's
    /// occasionally redundant with plain typing when the configured
    /// default already is `Filter`), or flips the active session's mode
    /// between `Jump`/`Filter` otherwise -- the query buffer carries over
    /// either way, immediately re-applied under the new mode.
    fn toggle_quick_filter(&mut self, cx: &mut Context<Self>) {
        let generation = self.state.update(cx, |state, _cx| {
            let delegate = state.delegate_mut();
            match delegate.quick_search.as_mut() {
                Some(session) => {
                    session.mode = session.mode.toggled();
                    session.generation += 1;
                }
                None => {
                    delegate.quick_search = Some(QuickSearchState {
                        mode: QuickSearchMode::Filter,
                        query: String::new(),
                        generation: 0,
                        jump_match: None,
                        filter_match_count: None,
                    });
                }
            }
            delegate
                .quick_search
                .as_ref()
                .expect("just set above")
                .generation
        });
        self.apply_quick_search(cx);
        self.schedule_quick_search_idle_timeout(generation, cx);
    }

    /// The wrapper `push_quick_search_char`/`toggle_quick_filter` both
    /// call after touching `self.quick_search` itself -- see
    /// `FileTableDelegate::apply_quick_search`'s own doc comment for the
    /// actual scoring/filtering work.
    fn apply_quick_search(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            // UAT: the cursor was jumping to the best match even when it
            // fell outside the currently visible scroll range, with
            // nothing bringing the viewport along -- always scroll to
            // wherever the cursor actually ends up after each keystroke,
            // same as every other cursor-moving command in this file
            // already does.
            if let Some(row) = state.delegate_mut().apply_quick_search() {
                state.scroll_to_row(row, cx);
            }
            cx.notify();
        });
    }

    /// FR-NAV-13's idle-timeout exit condition: schedules
    /// `exit_quick_search` to run `self.quick_search_idle_timeout` after
    /// this keystroke, unless a newer keystroke (a higher `generation`)
    /// has since landed. `cx.spawn` futures in this codebase have no
    /// exposed cancellation handle (unlike, say,
    /// `Workspace`'s `SESSION_PERIODIC_SAVE_INTERVAL` loop, which polls
    /// indefinitely rather than needing a one-shot reset per keystroke),
    /// so comparing generations after the timer fires is what makes an
    /// earlier keystroke's stale timer a no-op once a newer one has kept
    /// the session alive.
    fn schedule_quick_search_idle_timeout(&mut self, generation: u64, cx: &mut Context<Self>) {
        let timeout = self.quick_search_idle_timeout;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            let _ = this.update(cx, |this, cx| {
                let still_current =
                    this.state.read(cx).delegate().quick_search_generation() == Some(generation);
                if still_current {
                    this.exit_quick_search(cx);
                }
            });
        })
        .detach();
    }

    /// The core "change directory" primitive T-4.3.1's other navigation
    /// methods build on: updates `current_dir`, flips the table to its
    /// loading state immediately (visual feedback while the new listing
    /// is in flight, same as the very first load), and re-spawns both
    /// background queries (`spawn_directory_load`/`spawn_volume_stats_load`)
    /// against the new directory.
    ///
    /// `push_history`: `true` for ordinary navigation (entering a
    /// directory, going to the parent/root/home) -- pushes the *previous*
    /// `current_dir` onto `history_back` and clears `history_forward`,
    /// browser-style. `false` for `navigate_history_back`/`_forward`
    /// themselves, which move between the two stacks directly instead of
    /// adding to either.
    ///
    /// `select_name`: see `spawn_directory_load`'s doc comment.
    ///
    /// The new listing sorts by whatever this tab's *current* sort is
    /// (read back out of the live model via `sort_options()`), not a
    /// hardcoded Name-ascending default -- T-4.3.7: sorting by e.g. Size
    /// and then navigating into a subdirectory should keep sorting by
    /// Size, the same way TC itself treats sort as a per-panel/per-tab
    /// setting that survives ordinary navigation, not something each
    /// fresh directory listing resets on its own. This is also what makes
    /// session-restored sort state (`TabRestore::sort`, applied only to
    /// the very first load in `Self::new`) actually stick past the first
    /// navigation after restore, rather than reverting on the next
    /// directory change.
    ///
    /// T-4.3.2's tab lock: if [`Self::locked_navigation`] is set (this
    /// tab is locked *without* `lock_dir_change`), this doesn't navigate
    /// at all -- it hands `dir` to that callback instead, which opens a
    /// new, unlocked tab at `dir` and leaves this tab exactly where it
    /// was, matching TC's real locked-tab behaviour. `window` only exists
    /// on this signature for that redirect (constructing the new tab's
    /// `FileTable` needs a live `Window`, same as this one's own
    /// constructor does) -- the ordinary in-place-navigation path below
    /// never touches it.
    fn navigate_to(
        &mut self,
        dir: PathBuf,
        push_history: bool,
        select_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handler) = self.locked_navigation.clone() {
            handler(dir, window, cx);
            return;
        }
        // A quick-search session's `JumpMatch`/filter both refer to
        // *this* listing -- navigating away invalidates them outright
        // (this is also the single choke point every other navigation
        // method funnels through, so this alone covers Enter/Backspace/
        // Ctrl+\/Alt+Home/Alt+Left/Alt+Right too, not just this call
        // site).
        self.exit_quick_search(cx);
        if push_history {
            self.history_back.push(self.current_dir.clone());
            self.history_forward.clear();
        }
        self.current_dir = dir.clone();
        cx.emit(FileTableEvent::DirectoryChanged);
        let (generation, sort_options) = self.state.update(cx, |state, cx| {
            let generation = state.delegate_mut().bump_nav_generation();
            let sort_options = state.delegate().model().sort_options();
            state.delegate_mut().set_loading(true);
            cx.notify();
            (generation, sort_options)
        });
        spawn_directory_load(
            dir.clone(),
            self.tokio_handle.clone(),
            self.state.clone(),
            select_name,
            (sort_options.column, sort_options.ascending),
            generation,
            cx,
        );
        spawn_volume_stats_load(dir, self.tokio_handle.clone(), generation, cx);
    }

    /// Enter/Ctrl+PgDn (`nav.enter_dir`): descends into the directory
    /// under the cursor, or -- if the cursor is on the synthetic ".." row
    /// (T-4.3.1) -- goes up instead, same as Backspace. A no-op if the
    /// cursor isn't on either. Following a symlink into its target
    /// directory (`nav.follow_symlink`) and opening/executing a file (the
    /// other half of plain Enter's real TC behaviour, `nav.open_or_enter`)
    /// are both out of scope here; see the module doc comment.
    fn enter_cursor_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).delegate().cursor_on_parent() {
            self.navigate_to_parent(window, cx);
            return;
        }
        let name = self.state.read(cx).delegate().cursor_dir_name();
        if let Some(name) = name {
            let target = self.current_dir.join(name);
            self.navigate_to(target, true, None, window, cx);
        }
    }

    /// Backspace/Ctrl+PgUp (`nav.open_parent_and_select`): goes up one
    /// level, then -- once the parent's listing loads -- moves the
    /// cursor onto the directory just left ("the detail that makes
    /// navigation feel right" per this task's own AC). A no-op already
    /// at the root (`Path::parent()` returns `None`).
    fn navigate_to_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.current_dir.parent().map(PathBuf::from) else {
            return;
        };
        let child_name = self
            .current_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.navigate_to(parent, true, child_name, window, cx);
    }

    /// Ctrl+\ (`nav.root`): jumps to `/`. "Root of the *active drive*"
    /// once real multi-mount navigation exists (a later task); today
    /// there's only ever the one filesystem, so this simplifies to the
    /// filesystem root outright.
    fn navigate_to_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_to(PathBuf::from("/"), true, None, window, cx);
    }

    /// `nav.home`: jumps to `$HOME`, a no-op if it isn't set. Bound to
    /// Alt+Home -- unlike every other binding in this module,
    /// `docs/keymap-tc.csv` has *no row at all* for `nav.home` (TC
    /// predates "home directory" being as central a concept as it is on
    /// Unix), so this chord is my own reasonable-default choice, not a
    /// verified TC binding. Implemented anyway (rather than left
    /// unreachable, the call T-4.2.3's `select_same_name` made) because
    /// this task's own AC names "home" as one of the four required
    /// navigation capabilities, unlike that one.
    fn navigate_to_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(home) = std::env::var_os("HOME") {
            self.navigate_to(PathBuf::from(home), true, None, window, cx);
        }
    }

    /// Alt+Left (`nav.history_back`): moves to the previous directory in
    /// this panel's own history, if any.
    fn navigate_history_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prev) = self.history_back.pop() else {
            return;
        };
        self.history_forward.push(self.current_dir.clone());
        self.navigate_to(prev, false, None, window, cx);
    }

    /// Alt+Right (`nav.history_forward`): the redo half of
    /// [`Self::navigate_history_back`].
    fn navigate_history_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.history_forward.pop() else {
            return;
        };
        self.history_back.push(self.current_dir.clone());
        self.navigate_to(next, false, None, window, cx);
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
            // FR-NAV-07/FR-NAV-13's quick-search/quick-filter character
            // capture. GPUI's action-binding system is chord-based (one
            // exact keystroke string per `KeyBinding`) -- there's no way
            // to bind "any printable character," so this raw listener is
            // the only mechanism available for it. It's safe to add
            // without risking a double-handled keystroke: `Escape`,
            // `Ctrl+P`, the arrow keys, and every other bound chord in
            // this file are all matched *actions*, and GPUI's own
            // `Window::dispatch_key_event` sets `propagate_event = false`
            // ("Actions stop propagation by default during the bubble
            // phase") the moment a bound action is dispatched, *before*
            // the raw key-down listeners below it in the dispatch order
            // ever run (confirmed by reading `gpui-0.2.2/src/window.rs`)
            // -- so this listener only ever sees keystrokes nothing else
            // already claimed.
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    let modifiers = &event.keystroke.modifiers;
                    if modifiers.control
                        || modifiers.alt
                        || modifiers.platform
                        || modifiers.function
                    {
                        return;
                    }
                    let Some(key_char) = event.keystroke.key_char.as_deref() else {
                        return;
                    };
                    let mut chars = key_char.chars();
                    let Some(c) = chars.next() else {
                        return;
                    };
                    if chars.next().is_some() {
                        return;
                    }
                    this.push_quick_search_char(c, cx);
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _event, window, _cx| {
                    window.focus(&this.focus_handle);
                }),
            )
            .on_action(cx.listener(|this, _: &CursorUp, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorDown, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor(1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorHome, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor_to(0, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorEnd, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor_to(usize::MAX, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorPageUp, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor_by_page(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CursorPageDown, _window, cx| {
                this.exit_quick_search_if_jump(cx);
                this.move_cursor_by_page(1, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickSearchCancel, _window, cx| {
                this.exit_quick_search(cx);
            }))
            .on_action(cx.listener(|this, _: &QuickFilterToggle, _window, cx| {
                this.toggle_quick_filter(cx);
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
            .on_action(cx.listener(|this, _: &EnterDirectory, window, cx| {
                this.enter_cursor_directory(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateParent, window, cx| {
                this.navigate_to_parent(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateRoot, window, cx| {
                this.navigate_to_root(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateHome, window, cx| {
                this.navigate_to_home(window, cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryBack, window, cx| {
                this.navigate_history_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryForward, window, cx| {
                this.navigate_history_forward(window, cx);
            }))
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
            // T-4.3.8: mouse wheel scrolling needs no code here at all --
            // `Table`'s rows are a `gpui-component` `TableState` built on
            // top of GPUI's own `uniform_list`, which wires up wheel
            // scroll natively through its `UniformListScrollHandle`
            // (confirmed by reading `gpui-component-0.5.1/src/table/
            // state.rs`'s `vertical_scroll_handle` field and
            // `.track_scroll(...)` call). Verified end-to-end with a live
            // launch over a 60-entry directory (more than fits one
            // screen) rather than a synthetic `ScrollWheelEvent` test --
            // simulating one accurately would mean hardcoding
            // `gpui-component`'s internal row-height/hitbox geometry into
            // this codebase's own tests, the exact coupling this task's
            // click-handling tests (`panel.rs`) were deliberately built
            // to avoid.
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
/// see the struct doc comment. `select_name`: T-4.3.1's "cursor restores
/// to the child directory when going up" (also T-4.3.7's session-restore
/// cursor position -- both funnel through the same `select_row_by_name`
/// mechanism) -- if given, the cursor lands on that entry once loaded
/// instead of `set_model`'s row-0 default; `None` when there's no
/// specific row to restore. `sort`: applied to the freshly-loaded model
/// before anything else reads it, so the very first paint already shows
/// the right order -- see `FileTable::navigate_to`'s doc comment for why
/// this is never just a hardcoded default. `generation`: the
/// `FileTableDelegate::nav_generation` this load was started for -- see
/// that field's doc comment for why the result is silently discarded
/// (not applied) if a newer navigation has started by the time this
/// completes.
fn spawn_directory_load(
    dir: PathBuf,
    tokio_handle: tokio::runtime::Handle,
    state: Entity<TableState<FileTableDelegate>>,
    select_name: Option<String>,
    sort: (SortColumn, bool),
    generation: u64,
    cx: &mut Context<FileTable>,
) {
    // Computed up front, before `dir` moves into the tokio task below --
    // `Path::parent()` is a pure path-string operation, no I/O, so there's
    // no reason to thread a `has_parent: bool` parameter through every
    // call site when the callee can just derive it from `dir` itself.
    let has_parent = dir.parent().is_some();
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
        model.sort_by(sort.0, sort.1);
        let entry_count = model.order().len();

        let updated = state.update(cx, |state, cx| {
            if state.delegate().nav_generation() != generation {
                tracing::debug!(
                    target: "duet_ui::file_table",
                    generation,
                    current = state.delegate().nav_generation(),
                    "discarding stale directory listing (a newer navigation has since started)"
                );
                return false;
            }
            let delegate = state.delegate_mut();
            delegate.set_model(model);
            delegate.set_has_parent_row(has_parent);
            if let Some(name) = &select_name {
                delegate.select_row_by_name(name);
            }
            delegate.set_loading(false);
            // UAT: navigating up (Backspace) correctly moved the cursor
            // back onto the child directory just left (`select_name`),
            // but never scrolled the viewport to follow -- if that entry
            // sat below the fold in the parent's (often much longer)
            // listing, the cursor looked like it vanished off the
            // bottom, the exact same class of bug T-4.3.3's own
            // quick-search jump had. Scrolling here unconditionally
            // (not just when `select_name` found something) also covers
            // an ordinary fresh load landing on row 0, which is a no-op
            // if the view is already scrolled to the top.
            if let Some(row) = delegate.display_row() {
                state.scroll_to_row(row, cx);
            }
            cx.notify();
            true
        });

        match updated {
            Ok(true) => {
                tracing::info!(
                    target: "duet_ui::file_table",
                    "directory listing loaded: {entry_count} entries"
                );
            }
            Ok(false) => {}
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

/// Converts `dir` into a local `VPath` -- the shared first step every
/// `LocalFs` call this module makes off the UI thread needs.
fn local_vpath(dir: &std::path::Path) -> Result<VPath, String> {
    let path_str = dir
        .to_str()
        .ok_or_else(|| "directory path is not valid UTF-8".to_string())?;
    UnixPathBuf::new(path_str)
        .map(VPath::local)
        .map_err(|e| format!("invalid path {path_str:?}: {e}"))
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
    let vpath = local_vpath(&dir)?;

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

/// Starts a background query (T-4.2.7) for the volume's free/total space
/// backing `dir`, applying the result to `FileTable::volume_stats` once it
/// completes -- see `FileTable::new`'s doc comment for why this is a
/// separate task from the directory listing rather than combined with it.
fn spawn_volume_stats_load(
    dir: PathBuf,
    tokio_handle: tokio::runtime::Handle,
    generation: u64,
    cx: &mut Context<FileTable>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio_handle.spawn(async move {
        let result = volume_stats_for(dir).await;
        let _ = tx.send(result);
    });

    cx.spawn(async move |this, cx| {
        let stats = match rx.await {
            Ok(Ok(stats)) => stats,
            Ok(Err(err)) => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "volume-stats query failed: {err}"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: "duet_ui::file_table",
                    "volume-stats task was dropped before completing"
                );
                return;
            }
        };

        // See `FileTableDelegate::nav_generation`'s doc comment: a stale
        // result from a superseded navigation is silently discarded
        // rather than overwriting a newer one's stats.
        let _ = this.update(cx, |this, cx| {
            if this.state.read(cx).delegate().nav_generation() != generation {
                tracing::debug!(
                    target: "duet_ui::file_table",
                    generation,
                    "discarding stale volume-stats result (a newer navigation has since started)"
                );
                return;
            }
            this.volume_stats = Some(stats);
            cx.notify();
        });
    })
    .detach();
}

async fn volume_stats_for(dir: PathBuf) -> Result<duet_vfs::VolumeStats, String> {
    let vpath = local_vpath(&dir)?;
    LocalFs
        .volume_stats(&vpath)
        .await
        .map_err(|e| format!("volume_stats: {e}"))
}

#[cfg(test)]
mod tests {
    use duet_types::{EntryKind, Metadata, Timestamp};

    use super::*;

    #[test]
    fn char_indices_to_byte_ranges_merges_adjacent_characters() {
        // "gamma.txt", matched at char indices 0,1,2 (contiguous "gam")
        // -- must merge into one range, not three.
        let ranges = char_indices_to_byte_ranges("gamma.txt", &[0, 1, 2]);
        assert_eq!(ranges, vec![0..3]);
    }

    #[test]
    fn char_indices_to_byte_ranges_keeps_non_adjacent_characters_separate() {
        // "gamma.txt", matched at 0 ('g') and 4 ('a', the second one) --
        // two disjoint single-character ranges.
        let ranges = char_indices_to_byte_ranges("gamma.txt", &[0, 4]);
        assert_eq!(ranges, vec![0..1, 4..5]);
    }

    #[test]
    fn char_indices_to_byte_ranges_accounts_for_multi_byte_characters() {
        // "café.txt" -- 'é' is 2 bytes in UTF-8, so the char *after* it
        // ('.') must start at byte 5, not byte 4.
        let name = "café.txt";
        assert_eq!(name.len(), 9); // 8 chars, 'é' costs 2 bytes
        let ranges = char_indices_to_byte_ranges(name, &[3, 4]); // 'é', '.'
        assert_eq!(ranges, vec![3..6]); // 'é' (2 bytes) + '.' (1 byte)
    }

    #[test]
    fn char_indices_to_byte_ranges_tolerates_unsorted_and_duplicate_input() {
        let ranges = char_indices_to_byte_ranges("gamma.txt", &[2, 0, 1, 1]);
        assert_eq!(ranges, vec![0..3]);
    }

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
    fn display_rows_count_adds_one_when_parent_row_showing() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.display_rows_count(), 5);
        delegate.set_has_parent_row(true);
        assert_eq!(delegate.display_rows_count(), 6);
        delegate.set_has_parent_row(false);
        assert_eq!(delegate.display_rows_count(), 5);
    }

    #[test]
    fn move_cursor_by_steps_onto_and_off_the_parent_row() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);

        // Row 0 (real) is display row 1 while the ".." row is showing.
        assert_eq!(delegate.display_row(), Some(1));
        assert!(!delegate.cursor_on_parent());

        // Moving up from there lands on the ".." row (display row 0),
        // not off the front of the listing.
        assert_eq!(delegate.move_cursor_by(-1), Some(0));
        assert!(delegate.cursor_on_parent());
        assert_eq!(
            delegate.cursor_row(),
            Some(0),
            "cursor_row keeps its last real position while parked on .."
        );

        // A further move up is clamped -- ".." is the top, same as row 0
        // ordinarily would be.
        assert_eq!(delegate.move_cursor_by(-1), Some(0));
        assert!(delegate.cursor_on_parent());

        // Moving back down leaves the pseudo-row and re-lands on real row 0.
        assert_eq!(delegate.move_cursor_by(1), Some(1));
        assert!(!delegate.cursor_on_parent());
        assert_eq!(delegate.cursor_row(), Some(0));
    }

    #[test]
    fn move_cursor_to_0_with_parent_row_lands_on_parent() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(3);
        assert!(!delegate.cursor_on_parent());

        assert_eq!(delegate.move_cursor_to(0), Some(0));
        assert!(delegate.cursor_on_parent());

        assert_eq!(delegate.move_cursor_to(1), Some(1));
        assert!(!delegate.cursor_on_parent());
        assert_eq!(delegate.cursor_row(), Some(0));
    }

    #[test]
    fn cursor_dir_name_is_none_while_cursor_on_parent() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("adir", &meta(EntryKind::Directory, 0, 0));
        model.sort_by(SortColumn::Name, true);
        let mut delegate = FileTableDelegate::new(model);
        delegate.set_has_parent_row(true);

        delegate.move_cursor_to(0); // the ".." row
        assert!(delegate.cursor_on_parent());
        assert_eq!(
            delegate.cursor_dir_name(),
            None,
            "must not report a directory name while parked on the pseudo-row, even \
             though cursor_row still points at a real directory underneath it"
        );
    }

    #[test]
    fn set_has_parent_row_false_clears_cursor_on_parent() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(0);
        assert!(delegate.cursor_on_parent());

        // Navigating to the filesystem root turns the parent row off --
        // there's nowhere left to park the cursor, so it must fall back
        // to a real row rather than pointing at a row that no longer
        // exists.
        delegate.set_has_parent_row(false);
        assert!(!delegate.cursor_on_parent());
        assert_eq!(delegate.display_rows_count(), 5);
    }

    #[test]
    fn set_model_resets_cursor_on_parent_even_when_a_parent_row_is_still_showing() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(0);
        assert!(delegate.cursor_on_parent());

        // A fresh listing (e.g. Enter on a subdirectory while parked on
        // "..") must start on real row 0, not silently reopen on its own
        // ".." row just because the new directory also has a parent.
        delegate.set_model(five_file_model());
        assert!(!delegate.cursor_on_parent());
        assert_eq!(delegate.cursor_row(), Some(0));
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

    #[test]
    fn cursor_dir_name_returns_name_only_for_directories() {
        let mut model = DirectoryModel::new();
        model
            .entries_mut()
            .push("adir", &meta(EntryKind::Directory, 0, 0));
        model
            .entries_mut()
            .push("bfile.txt", &meta(EntryKind::File, 10, 0));
        model.sort_by(SortColumn::Name, true);
        let mut delegate = FileTableDelegate::new(model);

        delegate.move_cursor_to(0); // "adir"
        assert_eq!(delegate.cursor_dir_name(), Some("adir".to_string()));

        delegate.move_cursor_to(1); // "bfile.txt"
        assert_eq!(delegate.cursor_dir_name(), None);
    }

    #[test]
    fn cursor_dir_name_is_none_on_empty_listing() {
        let delegate = FileTableDelegate::new(DirectoryModel::new());
        assert_eq!(delegate.cursor_dir_name(), None);
    }

    #[test]
    fn select_row_by_name_finds_and_moves_cursor() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(0);
        assert!(delegate.select_row_by_name("f3"));
        assert_eq!(delegate.cursor_row(), Some(3));
    }

    /// Regression: `select_row_by_name` computes `row` in *model*-space
    /// but must land on the matching *display* row -- with the ".." row
    /// showing (`has_parent_row`, true for every directory but the
    /// filesystem root), those differ by `parent_offset()`. Caught by
    /// T-4.3.7's session-restore work: every previous test of this method
    /// used a delegate with no parent row at all, so a one-row-too-early
    /// bug here went uncaught until a real directory (which always has a
    /// parent) exercised it.
    #[test]
    fn select_row_by_name_accounts_for_the_parent_row_offset() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(0); // lands on ".." (display row 0)

        assert!(delegate.select_row_by_name("f3"));
        // "f3" is model row 3; with the ".." row ahead of it, that's
        // display row 4, i.e. still model row 3 -- `cursor_row()` (model-
        // space) must read back exactly what it did with no parent row.
        assert_eq!(delegate.cursor_row(), Some(3));
        assert!(!delegate.cursor_on_parent());
    }

    #[test]
    fn select_row_by_name_returns_false_and_leaves_cursor_when_not_found() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(2);
        assert!(!delegate.select_row_by_name("does-not-exist"));
        assert_eq!(delegate.cursor_row(), Some(2), "cursor stays put on a miss");
    }

    #[test]
    fn mouse_mode_parses_the_three_documented_settings_toml_values() {
        assert_eq!(MouseMode::from_settings_str("windows"), MouseMode::Windows);
        assert_eq!(MouseMode::from_settings_str("norton"), MouseMode::Norton);
        assert_eq!(MouseMode::from_settings_str("none"), MouseMode::None);
    }

    #[test]
    fn mouse_mode_falls_back_to_windows_for_an_unrecognized_value() {
        assert_eq!(MouseMode::from_settings_str("bogus"), MouseMode::Windows);
        assert_eq!(MouseMode::default(), MouseMode::Windows);
    }

    #[test]
    fn quick_search_mode_parses_the_two_documented_settings_toml_values() {
        assert_eq!(
            QuickSearchMode::from_settings_str("jump"),
            QuickSearchMode::Jump
        );
        assert_eq!(
            QuickSearchMode::from_settings_str("filter"),
            QuickSearchMode::Filter
        );
    }

    #[test]
    fn quick_search_mode_falls_back_to_jump_for_an_unrecognized_value() {
        assert_eq!(
            QuickSearchMode::from_settings_str("bogus"),
            QuickSearchMode::Jump
        );
        assert_eq!(QuickSearchMode::default(), QuickSearchMode::Jump);
    }

    #[test]
    fn quick_search_mode_toggled_flips_between_jump_and_filter() {
        assert_eq!(QuickSearchMode::Jump.toggled(), QuickSearchMode::Filter);
        assert_eq!(QuickSearchMode::Filter.toggled(), QuickSearchMode::Jump);
    }

    #[test]
    fn file_table_delegate_defaults_to_windows_mouse_mode_until_set() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.mouse_mode(), MouseMode::Windows);
        delegate.set_mouse_mode(MouseMode::Norton);
        assert_eq!(delegate.mouse_mode(), MouseMode::Norton);
    }

    #[test]
    fn context_menu_row_prep_moves_the_cursor_to_the_right_clicked_row() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);

        assert!(delegate.prepare_context_menu_row(3)); // display row 3 = model row 2 = f2
        assert_eq!(delegate.cursor_row(), Some(2));
        assert!(!delegate.cursor_on_parent());
    }

    #[test]
    fn context_menu_row_prep_on_the_parent_row_is_a_no_op() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(2); // land on a real row first (model row 1)

        assert!(!delegate.prepare_context_menu_row(0)); // ".."
        assert_eq!(
            delegate.cursor_row(),
            Some(1),
            "right-clicking \"..\" must not move the cursor"
        );
        assert!(!delegate.cursor_on_parent());
    }

    #[test]
    fn context_menu_row_prep_toggles_selection_in_norton_mode_only() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.set_mouse_mode(MouseMode::Norton);

        assert!(delegate.prepare_context_menu_row(2)); // model row 1 = f1
        let id = EntryId::new(delegate.model().order()[1]);
        assert!(
            delegate.model().is_selected(id),
            "Norton mode: right-click both moves the cursor and toggles selection"
        );

        assert!(delegate.prepare_context_menu_row(2)); // right-click it again
        assert!(
            !delegate.model().is_selected(id),
            "a second right-click on the same row toggles it back off"
        );
    }

    #[test]
    fn context_menu_row_prep_does_not_touch_selection_outside_norton_mode() {
        for mode in [MouseMode::Windows, MouseMode::None] {
            let mut delegate = FileTableDelegate::new(five_file_model());
            delegate.set_has_parent_row(true);
            delegate.set_mouse_mode(mode);

            assert!(delegate.prepare_context_menu_row(2));
            assert!(
                delegate.model().selection().is_empty(),
                "{mode:?}: right-click must only move the cursor, never select"
            );
        }
    }

    #[test]
    fn quick_search_indicator_text_is_none_when_no_session_active() {
        let delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.quick_search_indicator_text(), None);
    }

    #[test]
    fn quick_search_indicator_text_shows_jump_ordinal() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.quick_search = Some(QuickSearchState {
            mode: QuickSearchMode::Jump,
            query: "rmr".to_string(),
            generation: 1,
            jump_match: Some(JumpMatch {
                ordinal: 2,
                total: 5,
                model_row: 0,
                indices: vec![],
            }),
            filter_match_count: None,
        });
        assert_eq!(
            delegate.quick_search_indicator_text(),
            Some("find: rmr (2/5)".to_string())
        );
    }

    #[test]
    fn quick_search_indicator_text_shows_no_match_for_jump_with_nothing_found() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.quick_search = Some(QuickSearchState {
            mode: QuickSearchMode::Jump,
            query: "zzz".to_string(),
            generation: 1,
            jump_match: None,
            filter_match_count: None,
        });
        assert_eq!(
            delegate.quick_search_indicator_text(),
            Some("find: zzz (no match)".to_string())
        );
    }

    #[test]
    fn quick_search_indicator_text_shows_filter_match_count() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.quick_search = Some(QuickSearchState {
            mode: QuickSearchMode::Filter,
            query: "f".to_string(),
            generation: 1,
            jump_match: None,
            filter_match_count: Some(3),
        });
        assert_eq!(
            delegate.quick_search_indicator_text(),
            Some("filter: f (3 matches)".to_string())
        );
    }

    /// UAT regression: `apply_quick_search_jump` moved the cursor but
    /// nothing brought the viewport along, leaving the cursor jumping to
    /// matches outside the visible scroll range. `FileTable::
    /// apply_quick_search` fixes this by scrolling to whatever row this
    /// method returns -- this test is the delegate-level half of that
    /// fix: the row it returns must actually be the winner's *display*
    /// row (accounting for the synthetic ".." row), not its model row.
    #[test]
    fn apply_quick_search_jump_returns_the_winners_display_row() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.set_has_parent_row(true);
        delegate.quick_search = Some(QuickSearchState {
            mode: QuickSearchMode::Jump,
            query: "f3".to_string(),
            generation: 1,
            jump_match: None,
            filter_match_count: None,
        });

        let row = delegate.apply_quick_search_jump("f3");

        // "f3" is model row 3; with the ".." row ahead of it, that's
        // display row 4 -- the exact translation `select_row_by_name`'s
        // own regression test (T-4.3.7) already established elsewhere in
        // this file.
        assert_eq!(row, Some(4));
        assert_eq!(delegate.cursor_row(), Some(3));
    }

    /// UAT regression: narrowing the listing routinely filters out
    /// whatever entry the cursor was previously on, which used to leave
    /// `cursor_row` at `None` entirely -- no cursor highlighted, nothing
    /// for Enter/arrow keys to act on, even though the match count shown
    /// was already correct.
    #[test]
    fn apply_quick_search_filter_resets_cursor_to_row_zero_when_previous_entry_is_filtered_out() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        delegate.move_cursor_to(2); // land on "f2"
        delegate.quick_search = Some(QuickSearchState {
            mode: QuickSearchMode::Filter,
            query: String::new(),
            generation: 1,
            jump_match: None,
            filter_match_count: None,
        });

        // Only "f4" matches -- "f2" (the cursor's entry) is filtered out.
        let row = delegate.apply_quick_search_filter("f4");

        assert_eq!(
            row,
            Some(0),
            "cursor must reset to the first remaining visible row"
        );
        assert_eq!(delegate.cursor_row(), Some(0));
        assert_eq!(delegate.model().order().len(), 1);
    }

    #[test]
    fn nav_generation_starts_at_zero_and_increments() {
        let mut delegate = FileTableDelegate::new(five_file_model());
        assert_eq!(delegate.nav_generation(), 0);
        assert_eq!(delegate.bump_nav_generation(), 1);
        assert_eq!(delegate.nav_generation(), 1);
        assert_eq!(delegate.bump_nav_generation(), 2);
        assert_eq!(delegate.nav_generation(), 2);
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
