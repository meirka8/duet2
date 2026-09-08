// SPDX-License-Identifier: MIT
//! T-5.3.4's "Open With" chooser (FR-TOOL-08): the applications the
//! association database offers for the file under the cursor, default
//! first, in the same keyboard list overlay the hotlist uses (Up/Down,
//! Enter launches, Esc closes). Opened from the row context menu, the
//! command palette (`file.open_with`) and the `OpenWith` action; the
//! workspace owns the overlay and the launch, this module only renders
//! the rows and routes confirm/cancel back to it.

use std::path::PathBuf;

use duet_widgets::list::{IndexPath, ListDelegate, ListState, Selectable};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce,
    SharedString, Styled as _, Task, WeakEntity, Window, div, px,
};

use crate::launcher::AppChoice;
use crate::workspace::Workspace;

#[derive(IntoElement)]
pub(crate) struct OpenWithRow {
    ix: usize,
    name: SharedString,
    detail: SharedString,
    selected: bool,
}

impl OpenWithRow {
    fn new(ix: usize, choice: &AppChoice) -> Self {
        let mut detail = String::new();
        if choice.is_default {
            detail.push_str("default");
        }
        if choice.terminal {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str("runs in a terminal");
        }
        Self {
            ix,
            name: choice.name.clone().into(),
            detail: detail.into(),
            selected: false,
        }
    }
}

impl Selectable for OpenWithRow {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for OpenWithRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let (bg, fg) = if self.selected {
            (tokens.color.cursor_bg, tokens.color.cursor_fg)
        } else {
            (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
        };
        let show_detail = !self.detail.is_empty();
        div()
            .id(("open-with-row", self.ix))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .bg(bg)
            .text_color(fg)
            .child(div().child(self.name))
            .when(show_detail, |row| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(tokens.color.statusbar_fg)
                        .child(self.detail),
                )
            })
    }
}

pub(crate) struct OpenWithDelegate {
    /// The files the choice applies to (one, or the selection).
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) choices: Vec<AppChoice>,
    pub(crate) selected: Option<usize>,
    workspace: WeakEntity<Workspace>,
}

impl OpenWithDelegate {
    pub(crate) fn new(
        paths: Vec<PathBuf>,
        choices: Vec<AppChoice>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let selected = if choices.is_empty() { None } else { Some(0) };
        Self {
            paths,
            choices,
            selected,
            workspace,
        }
    }
}

impl ListDelegate for OpenWithDelegate {
    type Item = OpenWithRow;

    fn perform_search(
        &mut self,
        _query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.choices.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let choice = self.choices.get(ix.row)?;
        Some(OpenWithRow::new(ix.row, choice))
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
        let Some(choice) = self.selected.and_then(|ix| self.choices.get(ix)) else {
            return;
        };
        let id = choice.id.clone();
        let paths = self.paths.clone();
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.launch_open_with_choice(id, paths, window, cx);
        });
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_open_with(window, cx);
        });
    }
}
