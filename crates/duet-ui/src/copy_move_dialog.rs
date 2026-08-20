// SPDX-License-Identifier: MIT
//! The F5 (copy) / F6 (move) dialog (T-5.2.1, FR-OPS-01) -- the first
//! thing in `duet-ui` to drive `duet-ops`'s planner/executor/queue
//! (T-5.1.1..T-5.1.13, already real, tested, and merged). This module is
//! only the UI wiring on top: resolving what to operate on, a destination
//! field with a narrow, disclosed form of Tab-completion, a small set of
//! TC-standard options, and handing the result to
//! `duet_ops::QueueManager::enqueue`.
//!
//! # Overlay architecture
//!
//! Mirrors `crate::hotlist`'s `HotlistDelegate`/`Workspace::hotlist`
//! shape (see that module's own doc comment for the full reasoning) with
//! one structural difference: the hotlist overlay's real content is a
//! `duet_widgets::list::List` over a `ListDelegate`, so `Workspace` owns a
//! `ListState<HotlistDelegate>`. This dialog has no list to browse -- a
//! destination text field plus a few keyboard-toggled options -- so
//! there's no upstream widget whose `Delegate` trait this can slot into.
//! [`CopyMoveDialogState`] is instead a small, hand-rolled `Render`-
//! implementing view of its own (the same shape `Panel`/`FileTable`
//! already are), and `Workspace` holds `Option<Entity<CopyMoveDialogState>>`
//! directly rather than wrapping it in a upstream `*State` type.
//! `workspace::copy_move_dialog_overlay` builds the actual backdrop/card
//! chrome around it, identical to `hotlist_overlay`'s own (`.occlude()` on
//! both layers, `.on_mouse_down_out` on the card -- see that function's
//! doc comment for the real regression this pattern exists to avoid: a
//! click falling through to the panel underneath while the overlay stayed
//! rendered but no longer the thing anything was actually talking to).
//!
//! # Catching Enter/Escape/Tab inside the destination field
//!
//! `duet_widgets::input::InputState` (a passthrough facade over the
//! underlying `gpui-component` crate's own `input::InputState`, per R-G7)
//! already binds all three within its own `"Input"` key context
//! (`gpui-component-0.5.1/src/input/state.rs`):
//!
//! - **Enter** always emits `InputEvent::PressEnter` (even though, for a
//!   single-line field, it also lets the bound `Enter` action itself
//!   propagate) -- confirmed by reading `InputState::enter`'s body. So
//!   [`CopyMoveDialogState::new`] subscribes to `destination`'s events via
//!   `cx.subscribe_in` and treats `PressEnter` as confirm, rather than
//!   fighting the action-bubbling for a bound `"enter"` keystroke.
//! - **Escape** propagates unconditionally as long as `clean_on_escape`
//!   is left at its default `false` (confirmed by reading
//!   `InputState::escape`'s body) -- this dialog never sets it, so Escape
//!   always bubbles, unhandled, up the view tree as the *action instance*
//!   `duet_widgets::input::Escape`. [`CopyMoveDialogState::render`]
//!   registers its own `.on_action::<Escape>` on the card's own render
//!   root to catch that bubbled dispatch.
//! - **Tab**, for a *single-line* input, resolves to `IndentInline`
//!   (`InputState`'s own `"tab"` binding), whose handler
//!   (`InputMode::is_indentable()` is `false` for a single-line field)
//!   calls `cx.propagate()` immediately and does nothing -- confirmed by
//!   reading `gpui-component-0.5.1/src/input/indent.rs`. `IndentInline`
//!   then bubbles exactly like `Escape` above, and
//!   `gpui-component-0.5.1/src/input/search.rs`'s own `SearchPanel`
//!   already establishes this "catch the bubbled `IndentInline`" pattern
//!   for Tab-completion inside a search box -- this dialog mirrors it.
//!
//! None of the three needs a `KeyBinding` registered anywhere in this
//! module -- the binding already exists inside `InputState`'s own
//! `"Input"` context (installed once, at app start, by
//! `duet_widgets::init`, which forwards to the underlying widget crate's
//! own top-level init, in turn initializing its `input` module); this
//! module only ever registers `on_action` handlers for the *bubbled,
//! already-existing* action types. The dialog's own options (conflict
//! policy / verify / queue-vs-run) are the only bindings this module adds
//! (`bind_copy_move_dialog_keys`), deliberately chosen as `Ctrl+<key>`
//! chords unclaimed anywhere else in this app's keymap and unlikely to
//! ever be typed into a path -- plain digits are deliberately *not* used
//! for the conflict-policy choice, since those need to keep working as
//! ordinary characters inside the destination field itself.
//!
//! # Tab-completion: a disclosed scope cut, not an oversight
//!
//! "Destination with completion" (this task's own AC) is satisfied here
//! by the narrowest form that's honest without inventing new I/O this
//! task doesn't ask for in enough detail to justify: on Tab, split the
//! current text into a candidate parent directory and a partial last
//! segment ([`split_parent_prefix`]); if that parent exactly equals
//! either panel's *already-loaded* `current_dir()`, look up that panel's
//! already-in-memory `DirectoryModel::ordered_names()` for names starting
//! with the prefix ([`complete_against_model`]). Exactly one match
//! completes the field (plus a trailing `/` for a directory); zero or
//! multiple matches is a silent no-op -- no popup, no cycling, no new
//! directory listing kicked off. A real completion popup (browsing
//! directories the destination doesn't already have loaded) is left for
//! a later task.
//!
//! # What this dialog deliberately does not do
//!
//! No live per-conflict prompt (T-5.2.3) -- `resolver: None` is always
//! passed to `enqueue`, so any conflict the dialog's own
//! `default_conflict` choice doesn't already answer falls through to that
//! default rather than ever pausing to ask. Only three of
//! `ConflictPolicy`'s seven variants are reachable from this dialog's own
//! keyboard shortcuts (`Skip`/`Overwrite`/`AutoRename`) -- the other four
//! (`OverwriteIfOlder`, `OverwriteIfDifferentSize`, `RenameTarget`,
//! `Abort`) aren't meaningfully choosable as a single blanket default from
//! this dialog without a live per-conflict prompt to fall back on, which
//! is exactly what T-5.2.3 is for.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use duet_index::DirectoryModel;
use duet_ops::{
    CancelToken, ConflictPolicy, JobKind, PlanOptions, PlannerError, QueueManager, plan_copy,
    plan_move,
};
use duet_types::{EntryKind, VPath};
use duet_vfs::{FileSystem, LocalFs};
use duet_widgets::input::{Escape, IndentInline, Input, InputEvent, InputState};
use duet_widgets::theme::TokenPalette;
use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, actions,
    div, px,
};

