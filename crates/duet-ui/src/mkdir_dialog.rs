// SPDX-License-Identifier: MIT
//! T-5.2.7's F7 "create directory" dialog (FR-OPS-01,
//! `docs/keymap-tc.csv`'s `ops.mkdir` row -- a "known" TC binding, not this
//! codebase's own default).
//!
//! # One field, nested paths, and this task's own AC clause
//!
//! The AC asks that "F7 supports creating nested paths in one go, as TC
//! does." Nothing in this module implements that: `duet_ops::plan_mkdir`
//! already walks up from its target through the ancestors that don't exist
//! yet and emits one `Step::CreateDir` per missing level (see its own doc
//! comment). So this dialog is exactly one text field, pre-filled with the
//! active panel's current directory plus a trailing `/`, into which the
//! user types one segment (`photos`) or several (`a/b/c`) -- the field's
//! whole text is what gets planned, and the "one go" part is the planner's.
//!
//! A confirm with nothing typed past the pre-filled directory is
//! deliberately *not* special-cased into an error: `UnixPathBuf::new`
//! strips the trailing `/`, leaving the panel's own already-existing
//! directory, and `plan_mkdir`'s documented "already there is success"
//! convention turns that into a valid, zero-step `Plan`. The job runs,
//! correctly does nothing, and the dialog closes -- the same shape
//! `duet_ops::plan_delete`'s "already gone is success" short-circuit
//! produces for its own no-op case, rather than a second, dialog-local
//! notion of what counts as a pointless request.
//!
//! # Overlay architecture and keyboard handling
//!
//! Mirrors `crate::copy_move_dialog::CopyMoveDialogState` exactly, minus
//! its options row (there is no conflict-policy/verify/queue choice to
//! make for a `mkdir`): `Workspace` owns `Option<Entity<MkdirDialogState>>`
//! plus the focus handle to restore on close, and
//! `workspace::mkdir_dialog_overlay` builds the backdrop/card chrome.
//!
//! Enter and Escape need no `KeyBinding` registered by this module, for
//! the reason `copy_move_dialog.rs`'s module doc comment sets out at
//! length: `duet_widgets::input::InputState` already binds both inside its
//! own `"Input"` key context, Enter always emitting
//! `InputEvent::PressEnter` (subscribed to below) and Escape always
//! bubbling as the `duet_widgets::input::Escape` *action* (caught by
//! `.on_action` on this view's own render root). There is consequently no
//! `bind_mkdir_dialog_keys` to call from `workspace::run` -- unlike
//! `bind_copy_move_dialog_keys`/`bind_delete_dialog_keys`, this dialog adds
//! no chords of its own, and an empty registration function would be
//! nothing but ceremony.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use duet_ops::{ConflictResolver, JobKind, QueueManager, plan_mkdir};
use duet_widgets::input::{Escape, Input, InputEvent, InputState};
use duet_widgets::theme::TokenPalette;
use gpui::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, div, px,
};

use crate::dialog_job::{report_job_outcome, spawn_plan_and_enqueue};
use crate::file_table::local_vpath;
use crate::workspace::{NoticeLevel, Workspace};

/// F7's dialog. See the module doc comment for the full architecture;
/// `Workspace::open_mkdir_dialog` is the only constructor call site.
pub(crate) struct MkdirDialogState {
    /// Pre-filled with the active panel's `current_dir()` plus a trailing
    /// `/`, so the user only has to type the new segment(s).
    destination: Entity<InputState>,
    /// Guards against a double-Enter re-entering [`Self::confirm`] while a
    /// previous `plan_mkdir` is still running off the UI thread -- same
    /// reasoning as `CopyMoveDialogState::planning_in_progress`.
    planning_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    /// `duet_config::paths::duet_state_dir()`'s result, captured at open
    /// time -- `None` under the same rare XDG-resolution failure
    /// `Workspace`'s other config paths already tolerate. [`Self::confirm`]
    /// refuses to enqueue (with a toast, dialog left open) rather than
    /// guessing a location for a job's crash-safety journal.
    state_dir: Option<PathBuf>,
    /// The workspace's one live, interactive resolver -- see
    /// `crate::dialog_job::spawn_plan_and_enqueue`'s doc comment for why
    /// even a `mkdir` wants one (a non-directory already occupying the
    /// target is a real, promptable conflict).
    conflict_resolver: Arc<dyn ConflictResolver>,
    /// Keeps `destination`'s `PressEnter` subscription alive for as long as
    /// this view exists.
    _subscriptions: Vec<Subscription>,
}

impl MkdirDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        initial_destination: String,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let destination = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial_destination)
                .placeholder("New directory path")
        });
        let _subscriptions =
            vec![cx.subscribe_in(&destination, window, Self::on_destination_event)];
        destination.update(cx, |state, cx| state.focus(window, cx));

        Self {
            destination,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
            _subscriptions,
        }
    }

    /// Test-only: `workspace.rs`'s F7 tests assert the dialog opened
    /// pre-filled with the right directory without this field having to be
    /// public -- same reasoning as `CopyMoveDialogState::destination_value`.
    #[cfg(test)]
    pub(crate) fn destination_value(&self, cx: &gpui::App) -> String {
        self.destination.read(cx).value().to_string()
    }

    /// Test-only: types into the field the way a user would, without
    /// `VisualTestContext::simulate_keystrokes` -- which trips an unrelated
    /// upstream `gpui` panic against a focused, non-empty `InputState`
    /// (documented in full at `workspace.rs`'s own
    /// `f5_copy_end_to_end_copies_a_real_file_to_the_other_panels_directory`).
    /// Goes through the real `InputState::set_value`, so the field ends up
    /// in exactly the state real typing would leave it in.
    #[cfg(test)]
    pub(crate) fn set_destination_value(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.destination.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    fn on_destination_event(
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

    /// Escape: cancel without doing anything, exactly like
    /// `CopyMoveDialogState::cancel`.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_mkdir_dialog(window, cx);
        });
    }

    /// Enter: `plan_mkdir` + `QueueManager::enqueue`, both off the UI
    /// thread (`crate::dialog_job::spawn_plan_and_enqueue`), bridged back
    /// with `cx.spawn`. On success the dialog closes without waiting for
    /// the job itself (TC's own "dialog closes, operation proceeds in
    /// background" convention); on failure it stays open with the error as
    /// a toast, so the user can fix the path they typed rather than start
    /// over.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
            return;
        }

        let text = self.destination.read(cx).value().to_string();
        let Ok(dest) = local_vpath(Path::new(text.as_str())) else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    format!("\u{201c}{text}\u{201d} isn't a valid directory path."),
                    cx,
                );
            });
            return;
        };
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

        let rx = spawn_plan_and_enqueue(
            &self.tokio_handle,
            JobKind::CreateDir,
            self.queue.clone(),
            state_dir,
            self.conflict_resolver.clone(),
            move |fs| async move { plan_mkdir(fs.as_ref(), &dest).await },
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
                    workspace.close_mkdir_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }
}

impl Render for MkdirDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        div()
            .id("mkdir-dialog")
            .key_context("MkdirDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child("Create directory:"),
            )
            .child(Input::new(&self.destination))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("Nested paths (a/b/c) are created in one go. Enter to confirm, Esc to cancel."),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
    }
}
