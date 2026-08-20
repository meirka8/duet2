// SPDX-License-Identifier: MIT
//! T-5.2.2's expandable operation manager overlay (FR-OPS-02/03,
//! design.md §9.3: "a tray in the status bar showing aggregate progress,
//! expanding to a full manager with per-job pause/resume/cancel/reorder/
//! priority" -- this module is the "expanding to a full manager" half;
//! `Workspace::status_bar_row`'s own tray element is the compact half,
//! and [`tray_summary`] below is the one aggregation function both the
//! tray and (indirectly, via each row) this manager draw from, so the two
//! views can never disagree about what the queue is doing.
//!
//! # Overlay architecture
//!
//! Mirrors `crate::copy_move_dialog`'s `CopyMoveDialogState`/
//! `Workspace::copy_move_dialog` shape almost exactly (see that module's
//! own doc comment for the full reasoning) -- another small, hand-rolled
//! `Render`-implementing view rather than a `duet_widgets::list::List`
//! wrapper: the manager's rows carry per-row *controls* (Pause/Resume/
//! Cancel, conditional on job state), not just a selectable label, which
//! has no ready-made upstream shape to slot into any more than the copy/
//! move dialog's own destination-field-plus-options body did.
//! `Workspace` owns `Option<Entity<OperationManagerState>>` plus
//! `operation_manager_previous_focus: Option<FocusHandle>`, open/close
//! methods mirror `open_hotlist_for_panel`/`close_hotlist` exactly (no-op
//! if already open, restore focus on close), and the overlay is rendered
//! at the tail of `Workspace::render` via the same `.when_some(...)`
//! chain the hotlist/palette/copy-move-dialog overlays already use.
//! `workspace::operation_manager_overlay` builds the backdrop/card chrome
//! -- `.occlude()` on both layers, `.on_mouse_down_out` on the card --
//! identical to every other overlay in this crate; see
//! `workspace::hotlist_overlay`'s own doc comment for the real regression
//! that pattern exists to avoid (a click falling through to the panel
//! underneath while the overlay stayed rendered but no longer the thing
//! anything was actually talking to).
//!
//! Unlike the copy/move dialog, this view never mutates the queue engine
//! itself: [`OperationManagerState`] holds an `Arc<QueueManager>` (handed
//! in at open time, same convention `CopyMoveDialogState` already
//! established) purely to call its already-synchronous, already-`&self`
//! `pause`/`resume`/`cancel` -- no off-thread dispatch needed, unlike the
//! copy/move dialog's own `plan_copy`/`enqueue` calls, which do real I/O.
//! The one thing this view cannot get from `QueueManager` itself --
//! byte-level [`duet_ops::ProgressSnapshot`] samples, which live only in
//! `Workspace::job_progress` (the UI-side cache T-5.2.2 adds; see that
//! field's own doc comment for why it exists at all) -- is reached
//! through a `WeakEntity<Workspace>` and a small `pub(crate)` accessor
//! (`Workspace::job_progress_snapshot`), the same "workspace owns UI-only
//! state, the overlay reaches it through a weak handle" shape
//! `CopyMoveDialogState::try_complete_destination`/`completion_candidate`
//! already established for the destination-completion feature.
//!
//! # Keyboard navigation within the list
//!
//! FR-OPS-02/03 and this codebase's own TC-faithful, keyboard-first
//! design mean per-job Pause/Resume/Cancel need a real, keyboard-only way
//! to say *which* row they apply to -- there is no mouse-only escape
//! hatch anywhere else in this app's overlays. The scheme: a single
//! `cursor: usize` index into the manager's own, deterministically
//! sorted job list ([`sorted_jobs`] -- `QueueManager::snapshot`'s own
//! `Vec<Job>` comes back in undefined `HashMap`-iteration order, so
//! *something* has to impose a stable order before "row 2" means anything
//! from one render to the next). Plain `Up`/`Down` move the cursor (no
//! modifier -- this overlay has no text field anywhere in it to compete
//! with, the same reasoning `docs/keymap-tc.csv`'s survey already gives
//! for `duet_widgets::list::List`'s own bare-arrow-key `SelectUp`/
//! `SelectDown` bindings), and bare `P`/`R`/`C` act on whatever job is
//! currently at the cursor's row.
//!
//! This is a deliberately simple design, not an oversight: `cursor` is a
//! plain row index, re-clamped to the current job count on every render,
//! rather than tracking a `JobId` and re-deriving its row each time. The
//! tradeoff this accepts -- if a job's state change (e.g. it finishes)
//! reorders the sorted list between one keypress and the next, `P`/`R`/
//! `C` acts on whatever is *now* at that row index, not necessarily the
//! job the user was originally looking at -- is acceptable specifically
//! because every one of those three actions already has to tolerate
//! racing against a job that changed state a moment ago
//! (`QueueManager::pause`/`resume`/`cancel`'s own documented
//! `Err(QueueError::NotRunning)`/no-op behaviour for a job no longer in
//! the expected state) -- "acting on the wrong-but-still-valid row during
//! a fast reorder" and "acting on the right row a moment after it
//! changed state" are the same class of race, both already handled the
//! same harmless way ([`OperationManagerState::act_on_selected`] ignores
//! the `Result`), so this doesn't introduce a new failure mode, just a
//! narrow, disclosed edge case in an already-bounded, small list.
//!
//! # Performance (FR-OPS-02/03's AC: "redraws do not cost more than 0.5
//! ms/frame")
//!
//! No literal frame-time measurement was performed for this task -- this
//! codebase has no reusable GPUI element-render profiling harness yet
//! (`documentation/spikes/S-1.md` is the closest precedent, and it is a
//! one-off spike script, not something this task can invoke as a test).
//! Disclosing that honestly rather than fabricating a benchmark, mirroring
//! `duet_ops::executor`'s own "Scope: what this module deliberately does
//! not do" convention. What *is* done, concretely, to make the AC
//! plausible by construction rather than by measurement:
//!
//! - **No per-render heap allocation beyond trivial formatting.** Every
//!   row's text is built with `format!`/`String` (unavoidable -- GPUI
//!   elements are text, not pre-shaped strings) but nothing here retains,
//!   caches, or re-parses anything; the job list itself
//!   (`QueueManager::snapshot`) is already `O(job count)`, and this
//!   module never duplicates or re-derives it into a second, parallel
//!   structure -- see the module doc comment's "this view never mutates
//!   the queue engine" section.
//! - **No `O(job count²)` work.** [`sorted_jobs`] is one `O(n log n)`
//!   sort of whatever `snapshot()` returned, run once per render; there
//!   is no per-row lookup that itself scans the whole list again (the
//!   per-row progress lookup, `Workspace::job_progress_snapshot`, is a
//!   single `HashMap::get`).
//! - **Nothing here re-derives, at render time, anything the 100 ms
//!   `Progress` sampler already computed.** `ProgressSnapshot`'s own
//!   fields (`bytes_done`, `throughput_bytes_per_sec`, `eta_secs`, ...)
//!   are read as-is and only ever formatted for display, never
//!   recomputed -- the whole point of `Workspace::job_progress` existing
//!   as a cache is that this module (and the tray) can be O(job count)
//!   per render instead of O(job count × sample history).
//!
//! With realistically small job counts (a handful of concurrent
//! operations, matching `COPY_MOVE_QUEUE_MAX_CONCURRENT`'s own
//! magnitude), this is comfortably cheap; the honest caveat is that
//! "comfortably cheap by construction" is not the same claim as "measured
//! at 0.5 ms," and this module does not pretend otherwise.
//!
//! # What this overlay deliberately does not do
//!
//! Mirroring `crate::copy_move_dialog`'s own disclosed-scope-cuts
//! section:
//!
//! - **No live per-conflict prompt** (T-5.2.3) -- `JobEvent::
//!   ConflictDetected` is only logged (`Workspace::new`'s event-consumer
//!   loop), never surfaced as an interactive prompt here.
//! - **No itemized error/skip list with "re-run failed"** (T-5.2.4) -- a
//!   `Failed`/`CompletedWithSkips` row shows a terminal summary count
//!   (`report.errors.len()`/`report.skipped.len()`) via [`progress_line`],
//!   nothing more.
//! - **No interrupted-operation-recovery-at-startup UI** (T-5.2.5).
//! - **No queue reordering UI.** `QueueManager::reorder` exists and is
//!   real, but "per-job controls" (this task's own AC wording) reads
//!   naturally as pause/resume/cancel -- what a TC-style background
//!   transfer manager needs day to day -- not drag-and-drop reordering,
//!   which this task's own AC does not separately call for.