use crate::file_table::{FileTableDelegate, local_vpath};
use crate::workspace::Workspace;

// This dialog's own options -- see the module doc comment's "Catching
// Enter/Escape/Tab" section for why Enter/Escape/Tab themselves need no
// binding here. `Ctrl+1/2/3` for the conflict-policy choice (numbered,
// matching the three-option list `docs/commands.md`'s catalogue has no
// entry for -- this whole dialog's options are this codebase's own
// reasonable defaults, not a verified TC binding survey; TC's own
// per-conflict dialog bindings, `Alt+Y`/`Alt+A`/`Esc`/`Alt+S` in
// `docs/keymap-tc.csv`, apply to a *different*, later dialog -- T-5.2.3's
// live per-conflict prompt -- not this one's blanket default). `Ctrl+K`
// or "checKsum" for verify, `Ctrl+J` for "Job queue" (run now vs.
// queued). None of these five collide with any existing binding in this
// codebase (checked against every other `KeyBinding::new` call site) and
// none are digits/letters a destination path would ordinarily need to
// type without a modifier held.
actions!(
    duet_copy_move_dialog,
    [
        SetConflictSkip,
        SetConflictOverwrite,
        SetConflictAutoRename,
        ToggleVerify,
        ToggleQueued
    ]
);

/// Registers this dialog's own keybindings, scoped to `"CopyMoveDialog"`
/// (set on [`CopyMoveDialogState::render`]'s own root `div`). Called once
/// from `workspace::run`, alongside every other `bind_*_keys` function.
pub(crate) fn bind_copy_move_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-1", SetConflictSkip, Some("CopyMoveDialog")),
        KeyBinding::new("ctrl-2", SetConflictOverwrite, Some("CopyMoveDialog")),
        KeyBinding::new("ctrl-3", SetConflictAutoRename, Some("CopyMoveDialog")),
        KeyBinding::new("ctrl-k", ToggleVerify, Some("CopyMoveDialog")),
        KeyBinding::new("ctrl-j", ToggleQueued, Some("CopyMoveDialog")),
    ]);
}

