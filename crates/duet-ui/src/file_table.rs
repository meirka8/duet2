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
use duet_widgets::table::{Column, ColumnSort, Table, TableDelegate, TableRow, TableState};
use duet_widgets::theme::TokenPalette;
use futures_util::StreamExt;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
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
}

impl FileTableDelegate {
    /// Builds a delegate over `model` (which may be empty -- see
    /// `loading`'s doc comment for the "still populating" case).
    pub fn new(model: DirectoryModel) -> Self {
        let base_columns = vec![
            Column::new("name", "Name")
                .width(px(responsive::NAME_IDEAL))
                .sortable(),
            Column::new("size", "Size")
                .width(px(responsive::SIZE_WIDTH))
                .text_right()
                .sortable(),
            Column::new("modified", "Modified")
                .width(px(responsive::MODIFIED_WIDTH))
                .text_right()
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
            scratch: String::new(),
            loading: true,
        };
        delegate.rebuild_row_text();
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
    /// forces the text cache to rebuild for it.
    pub fn set_model(&mut self, model: DirectoryModel) {
        self.model = model;
        self.cached_generation = u64::MAX;
        self.rebuild_row_text();
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
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
        for &ix in self.model.order() {
            let id = EntryId::new(ix);
            let entries = self.model.entries();
            let kind = entries.kind(id);

            let name = SharedString::new(entries.name(id));

            self.scratch.clear();
            write_size(&mut self.scratch, kind, entries.size(id));
            let size = SharedString::new(self.scratch.as_str());

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
    }

    /// Row container: the only per-row work is an `order`/selection-bitmap
    /// lookup and a conditional background color -- no string formatting,
    /// no allocation (matching S-1 spike's `render_tr`).
    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> TableRow {
        let selected = self
            .model
            .order()
            .get(row_ix)
            .copied()
            .is_some_and(|ix| self.model.is_selected(EntryId::new(ix)));

        let row = div().id(("file-row", row_ix));
        if selected {
            let tokens = TokenPalette::current(cx);
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
        div()
            .id(("file-cell", row_ix as u64 * 8 + col_ix as u64))
            .px_2()
            .truncate()
            .child(text)
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

/// The real virtualised directory-table view: a thin `Render` wrapper
/// around `duet_widgets::table::TableState<FileTableDelegate>`, populated
/// from a real local directory listing via the core Tokio runtime --
/// design.md §8.2's "main thread does no I/O, ever", the same
/// executor-wiring pattern `workspace.rs`'s own T-4.1.1 demo
/// (`count_current_dir_entries`/`spawn_entry_count_demo`) already
/// established.
pub struct FileTable {
    state: Entity<TableState<FileTableDelegate>>,
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
        Self { state }
    }

    /// Exposes the underlying table state -- e.g. for a future status-bar
    /// selection-stats readout (T-4.2.7) that needs to reach
    /// `TableState::delegate().model()` from outside this view.
    pub fn state(&self) -> &Entity<TableState<FileTableDelegate>> {
        &self.state
    }
}

impl Render for FileTable {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        div()
            .size_full()
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
                // width change takes effect on the next frame (via
                // `cx.notify()`), same one-frame lag as the splitter.
                gpui::canvas(
                    move |bounds, _window, cx| {
                        state.update(cx, |state, cx| {
                            let delegate = state.delegate_mut();
                            if delegate.apply_responsive_widths(f32::from(bounds.size.width)) {
                                cx.notify();
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
