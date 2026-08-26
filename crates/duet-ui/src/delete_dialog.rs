// SPDX-License-Identifier: MIT
//! T-5.2.6's delete confirmation (FR-OPS-01, `docs/keymap-tc.csv`'s
//! `ops.delete`/`ops.delete_permanent` rows): F8/`Delete` confirm-then-run,
//! `Shift+F8`/`Shift+Delete` the same with trash explicitly bypassed.
//!
//! # What this module owns, and what it deliberately doesn't
//!
//! The planning half is already real, tested, and merged
//! (`duet_ops::plan_delete`, T-5.1.8) -- this module is only the UI on top:
//! a small keyboard-driven confirmation, the trash-versus-permanent choice
//! and its configured default, the non-empty-directory warning, and the
//! hand-off to `duet_ops::QueueManager::enqueue`. Deciding *whether* to
//! show the dialog at all (`operations.confirm_delete`'s three policies)
//! belongs to `Workspace::open_delete_dialog`, not here: this view is
//! constructed only once that decision has already been made.
//!
//! "Trash" here means exactly what `duet_ops::deleter`'s own module doc
//! comment says it means at the planner layer -- move the target into a
//! real directory (`duet_config::paths::trash_files_dir`) instead of
//! removing its content. It is **not** the freedesktop trash spec:
//! `.trashinfo` sidecars, `$topdir/.Trash-$uid` for other mounts, and a
//! browsable/restorable trash view are all T-5.3.1's own, later scope, and
//! nothing here needs undoing when that lands (see `trash_files_dir`'s own
//! doc comment: it is already the same destination the spec-compliant
//! implementation uses for a home-filesystem delete).
//!
//! # No `ConflictResolver`, deliberately
//!
//! Every other job this crate enqueues passes `Workspace`'s live,
//! interactive `ConflictResolver` (T-5.2.3). A delete job passes `None`,
//! and that is not an oversight in either mode: permanent delete has no
//! conflict concept at all (nothing is being written anywhere), and
//! `plan_delete` force-overrides `PlanOptions::default_conflict` to
//! `ConflictPolicy::AutoRename` for trash mode regardless of what the
//! caller asks for (`deleter.rs`'s own "one thing this module *does* still
//! own" paragraph), so a trashed name colliding with an
//! already-trashed one disambiguates itself with no human in the loop.
//!
//! # Overlay architecture
//!
//! Mirrors `crate::conflict_dialog::ConflictDialogState`, not
//! `crate::copy_move_dialog::CopyMoveDialogState`: there is no destination
//! to type here, only keyboard choices, so this view owns its own
//! `FocusHandle` and `.track_focus`es its render root rather than parking
//! focus in a `duet_widgets::input::InputState` delegate. With no
//! `InputState` anywhere in this tree, Enter/Escape need no bubbled-action
//! catching either (the dance `copy_move_dialog.rs`'s module doc comment
//! explains) -- plain `KeyBinding`s in this dialog's own `"DeleteDialog"`
//! key context are enough. `Workspace` owns `Option<Entity<
//! DeleteDialogState>>` and builds the backdrop/card chrome around it
//! (`workspace::delete_dialog_overlay`), same as every sibling overlay.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{CancelToken, DeleteMode, JobId, JobKind, PlanOptions, QueueManager, plan_delete};
use duet_types::VPath;
use duet_vfs::{FileSystem, ListOpts, LocalFs};
use duet_widgets::theme::TokenPalette;
use futures_util::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, Styled as _, WeakEntity, Window, actions, div, px,
};

use crate::copy_move_dialog::{JOB_CONCURRENCY, describe_planner_error};
use crate::dialog_job::report_job_outcome;
use crate::workspace::{NoticeLevel, Workspace};