/// A per-job worker-concurrency hint passed to `QueueManager::enqueue`'s
/// own `concurrency` argument (distinct from the *queue-wide*
/// `max_concurrent` `Workspace` constructs `QueueManager` with -- see
/// that constant's own doc comment). `executor::suggested_concurrency`
/// would give a real, device-aware answer, but needs a `FsProps` probe
/// this dialog has no reason to perform just to open -- `4` matches that
/// function's own non-rotational default, a reasonable, documented
/// placeholder pending real device detection at this call site.
const JOB_CONCURRENCY: usize = 4;

/// F5/F6's dialog (T-5.2.1): destination field, a narrow set of
/// TC-standard options, and the confirm/cancel actions that hand off to
/// `duet-ops`. See the module doc comment for the full architecture.
pub(crate) struct CopyMoveDialogState {
    kind: JobKind,
    /// Resolved once, at open time, by `Workspace::open_copy_move_dialog`
    /// -- see `resolve_source_names`'s doc comment for the selection-or-
    /// cursor-fallback logic that produced these.
    sources: Vec<VPath>,
    destination: Entity<InputState>,
    conflict_policy: ConflictPolicy,
    verify: bool,
    /// `false` = run now (`enqueue`'s `priority: 0`), `true` = queued
    /// (`priority: 10`) -- `QueueManager`'s own doc comment: "lower runs
    /// first," so queuing something is choosing a *worse* (higher)
    /// priority than whatever's already running now, not a separate
    /// concept from priority itself.
    queued: bool,
    /// Guards against a double-`Enter` re-entering `confirm` while a
    /// previous plan is still being computed off the UI thread -- there's
    /// no progress indicator in this dialog (T-5.2.2's job), so nothing
    /// else stops a second Enter from starting a second, redundant
    /// `plan_copy`/`plan_move` against the same sources.
    planning_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    /// `duet_config::paths::duet_state_dir()`'s result, captured once at
    /// open time -- `None` under the same rare XDG-resolution failure
    /// `Workspace`'s other config paths already tolerate. `confirm`
    /// refuses to enqueue (with a toast, dialog left open) rather than
    /// guessing a fallback location for a job's crash-safety journal.
    state_dir: Option<PathBuf>,
    /// Keeps `destination`'s `PressEnter` subscription alive for as long
    /// as this view exists -- dropped (and the subscription cancelled)
    /// when `Workspace` sets `copy_move_dialog` back to `None`.
    _subscriptions: Vec<Subscription>,
}