use std::sync::Arc;

use duet_ops::{
    Job, JobId, JobKind, JobOutcome, JobState, ProgressSnapshot, QueueError, QueueManager,
};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, Styled as _, WeakEntity, Window, actions, div, px,
};

use crate::file_table::write_byte_count;
use crate::workspace::Workspace;

// T-5.2.2's own actions (FR-OPS-02/03). `CloseOperationManager` mirrors
// every other overlay's Escape convention; the cursor/control actions are
// this module's own reasonable, unclaimed-elsewhere bindings (no text
// field lives inside this overlay for a bare letter key to compete with
// -- see the module doc comment's "Keyboard navigation" section).
// `Workspace::OpenOperationManager` (`Ctrl+O`), which opens this overlay
// in the first place, is declared in `workspace.rs` alongside every other
// workspace-level open-an-overlay action (`OpenHotlist`,
// `OpenCommandPalette`, ...), not here -- this module only owns the
// actions that make sense *once the overlay is already open*, the same
// split `copy_move_dialog.rs`'s own actions (all overlay-internal) versus
// `workspace.rs`'s `CopyDialog`/`MoveDialog` (open-the-overlay) already
// establishes.
actions!(
    duet_operation_manager,
    [
        CloseOperationManager,
        OperationManagerCursorUp,
        OperationManagerCursorDown,
        OperationManagerPauseSelected,
        OperationManagerResumeSelected,
        OperationManagerCancelSelected,
    ]
);

