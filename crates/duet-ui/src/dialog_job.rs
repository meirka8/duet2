// SPDX-License-Identifier: MIT
//! The off-UI-thread "plan it, then enqueue it" half every operation
//! dialog in this crate shares, plus the toast-on-failure reporting half
//! that bridges the result back onto GPUI.
//!
//! # Why this exists as its own module (T-5.2.7)
//!
//! T-5.2.1 (`crate::copy_move_dialog`) and T-5.2.6
//! (`crate::delete_dialog`) each grew their own copy of the same fifteen
//! lines: spawn onto `tokio_handle`, build a `Plan`, hand it to
//! `QueueManager::enqueue` *inside the same spawned task* (design.md
//! §8.2's "main thread does no I/O, ever" covers `Journal::open`'s
//! synchronous file creation just as much as the planning walk itself),
//! and send the outcome back through a `oneshot`. T-5.2.7 adds four more
//! dialogs (F7 mkdir, Shift+F6 rename, and the two link dialogs), which
//! would have made six copies. [`spawn_plan_and_enqueue`] is that block,
//! once, parameterised on the only part that genuinely differs between
//! callers: which planner to call and with what.
//!
//! [`report_job_outcome`] is T-5.2.6's own `report_delete_job_outcome`,
//! moved here and renamed -- nothing about it was ever delete-specific
//! (it matches on `Result<Result<JobId, String>, RecvError>` and toasts
//! the two failure arms), and it is exactly what all six dialogs' GPUI-side
//! continuations need.
//!
//! `crate::copy_move_dialog::CopyMoveDialogState::confirm` deliberately
//! still spawns its own task rather than routing through
//! [`spawn_plan_and_enqueue`]: it is the one caller that needs a
//! non-default `priority` (its Ctrl+J "queued" toggle) and non-default
//! `PlanOptions` (its conflict/verify toggles), so folding it in would
//! mean adding two parameters that five of the six callers would pass a
//! constant to. Its *reporting* half is shared here, though -- the
//! three-arm match below is byte-for-byte what it already did inline.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{ConflictResolver, JobId, JobKind, Plan, PlannerError, QueueManager};
use duet_vfs::{FileSystem, LocalFs};
use gpui::{AsyncApp, WeakEntity};

use crate::copy_move_dialog::{JOB_CONCURRENCY, describe_planner_error};
use crate::workspace::{NoticeLevel, Workspace};

/// Runs `plan` on the ops runtime and, if it succeeds, enqueues the
/// resulting [`Plan`] as a `kind` job -- both off the UI thread, in one
/// spawned task. Returns the `oneshot` receiver the caller bridges back
/// onto GPUI with its own `cx.spawn` (see [`report_job_outcome`]).
///
/// `plan` receives the same `Arc<dyn FileSystem>` the job will later be
/// executed against, so a planner that needs to `stat` something
/// (`duet_ops::plan_mkdir`, `duet_ops::plan_hardlink`) and one that needs
/// nothing at all (`duet_ops::plan_rename_in_place`,
/// `duet_ops::plan_symlink`) both fit the same signature -- the latter two
/// simply ignore the argument.
///
/// `priority: 0` ("run now") and [`JOB_CONCURRENCY`] are fixed here rather
/// than taken as parameters: every dialog that routes through this helper
/// is a single-target, effectively-instant operation with no queue-versus-
/// run-now choice to offer. `resolver` is passed through because
/// T-5.1.9's conflict engine runs underneath *all* of these -- a rename
/// onto an occupied name, a symlink at a path that already exists -- and
/// without a live resolver those would silently take
/// `PlanOptions::default_conflict` (`Skip`) instead of asking. See
/// `duet_ops::executor`'s own `create_dir_step`/`link_step`/`symlink_step`,
/// each of which already builds a real `ConflictPrompt` for its step kind.
pub(crate) fn spawn_plan_and_enqueue<F, Fut>(
    tokio_handle: &tokio::runtime::Handle,
    kind: JobKind,
    queue: Arc<QueueManager>,
    state_dir: PathBuf,
    resolver: Arc<dyn ConflictResolver>,
    plan: F,
) -> tokio::sync::oneshot::Receiver<Result<JobId, String>>
where
    F: FnOnce(Arc<dyn FileSystem>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Plan, PlannerError>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio_handle.spawn(async move {
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let outcome = match plan(Arc::clone(&fs)).await {
            Ok(plan) => Ok(queue.enqueue(
                kind,
                plan,
                0,
                fs,
                state_dir,
                JOB_CONCURRENCY,
                Some(resolver),
            )),
            Err(err) => Err(describe_planner_error(&err)),
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Surfaces a [`spawn_plan_and_enqueue`] (or
/// `crate::delete_dialog::spawn_delete_job`) outcome as a toast when it
/// failed, and reports whether the job was actually enqueued -- `true`
/// meaning the caller's dialog should now close.
///
/// Goes through `Workspace::push_pending_notice` rather than
/// `window.push_notification` because every call site is an async
/// continuation with no live `Window` (see that method's own doc comment).
pub(crate) fn report_job_outcome(
    outcome: Result<Result<JobId, String>, tokio::sync::oneshot::error::RecvError>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
) -> bool {
    match outcome {
        Ok(Ok(_job_id)) => true,
        Ok(Err(message)) => {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    format!("Couldn't plan the operation: {message}"),
                    cx,
                );
            });
            false
        }
        Err(_) => {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    "The planning task was dropped before completing.".to_string(),
                    cx,
                );
            });
            false
        }
    }
}