impl CopyMoveDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: JobKind,
        sources: Vec<VPath>,
        initial_destination: String,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let destination = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial_destination)
                .placeholder("Destination directory")
        });
        let _subscriptions =
            vec![cx.subscribe_in(&destination, window, Self::on_destination_event)];
        destination.update(cx, |state, cx| state.focus(window, cx));

        Self {
            kind,
            sources,
            destination,
            conflict_policy: ConflictPolicy::Skip,
            verify: false,
            queued: false,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            _subscriptions,
        }
    }

    /// Test-only: exposes `kind`/`sources`/`destination`'s live text so
    /// `workspace.rs`'s F5/F6 tests can assert the dialog opened with the
    /// right defaults without reaching into otherwise-private fields --
    /// same reasoning as `FileTable::new_tab_handler`'s own `#[cfg(test)]`
    /// accessor.
    #[cfg(test)]
    pub(crate) fn kind(&self) -> JobKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn sources(&self) -> &[VPath] {
        &self.sources
    }

    #[cfg(test)]
    pub(crate) fn destination_value(&self, cx: &App) -> String {
        self.destination.read(cx).value().to_string()
    }

    /// See the module doc comment's "Catching Enter/Escape/Tab" section:
    /// `InputState::enter` always emits `PressEnter`, live-`Window` or
    /// not (this callback always has one -- it only ever fires from a
    /// real keypress). `confirm` itself needs no `Window` -- it only ever
    /// reads/spawns, and defers its one close-path `Window` need (see
    /// `Workspace::close_copy_move_dialog_deferred`'s doc comment) -- so
    /// `_window` here is `subscribe_in`'s callback shape, not something
    /// this handler itself uses.
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

    fn set_conflict_policy(&mut self, policy: ConflictPolicy, cx: &mut Context<Self>) {
        self.conflict_policy = policy;
        cx.notify();
    }

    fn toggle_verify(&mut self, cx: &mut Context<Self>) {
        self.verify = !self.verify;
        cx.notify();
    }

    fn toggle_queued(&mut self, cx: &mut Context<Self>) {
        self.queued = !self.queued;
        cx.notify();
    }

    /// Escape: cancel without doing anything, exactly like
    /// `HotlistDelegate::cancel`.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_copy_move_dialog(window, cx);
        });
    }

    /// Tab, inside the destination field -- see the module doc comment's
    /// "Tab-completion" section for the full, deliberately narrow scope.
    fn try_complete_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.destination.read(cx).value().to_string();
        let Some((parent, prefix)) = split_parent_prefix(&text) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let completed = workspace
            .read(cx)
            .completion_candidate(&parent, &prefix, cx);
        if let Some(completed) = completed {
            self.destination.update(cx, |state, cx| {
                state.set_value(completed, window, cx);
            });
        }
    }

    /// Enter: builds `PlanOptions` from the dialog's current option
    /// state, runs `plan_copy`/`plan_move` off the UI thread
    /// (`tokio_handle`, the established `oneshot`-bridge shape every
    /// other background query in `duet-ui` already uses -- see
    /// `file_table::spawn_directory_load`), and on success hands the
    /// materialised `Plan` to `QueueManager::enqueue` -- also off the UI
    /// thread, in the same spawned task, since `enqueue` synchronously
    /// opens the job's journal file (`Journal::open`, real file I/O) and
    /// design.md §8.2's "main thread does no I/O, ever" applies here just
    /// as much as it does to `plan_copy`/`plan_move` themselves.
    ///
    /// On success the dialog closes immediately without waiting for the
    /// job itself to finish (TC's own "dialog closes, operation proceeds
    /// in background" convention) -- but *does* wait for planning (a
    /// walk of the source tree, not the actual copy) to complete first,
    /// since a failure there (e.g. an unwritable/nonexistent destination)
    /// is something the user can still fix in the same dialog. On
    /// failure the dialog stays open and the error is surfaced via
    /// `Workspace::push_pending_notice` (this callback has no live
    /// `Window` -- see that method's own doc comment for why) rather than
    /// closing and losing the sources/options the user already set up.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
            return;
        }

        let dest_text = self.destination.read(cx).value().to_string();
        let Ok(dest_vpath) = local_vpath(Path::new(dest_text.as_str())) else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    crate::workspace::NoticeLevel::Error,
                    format!("\u{201c}{dest_text}\u{201d} isn't a valid destination path."),
                    cx,
                );
            });
            return;
        };
        let Some(state_dir) = self.state_dir.clone() else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    crate::workspace::NoticeLevel::Error,
                    "Can't run the operation: no writable state directory found \
                     (is $HOME/$XDG_STATE_HOME set?)."
                        .to_string(),
                    cx,
                );
            });
            return;
        };

        self.planning_in_progress = true;

        let kind = self.kind;
        let sources = self.sources.clone();
        let options = PlanOptions {
            default_conflict: self.conflict_policy,
            verify: self.verify,
        };
        let priority = if self.queued { 10 } else { 0 };
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let queue = self.queue.clone();
        let workspace = self.workspace.clone();
        let this_entity = cx.entity();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let cancel = CancelToken::new();
            let plan_result = match kind {
                JobKind::Copy => {
                    plan_copy(fs.as_ref(), &sources, &dest_vpath, options, &cancel).await
                }
                JobKind::Move => {
                    plan_move(fs.as_ref(), &sources, &dest_vpath, options, &cancel).await
                }
                other => unreachable!(
                    "the copy/move dialog only ever plans JobKind::Copy/Move, got {other:?}"
                ),
            };
            let outcome = match plan_result {
                Ok(plan) => {
                    Ok(queue.enqueue(kind, plan, priority, fs, state_dir, JOB_CONCURRENCY, None))
                }
                Err(err) => Err(describe_planner_error(&err)),
            };
            let _ = tx.send(outcome);
        });

        cx.spawn(async move |_this, cx| {
            let outcome = rx.await;
            let _ = this_entity.update(cx, |this, cx| {
                this.planning_in_progress = false;
                cx.notify();
            });
            match outcome {
                Ok(Ok(_job_id)) => {
                    let _ = workspace.update(cx, |workspace, cx| {
                        workspace.close_copy_move_dialog_deferred(cx);
                    });
                }
                Ok(Err(message)) => {
                    let _ = workspace.update(cx, |workspace, cx| {
                        workspace.push_pending_notice(
                            crate::workspace::NoticeLevel::Error,
                            format!("Couldn't plan the operation: {message}"),
                            cx,
                        );
                    });
                }
                Err(_) => {
                    let _ = workspace.update(cx, |workspace, cx| {
                        workspace.push_pending_notice(
                            crate::workspace::NoticeLevel::Error,
                            "The planning task was dropped before completing.".to_string(),
                            cx,
                        );
                    });
                }
            }
        })
        .detach();
    }
}

