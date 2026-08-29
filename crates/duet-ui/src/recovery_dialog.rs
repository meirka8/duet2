// SPDX-License-Identifier: MIT
//! T-5.2.5's startup interrupted-operation recovery dialog (FR-OPS-07,
//! design.md §9.3: "On next launch, incomplete journals surface as 'N
//! interrupted operations — review'. The user can resume ..., discard
//! partials, or inspect.").
//!
//! # Two-phase task: this is phase 2
//!
//! Phase 1 (already merged, `duet-ops`) built everything this module needs
//! and nothing this module re-derives: `duet_ops::JournalReader::scan`
//! replays every job journal under `state_dir` into a
//! [`duet_ops::RecoveryReport`]; `duet_ops::plan_from_recovery` rebuilds a
//! resumable [`duet_ops::Plan`] from a report's `incomplete_steps`;
//! `duet_ops::orphaned_partial_path` re-derives an orphaned partial's real
//! on-disk path; and `duet_ops::Journal::resolve` durably retires a
//! report's dangling intents so it stops resurfacing. This module is only
//! the view and the two actions (resume/discard) on top, plus
//! `Workspace`'s startup wiring in `workspace.rs`.
//!
//! # The one thing this whole module exists to get right
//!
//! `duet_ops::journal`'s own module doc comment states it plainly:
//! `JournalReader::scan` recomputes `incomplete_steps`/`orphaned_partials`
//! *purely* from a journal's own `Intent`/`Completion` records. Deleting an
//! orphaned partial file, or handing its remaining steps to a brand-new
//! job, changes nothing about those records by itself. **If [`Self::
//! resume_selected`] or [`Self::discard_selected`] ever let a report drop
//! out of `self.reports` without a corresponding `Journal::resolve` call
//! having actually landed on disk, that exact report reappears on every
//! future launch, forever.** Both actions are therefore built as the same
//! two-phase shape: do the real work (enqueue a resume job / delete the
//! partials), *then* resolve the original job's journal, and only remove
//! the report from this dialog's own list ([`Self::retire`]) once
//! `resolve` itself has actually succeeded — never optimistically, and
//! never as a side effect of merely firing the keystroke.
//!
//! # Overlay architecture: mirrors `crate::operation_manager`, not
//! `crate::job_report_dialog`
//!
//! Every report needs its own keyboard-selectable target for Enter/`D`/
//! `Space` to act on — the same "a single `cursor: usize` index into a
//! deterministically ordered list, re-clamped every render" shape
//! `crate::operation_manager::OperationManagerState` already established
//! for exactly the same reason (bare `Up`/`Down`/`P`/`R`/`C`, no text field
//! anywhere in this overlay to compete with). `crate::job_report_dialog`'s
//! list is the wrong template here: it has no cursor at all, because
//! nothing in it needs one (Enter/`R` there act on the *whole* report, not
//! a selected row).
//!
//! Plain `div` rows inside an `.overflow_y_scroll()` container, not a
//! `duet_widgets::list::List`/`ListDelegate` — same reasoning
//! `job_report_dialog.rs`'s own module doc comment gives: a startup review
//! list of, realistically, a handful of interrupted jobs has no fuzzy
//! search or filtering need the `ListDelegate` machinery exists for.
//!
//! `Workspace` owns `Option<Entity<RecoveryDialogState>>` plus
//! `recovery_dialog_previous_focus: Option<FocusHandle>`, the same pair
//! every sibling dialog has — but see [`Self::finish_action`]'s doc comment
//! for why this dialog, uniquely, never calls a `_deferred` close variant.
//!
//! # Keybindings: no `InputState` anywhere, so bare letters are safe
//!
//! Unlike the attributes/mkdir/rename dialogs (which hold a live text
//! field, so a bare letter key would just get typed into it), nothing in
//! this dialog is a text field — the same situation `delete_dialog.rs`/
//! `job_report_dialog.rs`/`operation_manager.rs` are already in, which is
//! why they too bind plain, unmodified letters (`T`, `R`, `P`/`R`/`C`/`O`)
//! in their own key contexts. `D` for discard follows the same pattern.
//!
//! # Escape: closes without resolving anything, deliberately
//!
//! Escape here does *not* call `Journal::resolve` on anything still in
//! `self.reports`. This is a disclosed scope decision, not a gap: a report
//! still incomplete/carrying orphaned partials when the dialog closes this
//! way simply reappears on the *next* launch, completely unmutated —
//! which is exactly the crash-safety guarantee FR-OPS-07 promises in the
//! first place ("an interrupted operation leaves ... never a silently
//! truncated destination"). Nothing is lost by closing without acting;
//! it is deferred, precisely the way `duet_ops::journal`'s own module doc
//! comment frames the whole mechanism. Mirrors T-5.2.7's rename dialog,
//! whose own module doc comment discloses its stem/extension split as a
//! deliberate substitution rather than the literal spec — a judgment call
//! stated plainly rather than left implicit.
//!
//! # Discard needs no confirmation step, deliberately
//!
//! `crate::delete_dialog` is this codebase's "confirm before destroying
//! user data" pattern, and it deliberately does not apply here: every file
//! [`Self::discard_selected`] ever deletes is a `.duet-partial-<rand>`
//! staging file (`duet_ops::journal`'s own module doc comment: "Destination
//! files are written to `.duet-partial-<rand>` and renamed into place only
//! when complete"), never a real source or destination file the user asked
//! to keep — disposable scratch by construction, not user content. A bare
//! `D` with no second confirmation is therefore consistent with how the
//! rest of this app already treats actions that touch nothing but its own
//! staging artifacts, not an inconsistency with `delete_dialog.rs`.
//!
//! # Inspect is an inline toggle, not a third overlay layer
//!
//! `Space` flips [`RecoveryDialogState::inspecting`] for the cursor row and
//! [`Self::render`] expands that one row in place — no second dialog, no
//! extra `Entity`. Shows the job's kind, "`N` of `M` steps remaining", and
//! one short line per still-dangling step ([`describe_incomplete_step`]).
//!
//! # Resume/discard's off-thread shape, and why there is no `_deferred`
//! close variant here
//!
//! Both actions run their real work — `queue.enqueue`/`fs.remove` plus the
//! `Journal::open`/`resolve` call that makes it durable — inside one
//! `tokio_handle.spawn`'d task on `duet-ui`'s real multi-threaded Tokio
//! runtime, with no `spawn_blocking` wrapper around the blocking journal/FS
//! calls. That is not an oversight: `duet_ops::executor`'s own `execute()`
//! doc comment (its "Operational requirement: needs a genuinely
//! multi-threaded runtime" section) documents this codebase's actual
//! convention — blocking local-FS/journal calls run inline on a runtime
//! that already has enough worker threads to tolerate it, because the real
//! off-thread dispatch is the shell layer running the *whole task* off the
//! GPUI thread in the first place, not per-syscall isolation. Every sibling
//! dialog's `spawn_delete_job`/`spawn_plan_and_enqueue` already does exactly
//! this for `Journal::open`'s own synchronous file creation.
//!
//! The GPUI-side continuation, though, is `window.spawn` (an
//! `AsyncWindowContext`), not `cx.spawn` (a plain `AsyncApp`) — unlike
//! every sibling dialog's `confirm`/`rerun`, which use `cx.spawn` and
//! therefore have no live `Window` at the point they want to close,
//! forcing them through `Workspace::close_*_dialog_deferred` and its
//! `pending_focus_restore` one-render-later dance.
//! `WeakEntity::update_in` against an `AsyncWindowContext` *does* hand the
//! continuation a live `&mut Window`, so [`Self::retire`] calls
//! `Workspace::close_recovery_dialog` — the immediate variant — directly.
//! This module therefore defines no `close_recovery_dialog_deferred` at
//! all in `workspace.rs`: there is no "no live `Window`" gap here for one
//! to bridge.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{
    ConflictResolver, JobId, Journal, QueueManager, RecoveryReport, Step, orphaned_partial_path,
    plan_from_recovery,
};
use duet_types::{ErrorKind, VPath};
use duet_vfs::{FileSystem, LocalFs, RemoveKind};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, actions, div, px,
};

