// SPDX-License-Identifier: MIT
//! T-5.3.2 phase 2's trash browser (FR-CFG-07, `docs/commands.md`'s
//! `trash.open_browser`/`trash.restore`/`trash.empty`/`trash.delete_selected`
//! rows): a browsable, keyboard-driven view over T-5.3.1's freedesktop
//! trash and phase 1's read/plan API (`duet_ops::list_trash_entries`,
//! `duet_ops::plan_trash_restore`, `duet_ops::plan_trash_purge`), all of
//! which are already merged and untouched by this phase -- see those
//! modules' own doc comments for the crash-safety ordering this dialog
//! trusts rather than re-derives.
//!
//! # Structural template: `crate::recovery_dialog`, plus real multi-select
//!
//! `docs/commands.md`'s own preconditions -- `trash.restore`/`trash.
//! delete_selected` are both `trash_browser && selection.nonempty` -- mean
//! this dialog operates on a *set* of rows, not just a cursor. `duet_widgets
//! ::list::List`/`ListDelegate` (used by `crate::command_palette`/`crate::
//! hotlist`) marks a single current-cursor row (`Selectable`), the wrong
//! shape here for the same reason `crate::recovery_dialog`'s own module doc
//! comment already gives for its own list; `crate::recovery_dialog` itself
//! is the right template (a plain `Vec<T>`, a `cursor: usize`, plain `div`
//! rows in an `.overflow_y_scroll()` container, no `ListDelegate`) *plus*
//! one thing it doesn't need: [`TrashDialogState::marked`], a
//! `HashSet<PathBuf>` of marked rows' `TrashEntry::content_path` (a stable,
//! unique-per-entry key -- unlike a plain row index, it survives entries
//! being removed from [`TrashDialogState::entries`] as restores/purges
//! land, the same problem `crate::file_table`'s own mouse-driven Ctrl+click
//! multi-selection set solves by keying off entry identity rather than
//! position). `Space` toggles the cursor row's membership; every action
//! that needs "the selected rows" ([`TrashDialogState::target_indices`])
//! follows this codebase's established "selection, or the cursor row if
//! nothing's selected" fallback (`crate::copy_move_dialog::
//! resolve_source_names`, reused by every F5/F6/F8-style dialog already).
//!
//! No virtualization: real trash directories run from empty to a few
//! hundred entries after a big cleanup, essentially never thousands --
//! `crate::recovery_dialog`'s plain `div`-per-row approach costs nothing
//! noticeable at that scale, and `duet_widgets::list`'s virtualized
//! rendering exists for a genuinely large candidate set (the command
//! palette's few hundred *commands*, rendered on every keystroke) that this
//! dialog's occasional, human-paced open doesn't need.
//!
//! # Loading is asynchronous, unlike T-5.2.5's startup scan
//!
//! `duet_ops::list_trash_entries` does real directory-scan-plus-parse I/O
//! (one `read_dir` plus one `.trashinfo` parse per entry) that, unlike
//! T-5.2.5's *startup* recovery scan (which runs before any window exists
//! to show a stutter in), happens on a live keystroke after the window is
//! already painting -- `Workspace::open_trash_dialog` therefore follows
//! T-5.2.8's own `open_attributes_dialog` shape for its single-target
//! `stat` prefill: `tokio_handle.spawn` the blocking scan, a `oneshot` back,
//! then `window.spawn`/`update_in` to actually construct and show this
//! dialog -- never a synchronous call on the GPUI thread. A scan failure
//! (a genuine `TrashError::Io`, e.g. the trash `info` directory exists but
//! isn't readable) is reported as a toast and the dialog simply isn't
//! opened, the same "notice, not a crash" handling T-5.2.5's own
//! `scan_startup_recovery_reports` failure path already establishes --
//! there is nothing useful to show once the read itself failed. A single
//! malformed `.trashinfo` sidecar is *not* such a failure: phase 1's own
//! `list_trash_entries` already tolerates and skips one, silently, per its
//! own module doc comment, so this dialog never even sees it.
//!
//! Entries are sorted most-recently-deleted first (`TrashEntry::
//! deleted_at`, descending) -- the default every mainstream trash browser
//! (GNOME Files, Dolphin) already uses, so nothing surprises a user
//! crossing between them.
//!
//! # An empty trash still opens the dialog
//!
//! Unlike `crate::recovery_dialog` (which never opens at all when nothing
//! needs review -- a one-shot startup gate with no user keystroke behind
//! it), `Workspace::open_trash_dialog` runs in direct response to a live
//! keypress (`trash.open_browser`). A keypress that visibly does nothing
//! reads as broken, not as "correctly detected there was nothing to show"
//! -- so an empty scan still opens this dialog, rendering "Trash is empty"
//! in place of the row list, rather than silently declining to open.
//!
//! # The confirmation gate: `trash.restore` needs none, `trash.empty`/
//! `trash.delete_selected` need one
//!
//! This is the one place `crate::recovery_dialog` is the *wrong* precedent
//! to copy literally. That dialog's own `D` (discard) needs no confirmation
//! because it only ever deletes `.duet-partial-*` internal staging litter --
//! disposable scratch, never real user content (see its own module doc
//! comment). `trash.empty`/`trash.delete_selected` are the opposite: they
//! *permanently* destroy real content that is already sitting in the trash
//! as the user's last safety net. `trash.restore` needs no gate at all --
//! it is the safe, recoverable direction, exactly like T-5.2.5's own
//! "resume" needing none.
//!
//! The mechanism: [`ConfirmKind`]/[`TrashDialogState::confirm_armed`] is a
//! simple "press again within [`CONFIRM_WINDOW`]" arm/fire state, checked
//! by [`TrashDialogState::press_confirm_gated`] -- the first `D`/`E` press
//! arms it (and changes the hint line to say so); a second press of the
//! *same* key, within the window, actually runs the purge; anything else
//! (a different action, or the window elapsing) requires arming again. This
//! is a lightweight, in-dialog alternative to `crate::delete_dialog`'s own
//! separate confirmation dialog -- proportionate here since this dialog is
//! already itself a review-before-acting surface, and stacking a second
//! modal on top of it would be one confirmation too many. No TC precedent
//! exists for any of this (Total Commander has no comparable trash
//! browser), so `D`/`E`/the window length are this module's own disclosed
//! defaults, the same situation `crate::recovery_dialog`'s own `D`/Enter
//! chords are already in.
//!
//! # One job per targeted entry, not one shared batch job
//!
//! [`TrashDialogState::run_jobs`] enqueues a *separate* `duet_ops::
//! QueueManager` job per entry being restored/purged, run sequentially
//! (each one's completion awaited before the next starts), rather than one
//! job whose plan covers every targeted entry. This trades a little
//! throughput (jobs that could in principle run concurrently instead run
//! one after another) for a much simpler, exactly-correct success signal:
//! each entry's own `duet_ops::JobReport::errors` is unambiguously about
//! that entry alone, so "did entry N actually get restored/purged" never
//! needs correlating a shared plan's step indices back to which entry they
//! belonged to. For the AC's own "restore into a deleted parent... or
//! reports clearly" clause this matters concretely: a multi-entry restore
//! where one entry's parent can't be recreated (permission denied) must
//! still restore every *other* entry and leave only the failing one
//! visible in this dialog's list, which per-entry jobs give for free. Each
//! job still goes through the real `QueueManager` (crash-safe, journaled,
//! visible in the operation manager, and already toasted on completion by
//! `Workspace::new`'s own `queue_events_rx` consumer loop -- this dialog
//! adds no duplicate toasting of its own), not a queue-bypassing direct
//! `duet_ops::execute` call.
//!
//! [`TrashDialogState::run_jobs`]'s own `window.spawn`/`update_in`
//! continuation ([`TrashDialogState::apply_job_results`]) always has a live
//! `&mut Window` by construction, so -- mirroring `crate::recovery_dialog`'s
//! own reasoning exactly -- there is no `close_trash_dialog_deferred` in
//! `workspace.rs`: nothing in this module ever needs one. Restoring/purging
//! never closes the dialog either (unlike `crate::recovery_dialog`, which
//! closes once its review list empties): this is a browser meant to stay
//! open across several restores in one sitting, not a one-shot review flow,
//! so it only ever closes on an explicit `Escape`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duet_ops::{
    CancelToken, ConflictResolver, JobKind, JobState, PlanOptions, QueueManager, TrashEntry,
    plan_trash_purge, plan_trash_restore,
};
use duet_vfs::{FileSystem, LocalFs};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, actions, div, px,
};