impl Render for CopyMoveDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let verb = match self.kind {
            JobKind::Copy => "Copy",
            JobKind::Move => "Move",
            _ => "Operate on",
        };
        let title = format!(
            "{verb} {} item{} to:",
            self.sources.len(),
            if self.sources.len() == 1 { "" } else { "s" }
        );
        let conflict_label = match self.conflict_policy {
            ConflictPolicy::Skip => "Skip",
            ConflictPolicy::Overwrite => "Overwrite",
            ConflictPolicy::AutoRename => "Auto-rename",
            _ => "Skip",
        };
        let mode_label = if self.queued { "Queued" } else { "Run now" };
        let verify_label = if self.verify { "on" } else { "off" };

        div()
            .id("copy-move-dialog")
            .key_context("CopyMoveDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(Input::new(&self.destination))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(format!(
                        "Conflict: {conflict_label} (Ctrl+1/2/3)    Verify: {verify_label} \
                         (Ctrl+K)    Mode: {mode_label} (Ctrl+J)"
                    )),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("Enter to confirm, Esc to cancel, Tab to complete the destination"),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.try_complete_destination(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SetConflictSkip, _window, cx| {
                this.set_conflict_policy(ConflictPolicy::Skip, cx);
            }))
            .on_action(cx.listener(|this, _: &SetConflictOverwrite, _window, cx| {
                this.set_conflict_policy(ConflictPolicy::Overwrite, cx);
            }))
            .on_action(cx.listener(|this, _: &SetConflictAutoRename, _window, cx| {
                this.set_conflict_policy(ConflictPolicy::AutoRename, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleVerify, _window, cx| this.toggle_verify(cx)))
            .on_action(cx.listener(|this, _: &ToggleQueued, _window, cx| this.toggle_queued(cx)))
    }
}

