// SPDX-License-Identifier: MIT
//! T-5.2.4's error/skip report (`docs/commands.md`'s
//! `ops.queue.show_errors` and `ops.queue.retry_failed`): the one
//! actionable list a finished job's [`duet_ops::JobReport`] becomes,
//! instead of the fifty modal dialogs this task's own AC exists to
//! prevent -- "a job with 50 permission errors ends with an actionable
//! list, not 50 dialogs."
//!
//! # What this module owns, and what it deliberately doesn't
//!
//! The *producing* half has been real and tested since T-5.1.3:
//! `duet_ops::JobReport` already carries every `StepFailure` and
//! `SkipEntry` a job accumulated, and `JobState::Terminal` makes carrying
//! one a type-level fact rather than a convention. The *re-planning* half
//! is `duet_ops::plan_from_report` (T-5.2.4's `duet-ops` side). This
//! module is only the view on top: a read-only list, and one key that
//! re-enqueues the rebuilt plan.
//!
//! # Keybindings: this codebase's own defaults, not verified TC chords
//!
//! `docs/commands.md` catalogues both commands, but neither has a
//! keybinding anywhere in this repo -- `docs/keymap-tc.csv` (the TC
//! survey, and the only source of "known" bindings here) has no row for
//! either, and Total Commander's own transfer manager has no comparable
//! itemized report to have bound a chord for. So these are disclosed
//! defaults, the same pattern `crate::link_dialog`'s `Ctrl+Shift+S`/
//! `Ctrl+Shift+H` already establishes:
//!
//! - **Enter** (and **O**, for "open") on a row in the operation manager
//!   opens that job's report -- Enter because "view details on the
//!   selected row" is a near-universal list convention, and this crate's
//!   own `FileTable` already binds Enter to exactly that shape of "act on
//!   the cursor row." Declared in `crate::operation_manager` (the
//!   overlay the action fires *from*), not here.
//! - **R** re-runs the failed and skipped items, in this dialog's own
//!   `"JobReport"` key context -- so it never competes with the operation
//!   manager's own bare `R` (resume), which lives in the
//!   `"OperationManager"` context and is not in this dialog's dispatch
//!   path. **Enter** is bound to the same action, matching every other
//!   dialog in this crate where Enter means "do the thing."
//! - **Escape** closes, like every sibling overlay.
//!
//! Both chords were confirmed unclaimed against every other
//! `KeyBinding::new` call site in this crate.
//!
//! # A snapshot, not a live handle
//!
//! [`JobReportDialogState`] clones the job's `kind`, `plan`, and `report`
//! out of the `Job` at open time rather than holding a `JobId` and
//! re-reading `QueueManager::job` each render. That is safe here in a way
//! it would not be for an in-progress job: this dialog only ever opens on
//! a `JobState::Terminal` job, and a terminal state never changes again
//! (`QueueManager` has no transition out of `Terminal`). The snapshot buys
//! a render path that touches no lock and a re-run that cannot race
//! against the queue mutating the plan underneath it.
//!
//! # Overlay architecture
//!
//! Mirrors `crate::delete_dialog::DeleteDialogState`, not
//! `CopyMoveDialogState`: nothing here is user-typed, so this view owns
//! its own `FocusHandle` and `.track_focus`es its render root rather than
//! parking focus in a `duet_widgets::input::InputState` delegate. With no
//! `InputState` in the tree, Enter/Escape need plain `KeyBinding`s in this
//! dialog's own key context ([`bind_job_report_dialog_keys`]) rather than
//! the bubbled-action catching the `InputState`-holding dialogs rely on.
//!
//! The list itself is plain `div` rows inside an `.overflow_y_scroll()`
//! container, deliberately *not* a `duet_widgets::list::List`/
//! `ListDelegate`: this report has no per-row interaction at all -- no
//! selection, no navigation target, nothing to confirm on a row -- so the
//! `ListDelegate` machinery `crate::hotlist`/`crate::command_palette` use
//! for a genuinely interactive list would be pure complexity here. The
//! bounded-height, scrolling rows follow `crate::operation_manager`'s own
//! job-list container, the closest precedent in this crate for "a list of
//! rows inside an overlay card."
//!
//! # Disclosed judgment call: opening the report *replaces* the manager
//!
//! `Workspace::open_job_report_dialog` closes the operation manager as it
//! opens this dialog -- a "drill down, replacing the view" feel rather
//! than a stacked one. Either is defensible and this task's AC doesn't
//! specify; two concrete things decided it. First, every overlay in this
//! crate paints its own full-window `hsla(0, 0, 0, 0.5)` backdrop, so two
//! at once would darken the workspace to 75% -- a visible artifact, not a
//! neutral stack. Second, the operation manager's card carries
//! `.on_mouse_down_out`, which is a window-wide capture-phase check rather
//! than a bounds test: a click anywhere on *this* dialog would close the
//! manager underneath it anyway, leaving this dialog's saved
//! previous-focus handle pointing at an element that no longer exists --
//! exactly the "overlay still rendered but no longer the thing anything
//! is talking to" failure `workspace::command_palette_overlay`'s own doc
//! comment records from UAT. Closing the manager deliberately, up front,
//! makes that unrepresentable; Escape here restores focus to the panel
//! Ctrl+O was pressed in, and Ctrl+O reopens the list.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{
    ConflictResolver, JobKind, JobReport, Plan, QueueManager, SkipEntry, StepFailure,
    plan_from_report,
};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, actions, div, px,
};