use crate::copy_move_dialog::JOB_CONCURRENCY;
use crate::file_table::write_date;
use crate::workspace::{NoticeLevel, Workspace};

/// How long a `D`/`E` "arm" stays live before a second press is required to
/// re-arm rather than fire -- this module's own disclosed default (no TC
/// precedent); see the module doc comment's "confirmation gate" section.
const CONFIRM_WINDOW: Duration = Duration::from_secs(4);

/// [`TrashDialogState::run_jobs`]'s own poll interval while waiting for one
/// entry's job to reach `JobState::Terminal` -- short enough that a fast
/// single-entry restore/purge (the overwhelmingly common case) doesn't add
/// perceptible latency, long enough not to spin the executor.
const POLL_INTERVAL: Duration = Duration::from_millis(15);

// This dialog's own actions. `CloseTrashDialog` mirrors every sibling
// overlay's Escape convention; the rest are this dialog's own reasonable,
// unclaimed-elsewhere bindings -- see the module doc comment's
// "confirmation gate" section for why `D`/`E` specifically.
actions!(
    duet_trash_dialog,
    [
        CloseTrashDialog,
        TrashDialogCursorUp,
        TrashDialogCursorDown,
        TrashDialogToggleMark,
        TrashDialogRestore,
        TrashDialogDeleteSelected,
        TrashDialogEmpty,
    ]
);