/// F5/F6's "what to operate on" resolution (T-5.2.1): every selected
/// entry's name, in display order, or -- if nothing is selected -- the
/// cursor row's own entry, matching how Total Commander (and this app's
/// own context menu, `FileTableDelegate::context_menu`) already treat "no
/// explicit selection" as "the thing the cursor is on." Returns an empty
/// `Vec` if there's nothing usable either way (an empty listing, or the
/// cursor parked on the synthetic ".." row -- see
/// `FileTableDelegate::cursor_entry_name`'s own doc comment for why that
/// case deliberately returns `None` rather than the parent directory's
/// own name).
///
/// A plain function over `&FileTableDelegate` rather than a `Workspace`
/// method specifically so it's unit-testable without any GPUI harness --
/// `FileTableDelegate::new` and every method this calls are plain,
/// GPUI-free Rust (confirmed by reading `file_table.rs`'s own test
/// module, which already constructs delegates this way).
pub(crate) fn resolve_source_names(delegate: &FileTableDelegate) -> Vec<String> {
    let model = delegate.model();
    let selected: Vec<String> = model
        .order()
        .iter()
        .filter_map(|&ix| {
            let id = duet_types::EntryId::new(ix);
            model
                .is_selected(id)
                .then(|| model.entries().name(id).to_string())
        })
        .collect();
    if !selected.is_empty() {
        return selected;
    }
    delegate.cursor_entry_name().into_iter().collect()
}

/// Splits the destination field's current text into a candidate parent
/// directory and the partial last segment to complete -- see the module
/// doc comment's "Tab-completion" section. A trailing `/` means the user
/// hasn't typed anything for the next segment yet, so the whole string is
/// the parent and the prefix is empty (`Path::file_name` would otherwise
/// return `None` for a path ending in `/`, which would wrongly treat
/// "already typed a full directory, hit Tab" as unparseable).
fn split_parent_prefix(text: &str) -> Option<(PathBuf, String)> {
    let path = Path::new(text);
    if text.ends_with('/') {
        return Some((path.to_path_buf(), String::new()));
    }
    let parent = path.parent()?;
    let prefix = path.file_name()?.to_str()?;
    Some((parent.to_path_buf(), prefix.to_string()))
}

/// The actual "does exactly one name in `model` start with `prefix`"
/// check -- factored out from `Workspace::completion_candidate` so it's
/// unit-testable against a plain `DirectoryModel` with no live panel/
/// GPUI state needed, same reasoning as `resolve_source_names`.
/// Case-sensitive, matching the Linux filesystem itself.
pub(crate) fn complete_against_model(
    model: &DirectoryModel,
    parent: &Path,
    prefix: &str,
) -> Option<String> {
    let mut matches = model
        .ordered_names()
        .filter(|(_, name)| name.starts_with(prefix));
    let (id, name) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let mut completed = parent.join(name).to_string_lossy().into_owned();
    if model.entries().kind(id) == EntryKind::Directory && !completed.ends_with('/') {
        completed.push('/');
    }
    Some(completed)
}

/// Renders a [`PlannerError`] for a user-facing toast -- `PlannerError`
/// itself only derives `Debug` (there's no reason for `duet-ops` to carry
/// a `Display` impl purely for this one caller), so this is the one place
/// that needs a readable message.
fn describe_planner_error(err: &PlannerError) -> String {
    match err {
        PlannerError::Cancelled => "cancelled".to_string(),
        PlannerError::Vfs(e) => e.to_string(),
        PlannerError::NoFileName(path) => format!("{path} has no file name"),
    }
}

#[cfg(test)]
mod tests {
    use duet_types::{EntryKind as Kind, Metadata, Timestamp};

    use super::*;
    use crate::file_table::FileTableDelegate;

    fn meta(kind: Kind, size: u64) -> Metadata {
        let mut m = Metadata::minimal(kind);
        m.size = size;
        m.modified = Some(Timestamp::new(0, 0));
        m
    }

    fn model_with(entries: &[(&str, Kind)]) -> DirectoryModel {
        let mut model = DirectoryModel::new();
        for (name, kind) in entries {
            model.entries_mut().push(name, &meta(*kind, 0));
        }
        model.sort_by(duet_index::SortColumn::Name, true);
        model
    }