use crate::dialog_job::{report_job_outcome, spawn_plan_and_enqueue};
use crate::operation_manager::describe_kind;
use crate::workspace::{NoticeLevel, Workspace};

// This dialog's own two actions. See the module doc comment for why both
// chords are this codebase's own disclosed defaults rather than verified
// TC bindings, and why bare `R` here can never collide with the operation
// manager's own bare `R` (resume).
actions!(duet_job_report_dialog, [RerunFailedItems, CloseJobReport]);

/// Registers this dialog's own keybindings, scoped to `"JobReport"` (set
/// on [`JobReportDialogState::render`]'s own root `div`). Called once from
/// `workspace::run` (and from that module's own `with_workspace` test
/// harness), alongside every other `bind_*_keys` function.
pub(crate) fn bind_job_report_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("r", RerunFailedItems, Some("JobReport")),
        KeyBinding::new("enter", RerunFailedItems, Some("JobReport")),
        KeyBinding::new("escape", CloseJobReport, Some("JobReport")),
    ]);
}

/// T-5.2.4's error/skip report view. See the module doc comment for the
/// full architecture; `Workspace::open_job_report_dialog` is the only
/// constructor call site, and it has already established that the job is
/// terminal and that its report has something worth showing.
pub(crate) struct JobReportDialogState {
    /// The finished job's own kind, so the re-run enqueues the same kind
    /// of job the user originally asked for (a retried copy is still a
    /// copy) and the title can name it.
    kind: JobKind,
    /// The finished job's whole plan -- the step list
    /// `duet_ops::plan_from_report` clones the redo set out of. See the
    /// module doc comment on why this is a snapshot.
    plan: Plan,
    report: JobReport,
    focus_handle: FocusHandle,
    /// Guards against a double-Enter re-entering [`Self::rerun`] while a
    /// previous enqueue is still in flight -- same reasoning as
    /// `DeleteDialogState::planning_in_progress`.
    rerun_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    /// `duet_config::paths::duet_state_dir()`'s result, captured at open
    /// time -- `None` under the same rare XDG-resolution failure every
    /// sibling dialog already tolerates. [`Self::rerun`] refuses to
    /// enqueue (with a toast, dialog left open) rather than guessing a
    /// location for the retry job's crash-safety journal.
    state_dir: Option<PathBuf>,
    /// The live, interactive resolver (T-5.2.3) -- passed to the retry
    /// job, deliberately, rather than `None`. A skipped step being retried
    /// very plausibly hits the identical conflict again, and the
    /// interactive resolver is exactly what lets the user make a different
    /// call this time instead of silently re-skipping through
    /// `PlanOptions::default_conflict`.
    conflict_resolver: Arc<dyn ConflictResolver>,
}