/// Registers this overlay's own keybindings, scoped to
/// `"OperationManager"` (set on [`OperationManagerState::render`]'s own
/// root `div`). Called once from `workspace::run` (and from the test
/// module's own `with_workspace` harness), alongside every other
/// `bind_*_keys` function.
pub(crate) fn bind_operation_manager_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseOperationManager, Some("OperationManager")),
        KeyBinding::new("up", OperationManagerCursorUp, Some("OperationManager")),
        KeyBinding::new("down", OperationManagerCursorDown, Some("OperationManager")),
        KeyBinding::new("p", OperationManagerPauseSelected, Some("OperationManager")),
        KeyBinding::new(
            "r",
            OperationManagerResumeSelected,
            Some("OperationManager"),
        ),
        KeyBinding::new(
            "c",
            OperationManagerCancelSelected,
            Some("OperationManager"),
        ),
    ]);
}

/// T-5.2.2's operation manager: a live list of every job the queue knows
/// about, with per-row Pause/Resume/Cancel. See the module doc comment
/// for the full architecture and the keyboard-cursor design rationale.
pub(crate) struct OperationManagerState {
    workspace: WeakEntity<Workspace>,
    /// The same `Arc<QueueManager>` `Workspace` itself owns -- handed in
    /// once, at open time, by `Workspace::open_operation_manager`, same
    /// convention `CopyMoveDialogState::queue` already established. Used
    /// directly for `snapshot`/`pause`/`resume`/`cancel`; only the live
    /// `ProgressSnapshot` numbers (not in `Job`/`JobState` at all) need
    /// the separate `workspace` handle above -- see the module doc
    /// comment's "this view never mutates the queue engine" section for
    /// why these two data sources are kept deliberately separate.
    queue: Arc<QueueManager>,
    /// The selected row, as an index into [`sorted_jobs`]'s own output --
    /// see the module doc comment's "Keyboard navigation" section for why
    /// this is a plain, per-render-reclamped index rather than a tracked
    /// `JobId`.
    cursor: usize,
    focus_handle: FocusHandle,
}

impl OperationManagerState {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        queue: Arc<QueueManager>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspace,
            queue,
            cursor: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The manager's own display order -- see [`sorted_jobs`].
    fn jobs(&self) -> Vec<Job> {
        sorted_jobs(self.queue.snapshot())
    }

