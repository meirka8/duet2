// SPDX-License-Identifier: MIT
//! T-5.2.7's Shift+F6 "rename in place" dialog (FR-OPS-01,
//! `docs/keymap-tc.csv`'s `ops.rename_in_place` row -- a "known" TC
//! binding: "Rename the file/dir under cursor in place (no move dialog;
//! target directory fixed to current)").
//!
//! # "Selects the stem, not the extension": why this is a split field
//!
//! This task's AC (and `docs/commands.md`'s own `ops.rename_in_place` row)
//! asks that an inline rename "selects the stem, not the extension" -- i.e.
//! Total Commander's behaviour, where the rename field opens with `photo`
//! highlighted and `.jpg` left alone, so the first keystroke replaces the
//! stem and the extension survives untouched.
//!
//! **That cannot be implemented literally against the widget version this
//! codebase pins.** A visible, multi-character text selection inside a
//! `duet_widgets::input::InputState` (a one-line re-export of
//! `gpui_component::input::*`, per R-G7) is not reachable from any public
//! API: `InputState::selected_range` is a private field;
//! `InputState::move_to`, the only method that writes it, is `pub(crate)`
//! *inside `gpui-component` itself*; `InputState::select_all` is
//! `pub(super)` and reachable only as a bound action; and the one public
//! entry point that touches the cursor at all, `InputState::
//! set_cursor_position`, always calls `move_to(offset, None, cx)` --
//! collapsing to a zero-width cursor, never producing a range. (All four
//! verified by reading `gpui-component-0.5.1/src/input/state.rs`.)
//!
//! So this module gets to the same *functional* place by a different
//! route, which is a disclosed design decision and not an accidental
//! narrowing of the AC:
//!
//! - The cursor entry's name is split into a stem and an extension using
//!   `std::path::Path::extension()` -- the exact convention
//!   `FileTableDelegate::select_same_extension` already uses for Shift+`+`,
//!   which correctly treats a dotfile like `.bashrc` as having *no*
//!   extension rather than an extension of `bashrc`.
//! - Only the **stem** goes into the editable `InputState`. The extension
//!   is rendered beside it as a static, non-editable `.{ext}` label,
//!   styled to read as part of the same field.
//! - On confirm the two halves are rejoined (`format!("{stem}.{ext}")`)
//!   and handed to `duet_ops::plan_rename_in_place`.
//!
//! The outcome the AC actually cares about therefore holds: the extension
//! cannot be damaged by accident, and the editable text is exactly the
//! stem. It is also *stronger* than TC's own behaviour in one respect
//! (TC's extension is merely unselected, still editable; here it is fixed)
//! and weaker in another (nothing is highlighted, so typing appends to the
//! stem rather than replacing it -- `Ctrl+A`, already bound inside
//! `InputState`'s own `"Input"` context, selects exactly the stem, since
//! the stem is all the field contains). If a future `gpui-component` bump
//! exposes a real selection setter, the honest change is to keep this
//! split and add `select_all`-on-open, not to rebuild the field.
//!
//! A name with **no** extension, and a dotfile, are fully editable as one
//! piece with no fixed suffix shown -- the same thing TC does, and what the
//! AC's own wording carves out by only ever mentioning "the extension."
//!
//! # Scope: the cursor entry, one at a time
//!
//! Unlike F5/F6/F8, this does *not* use
//! `crate::copy_move_dialog::resolve_source_names`' selection-or-cursor
//! fallback. Shift+F6 renames exactly one thing --
//! `docs/commands.md`'s own row says "Rename the cursor entry in place" --
//! so `Workspace::open_rename_dialog` resolves
//! `FileTableDelegate::cursor_entry_name()` and nothing else. Batch
//! renaming is `tool.multi_rename`'s (F2-style multi-rename tool) own,
//! separate command, not this one.
//!
//! # Overlay architecture and keyboard handling
//!
//! Identical to `crate::mkdir_dialog`'s -- see that module's doc comment,
//! and `copy_move_dialog.rs`'s for the full "why Enter/Escape need no
//! `KeyBinding` of this module's own" reasoning.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{ConflictResolver, JobKind, PlanOptions, QueueManager, plan_rename_in_place};
use duet_types::VPath;
use duet_widgets::input::{Escape, Input, InputEvent, InputState};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, div, px,
};

use crate::dialog_job::{focus_at_end, report_job_outcome, spawn_plan_and_enqueue};
use crate::workspace::{NoticeLevel, Workspace};