impl JobReportDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: JobKind,
        plan: Plan,
        report: JobReport,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Same as `DeleteDialogState::new`: every path that opens this
        // dialog ends with a live `Window`, so focus is taken immediately
        // rather than deferred to the next render.
        window.focus(&focus_handle);
        Self {
            kind,
            plan,
            report,
            focus_handle,
            rerun_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
        }
    }

    /// Test-only accessors -- same reasoning as `DeleteDialogState`'s own
    /// `#[cfg(test)]` block: `workspace.rs`'s end-to-end tests need to read
    /// this otherwise-private state without a public API surface
    /// production code would never use.
    #[cfg(test)]
    pub(crate) fn title_text(&self) -> String {
        report_title(self.kind, &self.report)
    }

    #[cfg(test)]
    pub(crate) fn error_lines(&self) -> Vec<String> {
        self.report
            .errors
            .iter()
            .map(|failure| format!("{} {}", error_path(failure), error_detail(failure)))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn skip_lines(&self) -> Vec<String> {
        self.report
            .skipped
            .iter()
            .map(|entry| format!("{} {}", skip_path(entry), entry.reason))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> JobKind {
        self.kind
    }

    /// Escape: close without re-running anything, mirroring
    /// `DeleteDialogState::cancel`.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_job_report_dialog(window, cx);
        });
    }

    /// `R`/Enter: rebuilds the failed-and-skipped subset of this job's plan
    /// (`duet_ops::plan_from_report`) and enqueues it as a fresh job of the
    /// same kind, off the UI thread through the shared
    /// [`spawn_plan_and_enqueue`]. The rebuilt plan is already in hand and
    /// needs no planner run at all, so the closure simply hands it back as
    /// `Ok(...)` -- the same shape `crate::link_dialog`'s own
    /// `LinkKind::Symlink` arm uses for the equally synchronous,
    /// infallible `plan_symlink`.
    ///
    /// On success the dialog closes immediately without waiting for the
    /// retry job itself (this app's own "dialog closes, operation proceeds
    /// in background" convention); on failure it stays open with the error
    /// surfaced as a toast.
    fn rerun(&mut self, cx: &mut Context<Self>) {
        if self.rerun_in_progress || !has_anything_to_report(&self.report) {
            return;
        }
        let Some(state_dir) = self.state_dir.clone() else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    "Can't re-run the operation: no writable state directory found \
                     (is $HOME/$XDG_STATE_HOME set?)."
                        .to_string(),
                    cx,
                );
            });
            return;
        };

        self.rerun_in_progress = true;

        let plan = plan_from_report(&self.plan, &self.report);
        let rx = spawn_plan_and_enqueue(
            &self.tokio_handle,
            self.kind,
            self.queue.clone(),
            state_dir,
            self.conflict_resolver.clone(),
            move |_fs| async move { Ok(plan) },
        );
        let workspace = self.workspace.clone();
        let this_entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            let outcome = rx.await;
            let _ = this_entity.update(cx, |this, cx| {
                this.rerun_in_progress = false;
                cx.notify();
            });
            if report_job_outcome(outcome, &workspace, cx) {
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.close_job_report_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }
}

impl Focusable for JobReportDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Whether `report` has anything this view exists to show -- the same
/// condition `docs/commands.md` spells as the `job.has_errors` context
/// predicate for both `ops.queue.show_errors` and
/// `ops.queue.retry_failed`. Skips count: `docs/commands.md`'s own title
/// for the retry command is "re-run the failed/**skipped** items."
pub(crate) fn has_anything_to_report(report: &JobReport) -> bool {
    !report.errors.is_empty() || !report.skipped.is_empty()
}