    /// A given job's latest progress sample, if any has landed yet --
    /// reached through `workspace` since `job_progress` lives on
    /// `Workspace`, not `QueueManager` (see the module doc comment).
    /// `None` if the workspace entity is somehow already gone (the
    /// overlay is about to be torn down) or no sample has landed for this
    /// job yet.
    fn progress_for(&self, id: JobId, cx: &App) -> Option<ProgressSnapshot> {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).job_progress_snapshot(id))
    }

    fn cursor_up(&mut self, cx: &mut Context<Self>) {
        self.cursor = self.cursor.saturating_sub(1);
        cx.notify();
    }

    fn cursor_down(&mut self, cx: &mut Context<Self>) {
        let len = self.jobs().len();
        if len > 0 {
            self.cursor = (self.cursor + 1).min(len - 1);
        }
        cx.notify();
    }

    /// The shared shape of Pause/Resume/Cancel: look up whichever job is
    /// currently at `cursor`'s row and call `op` on it, ignoring the
    /// `Result` -- `QueueManager::pause`/`resume`/`cancel` are all
    /// already documented to treat "this job isn't in the expected state
    /// any more" as a harmless no-op (a race against the job's own
    /// concurrent progress, not a caller error), so there is nothing
    /// meaningful for this UI to do with an `Err` beyond not crashing on
    /// it. `cx.notify()` unconditionally afterward -- even a no-op action
    /// might still be worth a redraw (e.g. the cursor row's controls hint
    /// depends on state that could have changed concurrently).
    fn act_on_selected(
        &mut self,
        op: fn(&QueueManager, JobId) -> Result<(), QueueError>,
        cx: &mut Context<Self>,
    ) {
        let jobs = self.jobs();
        if let Some(job) = jobs.get(self.cursor) {
            let _ = op(&self.queue, job.id);
        }
        cx.notify();
    }

    fn pause_selected(&mut self, cx: &mut Context<Self>) {
        self.act_on_selected(QueueManager::pause, cx);
    }

    fn resume_selected(&mut self, cx: &mut Context<Self>) {
        self.act_on_selected(QueueManager::resume, cx);
    }

    fn cancel_selected(&mut self, cx: &mut Context<Self>) {
        self.act_on_selected(QueueManager::cancel, cx);
    }

    /// Escape: close without acting on anything, mirroring
    /// `CopyMoveDialogState::cancel`/`HotlistDelegate::cancel` exactly.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_operation_manager(window, cx);
        });
    }

    /// Test-only: exposes the manager's own display-ordered job list and
    /// cursor without reaching into otherwise-private fields -- same
    /// reasoning as `CopyMoveDialogState`'s own `#[cfg(test)]` accessors.
    #[cfg(test)]
    pub(crate) fn jobs_for_test(&self) -> Vec<Job> {
        self.jobs()
    }

    #[cfg(test)]
    pub(crate) fn cursor_for_test(&self) -> usize {
        self.cursor
    }
}

