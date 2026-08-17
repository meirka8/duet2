// SPDX-License-Identifier: MIT
//! The command palette (T-4.3.6, FR-TOOL-11): a fuzzy-searchable overlay
//! listing every registered command with its current keybinding(s),
//! wrapping `duet_commands::palette::PaletteIndex` in a real
//! `duet_widgets::list::ListDelegate` -- the exact use case that facade
//! module's own doc comment already names ("command palette results").
//!
//! # Dispatch is deliberately not routed through `Command::handler`
//!
//! Every built-in command's handler (`duet_commands::catalogue`) is an
//! intentional stub that always errors -- wiring real handlers is
//! separate, ongoing work spanning many future tasks, since most of the
//! 302-command catalogue corresponds to features (archives, the viewer,
//! remote VFS, plugins, ...) that don't exist in this app at all yet.
//! Building a `CommandContext` bridge just to get a guaranteed
//! `HandlerNotWired` error back would be wasted complexity for this task.
//!
//! Instead, [`Workspace::dispatch_palette_command`] looks the confirmed id
//! up in a small, explicit `match` covering only the commands that
//! already have a real implementation reachable elsewhere in `duet-ui` --
//! today, the T-4.3.2 tab commands with no `keymap-tc.csv` binding of
//! their own (`tab.duplicate`, `tab.lock`, `tab.lock_dir_change`,
//! `tab.close_others`, `tab.move_left`, `tab.move_right`), plus the
//! already-bound tab/focus commands (included so the palette can invoke
//! *anything* reachable, not just the newly-unlocked ones -- standard
//! command-palette behaviour, e.g. VSCode's palette can run a command
//! that already has its own keybinding too). `tab.rename` (no rename
//! feature exists at all), `tab.goto_index` (needs an `index: u32` arg --
//! prompting for one is a UI surface this task doesn't build), and
//! `tab.restore_session` (session restore only ever happens at startup,
//! there's nothing to "run" mid-session) are deliberately left
//! undispatched. Every other one of the 302 registered commands is a
//! real, fuzzy-searchable, selectable entry that just doesn't do anything
//! yet -- selecting it shows a "not implemented" toast rather than
//! silently no-op-ing.

use std::rc::Rc;

use duet_commands::palette::{PaletteEntry, PaletteIndex};
use duet_widgets::list::{IndexPath, ListDelegate, ListState, Selectable};
use duet_widgets::theme::TokenPalette;
use gpui::{App, Context, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce};
use gpui::{SharedString, Styled as _, Task, WeakEntity, Window, div, px};

use crate::workspace::Workspace;

/// One rendered row: title, category, and its current binding (if any),
/// highlighted when it's the row the keyboard/mouse currently has
/// selected. A thin `RenderOnce` wrapper -- `#[derive(IntoElement)]` is
/// gpui's own way of getting `IntoElement` for a `RenderOnce` type (see
/// `gpui-0.2.2/src/element.rs`'s module doc comment) -- mirroring
/// `gpui-component`'s own `select::SelectListItem`, the closest existing
/// example of a real `ListDelegate::Item` in this dependency.
#[derive(gpui::IntoElement)]
pub(crate) struct PaletteRow {
    ix: usize,
    title: SharedString,
    category: SharedString,
    binding: SharedString,
    selected: bool,
}

impl PaletteRow {
    fn new(ix: usize, title: SharedString, category: SharedString, binding: SharedString) -> Self {
        Self {
            ix,
            title,
            category,
            binding,
            selected: false,
        }
    }
}

impl Selectable for PaletteRow {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for PaletteRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let (bg, fg) = if self.selected {
            (tokens.color.cursor_bg, tokens.color.cursor_fg)
        } else {
            (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
        };
        div()
            .id(("command-palette-row", self.ix))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .justify_between()
            .items_center()
            .bg(bg)
            .text_color(fg)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(div().child(self.title))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(tokens.color.statusbar_fg)
                            .child(self.category),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(self.binding),
            )
    }
}

/// Wraps `PaletteIndex` (built once, at `Workspace::new` time -- see that
/// field's doc comment for why) into a `ListDelegate`: `perform_search`
/// re-runs `PaletteIndex::search` on every query-box keystroke,
/// `confirm`/`cancel` call back into `Workspace` (via `weak_workspace`) to
/// dispatch the selected command and/or close the overlay. `window` is
/// available directly on both `confirm`/`cancel`'s own signatures (no
/// cross-entity `Window::spawn` dance needed the way `Panel`'s
/// locked-tab-navigation redirect required -- that closure was stored and
/// invoked at an arbitrary *later* point with no `window` of its own in
/// scope; this one runs synchronously, in the same call stack that
/// already has one).
pub(crate) struct CommandPaletteDelegate {
    index: Rc<PaletteIndex>,
    matches: Vec<PaletteEntry>,
    selected: Option<usize>,
    workspace: WeakEntity<Workspace>,
}

impl CommandPaletteDelegate {
    pub(crate) fn new(index: Rc<PaletteIndex>, workspace: WeakEntity<Workspace>) -> Self {
        let matches = search(&index, "");
        Self {
            index,
            matches,
            selected: None,
            workspace,
        }
    }
}

/// `PaletteIndex::search` returns `Vec<PaletteMatch<'_>>`, borrowing from
/// `index` -- cloned into owned `PaletteEntry`s immediately so
/// `CommandPaletteDelegate` can store `matches` as a plain field alongside
/// `index` itself without a self-referential-lifetime problem.
fn search(index: &PaletteIndex, query: &str) -> Vec<PaletteEntry> {
    index
        .search(query)
        .into_iter()
        .map(|m| m.entry.clone())
        .collect()
}

impl ListDelegate for CommandPaletteDelegate {
    type Item = PaletteRow;

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.matches = search(&self.index, query);
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry = self.matches.get(ix.row)?;
        let binding = entry
            .bindings
            .first()
            .map(|b| b.to_string())
            .unwrap_or_default();
        Some(PaletteRow::new(
            ix.row,
            entry.title.to_string().into(),
            entry.category.to_string().into(),
            binding.into(),
        ))
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
        let Some(entry) = self.selected.and_then(|ix| self.matches.get(ix)) else {
            return;
        };
        let id = entry.id.clone();
        let title = entry.title.clone();
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.dispatch_palette_command(&id, &title, window, cx);
        });
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_command_palette(window, cx);
        });
    }
}