use crate::copy_move_dialog::JOB_CONCURRENCY;
use crate::operation_manager::describe_kind;
use crate::workspace::{NoticeLevel, Workspace};

// This dialog's own actions. `CloseRecoveryDialog` mirrors every sibling
// overlay's Escape convention (see the module doc comment for why this one
// deliberately resolves nothing on the way out); the rest are this
// dialog's own reasonable, unclaimed-elsewhere bindings -- there is no TC
// precedent to match (Total Commander has no comparable startup recovery
// UI), the same disclosed-default situation `job_report_dialog.rs`'s own
// `R`/Enter chords are already in.
actions!(
    duet_recovery_dialog,
    [
        CloseRecoveryDialog,
        RecoveryDialogCursorUp,
        RecoveryDialogCursorDown,
        RecoveryDialogToggleInspect,
        RecoveryDialogResume,
        RecoveryDialogDiscard,
    ]
);

/// Registers this dialog's own keybindings, scoped to `"RecoveryDialog"`
/// (set on [`RecoveryDialogState::render`]'s own root `div`). Called once
/// from `workspace::run` (and from that module's own test harness),
/// alongside every other `bind_*_keys` function.
pub(crate) fn bind_recovery_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseRecoveryDialog, Some("RecoveryDialog")),
        KeyBinding::new("up", RecoveryDialogCursorUp, Some("RecoveryDialog")),
        KeyBinding::new("down", RecoveryDialogCursorDown, Some("RecoveryDialog")),
        KeyBinding::new("space", RecoveryDialogToggleInspect, Some("RecoveryDialog")),
        KeyBinding::new("enter", RecoveryDialogResume, Some("RecoveryDialog")),
        KeyBinding::new("d", RecoveryDialogDiscard, Some("RecoveryDialog")),
    ]);
}