impl Focusable for OperationManagerState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OperationManagerState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let jobs = self.jobs();
        self.cursor = if jobs.is_empty() {
            0
        } else {
            self.cursor.min(jobs.len() - 1)
        };

        let rows: Vec<_> = jobs
            .iter()
            .enumerate()
            .map(|(ix, job)| {
                let progress = self.progress_for(job.id, cx);
                render_job_row(job, progress, ix == self.cursor, tokens)
            })
            .collect();

        div()
            .id("operation-manager")
            .key_context("OperationManager")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child("Operations"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(320.))
                    .overflow_hidden()
                    .when(jobs.is_empty(), |this| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .text_color(tokens.color.statusbar_fg)
                                .child("No operations yet."),
                        )
                    })
                    .children(rows),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(
                        "Up/Down select \u{2022} P pause \u{2022} R resume \u{2022} \
                         C cancel \u{2022} Esc close",
                    ),
            )
            .on_action(cx.listener(|this, _: &CloseOperationManager, window, cx| {
                this.close(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &OperationManagerCursorUp, _window, cx| {
                    this.cursor_up(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &OperationManagerCursorDown, _window, cx| {
                    this.cursor_down(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &OperationManagerPauseSelected, _window, cx| {
                    this.pause_selected(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &OperationManagerResumeSelected, _window, cx| {
                    this.resume_selected(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &OperationManagerCancelSelected, _window, cx| {
                    this.cancel_selected(cx);
                }),
            )
    }
}

/// Orders `QueueManager::snapshot`'s jobs for display. `snapshot` itself
/// comes back in undefined `HashMap`-iteration order (see that method's
/// own implementation), so *something* has to impose a stable,
/// meaningful order before a row index means anything from one render to
/// the next -- see the module doc comment's "Keyboard navigation"
/// section. Still-active jobs (`JobState::is_active`) sort before
/// anything `Terminal`, and within each group the most recently enqueued
/// job sorts first (`JobId` is allocated in strictly increasing order by
/// `QueueManager::enqueue`, so a larger id is always younger) -- a
/// reasonable "what do I most likely want to look at" default for a
/// small, unfiltered list, not a verified TC convention (TC's own
/// transfer manager predates this exact UI shape).
///
/// A free function, not a method, so it is directly unit-testable against
/// hand-built `Job` values with no `QueueManager`/GPUI needed.
pub(crate) fn sorted_jobs(mut jobs: Vec<Job>) -> Vec<Job> {
    jobs.sort_by(|a, b| {
        a.state
            .is_terminal()
            .cmp(&b.state.is_terminal())
            .then_with(|| b.id.0.cmp(&a.id.0))
    });
    jobs
}

/// The status-bar tray's own aggregate line (`Workspace::
/// operations_tray_text`, FR-OPS-02's "tray... showing aggregate
/// progress"). `None` whenever nothing is active
/// (`JobState::is_active`) -- the AC's own word is "unobtrusive," and the
/// least obtrusive tray is one that renders nothing at all rather than a
/// "0 operations" placeholder. When active: the active job count, plus --
/// only once at least one `Running` job has a live `Progress` sample
/// (`job_progress`'s own doc comment: there is a real window between
/// `Started` and the first 100 ms sample) -- the summed throughput across
/// every sampled `Running` job and the *largest* known ETA (the job
/// furthest from done). Deliberately the simplest aggregate that is
/// still honest, matching this task's own "glanceable, not a dashboard"
/// framing: the count alone already tells the user "more than one," so
/// the one ETA worth showing is the one that keeps a promise it can
/// actually keep, not an average that overpromises on the slower job.
pub(crate) fn tray_summary(
    jobs: &[Job],
    progress: &std::collections::HashMap<JobId, ProgressSnapshot>,
) -> Option<String> {
    let active = jobs.iter().filter(|job| job.state.is_active()).count();
    if active == 0 {
        return None;
    }

    let mut throughput_total: u64 = 0;
    let mut max_eta: Option<u64> = None;
    for job in jobs {
        if !matches!(job.state, JobState::Running { .. }) {
            continue;
        }
        if let Some(snapshot) = progress.get(&job.id) {
            throughput_total = throughput_total.saturating_add(snapshot.throughput_bytes_per_sec);
            if let Some(eta) = snapshot.eta_secs {
                max_eta = Some(max_eta.map_or(eta, |current| current.max(eta)));
            }
        }
    }

    let mut text = format!("{active} operation{}", if active == 1 { "" } else { "s" });
    if throughput_total > 0 {
        let mut bytes = String::new();
        write_byte_count(&mut bytes, throughput_total);
        text.push_str(&format!(" \u{2014} {bytes}/s"));
    }
    if let Some(eta) = max_eta {
        text.push_str(&format!(" \u{2014} ETA {}", format_eta_secs(eta)));
    }
    Some(text)
}

/// `JobKind`'s own human-readable label -- covers every variant
/// explicitly (no wildcard arm) so a future `JobKind` addition fails to
/// compile here rather than silently falling back to a placeholder.
fn describe_kind(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Copy => "Copy",
        JobKind::Move => "Move",
        JobKind::Delete { permanent: true } => "Delete (permanent)",
        JobKind::Delete { permanent: false } => "Delete (trash)",
        JobKind::CreateDir => "Create directory",
        JobKind::CreateSymlink => "Create symlink",
        JobKind::CreateHardlink => "Create hardlink",
    }
}

/// `JobState`'s own human-readable label, ignoring the `Terminal`
/// variant's attached report (that's [`progress_line`]'s job).
fn describe_state(state: &JobState) -> &'static str {
    match state {
        JobState::Queued => "Queued",
        JobState::Planning => "Planning",
        JobState::Running { .. } => "Running",
        JobState::Paused { .. } => "Paused",
        JobState::Terminal { outcome, .. } => match outcome {
            JobOutcome::Completed => "Completed",
            JobOutcome::CompletedWithSkips => "Completed (with skips)",
            JobOutcome::Cancelled => "Cancelled",
            JobOutcome::Failed => "Failed",
        },
    }
}

/// Which of Pause/Resume/Cancel are meaningful for `state` -- the row's
/// own reminder of what `P`/`R`/`C` will do to it if it's the cursor row,
/// matching `QueueManager::pause`/`resume`/`cancel`'s own documented
/// state requirements exactly (`Running`-only pause, `Paused`-only
/// resume, cancel for anything not yet `Terminal`).
fn controls_hint(state: &JobState) -> &'static str {
    match state {
        JobState::Running { .. } => "P pause \u{2022} C cancel",
        JobState::Paused { .. } => "R resume \u{2022} C cancel",
        JobState::Queued | JobState::Planning => "C cancel",
        JobState::Terminal { .. } => "",
    }
}

/// The row's second line: live progress for `Running`/`Paused`, a short
/// status line for `Queued`/`Planning`, or the terminal summary for a
/// finished job -- the "3 errors"/"N skipped" counts T-5.2.4's own
/// itemized view will later expand on, not attempted here (see the
/// module doc comment's "What this overlay deliberately does not do").
fn progress_line(job: &Job, progress: Option<ProgressSnapshot>) -> String {
    match &job.state {
        JobState::Running { .. } | JobState::Paused { .. } => match progress {
            Some(sample) => {
                let totals = &job.plan.totals;
                let mut bytes_done = String::new();
                write_byte_count(&mut bytes_done, sample.bytes_done);
                let mut bytes_total = String::new();
                write_byte_count(&mut bytes_total, totals.bytes);
                let mut throughput = String::new();
                write_byte_count(&mut throughput, sample.throughput_bytes_per_sec);
                let eta = sample
                    .eta_secs
                    .map(format_eta_secs)
                    .unwrap_or_else(|| "--".to_string());
                format!(
                    "{}/{} files \u{2022} {bytes_done} of {bytes_total} \u{2022} \
                     {throughput}/s \u{2022} ETA {eta}",
                    sample.files_done, totals.files
                )
            }
            // The real window between `JobEvent::Started` and this job's
            // first 100 ms `Progress` sample -- see the module doc
            // comment. Nothing to synthesize for a sample that hasn't
            // happened yet.
            None => "No progress data yet.".to_string(),
        },
        JobState::Queued => "Waiting in queue.".to_string(),
        JobState::Planning => "Scanning source...".to_string(),
        JobState::Terminal { outcome, report } => match outcome {
            JobOutcome::Completed => {
                let mut bytes = String::new();
                write_byte_count(&mut bytes, report.bytes_completed);
                format!("{} file(s), {bytes}.", report.files_completed)
            }
            JobOutcome::CompletedWithSkips => format!(
                "{} file(s) completed, {} skipped.",
                report.files_completed,
                report.skipped.len()
            ),
            JobOutcome::Cancelled => "Cancelled.".to_string(),
            JobOutcome::Failed => format!("Failed \u{2014} {} error(s).", report.errors.len()),
        },
    }
}

/// `mm:ss`, or `h:mm:ss` once an hour is reached -- a plain, dependency-
/// free formatter; no existing formatter in this crate covers a duration
/// in seconds (`file_table::write_byte_count` is bytes, not time).
fn format_eta_secs(total_secs: u64) -> String {
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// One job's row: kind + state, the controls hint for whichever of
/// Pause/Resume/Cancel apply, and the live-or-terminal progress line.
/// Highlighted when it's the cursor row. A free function (not a method)
/// -- it only ever reads already-gathered data (`job`, `progress`,
/// `selected`, the current theme), the same "no reason for this to be a
/// method" shape `crate::hotlist::HotlistRow` already establishes for its
/// own per-row rendering.
fn render_job_row(
    job: &Job,
    progress: Option<ProgressSnapshot>,
    selected: bool,
    tokens: &TokenPalette,
) -> impl IntoElement {
    div()
        .id(("operation-manager-row", job.id.0 as usize))
        .flex()
        .flex_col()
        .gap_px()
        .px_2()
        .py_1()
        .when(selected, |this| this.bg(tokens.color.selection_bg))
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(12.))
                .child(format!(
                    "{} \u{2014} {}",
                    describe_kind(job.kind),
                    describe_state(&job.state)
                ))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(tokens.color.statusbar_fg)
                        .child(controls_hint(&job.state)),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(tokens.color.statusbar_fg)
                .child(progress_line(job, progress)),
        )
}

#[cfg(test)]
mod tests {
    use duet_ops::{Plan, PlanOptions, PlanTotals, SkipEntry, StepFailure};
    use duet_types::{ErrorKind, VPath};

    use super::*;

    fn plan_with_totals(files: u64, bytes: u64) -> Plan {
        Plan {
            steps: Vec::new(),
            totals: PlanTotals {
                dirs: 0,
                files,
                bytes,
                hardlinks_preserved: 0,
            },
            options: PlanOptions::default(),
        }
    }

    fn job(id: u64, kind: JobKind, state: JobState) -> Job {
        Job {
            id: JobId(id),
            kind,
            plan: plan_with_totals(10, 1024),
            state,
            priority: 0,
        }
    }

    fn running(step: u32) -> JobState {
        JobState::Running { current_step: step }
    }

    fn completed() -> JobState {
        JobState::Terminal {
            outcome: JobOutcome::Completed,
            report: duet_ops::JobReport::default(),
        }
    }

    fn snapshot(
        files_done: u64,
        bytes_done: u64,
        throughput: u64,
        eta: Option<u64>,
    ) -> ProgressSnapshot {
        ProgressSnapshot {
            files_done,
            bytes_done,
            current_file_bytes_done: 0,
            current_file_bytes_total: 0,
            throughput_bytes_per_sec: throughput,
            eta_secs: eta,
        }
    }

    // -- sorted_jobs -------------------------------------------------------

    #[test]
    fn sorted_jobs_puts_active_jobs_before_terminal_ones() {
        let jobs = vec![
            job(1, JobKind::Copy, completed()),
            job(2, JobKind::Move, running(0)),
        ];
        let sorted = sorted_jobs(jobs);
        assert_eq!(sorted[0].id, JobId(2), "the active job must sort first");
        assert_eq!(sorted[1].id, JobId(1));
    }

    #[test]
    fn sorted_jobs_orders_same_group_by_most_recently_enqueued_first() {
        let jobs = vec![
            job(1, JobKind::Copy, running(0)),
            job(3, JobKind::Copy, running(0)),
            job(2, JobKind::Copy, running(0)),
        ];
        let sorted = sorted_jobs(jobs);
        let ids: Vec<u64> = sorted.iter().map(|j| j.id.0).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    // -- tray_summary --------------------------------------------------------

    #[test]
    fn tray_summary_is_none_when_no_job_is_active() {
        let jobs = vec![job(1, JobKind::Copy, completed())];
        assert_eq!(tray_summary(&jobs, &std::collections::HashMap::new()), None);
    }

    #[test]
    fn tray_summary_is_none_for_an_empty_queue() {
        assert_eq!(tray_summary(&[], &std::collections::HashMap::new()), None);
    }

    #[test]
    fn tray_summary_shows_the_count_alone_before_any_progress_sample_lands() {
        let jobs = vec![job(1, JobKind::Copy, running(0))];
        let text = tray_summary(&jobs, &std::collections::HashMap::new()).unwrap();
        assert_eq!(text, "1 operation");
    }

    #[test]
    fn tray_summary_aggregates_throughput_and_takes_the_largest_eta() {
        let jobs = vec![
            job(1, JobKind::Copy, running(0)),
            job(2, JobKind::Move, running(0)),
        ];
        let mut progress = std::collections::HashMap::new();
        progress.insert(JobId(1), snapshot(1, 1024, 1024, Some(10)));
        progress.insert(JobId(2), snapshot(1, 1024, 2048, Some(90)));
        let text = tray_summary(&jobs, &progress).unwrap();
        assert!(text.starts_with("2 operations"));
        assert!(text.contains("3.0 KB/s"), "text was: {text}");
        assert!(text.contains("ETA 1:30"), "text was: {text}");
    }

    #[test]
    fn tray_summary_pluralizes_correctly_for_exactly_one_operation() {
        let jobs = vec![job(1, JobKind::Copy, JobState::Queued)];
        let text = tray_summary(&jobs, &std::collections::HashMap::new()).unwrap();
        assert_eq!(text, "1 operation");
    }

    // -- describe_kind / describe_state / controls_hint ---------------------

    #[test]
    fn describe_kind_covers_delete_permanent_and_trash_distinctly() {
        assert_eq!(
            describe_kind(JobKind::Delete { permanent: true }),
            "Delete (permanent)"
        );
        assert_eq!(
            describe_kind(JobKind::Delete { permanent: false }),
            "Delete (trash)"
        );
    }

    #[test]
    fn controls_hint_offers_only_whats_valid_for_the_state() {
        assert_eq!(controls_hint(&running(0)), "P pause \u{2022} C cancel");
        assert_eq!(
            controls_hint(&JobState::Paused { current_step: 0 }),
            "R resume \u{2022} C cancel"
        );
        assert_eq!(controls_hint(&JobState::Queued), "C cancel");
        assert_eq!(controls_hint(&completed()), "");
    }

    // -- progress_line -------------------------------------------------------

    #[test]
    fn progress_line_reports_no_data_yet_before_the_first_sample() {
        let j = job(1, JobKind::Copy, running(0));
        assert_eq!(progress_line(&j, None), "No progress data yet.");
    }

    #[test]
    fn progress_line_formats_a_live_sample() {
        let j = job(1, JobKind::Copy, running(0));
        let line = progress_line(&j, Some(snapshot(3, 512, 256, Some(5))));
        assert!(line.starts_with("3/10 files"), "line was: {line}");
        assert!(line.contains("ETA 0:05"), "line was: {line}");
    }

    #[test]
    fn progress_line_summarizes_a_failed_terminal_job() {
        let report = duet_ops::JobReport {
            errors: vec![StepFailure {
                step_index: 0,
                path: Some(VPath::local(
                    duet_types::UnixPathBuf::new("/tmp/x").unwrap(),
                )),
                kind: ErrorKind::Permission,
                message: "denied".to_string(),
            }],
            ..Default::default()
        };
        let j = job(
            1,
            JobKind::Copy,
            JobState::Terminal {
                outcome: JobOutcome::Failed,
                report,
            },
        );
        assert_eq!(progress_line(&j, None), "Failed \u{2014} 1 error(s).");
    }

    #[test]
    fn progress_line_summarizes_a_completed_with_skips_terminal_job() {
        let report = duet_ops::JobReport {
            files_completed: 4,
            skipped: vec![SkipEntry {
                step_index: 0,
                path: VPath::local(duet_types::UnixPathBuf::new("/tmp/y").unwrap()),
                reason: "already exists".to_string(),
            }],
            ..Default::default()
        };
        let j = job(
            1,
            JobKind::Copy,
            JobState::Terminal {
                outcome: JobOutcome::CompletedWithSkips,
                report,
            },
        );
        assert_eq!(progress_line(&j, None), "4 file(s) completed, 1 skipped.");
    }

    // -- format_eta_secs -------------------------------------------------------

    #[test]
    fn format_eta_secs_formats_minutes_and_seconds() {
        assert_eq!(format_eta_secs(5), "0:05");
        assert_eq!(format_eta_secs(90), "1:30");
    }

    #[test]
    fn format_eta_secs_formats_hours_once_an_hour_is_reached() {
        assert_eq!(format_eta_secs(3661), "1:01:01");
    }
}