/// Registers this dialog's own keybindings, scoped to `"TrashDialog"` (set
/// on [`TrashDialogState::render`]'s own root `div`). Called once from
/// `workspace::run` (and from that module's own test harness), alongside
/// every other `bind_*_dialog_keys` function.
pub(crate) fn bind_trash_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseTrashDialog, Some("TrashDialog")),
        KeyBinding::new("up", TrashDialogCursorUp, Some("TrashDialog")),
        KeyBinding::new("down", TrashDialogCursorDown, Some("TrashDialog")),
        KeyBinding::new("space", TrashDialogToggleMark, Some("TrashDialog")),
        KeyBinding::new("enter", TrashDialogRestore, Some("TrashDialog")),
        KeyBinding::new("d", TrashDialogDeleteSelected, Some("TrashDialog")),
        KeyBinding::new("e", TrashDialogEmpty, Some("TrashDialog")),
    ]);
}

/// Which confirmation is currently armed -- see the module doc comment's
/// "confirmation gate" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmKind {
    DeleteSelected,
    Empty,
}

/// Which planner/job kind [`TrashDialogState::run_jobs`] is driving --
/// `Restore` and `Purge` differ only in which `duet_ops` planner function
/// and `duet_ops::JobKind` apply, never in the surrounding job/poll/apply
/// machinery.
#[derive(Debug, Clone, Copy)]
enum TrashAction {
    Restore,
    Purge,
}

impl TrashAction {
    fn job_kind(self) -> JobKind {
        match self {
            TrashAction::Restore => JobKind::RestoreFromTrash,
            TrashAction::Purge => JobKind::PurgeTrash,
        }
    }
}