/// T-5.2.5's startup recovery view. See the module doc comment for the
/// full architecture; `Workspace::new` is the only constructor call site,
/// reached only when a startup journal scan found at least one report that
/// `!incomplete_steps.is_empty() || !orphaned_partials.is_empty()` (that
/// filter itself lives in `workspace.rs`, not here -- this module trusts
/// whatever list it is handed).
pub(crate) struct RecoveryDialogState {
    /// Shrinks as reports are resumed/discarded ([`Self::retire`]) -- never
    /// re-scanned from disk after construction, matching
    /// `JobReportDialogState`'s own "a snapshot, not a live handle"
    /// precedent (this dialog only ever opens once, at startup, against a
    /// job set that cannot itself change out from under it before the user
    /// acts).
    reports: Vec<RecoveryReport>,
    /// The selected row, as an index into `reports` -- see the module doc
    /// comment's "mirrors `crate::operation_manager`" section.
    cursor: usize,
    /// Whether the cursor row's detail is expanded -- see the module doc
    /// comment's "Inspect is an inline toggle" section.
    inspecting: bool,
    /// `true` while a resume/discard is in flight for the cursor row --
    /// guards against a double-Enter/double-`D` re-entering
    /// [`Self::resume_selected`]/[`Self::discard_selected`] while a
    /// previous one is still running, same reasoning as every sibling
    /// dialog's own `planning_in_progress`/`rerun_in_progress` guard.
    busy: bool,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    /// The same `state_dir` `Workspace::new`'s own startup scan already
    /// resolved -- passed in whole, not re-derived, since a `RecoveryReport`
    /// carries no path of its own back to the journal file it came from
    /// (`duet_ops::journal::jobs_dir`'s naming convention is private to
    /// that module).
    state_dir: PathBuf,
    conflict_resolver: Arc<dyn ConflictResolver>,
}