/// Splits an entry name into the editable stem and the fixed extension --
/// see the module doc comment for why the split exists at all.
///
/// `Path::extension()` decides where the boundary is (so `.bashrc` and
/// `README` both come back as `(whole name, None)`, and `archive.tar.gz`
/// as `("archive.tar", Some("gz"))`), but the stem is taken as a *slice of
/// the original name* rather than from `Path::file_stem()`, which
/// guarantees by construction that `format!("{stem}.{ext}")` reproduces
/// the input byte for byte. A free function so it is unit-testable with no
/// GPUI harness, same reasoning as
/// `crate::copy_move_dialog::split_parent_prefix`.
pub(crate) fn split_stem_extension(name: &str) -> (String, Option<String>) {
    match std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
    {
        // `+ 1` for the `.` that `extension()` excludes; the subtraction
        // cannot underflow, since an extension of length `n` implies a
        // name of length at least `n + 1`.
        Some(ext) => (
            name[..name.len() - ext.len() - 1].to_string(),
            Some(ext.to_string()),
        ),
        None => (name.to_string(), None),
    }
}

/// Rejoins what [`split_stem_extension`] separated. The inverse direction
/// of the same convention, kept next to it so the two can't drift.
pub(crate) fn join_stem_extension(stem: &str, extension: Option<&str>) -> String {
    match extension {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    }
}

/// Shift+F6's dialog. See the module doc comment for the full architecture;
/// `Workspace::open_rename_dialog` is the only constructor call site and
/// has already resolved everything below.
pub(crate) struct RenameDialogState {
    /// The entry being renamed, resolved at open time. Fixed: Shift+F6
    /// never relocates anything, and `plan_rename_in_place` rejects a
    /// `new_name` containing `/` for exactly that reason.
    source: VPath,
    /// The entry's original, complete name -- used for the dialog title
    /// and to detect a confirm that wouldn't change anything.
    original_name: String,
    /// The fixed, non-editable suffix, or `None` for an extension-less
    /// name or a dotfile (see the module doc comment).
    extension: Option<String>,
    /// The editable half: pre-filled with the stem only.
    stem_input: Entity<InputState>,
    planning_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    state_dir: Option<PathBuf>,
    conflict_resolver: Arc<dyn ConflictResolver>,
    _subscriptions: Vec<Subscription>,
}

impl RenameDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source: VPath,
        original_name: String,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (stem, extension) = split_stem_extension(&original_name);
        let stem_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(stem)
                .placeholder("New name")
        });
        let _subscriptions = vec![cx.subscribe_in(&stem_input, window, Self::on_stem_event)];
        stem_input.update(cx, |state, cx| focus_at_end(state, window, cx));

        Self {
            source,
            original_name,
            extension,
            stem_input,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
            _subscriptions,
        }
    }

    /// Test-only accessors -- same reasoning as `DeleteDialogState`'s own
    /// `#[cfg(test)]` block.
    #[cfg(test)]
    pub(crate) fn stem_value(&self, cx: &gpui::App) -> String {
        self.stem_input.read(cx).value().to_string()
    }

    /// Test-only: UAT regression check -- see
    /// `MkdirDialogState::destination_cursor_at_end`'s own doc comment.
    #[cfg(test)]
    pub(crate) fn stem_cursor_at_end(&self, cx: &gpui::App) -> bool {
        let state = self.stem_input.read(cx);
        state.cursor() == state.value().len()
    }

    #[cfg(test)]
    pub(crate) fn extension(&self) -> Option<&str> {
        self.extension.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &VPath {
        &self.source
    }

    /// Test-only: see `MkdirDialogState::set_destination_value`'s doc
    /// comment for why the tests set the field this way rather than
    /// simulating keystrokes.
    #[cfg(test)]
    pub(crate) fn set_stem_value(&self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.stem_input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    fn on_stem_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.confirm(cx);
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_rename_dialog(window, cx);
        });
    }

    /// Enter: rejoins stem + extension, plans the rename, enqueues it --
    /// the plan itself is synchronous and infallible-ish
    /// (`plan_rename_in_place` needs no `FileSystem` at all), but it still
    /// goes through `crate::dialog_job::spawn_plan_and_enqueue` because
    /// `QueueManager::enqueue` synchronously opens the job's journal file,
    /// and design.md §8.2's "main thread does no I/O, ever" covers that.
    ///
    /// A confirm that doesn't change the name closes the dialog without
    /// enqueuing anything -- renaming `a.txt` to `a.txt` is not an error
    /// worth a toast, but it is also not worth a journal entry and a
    /// `rename(2)` (which under the executor's `Skip` default would be
    /// reported as a skipped step, i.e. visible noise in the operation
    /// manager for a request that meant nothing).
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
            return;
        }

        let stem = self.stem_input.read(cx).value().to_string();
        let new_name = join_stem_extension(&stem, self.extension.as_deref());
        if new_name == self.original_name {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.close_rename_dialog_deferred(cx);
            });
            return;
        }
        if stem.is_empty() {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Warning,
                    "A new name is required.".to_string(),
                    cx,
                );
            });
            return;
        }
        let Some(state_dir) = self.state_dir.clone() else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    "Can't run the operation: no writable state directory found \
                     (is $HOME/$XDG_STATE_HOME set?)."
                        .to_string(),
                    cx,
                );
            });
            return;
        };

        self.planning_in_progress = true;

        let source = self.source.clone();
        let rx = spawn_plan_and_enqueue(
            &self.tokio_handle,
            // A rename *is* a move, as far as the queue's own vocabulary
            // goes (`JobKind` has no `Rename` variant, and `plan_move`
            // emits the very same `Step::Rename` for a same-device move) --
            // the operation manager labels this "Move", which is honest.
            JobKind::Move,
            self.queue.clone(),
            state_dir,
            self.conflict_resolver.clone(),
            move |_fs| async move { plan_rename_in_place(&source, &new_name, PlanOptions::default()) },
        );
        let workspace = self.workspace.clone();
        let this_entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            let outcome = rx.await;
            let _ = this_entity.update(cx, |this, cx| {
                this.planning_in_progress = false;
                cx.notify();
            });
            if report_job_outcome(outcome, &workspace, cx) {
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.close_rename_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }
}