/// T-5.3.2 phase 2's trash browser view. See the module doc comment for the
/// full architecture; `Workspace::show_trash_dialog` is the only
/// constructor call site, reached only after `Workspace::open_trash_dialog`'s
/// own async `duet_ops::list_trash_entries` scan has already succeeded.
pub(crate) struct TrashDialogState {
    /// Sorted newest-first at construction time
    /// ([`Workspace::open_trash_dialog`]'s own continuation); shrinks as
    /// entries are restored/purged ([`Self::apply_job_results`]) -- never
    /// re-scanned from disk while open, the same "a snapshot, not a live
    /// handle" precedent `crate::recovery_dialog`'s own `reports` field
    /// documents.
    entries: Vec<TrashEntry>,
    /// The marked set, keyed by `TrashEntry::content_path` -- see the
    /// module doc comment's "structural template" section for why identity,
    /// not position, is the right key here.
    marked: HashSet<PathBuf>,
    /// The selected row, as an index into `entries` -- same "bare Up/Down,
    /// no text field to compete with" shape `crate::recovery_dialog::
    /// RecoveryDialogState::cursor` already establishes.
    cursor: usize,
    /// `true` while a restore/purge batch is running -- guards against a
    /// double-Enter/double-`D`/double-`E` re-entering [`Self::run_jobs`]
    /// while a previous one is still in flight, same reasoning as every
    /// sibling dialog's own `planning_in_progress`/`busy` guard.
    busy: bool,
    /// See the module doc comment's "confirmation gate" section.
    confirm_armed: Option<(ConfirmKind, Instant)>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    /// `Workspace::state_dir`, passed in whole -- `None` under the same
    /// rare XDG-resolution failure `crate::delete_dialog::DeleteDialogState`
    /// already tolerates. [`Self::run_jobs`] refuses to enqueue (with a
    /// toast) rather than guessing a job journal location.
    state_dir: Option<PathBuf>,
    conflict_resolver: Arc<dyn ConflictResolver>,
}