impl RecoveryDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        reports: Vec<RecoveryReport>,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: PathBuf,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // `Workspace::new` always has a live `Window` at its own
        // construction call site (it is one of `new`'s own parameters),
        // same as every sibling dialog's `new` -- so focus is taken
        // immediately rather than deferred to the next render.
        window.focus(&focus_handle);
        Self {
            reports,
            cursor: 0,
            inspecting: false,
            busy: false,
            focus_handle,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
        }
    }

    /// Test-only accessors -- same reasoning as every sibling dialog's own
    /// `#[cfg(test)]` block: `workspace.rs`'s end-to-end tests need to read
    /// this otherwise-private state without a public API surface
    /// production code would never use.
    #[cfg(test)]
    pub(crate) fn reports(&self) -> &[RecoveryReport] {
        &self.reports
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn inspecting(&self) -> bool {
        self.inspecting
    }

    /// Clamps, doesn't wrap -- same convention `crate::file_table::
    /// move_cursor_by`/`crate::operation_manager::cursor_up` already
    /// establish for a plain row cursor.
    fn cursor_up(&mut self, cx: &mut Context<Self>) {
        self.cursor = self.cursor.saturating_sub(1);
        cx.notify();
    }

    fn cursor_down(&mut self, cx: &mut Context<Self>) {
        let len = self.reports.len();
        if len > 0 {
            self.cursor = (self.cursor + 1).min(len - 1);
        }
        cx.notify();
    }

    fn toggle_inspect(&mut self, cx: &mut Context<Self>) {
        self.inspecting = !self.inspecting;
        cx.notify();
    }

    /// Escape: closes without resolving anything -- see the module doc
    /// comment's own section on why that is a deliberate, disclosed
    /// choice, not a gap.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_recovery_dialog(window, cx);
        });
    }

    /// Enter: hands the cursor report's still-dangling steps to a fresh job
    /// (`duet_ops::plan_from_recovery` -> `QueueManager::enqueue`) and,
    /// only once that has actually run, durably resolves the *original*
    /// job's journal so it stops being reported by a future scan -- see the
    /// module doc comment's "the one thing this whole module exists to get
    /// right" section. Both steps happen inline inside one
    /// `tokio_handle.spawn`'d task; see the module doc comment for why no
    /// `spawn_blocking` wrapper belongs here.
    fn resume_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(report) = self.reports.get(self.cursor).cloned() else {
            return;
        };
        self.busy = true;
        cx.notify();

        let state_dir = self.state_dir.clone();
        let queue = self.queue.clone();
        let resolver = self.conflict_resolver.clone();
        let job_id = report.job_id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let plan = plan_from_recovery(&report);
            let new_job_id = queue.enqueue(
                report.kind,
                plan,
                0,
                fs,
                state_dir.clone(),
                JOB_CONCURRENCY,
                Some(resolver),
            );
            let reason = format!("resumed as job #{}", new_job_id.0);
            let resolved = Journal::open(job_id, &state_dir)
                .and_then(|journal| journal.resolve(&report, &reason));
            let _ = tx.send(resolved);
        });

        self.finish_action(job_id, rx, window, cx);
    }

    /// `D`: deletes every orphaned `.duet-partial-*` file the cursor
    /// report names and, only once every one of them is actually gone
    /// (or was already gone -- see below), durably resolves the original
    /// job's journal. Same two-phase shape as [`Self::resume_selected`],
    /// with "delete the partials" standing in for "enqueue a resume job".
    ///
    /// A real removal failure (permission denied, say -- anything but
    /// `ErrorKind::NotFound`, which this treats as success, matching
    /// `duet_ops::deleter`/`executor`'s own "already gone is fine"
    /// convention throughout) aborts *before* calling `Journal::resolve`:
    /// a partial still sitting on disk, unaccounted for, must never be
    /// reported as durably resolved, or `discard` would have silently
    /// abandoned it. See the module doc comment's "Discard needs no
    /// confirmation" section for why this action has no confirm step at
    /// all -- deleted files are always disposable scratch, never user data.
    fn discard_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(report) = self.reports.get(self.cursor).cloned() else {
            return;
        };
        self.busy = true;
        cx.notify();

        let state_dir = self.state_dir.clone();
        let job_id = report.job_id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            for (step_index, partial_name) in &report.orphaned_partials {
                let Some(path) = orphaned_partial_path(&report, *step_index, partial_name) else {
                    // Stale/foreign step index -- degrades safely per
                    // `orphaned_partial_path`'s own doc comment: nothing to
                    // delete for this entry.
                    continue;
                };
                match fs.remove(&path, RemoveKind::File).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
            let resolved = Journal::open(job_id, &state_dir)
                .and_then(|journal| journal.resolve(&report, "discarded at startup recovery"));
            let _ = tx.send(resolved);
        });

        self.finish_action(job_id, rx, window, cx);
    }

    /// The shared "await the tokio task, then reconcile GPUI state" half of
    /// both [`Self::resume_selected`] and [`Self::discard_selected`] --
    /// they differ only in what runs *before* this (enqueue-then-resolve
    /// versus delete-partials-then-resolve), never in how the outcome is
    /// folded back.
    ///
    /// Uses `window.spawn`, not `cx.spawn` -- see the module doc comment's
    /// own section on why that gives [`Self::retire`] a live `&mut Window`
    /// to call `Workspace::close_recovery_dialog` (the immediate variant)
    /// directly, with no `_deferred` sibling needed anywhere in this
    /// dialog.
    fn finish_action(
        &mut self,
        job_id: JobId,
        rx: tokio::sync::oneshot::Receiver<duet_types::Result<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let weak = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let outcome = rx.await;
                let _ = weak.update_in(cx, |this, window, cx| {
                    this.busy = false;
                    match outcome {
                        Ok(Ok(())) => this.retire(job_id, window, cx),
                        Ok(Err(err)) => {
                            cx.notify();
                            let _ = workspace.update(cx, |workspace, cx| {
                                workspace.push_pending_notice(
                                    NoticeLevel::Error,
                                    format!(
                                        "The operation ran, but its journal couldn't be marked \
                                         resolved ({err}) \u{2014} it may reappear at the next \
                                         launch."
                                    ),
                                    cx,
                                );
                            });
                        }
                        Err(_) => {
                            cx.notify();
                            let _ = workspace.update(cx, |workspace, cx| {
                                workspace.push_pending_notice(
                                    NoticeLevel::Error,
                                    "The recovery task was dropped before completing.".to_string(),
                                    cx,
                                );
                            });
                        }
                    }
                });
            })
            .detach();
    }

    /// Removes `job_id`'s report -- called only once its journal has
    /// actually been marked resolved, never before (see the module doc
    /// comment). Looks the report up by `job_id` rather than assuming it
    /// is still at `self.cursor`: the async gap between firing the action
    /// and this landing means the cursor may have moved on to a different
    /// report in the meantime. Clamps the cursor into the shrunk list, and
    /// closes the dialog once nothing is left to review.
    fn retire(&mut self, job_id: JobId, window: &mut Window, cx: &mut Context<Self>) {
        self.reports.retain(|r| r.job_id != job_id);
        if self.reports.is_empty() {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.close_recovery_dialog(window, cx);
            });
            return;
        }
        self.cursor = self.cursor.min(self.reports.len() - 1);
        cx.notify();
    }
}

