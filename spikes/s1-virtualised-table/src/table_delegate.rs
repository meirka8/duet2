//! `TableDelegate` implementation over `EntryStore`. This is the piece the
//! S-1 spike is really about: does gpui-component's delegate API let us hand
//! it SoA data without forcing a per-cell heap allocation?

use gpui::{
    App, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Stateful, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _,
    table::{Column, ColumnSort, TableDelegate, TableState},
};

use crate::store::{Bitset, EntryStore};

pub struct FileTableDelegate {
    pub store: EntryStore,
    pub selection: Bitset,
    columns: Vec<Column>,
    sort_state: [Option<ColumnSort>; 5],
    pub last_sort_duration: Option<std::time::Duration>,
}

impl FileTableDelegate {
    pub fn new(store: EntryStore) -> Self {
        let n = store.len();
        let columns = vec![
            Column::new("name", "Name").width(px(320.)).sortable(),
            Column::new("size", "Size").width(px(110.)).text_right().sortable(),
            Column::new("date", "Modified").width(px(170.)).sortable(),
            Column::new("mode", "Mode").width(px(130.)).sortable(),
            Column::new("ext", "Ext").width(px(80.)).sortable(),
        ];

        Self {
            store,
            selection: Bitset::with_capacity(n),
            columns,
            sort_state: [None; 5],
            last_sort_duration: None,
        }
    }
}

impl TableDelegate for FileTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.store.len()
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
        let start = std::time::Instant::now();
        match sort {
            ColumnSort::Default => self.store.reset_order(),
            ColumnSort::Ascending => self.store.sort_by_column(col_ix, true),
            ColumnSort::Descending => self.store.sort_by_column(col_ix, false),
        }
        self.last_sort_duration = Some(start.elapsed());
        self.sort_state[col_ix.min(4)] = Some(sort);
    }

    /// Row container: the ONLY per-row work here is a bitset lookup and a
    /// conditional background colour — no string formatting, no allocation.
    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let stable_id = self.store.order[row_ix] as usize;
        let selected = self.selection.get(stable_id);
        let bg = cx.theme().selection.opacity(0.35);

        let row = div().id(("row", row_ix));
        if selected { row.bg(bg) } else { row }
    }

    /// Cell content: every column's text is either a `&'static str` literal
    /// (mode, ext) or a slice of the pre-formatted `&'static` arena (name,
    /// size, date). `SharedString::new_static` wraps a `&'static str` with
    /// zero allocation (it's `ArcCow::Borrowed`, not even a refcount bump).
    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let stable_id = self.store.order[row_ix] as usize;
        let text: SharedString = match col_ix {
            0 => SharedString::new_static(self.store.name(stable_id)),
            1 => SharedString::new_static(self.store.size_text(stable_id)),
            2 => SharedString::new_static(self.store.date_text(stable_id)),
            3 => SharedString::new_static(self.store.mode_text(stable_id)),
            4 => SharedString::new_static(self.store.ext_text(stable_id)),
            _ => SharedString::default(),
        };

        div()
            .id(("cell", row_ix as u64 * 8 + col_ix as u64))
            .px_2()
            .child(text)
    }
}