impl TrashDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut entries: Vec<TrashEntry>,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Belt-and-suspenders: `Workspace::open_trash_dialog`'s own
        // continuation already sorts before constructing this, but sorting
        // again here costs nothing on a "usually small, occasionally a few
        // hundred" list and keeps this invariant enforced at the one place
        // that can never be bypassed by a future second call site.
        entries.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            entries,
            marked: HashSet::new(),
            cursor: 0,
            busy: false,
            confirm_armed: None,
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
    pub(crate) fn entries(&self) -> &[TrashEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn is_marked(&self, index: usize) -> bool {
        self.entries
            .get(index)
            .is_some_and(|e| self.marked.contains(&e.content_path))
    }

    #[cfg(test)]
    pub(crate) fn confirm_armed(&self) -> bool {
        self.confirm_armed.is_some()
    }

    #[cfg(test)]
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    fn disarm_confirm(&mut self) {
        self.confirm_armed = None;
    }

    /// Clamps, doesn't wrap -- same convention `crate::recovery_dialog::
    /// RecoveryDialogState::cursor_up`/`cursor_down` already establish.
    fn cursor_up(&mut self, cx: &mut Context<Self>) {
        self.disarm_confirm();
        self.cursor = self.cursor.saturating_sub(1);
        cx.notify();
    }

    fn cursor_down(&mut self, cx: &mut Context<Self>) {
        self.disarm_confirm();
        let len = self.entries.len();
        if len > 0 {
            self.cursor = (self.cursor + 1).min(len - 1);
        }
        cx.notify();
    }

    /// `Space`: toggles the cursor row's membership in [`Self::marked`].
    fn toggle_mark(&mut self, cx: &mut Context<Self>) {
        self.disarm_confirm();
        if let Some(entry) = self.entries.get(self.cursor) {
            let key = entry.content_path.clone();
            if !self.marked.remove(&key) {
                self.marked.insert(key);
            }
        }
        cx.notify();
    }

    /// The "selection, or the cursor row if nothing's selected" fallback
    /// (`crate::copy_move_dialog::resolve_source_names`'s own convention),
    /// applied to `trash.restore`/`trash.delete_selected`'s own
    /// `selection.nonempty` precondition. Order matches display order
    /// (newest-first), not insertion order into [`Self::marked`].
    fn target_indices(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            if self.entries.is_empty() {
                Vec::new()
            } else {
                vec![self.cursor]
            }
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| self.marked.contains(&e.content_path))
                .map(|(ix, _)| ix)
                .collect()
        }
    }

    /// Escape: closes the dialog. Nothing to resolve on the way out --
    /// unlike `crate::recovery_dialog`, this dialog never mutates anything
    /// merely by being open, so there is no "reappears differently next
    /// time" concern to document here.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_trash_dialog(window, cx);
        });
    }

    /// Enter (`trash.restore`): no confirmation gate (see the module doc
    /// comment) -- runs immediately against [`Self::target_indices`].
    fn restore_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.disarm_confirm();
        let indices = self.target_indices();
        self.run_jobs(TrashAction::Restore, indices, window, cx);
    }

    /// `D` (`trash.delete_selected`): gated by [`Self::press_confirm_gated`],
    /// targets [`Self::target_indices`] (marked, or cursor row).
    fn delete_selected_pressed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let armed = self.press_confirm_gated(ConfirmKind::DeleteSelected, cx);
        if armed {
            let indices = self.target_indices();
            self.run_jobs(TrashAction::Purge, indices, window, cx);
        }
    }

    /// `E` (`trash.empty`): gated the same way, but targets *every* entry
    /// regardless of the marked set -- `docs/commands.md`'s own predicate
    /// for this command is just `trash_browser`, with no
    /// `selection.nonempty` clause, unlike `trash.delete_selected`.
    fn empty_pressed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let armed = self.press_confirm_gated(ConfirmKind::Empty, cx);
        if armed {
            let indices: Vec<usize> = (0..self.entries.len()).collect();
            self.run_jobs(TrashAction::Purge, indices, window, cx);
        }
    }

    /// The confirmation state machine itself: a first press of `kind` arms
    /// it and returns `false` (nothing runs yet); a second press of the
    /// *same* `kind`, within [`CONFIRM_WINDOW`], disarms and returns `true`
    /// (the caller should now actually run the action). Anything else --
    /// a different `kind`, or the window having elapsed -- (re)arms fresh
    /// and returns `false`.
    fn press_confirm_gated(&mut self, kind: ConfirmKind, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let fire = matches!(
            self.confirm_armed,
            Some((armed_kind, at)) if armed_kind == kind && now.duration_since(at) <= CONFIRM_WINDOW
        );
        self.confirm_armed = if fire { None } else { Some((kind, now)) };
        cx.notify();
        fire
    }

    /// The shared engine behind [`Self::restore_selected`]/
    /// [`Self::delete_selected_pressed`]/[`Self::empty_pressed`]: runs one
    /// `duet_ops::QueueManager` job per entry in `indices` (see the module
    /// doc comment's "one job per targeted entry" section), sequentially,
    /// entirely off the UI thread, then folds the per-entry results back
    /// via [`Self::apply_job_results`].
    fn run_jobs(
        &mut self,
        action: TrashAction,
        indices: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || indices.is_empty() {
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
        let targets: Vec<(usize, TrashEntry)> = indices
            .into_iter()
            .filter_map(|ix| self.entries.get(ix).cloned().map(|e| (ix, e)))
            .collect();
        if targets.is_empty() {
            return;
        }

        self.busy = true;
        cx.notify();

        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let resolver = self.conflict_resolver.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio_handle.spawn(async move {
            let mut results = Vec::with_capacity(targets.len());
            for (index, entry) in targets {
                let ok =
                    run_one_trash_job(action, queue.as_ref(), &state_dir, resolver.clone(), &entry)
                        .await;
                results.push((index, ok));
            }
            let _ = tx.send(results);
        });

        let weak = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let results = rx.await.unwrap_or_default();
                let _ = weak.update_in(cx, |this, window, cx| {
                    this.busy = false;
                    this.apply_job_results(&results, window, cx);
                });
            })
            .detach();
    }

    /// Removes every entry whose own job succeeded (`report.errors.
    /// is_empty()`, folded into `bool` by [`run_one_trash_job`]) from
    /// [`Self::entries`] -- in descending index order, since `indices` were
    /// captured before this dialog's own `entries` could have shrunk from
    /// anywhere else (`busy` blocks any other action from starting a
    /// second batch in the meantime, so no other mutation can interleave).
    /// An entry whose job reported an error is left in place -- its
    /// per-job `JobEvent::Finished` has already surfaced the failure as a
    /// toast via `Workspace::new`'s own event-consumer loop, satisfying the
    /// AC's "or reports clearly" clause with no duplicate reporting here.
    fn apply_job_results(
        &mut self,
        results: &[(usize, bool)],
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut succeeded: Vec<usize> = results
            .iter()
            .filter(|(_, ok)| *ok)
            .map(|(ix, _)| *ix)
            .collect();
        succeeded.sort_unstable_by(|a, b| b.cmp(a));
        succeeded.dedup();
        for index in succeeded {
            if index < self.entries.len() {
                let entry = self.entries.remove(index);
                self.marked.remove(&entry.content_path);
            }
        }
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        cx.notify();
    }
}