/// The dialog's headline: what kind of job this was, and how much went
/// wrong. Pluralised the same way `delete_dialog::delete_title` does its
/// own counts. A free function so it's unit-testable without a GPUI
/// harness.
fn report_title(kind: JobKind, report: &JobReport) -> String {
    let errors = report.errors.len();
    let skipped = report.skipped.len();
    let counts = match (errors, skipped) {
        (0, 0) => "nothing to report".to_string(),
        (0, s) => format!("{s} skipped"),
        (e, 0) => format!("{e} error{}", plural(e)),
        (e, s) => format!("{e} error{}, {s} skipped", plural(e)),
    };
    format!("{} \u{2014} {counts}", describe_kind(kind))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// An error row's first line: the plain absolute path the failure happened
/// at, or `"(no path)"` for the failures that genuinely have none (a
/// journal write failure, for instance, isn't *about* any one path).
///
/// `VPath::inner().as_str()`, not `VPath`'s `Display` -- the latter renders
/// the `file:///...` URI form, which is right for error messages and wrong
/// for a list a user is scanning for filenames. Same choice
/// `crate::link_dialog` already documents for its own detail line.
fn error_path(failure: &StepFailure) -> String {
    match &failure.path {
        Some(path) => path.inner().as_str().to_string(),
        None => "(no path)".to_string(),
    }
}

/// An error row's second line: the classified [`duet_types::ErrorKind`]
/// (its `thiserror` `Display`, e.g. "permission denied") plus the
/// executor's own message. Both, not one or the other: the kind is what
/// makes fifty rows scannable as *one* problem, the message is what makes
/// any single row actionable.
fn error_detail(failure: &StepFailure) -> String {
    format!("{} \u{2014} {}", failure.kind, failure.message)
}

/// A skip row's first line -- a `SkipEntry`'s path is non-optional, unlike
/// a `StepFailure`'s.
fn skip_path(entry: &SkipEntry) -> String {
    entry.path.inner().as_str().to_string()
}

impl Render for JobReportDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = report_title(self.kind, &self.report);

        let error_rows: Vec<_> = self
            .report
            .errors
            .iter()
            .map(|failure| {
                report_row(
                    error_path(failure),
                    error_detail(failure),
                    tokens.color.error,
                    tokens.color.statusbar_fg,
                )
            })
            .collect();
        let skip_rows: Vec<_> = self
            .report
            .skipped
            .iter()
            .map(|entry| {
                report_row(
                    skip_path(entry),
                    entry.reason.clone(),
                    tokens.color.warning,
                    tokens.color.statusbar_fg,
                )
            })
            .collect();

        div()
            .id("job-report-dialog")
            .key_context("JobReport")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .id("job-report-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(320.))
                    // The AC's whole point: fifty failures are fifty rows
                    // in one scrollable list, never fifty dialogs. The
                    // bound plus scrolling is what keeps the card a fixed
                    // size no matter how badly the job went.
                    .overflow_y_scroll()
                    .when(!error_rows.is_empty(), |this| {
                        this.child(section_heading("Errors", tokens.color.statusbar_fg))
                    })
                    .children(error_rows)
                    .when(!skip_rows.is_empty(), |this| {
                        this.child(section_heading("Skipped", tokens.color.statusbar_fg))
                    })
                    .children(skip_rows),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("Enter/R to re-run the failed and skipped items, Esc to close"),
            )
            .on_action(cx.listener(|this, _: &RerunFailedItems, _window, cx| this.rerun(cx)))
            .on_action(cx.listener(|this, _: &CloseJobReport, window, cx| this.cancel(window, cx)))
    }
}

fn section_heading(text: &'static str, color: gpui::Hsla) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(text)
}

/// One report row: the path on top, the reason underneath. A free function
/// (not a method) for the same reason `operation_manager::render_job_row`
/// is one -- it only ever reads already-gathered data.
///
/// `min_w(0)` on both lines for the same reason `delete_dialog.rs`'s
/// warning line carries it: a long, unbroken path would otherwise widen
/// this flex child past the card instead of wrapping inside it.
fn report_row(
    path: String,
    detail: String,
    path_color: gpui::Hsla,
    detail_color: gpui::Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_px()
        .px_2()
        .py_1()
        .child(
            div()
                .text_size(px(12.))
                .min_w(px(0.))
                .text_color(path_color)
                .child(path),
        )
        .child(
            div()
                .text_size(px(11.))
                .min_w(px(0.))
                .text_color(detail_color)
                .child(detail),
        )
}