// This dialog's own three actions. Enter/Escape are the same
// confirm/cancel keys every dialog in this app already uses. `Ctrl+T` for
// the trash/permanent toggle is this codebase's own reasonable default --
// Total Commander offers no in-dialog toggle at all (its own delete
// confirmation is a fixed yes/no whose *mode* was already decided by which
// key opened it), so `docs/keymap-tc.csv` has no row to follow here. It
// does not collide with `crate::panel`'s own `Ctrl+T` ("new tab"): that
// binding is scoped to the `"Panel"` key context, which is not in the
// dispatch path while this dialog -- a sibling of the panels under
// `Workspace`'s root, not a descendant of either -- holds focus.
actions!(
    duet_delete_dialog,
    [ToggleTrashPermanent, ConfirmDelete, CancelDelete]
);

/// Registers this dialog's own keybindings, scoped to `"DeleteDialog"`
/// (set on [`DeleteDialogState::render`]'s own root `div`). Called once
/// from `workspace::run` (and from that module's own `with_workspace` test
/// harness), alongside every other `bind_*_keys` function.
pub(crate) fn bind_delete_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-t", ToggleTrashPermanent, Some("DeleteDialog")),
        KeyBinding::new("enter", ConfirmDelete, Some("DeleteDialog")),
        KeyBinding::new("escape", CancelDelete, Some("DeleteDialog")),
    ]);
}

/// T-5.2.6's delete confirmation view. See the module doc comment for the
/// full architecture; `Workspace::open_delete_dialog` is the only
/// constructor call site and has already resolved everything below.
pub(crate) struct DeleteDialogState {
    /// Resolved once, at open time, by `Workspace::open_delete_dialog` --
    /// the same selection-or-cursor-fallback logic F5/F6 use
    /// (`crate::copy_move_dialog::resolve_source_names`), not a second,
    /// parallel answer to "what am I operating on".
    targets: Vec<VPath>,
    /// The dialog's current choice: `true` removes content, `false` moves
    /// it into `duet_config::paths::trash_files_dir()`. Defaults from
    /// `operations.delete_default` (and `trash.enabled`, see
    /// `workspace::load_delete_default_permanent`) unless
    /// `permanent_forced` is set.
    permanent: bool,
    /// `true` when opened via `Shift+F8`/`Shift+Delete` -- this task's own
    /// AC clause "Shift+Del bypasses trash with an explicit confirmation."
    /// Suppresses the in-dialog toggle entirely (see
    /// [`Self::toggle_trash_permanent`]) rather than merely pre-setting
    /// `permanent`.
    permanent_forced: bool,
    /// Names of the directory targets found to be non-empty, computed by
    /// `Workspace::open_delete_dialog` *before* this dialog was
    /// constructed (it is one of the inputs to the "should a dialog even
    /// show" decision under `confirm_delete = "non_empty_dirs"`). Empty
    /// when nothing non-empty was found -- or when there were no directory
    /// targets to check in the first place. Used only for the warning
    /// line.
    non_empty_dir_names: Vec<String>,
    focus_handle: FocusHandle,
    /// Guards against a double-Enter re-entering [`Self::confirm`] while a
    /// previous `plan_delete` is still running off the UI thread -- same
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
}