impl Focusable for RecoveryDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One report's collapsed summary line: kind, job id (so a repeated kind
/// across several reports is still distinguishable), and what needs
/// attention. A free function so it's unit-testable without a GPUI
/// harness, same reasoning as `job_report_dialog::report_title`.
fn report_summary(report: &RecoveryReport) -> String {
    let steps = report.incomplete_steps.len();
    let partials = report.orphaned_partials.len();
    let mut parts = Vec::new();
    if steps > 0 {
        parts.push(format!("{steps} step{} interrupted", plural(steps)));
    }
    if partials > 0 {
        parts.push(format!("{partials} orphaned partial{}", plural(partials)));
    }
    let detail = if parts.is_empty() {
        // Unreachable through the UI (`Workspace::new`'s own startup scan
        // filters a report out unless one of the two counts above is
        // non-zero), but this still has to say something honest rather
        // than an empty string, same reasoning `job_report_dialog::
        // report_title`'s own "nothing to report" fallback documents.
        "nothing outstanding".to_string()
    } else {
        parts.join(", ")
    };
    format!(
        "{} (job #{}) \u{2014} {detail}",
        describe_kind(report.kind),
        report.job_id.0
    )
}

/// A short, human-readable line for one still-dangling step -- shown only
/// inside the inline "inspect" expansion. Every [`Step`] variant is matched
/// explicitly (no wildcard arm), same "a future variant fails to compile
/// here rather than silently getting a placeholder" convention `Step::
/// kind`/`Step::depends_on` themselves already establish.
fn describe_incomplete_step(step: &Step) -> String {
    match step {
        Step::CreateDir { dest, .. } => format!("Create directory {}", path_str(dest)),
        Step::CopyFile { dest, .. } => format!("Copy to {}", path_str(dest)),
        Step::Reflink { dest, .. } => format!("Reflink to {}", path_str(dest)),
        Step::Rename { dest, .. } => format!("Rename to {}", path_str(dest)),
        Step::Link { dest, .. } => format!("Hardlink to {}", path_str(dest)),
        Step::Symlink { link_path, .. } => format!("Create symlink {}", path_str(link_path)),
        Step::SetMeta { target, .. } => format!("Set attributes on {}", path_str(target)),
        Step::Remove { target, .. } => format!("Remove {}", path_str(target)),
        Step::Verify { dest, .. } => format!("Verify {}", path_str(dest)),
    }
}