#[cfg(test)]
mod tests {
    use duet_ops::{JobReport, SkipEntry, StepFailure};
    use duet_types::{ErrorKind, UnixPathBuf, VPath};

    use super::*;

    fn vpath(p: &str) -> VPath {
        VPath::local(UnixPathBuf::new(p).unwrap())
    }

    fn failure(path: Option<&str>, kind: ErrorKind, message: &str) -> StepFailure {
        StepFailure {
            step_index: 0,
            path: path.map(vpath),
            kind,
            message: message.to_string(),
        }
    }

    fn skip(path: &str, reason: &str) -> SkipEntry {
        SkipEntry {
            step_index: 0,
            path: vpath(path),
            reason: reason.to_string(),
        }
    }

    // -- report_title ---------------------------------------------------------

    #[test]
    fn report_title_names_the_kind_and_both_counts() {
        let report = JobReport {
            errors: vec![
                failure(Some("/a"), ErrorKind::Permission, "denied"),
                failure(Some("/b"), ErrorKind::Permission, "denied"),
            ],
            skipped: vec![skip("/c", "already there")],
            ..Default::default()
        };
        assert_eq!(
            report_title(JobKind::Copy, &report),
            "Copy \u{2014} 2 errors, 1 skipped"
        );
    }

    #[test]
    fn report_title_singularises_exactly_one_error() {
        let report = JobReport {
            errors: vec![failure(Some("/a"), ErrorKind::Permission, "denied")],
            ..Default::default()
        };
        assert_eq!(
            report_title(JobKind::Delete { permanent: true }, &report),
            "Delete (permanent) \u{2014} 1 error"
        );
    }

    #[test]
    fn report_title_omits_the_error_half_when_there_are_only_skips() {
        let report = JobReport {
            skipped: vec![skip("/c", "already there"), skip("/d", "already there")],
            ..Default::default()
        };
        assert_eq!(
            report_title(JobKind::Move, &report),
            "Move \u{2014} 2 skipped"
        );
    }

    /// Unreachable through the UI (the operation manager refuses to open a
    /// report with nothing in it), but the title function still has to say
    /// something honest rather than "0 errors, 0 skipped".
    #[test]
    fn report_title_degrades_gracefully_for_an_empty_report() {
        assert_eq!(
            report_title(JobKind::Copy, &JobReport::default()),
            "Copy \u{2014} nothing to report"
        );
    }

    // -- has_anything_to_report -------------------------------------------------

    #[test]
    fn has_anything_to_report_is_false_only_for_a_genuinely_empty_report() {
        assert!(!has_anything_to_report(&JobReport::default()));
        assert!(has_anything_to_report(&JobReport {
            errors: vec![failure(None, ErrorKind::Fatal, "boom")],
            ..Default::default()
        }));
        assert!(has_anything_to_report(&JobReport {
            skipped: vec![skip("/c", "already there")],
            ..Default::default()
        }));
    }

    // -- row text ----------------------------------------------------------------

    #[test]
    fn error_path_renders_a_plain_absolute_path_not_a_file_uri() {
        let f = failure(Some("/tmp/locked/a.txt"), ErrorKind::Permission, "EACCES");
        assert_eq!(error_path(&f), "/tmp/locked/a.txt");
    }

    #[test]
    fn error_path_falls_back_for_a_failure_with_no_path_at_all() {
        let f = failure(None, ErrorKind::Fatal, "failed to journal JobStarted");
        assert_eq!(error_path(&f), "(no path)");
    }

    #[test]
    fn error_detail_carries_both_the_classification_and_the_message() {
        let f = failure(Some("/a"), ErrorKind::Permission, "EACCES on unlinkat");
        assert_eq!(
            error_detail(&f),
            "permission denied \u{2014} EACCES on unlinkat"
        );
    }

    #[test]
    fn skip_path_renders_the_plain_path() {
        assert_eq!(skip_path(&skip("/dst/a.txt", "exists")), "/dst/a.txt");
    }
}