impl DeleteDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        targets: Vec<VPath>,
        permanent: bool,
        permanent_forced: bool,
        non_empty_dir_names: Vec<String>,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Unlike `ConflictDialogState` (constructed from a background
        // consumer loop with no `Window` at all -- see that module's own
        // doc comment), every path that opens *this* dialog ends with a
        // live `Window`, so focus can be taken immediately here rather
        // than deferred to the next render.
        window.focus(&focus_handle);
        Self {
            targets,
            permanent,
            permanent_forced,
            non_empty_dir_names,
            focus_handle,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
        }
    }

    /// Test-only accessors -- same reasoning as `CopyMoveDialogState`'s own
    /// `#[cfg(test)]` block: `workspace.rs`'s end-to-end tests need to read
    /// this otherwise-private state without a public API surface
    /// production code would never use.
    #[cfg(test)]
    pub(crate) fn targets(&self) -> &[VPath] {
        &self.targets
    }

    #[cfg(test)]
    pub(crate) fn permanent(&self) -> bool {
        self.permanent
    }

    #[cfg(test)]
    pub(crate) fn non_empty_dir_names(&self) -> &[String] {
        &self.non_empty_dir_names
    }

    #[cfg(test)]
    pub(crate) fn warning_text(&self) -> Option<String> {
        non_empty_warning(&self.non_empty_dir_names)
    }

    #[cfg(test)]
    pub(crate) fn title_text(&self) -> String {
        delete_title(self.targets.len(), self.permanent)
    }

    /// `Ctrl+T`: flips between trash and permanent. A deliberate no-op when
    /// `permanent_forced` -- `Shift+Del`'s whole point is that it does
    /// *not* offer the trash option, so a dialog opened that way has no
    /// path back to it (the render below shows no toggle hint in that
    /// state either).
    fn toggle_trash_permanent(&mut self, cx: &mut Context<Self>) {
        if self.permanent_forced {
            return;
        }
        self.permanent = !self.permanent;
        cx.notify();
    }

    /// Escape: cancel without doing anything, mirroring
    /// `CopyMoveDialogState::cancel`.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_delete_dialog(window, cx);
        });
    }

    /// Enter: runs `plan_delete` + `QueueManager::enqueue` off the UI
    /// thread ([`spawn_delete_job`], the same background half
    /// `Workspace::start_delete_job` uses for the no-confirmation policy)
    /// and bridges the result back with `cx.spawn`, mirroring
    /// `CopyMoveDialogState::confirm`'s shape exactly. On success the
    /// dialog closes immediately without waiting for the job itself (TC's
    /// own "dialog closes, operation proceeds in background" convention);
    /// on failure it stays open with the error surfaced as a toast.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
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

        let rx = spawn_delete_job(
            &self.tokio_handle,
            self.targets.clone(),
            self.permanent,
            self.queue.clone(),
            state_dir,
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
                    workspace.close_delete_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }
}

impl Focusable for DeleteDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The dialog's headline -- pluralised exactly the way
/// `CopyMoveDialogState::render`'s own `"{verb} {n} item{s} to:"` is. A
/// free function so it's unit-testable without a GPUI harness, same
/// reasoning as `conflict_dialog::hash_line`.
fn delete_title(count: usize, permanent: bool) -> String {
    let plural = if count == 1 { "" } else { "s" };
    if permanent {
        format!("Delete {count} item{plural} permanently?")
    } else {
        format!("Move {count} item{plural} to trash?")
    }
}

/// T-5.2.6's "non-empty directory warning" line, or `None` when there is
/// nothing to warn about. Names the directory when there's exactly one
/// (the overwhelmingly common case, and the only one where a name is
/// short enough to be more useful than a count); counts them otherwise,
/// rather than rendering an unbounded list of names into a fixed-width
/// dialog card.
fn non_empty_warning(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [only] => Some(format!(
            "Warning: \u{201c}{only}\u{201d} is not empty \u{2014} its whole contents go too."
        )),
        many => Some(format!(
            "Warning: {} of the selected directories are not empty ({}) \u{2014} their whole \
             contents go too.",
            many.len(),
            many.join(", ")
        )),
    }
}

impl Render for DeleteDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = delete_title(self.targets.len(), self.permanent);
        let mode_line = if self.permanent_forced {
            "Permanent delete (Shift+Del) \u{2014} the trash is bypassed.".to_string()
        } else {
            let current = if self.permanent { "Permanent" } else { "Trash" };
            format!("Mode: {current} \u{2014} Trash / Permanent (Ctrl+T)")
        };
        let warning = non_empty_warning(&self.non_empty_dir_names);

        div()
            .id("delete-dialog")
            .key_context("DeleteDialog")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(mode_line),
            )
            .when_some(warning, |this, warning| {
                this.child(
                    div()
                        .text_size(px(11.))
                        // `min_w(0)` for the same reason
                        // `conflict_dialog.rs`'s metadata columns carry it:
                        // a long, unbroken directory name would otherwise
                        // widen this flex child past the card instead of
                        // wrapping inside it.
                        .min_w(px(0.))
                        .text_color(tokens.color.error)
                        .child(warning),
                )
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("Enter to confirm, Esc to cancel"),
            )
            .on_action(cx.listener(|this, _: &ToggleTrashPermanent, _window, cx| {
                this.toggle_trash_permanent(cx);
            }))
            .on_action(cx.listener(|this, _: &ConfirmDelete, _window, cx| this.confirm(cx)))
            .on_action(cx.listener(|this, _: &CancelDelete, window, cx| this.cancel(window, cx)))
    }
}