/// The plain absolute path, not `VPath`'s `file:///...` `Display` form --
/// same choice `job_report_dialog::error_path`/`crate::link_dialog` already
/// make for a line a user is scanning for a filename.
fn path_str(p: &VPath) -> &str {
    p.inner().as_str()
}

impl Render for RecoveryDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = format!(
            "{} interrupted operation{} \u{2014} review",
            self.reports.len(),
            plural(self.reports.len())
        );

        let rows: Vec<_> = self
            .reports
            .iter()
            .enumerate()
            .map(|(ix, report)| {
                render_report_row(
                    report,
                    ix == self.cursor,
                    ix == self.cursor && self.inspecting,
                    tokens,
                )
            })
            .collect();

        let hint = if self.busy {
            "Working\u{2026}".to_string()
        } else {
            "Up/Down select \u{2022} Enter resume \u{2022} D discard \u{2022} Space inspect \
             \u{2022} Esc close (reappears next launch)"
                .to_string()
        };

        div()
            .id("recovery-dialog")
            .key_context("RecoveryDialog")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .id("recovery-dialog-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(hint),
            )
            .on_action(cx.listener(|this, _: &CloseRecoveryDialog, window, cx| {
                this.close(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &RecoveryDialogCursorUp, _window, cx| {
                    this.cursor_up(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &RecoveryDialogCursorDown, _window, cx| {
                    this.cursor_down(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &RecoveryDialogToggleInspect, _window, cx| {
                    this.toggle_inspect(cx);
                }),
            )
            .on_action(cx.listener(|this, _: &RecoveryDialogResume, window, cx| {
                this.resume_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RecoveryDialogDiscard, window, cx| {
                this.discard_selected(window, cx);
            }))
    }
}

/// One report's row: the collapsed summary, plus (only for the cursor row,
/// only while [`RecoveryDialogState::inspecting`]) the expanded detail. A
/// free function, not a method -- it only ever reads already-gathered data,
/// the same "no reason for this to be a method" shape `crate::
/// operation_manager::render_job_row` already establishes for its own
/// per-row rendering.
fn render_report_row(
    report: &RecoveryReport,
    selected: bool,
    inspecting: bool,
    tokens: &TokenPalette,
) -> impl IntoElement {
    div()
        .id(("recovery-dialog-row", report.job_id.0 as usize))
        .flex()
        .flex_col()
        .gap_px()
        .px_2()
        .py_1()
        .when(selected, |this| this.bg(tokens.color.selection_bg))
        .child(
            div()
                .text_size(px(12.))
                .min_w(px(0.))
                .child(report_summary(report)),
        )
        .when(inspecting, |this| {
            this.child(render_inspect_detail(report, tokens))
        })
}

/// The cursor row's expanded detail: "`N` of `M` steps remaining", plus one
/// line per still-dangling step.
fn render_inspect_detail(report: &RecoveryReport, tokens: &TokenPalette) -> impl IntoElement {
    let step_lines: Vec<_> = report
        .incomplete_steps
        .iter()
        .filter_map(|&ix| report.plan.steps.get(ix as usize).map(|step| (ix, step)))
        .map(|(ix, step)| {
            div()
                .text_size(px(11.))
                .min_w(px(0.))
                .text_color(tokens.color.statusbar_fg)
                .child(format!("step {ix}: {}", describe_incomplete_step(step)))
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .gap_px()
        .pl_2()
        .child(
            div()
                .text_size(px(11.))
                .text_color(tokens.color.statusbar_fg)
                .child(format!(
                    "{} of {} steps remaining",
                    report.incomplete_steps.len(),
                    report.plan.steps.len()
                )),
        )
        .children(step_lines)
}

#[cfg(test)]
mod tests {
    use duet_ops::{JobId, JobKind, Plan, PlanOptions};
    use duet_types::{MountId, UnixPathBuf, VPath};

    use super::*;

    fn vpath(p: &str) -> VPath {
        VPath::new(MountId::local(), UnixPathBuf::new(p).unwrap())
    }

    fn create_dir(path: &str) -> Step {
        Step::CreateDir {
            dest: vpath(path),
            mode: None,
        }
    }

    fn copy_file(name: &str) -> Step {
        Step::CopyFile {
            source: vpath(&format!("/src/{name}")),
            dest: vpath(&format!("/dst/{name}")),
            size: 10,
            conflict: None,
        }
    }

    fn report(kind: JobKind, incomplete: Vec<u32>, partials: Vec<(u32, String)>) -> RecoveryReport {
        RecoveryReport {
            job_id: JobId(1),
            kind,
            plan: Plan::new(
                vec![create_dir("/dst"), copy_file("a.txt")],
                PlanOptions::default(),
            ),
            incomplete_steps: incomplete,
            orphaned_partials: partials,
            last_outcome: None,
        }
    }

    // -- report_summary ------------------------------------------------------

    #[test]
    fn report_summary_names_the_kind_job_id_and_both_counts() {
        let r = report(
            JobKind::Copy,
            vec![1],
            vec![(1, ".duet-partial-x".to_string())],
        );
        assert_eq!(
            report_summary(&r),
            "Copy (job #1) \u{2014} 1 step interrupted, 1 orphaned partial"
        );
    }

    #[test]
    fn report_summary_pluralises_beyond_one() {
        let r = report(
            JobKind::Move,
            vec![0, 1],
            vec![
                (0, ".duet-partial-a".to_string()),
                (1, ".duet-partial-b".to_string()),
            ],
        );
        let text = report_summary(&r);
        assert!(text.starts_with("Move (job #1)"), "{text}");
        assert!(text.contains("2 steps interrupted"), "{text}");
        assert!(text.contains("2 orphaned partials"), "{text}");
    }

    #[test]
    fn report_summary_omits_the_partials_half_when_there_are_none() {
        let r = report(JobKind::CreateDir, vec![0], Vec::new());
        assert_eq!(
            report_summary(&r),
            "Create directory (job #1) \u{2014} 1 step interrupted"
        );
    }

    // -- describe_incomplete_step ---------------------------------------------

    #[test]
    fn describe_incomplete_step_names_the_destination_for_a_copy() {
        let step = copy_file("a.txt");
        assert_eq!(describe_incomplete_step(&step), "Copy to /dst/a.txt");
    }

    #[test]
    fn describe_incomplete_step_names_the_target_for_a_create_dir() {
        let step = create_dir("/dst");
        assert_eq!(describe_incomplete_step(&step), "Create directory /dst");
    }

    // -- plural ----------------------------------------------------------------

    #[test]
    fn plural_is_empty_only_for_exactly_one() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