impl Render for RenameDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let suffix = self.extension.clone();
        div()
            .id("rename-dialog")
            .key_context("RenameDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(format!("Rename \u{201c}{}\u{201d} to:", self.original_name)),
            )
            .child(
                // No `gap` between the two: the fixed extension label has
                // to read as the tail of the same field, not as a separate
                // control sitting next to it. `flex_1` on the input so the
                // suffix keeps its natural width and the editable half
                // takes everything else.
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().flex_1().child(Input::new(&self.stem_input)))
                    .when_some(suffix, |this, ext| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .pl(px(2.))
                                .text_color(tokens.color.statusbar_fg)
                                .child(format!(".{ext}")),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(match self.extension.as_deref() {
                        Some(_) => "The extension is kept as-is. Enter to confirm, Esc to cancel."
                            .to_string(),
                        None => "Enter to confirm, Esc to cancel.".to_string(),
                    }),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_an_ordinary_name_at_the_last_dot() {
        assert_eq!(
            split_stem_extension("photo.jpg"),
            ("photo".to_string(), Some("jpg".to_string()))
        );
    }

    /// The `select_same_extension` convention this borrows: only the
    /// *last* dot separates a name from its extension, so a doubly-suffixed
    /// archive keeps `archive.tar` as its editable stem.
    #[test]
    fn splits_a_double_extension_at_the_last_dot_only() {
        assert_eq!(
            split_stem_extension("archive.tar.gz"),
            ("archive.tar".to_string(), Some("gz".to_string()))
        );
    }

    /// This is the case that makes `Path::extension()` the right primitive
    /// rather than a hand-rolled `rsplit_once('.')`: a dotfile has no
    /// extension, so the whole name stays editable.
    #[test]
    fn a_dotfile_has_no_extension_and_stays_editable_whole() {
        assert_eq!(
            split_stem_extension(".bashrc"),
            (".bashrc".to_string(), None)
        );
    }

    #[test]
    fn an_extensionless_name_stays_editable_whole() {
        assert_eq!(split_stem_extension("README"), ("README".to_string(), None));
    }

    #[test]
    fn the_split_always_round_trips() {
        for name in [
            "photo.jpg",
            "archive.tar.gz",
            ".bashrc",
            "README",
            "trailing.",
            ".hidden.txt",
        ] {
            let (stem, ext) = split_stem_extension(name);
            assert_eq!(
                join_stem_extension(&stem, ext.as_deref()),
                name,
                "splitting and rejoining {name} must be lossless"
            );
        }
    }
}