/// The plan-and-enqueue half of a delete, off the UI thread -- shared by
/// [`DeleteDialogState::confirm`] and `Workspace::start_delete_job` (the
/// `confirm_delete = "never"` path and the "`non_empty_dirs` found nothing
/// to warn about" path, neither of which ever constructs a dialog). Returns
/// the `oneshot` receiver each caller bridges back onto GPUI with its own
/// `cx.spawn`, since what they do with the result differs (close a dialog
/// versus nothing at all) even though how they compute it does not.
///
/// `enqueue` runs inside the spawned task, not on the caller's thread, for
/// the same reason `CopyMoveDialogState::confirm` does it there: it
/// synchronously opens the job's journal file, and design.md §8.2's "main
/// thread does no I/O, ever" covers that just as much as the planning walk
/// itself.
pub(crate) fn spawn_delete_job(
    tokio_handle: &tokio::runtime::Handle,
    targets: Vec<VPath>,
    permanent: bool,
    queue: Arc<QueueManager>,
    state_dir: PathBuf,
) -> tokio::sync::oneshot::Receiver<Result<JobId, String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio_handle.spawn(async move {
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let outcome = match resolve_delete_mode(permanent).await {
            Ok(mode) => {
                let cancel = CancelToken::new();
                match plan_delete(fs.as_ref(), &targets, mode, PlanOptions::default(), &cancel)
                    .await
                {
                    Ok(plan) => Ok(queue.enqueue(
                        JobKind::Delete { permanent },
                        plan,
                        0,
                        fs,
                        state_dir,
                        JOB_CONCURRENCY,
                        // No resolver, deliberately -- see the module doc
                        // comment's "No `ConflictResolver`" section.
                        None,
                    )),
                    Err(err) => Err(describe_planner_error(&err)),
                }
            }
            Err(message) => Err(message),
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Turns the dialog's `permanent` choice into the [`DeleteMode`]
/// `plan_delete` takes, creating the trash directory if this is the first
/// thing ever trashed on this machine (`plan_move`, which trash mode is
/// built on, needs a destination that already exists). Any failure here
/// becomes the same user-facing "couldn't plan the operation" toast a
/// planner error would.
async fn resolve_delete_mode(permanent: bool) -> Result<DeleteMode, String> {
    if permanent {
        return Ok(DeleteMode::Permanent);
    }
    let dir = duet_config::paths::trash_files_dir().map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("couldn't create the trash directory {}: {e}", dir.display()))?;
    let trash_dir = crate::file_table::local_vpath(&dir)?;
    Ok(DeleteMode::Trash { trash_dir })
}

/// Which of `dirs` (name plus already-resolved path) actually have at
/// least one entry in them -- `operations.confirm_delete = "non_empty_dirs"`'s
/// whole decision, and the warning line's content under every policy that
/// checks. Streams through `FileSystem::read_dir` with
/// `ListOpts::names_only()` (no `stat` per entry) and stops at the first
/// non-empty chunk, so the cost is one `getdents64` per directory, not a
/// full listing.
///
/// A directory that can't be read at all (permission denied, or it vanished
/// between listing and this call) is treated as *not* warned about: the
/// delete itself will surface the real error through the job's own error
/// report (T-5.2.4), and inventing a "this might not be empty" warning from
/// an unrelated failure would be misleading. A free, `FileSystem`-generic
/// async function so it's directly testable against a real `LocalFs` and
/// real tempdirs with no GPUI involved.
pub(crate) async fn non_empty_directory_names(
    fs: &dyn FileSystem,
    dirs: &[(String, VPath)],
) -> Vec<String> {
    let mut found = Vec::new();
    for (name, path) in dirs {
        if directory_is_non_empty(fs, path).await {
            found.push(name.clone());
        }
    }
    found
}

async fn directory_is_non_empty(fs: &dyn FileSystem, path: &VPath) -> bool {
    let mut stream = fs.read_dir(path, ListOpts::names_only());
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(entries) if !entries.is_empty() => return true,
            // An empty chunk is legal (a backend may yield one before
            // ending) -- keep reading rather than concluding "empty" early.
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- delete_title -----------------------------------------------------

    #[test]
    fn delete_title_distinguishes_trash_from_permanent() {
        assert_eq!(delete_title(1, false), "Move 1 item to trash?");
        assert_eq!(delete_title(1, true), "Delete 1 item permanently?");
    }

    #[test]
    fn delete_title_pluralises_beyond_one_item() {
        assert_eq!(delete_title(3, false), "Move 3 items to trash?");
        assert_eq!(delete_title(0, true), "Delete 0 items permanently?");
    }

    // -- non_empty_warning --------------------------------------------------

    #[test]
    fn non_empty_warning_is_absent_with_nothing_to_warn_about() {
        assert_eq!(non_empty_warning(&[]), None);
    }

    #[test]
    fn non_empty_warning_names_a_single_directory() {
        let warning = non_empty_warning(&["photos".to_string()]).unwrap();
        assert!(warning.contains("photos"), "{warning}");
        assert!(warning.starts_with("Warning:"), "{warning}");
    }

    #[test]
    fn non_empty_warning_counts_and_lists_several_directories() {
        let warning =
            non_empty_warning(&["photos".to_string(), "docs".to_string(), "src".to_string()])
                .unwrap();
        assert!(warning.contains('3'), "{warning}");
        assert!(warning.contains("photos"), "{warning}");
        assert!(warning.contains("src"), "{warning}");
    }

    // -- non_empty_directory_names --------------------------------------------

    #[tokio::test]
    async fn non_empty_directory_names_finds_only_the_directories_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        let full = dir.path().join("full");
        std::fs::create_dir(&empty).unwrap();
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("a.txt"), b"x").unwrap();

        let fs = LocalFs;
        let dirs = vec![
            (
                "empty".to_string(),
                crate::file_table::local_vpath(&empty).unwrap(),
            ),
            (
                "full".to_string(),
                crate::file_table::local_vpath(&full).unwrap(),
            ),
        ];
        assert_eq!(
            non_empty_directory_names(&fs, &dirs).await,
            vec!["full".to_string()]
        );
    }

    /// A directory holding only a *hidden* entry is still non-empty --
    /// `ListOpts::names_only()` doesn't filter dotfiles, and a delete would
    /// take that entry with it, so the warning must fire.
    #[tokio::test]
    async fn a_directory_holding_only_a_dotfile_counts_as_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join("hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join(".config"), b"x").unwrap();

        let fs = LocalFs;
        let dirs = vec![(
            "hidden".to_string(),
            crate::file_table::local_vpath(&hidden).unwrap(),
        )];
        assert_eq!(
            non_empty_directory_names(&fs, &dirs).await,
            vec!["hidden".to_string()]
        );
    }

    #[tokio::test]
    async fn an_unreadable_directory_is_not_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs;
        let dirs = vec![(
            "missing".to_string(),
            crate::file_table::local_vpath(&dir.path().join("missing")).unwrap(),
        )];
        assert!(non_empty_directory_names(&fs, &dirs).await.is_empty());
    }

    // -- resolve_delete_mode -------------------------------------------------

    #[tokio::test]
    async fn resolve_delete_mode_is_permanent_without_touching_the_filesystem() {
        assert!(matches!(
            resolve_delete_mode(true).await,
            Ok(DeleteMode::Permanent)
        ));
    }
}