    // -- resolve_source_names --------------------------------------------

    #[test]
    fn resolve_source_names_prefers_the_real_selection() {
        let model = model_with(&[
            ("a.txt", Kind::File),
            ("b.txt", Kind::File),
            ("c.txt", Kind::File),
        ]);
        let mut delegate = FileTableDelegate::new(model);
        // Select "a.txt" and "c.txt" (rows 0 and 2 in name-ascending
        // order) directly on the model -- `toggle_cursor_selection` is
        // private to `file_table` (keyboard-only), so this goes through
        // `model_mut()` instead, same as `examples/bench_file_table.rs`
        // already does for the same reason (see that method's own doc
        // comment).
        let order = delegate.model().order().to_vec();
        delegate
            .model_mut()
            .select(duet_types::EntryId::new(order[0]));
        delegate
            .model_mut()
            .select(duet_types::EntryId::new(order[2]));

        let mut names = resolve_source_names(&delegate);
        names.sort();
        assert_eq!(names, vec!["a.txt".to_string(), "c.txt".to_string()]);
    }

    #[test]
    fn resolve_source_names_falls_back_to_the_cursor_row_when_nothing_selected() {
        let model = model_with(&[("only.txt", Kind::File)]);
        let delegate = FileTableDelegate::new(model);
        assert_eq!(
            resolve_source_names(&delegate),
            vec!["only.txt".to_string()]
        );
    }

    #[test]
    fn resolve_source_names_is_empty_when_cursor_is_on_the_parent_row() {
        let model = model_with(&[("dir", Kind::Directory)]);
        let mut delegate = FileTableDelegate::new(model);
        delegate.set_has_parent_row(true);
        delegate.move_cursor_to(0); // lands on the synthetic ".." row
        assert!(resolve_source_names(&delegate).is_empty());
    }

    #[test]
    fn resolve_source_names_is_empty_for_an_empty_listing() {
        let delegate = FileTableDelegate::new(DirectoryModel::new());
        assert!(resolve_source_names(&delegate).is_empty());
    }

    // -- split_parent_prefix ---------------------------------------------

    #[test]
    fn split_parent_prefix_splits_a_partial_last_segment() {
        assert_eq!(
            split_parent_prefix("/home/user/doc"),
            Some((PathBuf::from("/home/user"), "doc".to_string()))
        );
    }

    #[test]
    fn split_parent_prefix_treats_a_trailing_slash_as_an_empty_prefix() {
        assert_eq!(
            split_parent_prefix("/home/user/"),
            Some((PathBuf::from("/home/user/"), String::new()))
        );
    }

    // -- complete_against_model -------------------------------------------

    #[test]
    fn complete_against_model_completes_a_single_match() {
        let model = model_with(&[
            ("documents", Kind::Directory),
            ("downloads", Kind::Directory),
        ]);
        let completed = complete_against_model(&model, Path::new("/home/user"), "doc");
        assert_eq!(completed, Some("/home/user/documents/".to_string()));
    }

    #[test]
    fn complete_against_model_appends_no_slash_for_a_file() {
        let model = model_with(&[("report.txt", Kind::File)]);
        let completed = complete_against_model(&model, Path::new("/home/user"), "rep");
        assert_eq!(completed, Some("/home/user/report.txt".to_string()));
    }

    #[test]
    fn complete_against_model_is_a_noop_on_zero_matches() {
        let model = model_with(&[("documents", Kind::Directory)]);
        assert_eq!(
            complete_against_model(&model, Path::new("/home/user"), "zzz"),
            None
        );
    }

    #[test]
    fn complete_against_model_is_a_noop_on_multiple_matches() {
        let model = model_with(&[
            ("documents", Kind::Directory),
            ("downloads", Kind::Directory),
        ]);
        assert_eq!(
            complete_against_model(&model, Path::new("/home/user"), "do"),
            None
        );
    }
}
