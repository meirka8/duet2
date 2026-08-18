// SPDX-License-Identifier: MIT
//! The directory hotlist overlay (T-4.3.5, FR-NAV-08): a `Ctrl+D`-invoked
//! list of bookmarked directories, arrow+Enter navigates, entries persist
//! to `hotlist.toml`.
//!
//! Architecture mirrors T-4.3.6's command palette
//! (`crate::command_palette`) almost exactly -- `duet_widgets::list::
//! {List, ListState, ListDelegate}`, an absolutely-positioned `.occlude()`
//! backdrop, `Workspace` owning open/close state and the previous-focus
//! save/restore. The one real difference: the palette is read-only (its
//! `ListDelegate` only ever searches an existing index), while this one is
//! *editable* -- `hotlist.add`/`hotlist.remove`/`hotlist.reorder`
//! (`docs/commands.md`) mutate `HotlistDelegate::entries` directly and
//! persist the result, so there's no existing precedent for that half in
//! this codebase.
//!
//! **Explicitly out of scope** (see the T-4.3.5 PR description for the
//! full reasoning): `hotlist.rename_entry` (needs new inline-text-edit UI
//! with no precedent anywhere in this app; every entry's `label` stays
//! `None`, displayed by `path`) and `hotlist.add_submenu` (nested
//! categories -- the catalogue has this command with no keybinding, no
//! notes, and no FR trace anywhere).

use duet_config::HotlistEntry;
use duet_widgets::list::{IndexPath, ListDelegate, ListState, Selectable};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{App, Context, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce};
use gpui::{SharedString, Styled as _, Task, WeakEntity, Window, div, px};

use crate::workspace::Workspace;

/// One rendered row: the entry's label (or, absent one, its path),
/// highlighted when it's the row the keyboard currently has selected.
/// Mirrors `command_palette::PaletteRow` -- see that struct's own doc
/// comment for why `#[derive(IntoElement)]` is the right way to get
/// `IntoElement` for a `RenderOnce` type here.
#[derive(gpui::IntoElement)]
pub(crate) struct HotlistRow {
    ix: usize,
    display_name: SharedString,
    path: SharedString,
    selected: bool,
}

impl HotlistRow {
    fn new(ix: usize, entry: &HotlistEntry) -> Self {
        let display_name = entry
            .label
            .clone()
            .unwrap_or_else(|| entry.path.clone())
            .into();
        Self {
            ix,
            display_name,
            path: entry.path.clone().into(),
            selected: false,
        }
    }
}

impl Selectable for HotlistRow {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for HotlistRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let (bg, fg) = if self.selected {
            (tokens.color.cursor_bg, tokens.color.cursor_fg)
        } else {
            (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
        };
        // Path is shown as a secondary line only when it differs from
        // what's already on the first line -- an entry with no label
        // (the only kind T-4.3.5 itself ever creates) would otherwise
        // show the exact same string twice.
        let show_path_line = self.display_name != self.path;
        div()
            .id(("hotlist-row", self.ix))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_0p5()
            .bg(bg)
            .text_color(fg)
            .child(div().child(self.display_name))
            .when(show_path_line, |row| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(tokens.color.statusbar_fg)
                        .child(self.path),
                )
            })
    }
}

/// Wraps a working copy of the hotlist's entries (T-4.3.5's `hotlist.add`/
/// `remove`/`reorder` all mutate `entries` directly, then hand the updated
/// `Vec` back to `Workspace` to persist -- see `Workspace::
/// persist_hotlist`) into a `ListDelegate`. `confirm`/`cancel` call back
/// into `Workspace` (via `workspace`) exactly like
/// `CommandPaletteDelegate`'s own -- `window` is already in scope on both
/// signatures, no cross-entity `Window::spawn` dance needed, same
/// reasoning as that struct's own doc comment.
pub(crate) struct HotlistDelegate {
    pub(crate) entries: Vec<HotlistEntry>,
    pub(crate) selected: Option<usize>,
    workspace: WeakEntity<Workspace>,
}

impl HotlistDelegate {
    pub(crate) fn new(entries: Vec<HotlistEntry>, workspace: WeakEntity<Workspace>) -> Self {
        let selected = if entries.is_empty() { None } else { Some(0) };
        Self {
            entries,
            selected,
            workspace,
        }
    }
}

impl ListDelegate for HotlistDelegate {
    type Item = HotlistRow;

    fn perform_search(
        &mut self,
        _query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        // No search box: the hotlist is a short, curated list (unlike the
        // palette's 300+ commands), browsed by arrow keys alone.
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.entries.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry = self.entries.get(ix.row)?;
        Some(HotlistRow::new(ix.row, entry))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|i| i.row);
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let Some(entry) = self.selected.and_then(|ix| self.entries.get(ix)) else {
            return;
        };
        let path = entry.path.clone();
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.navigate_to_hotlist_entry(path, window, cx);
        });
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_hotlist(window, cx);
        });
    }
}