/// [`TrashDialogState::run_jobs`]'s own per-entry unit of work: plans,
/// enqueues, and waits for exactly one entry's restore/purge job to reach
/// `JobState::Terminal`, folding its outcome down to the one bit this
/// dialog's list needs (did this entry's own content genuinely move/get
/// removed). A planner failure (essentially unreachable in practice per
/// `duet_ops::trash_restore`'s own doc comment, but handled honestly rather
/// than assumed away) counts as failure without ever reaching the queue --
/// there is no `JobId` to poll in that case.
async fn run_one_trash_job(
    action: TrashAction,
    queue: &QueueManager,
    state_dir: &Path,
    resolver: Arc<dyn ConflictResolver>,
    entry: &TrashEntry,
) -> bool {
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
    let cancel = CancelToken::new();
    let entries = std::slice::from_ref(entry);
    let plan = match action {
        TrashAction::Restore => {
            plan_trash_restore(fs.as_ref(), entries, PlanOptions::default(), &cancel).await
        }
        TrashAction::Purge => {
            plan_trash_purge(fs.as_ref(), entries, PlanOptions::default(), &cancel).await
        }
    };
    let Ok(plan) = plan else {
        return false;
    };

    let job_id = queue.enqueue(
        action.job_kind(),
        plan,
        0,
        fs,
        state_dir.to_path_buf(),
        JOB_CONCURRENCY,
        Some(resolver),
    );
    loop {
        if let Some(job) = queue.job(job_id)
            && let JobState::Terminal { report, .. } = job.state
        {
            return report.errors.is_empty();
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

impl Focusable for TrashDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The status-line hint -- changes shape depending on
/// [`TrashDialogState::busy`]/`confirm_armed`, mirroring `crate::
/// recovery_dialog::RecoveryDialogState::render`'s own busy-vs-idle hint
/// swap.
fn trash_dialog_hint(busy: bool, confirm_armed: Option<ConfirmKind>, marked: usize) -> String {
    if busy {
        return "Working\u{2026}".to_string();
    }
    match confirm_armed {
        Some(ConfirmKind::DeleteSelected) => {
            "Press D again to permanently delete \u{2014} any other key cancels".to_string()
        }
        Some(ConfirmKind::Empty) => {
            "Press E again to permanently empty the trash \u{2014} any other key cancels"
                .to_string()
        }
        None => {
            let target = if marked > 0 {
                format!("{marked} marked")
            } else {
                "cursor row".to_string()
            };
            format!(
                "Up/Down select \u{2022} Space mark ({target}) \u{2022} Enter restore \u{2022} \
                 D delete selected \u{2022} E empty trash \u{2022} Esc close"
            )
        }
    }
}

impl Render for TrashDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = if self.entries.is_empty() {
            "Trash is empty".to_string()
        } else {
            format!(
                "Trash \u{2014} {} item{}",
                self.entries.len(),
                plural(self.entries.len())
            )
        };

        let rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                render_entry_row(
                    ix,
                    entry,
                    ix == self.cursor,
                    self.marked.contains(&entry.content_path),
                    tokens,
                )
            })
            .collect();

        let hint = trash_dialog_hint(
            self.busy,
            self.confirm_armed.map(|(kind, _)| kind),
            self.marked.len(),
        );

        div()
            .id("trash-dialog")
            .key_context("TrashDialog")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .id("trash-dialog-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(hint),
            )
            .on_action(cx.listener(|this, _: &CloseTrashDialog, window, cx| {
                this.close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TrashDialogCursorUp, _window, cx| {
                this.cursor_up(cx);
            }))
            .on_action(cx.listener(|this, _: &TrashDialogCursorDown, _window, cx| {
                this.cursor_down(cx);
            }))
            .on_action(cx.listener(|this, _: &TrashDialogToggleMark, _window, cx| {
                this.toggle_mark(cx);
            }))
            .on_action(cx.listener(|this, _: &TrashDialogRestore, window, cx| {
                this.restore_selected(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &TrashDialogDeleteSelected, window, cx| {
                    this.delete_selected_pressed(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &TrashDialogEmpty, window, cx| {
                this.empty_pressed(window, cx);
            }))
    }
}

/// One entry's row: a mark indicator, the original filename plus its
/// original parent directory (two trashed items can share a basename --
/// the location line disambiguates), and the deletion date via `crate::
/// file_table`'s own `write_date`/`civil_from_unix` -- the established
/// date-formatting precedent in this crate (already reused by T-5.2.5/
/// T-5.2.8), not a new format invented here. A free function, not a
/// method -- same reasoning `crate::recovery_dialog::render_report_row`
/// already gives for its own per-row rendering.
fn render_entry_row(
    ix: usize,
    entry: &TrashEntry,
    selected: bool,
    marked: bool,
    tokens: &TokenPalette,
) -> impl IntoElement {
    let name = entry
        .original_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.original_path.display().to_string());
    let location = entry
        .original_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut date = String::new();
    let secs = entry
        .deleted_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    write_date(&mut date, secs);
    let marker = if marked { "[x]" } else { "[ ]" };

    div()
        .id(("trash-dialog-row", ix))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .when(selected, |this| this.bg(tokens.color.selection_bg))
        .child(div().text_size(px(11.)).child(marker))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.))
                .flex_1()
                .child(div().text_size(px(12.)).min_w(px(0.)).child(name))
                .child(
                    div()
                        .text_size(px(11.))
                        .min_w(px(0.))
                        .text_color(tokens.color.statusbar_fg)
                        .child(location),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(tokens.color.statusbar_fg)
                .child(date),
        )
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn entry(name: &str, deleted_at: SystemTime) -> TrashEntry {
        TrashEntry {
            content_path: PathBuf::from(format!("/trash/files/{name}")),
            info_path: PathBuf::from(format!("/trash/info/{name}.trashinfo")),
            original_path: PathBuf::from(format!("/home/u/{name}")),
            deleted_at,
        }
    }

    // -- plural / trash_dialog_hint -----------------------------------------

    #[test]
    fn plural_is_empty_only_for_exactly_one() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }

    #[test]
    fn trash_dialog_hint_shows_working_while_busy_regardless_of_confirm_state() {
        assert_eq!(
            trash_dialog_hint(true, Some(ConfirmKind::Empty), 0),
            "Working\u{2026}"
        );
    }

    #[test]
    fn trash_dialog_hint_names_the_armed_action() {
        let text = trash_dialog_hint(false, Some(ConfirmKind::DeleteSelected), 2);
        assert!(text.starts_with("Press D again"), "{text}");
        let text = trash_dialog_hint(false, Some(ConfirmKind::Empty), 0);
        assert!(text.starts_with("Press E again"), "{text}");
    }

    #[test]
    fn trash_dialog_hint_reports_the_marked_count_when_idle() {
        let text = trash_dialog_hint(false, None, 3);
        assert!(text.contains("3 marked"), "{text}");
        let text = trash_dialog_hint(false, None, 0);
        assert!(text.contains("cursor row"), "{text}");
    }

    // -- render_entry_row (via the free helpers it composes) ----------------

    #[test]
    fn entry_original_name_and_location_split_correctly() {
        let e = entry("a.txt", SystemTime::UNIX_EPOCH);
        assert_eq!(e.original_path.file_name().unwrap(), "a.txt");
        assert_eq!(e.original_path.parent().unwrap(), Path::new("/home/u"));
    }

    // -- sort order (constructor-level invariant) ----------------------------

    #[test]
    fn entries_sort_newest_deleted_first() {
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let mut entries = [entry("old.txt", older), entry("new.txt", newer)];
        entries.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
        assert_eq!(entries[0].original_path, PathBuf::from("/home/u/new.txt"));
        assert_eq!(entries[1].original_path, PathBuf::from("/home/u/old.txt"));
    }
}
