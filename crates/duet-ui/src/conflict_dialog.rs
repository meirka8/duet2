// SPDX-License-Identifier: MIT
//! T-5.2.3's live, interactive conflict-resolution dialog (FR-OPS-04) --
//! side-by-side source/destination metadata, on-demand BLAKE3 hashing per
//! side, and all seven [`ConflictPolicy`] variants, each choosable as
//! "this conflict only" or "apply to all remaining" (with one disclosed
//! exception -- `RenameTarget`, see "Keybindings" below).
//!
//! # Bridging a synchronous, blocking `ConflictResolver` onto GPUI
//!
//! `duet_ops::conflict::ConflictResolver::resolve` is deliberately
//! synchronous (see that trait's own doc comment: "a future interactive
//! implementation that genuinely has to wait on a human is expected to
//! bridge that itself"). [`InteractiveConflictResolver`] is that bridge:
//! `resolve` hands the [`ConflictPrompt`] to a `tokio::sync::mpsc` request
//! channel -- `Workspace::new`'s conflict-request consumer loop (a
//! `cx.spawn` task mirroring its pre-existing `queue_events_rx` loop for
//! `JobEvent`s) is the sole reader -- then blocks the *calling* (executor)
//! thread on a plain, blocking `std::sync::mpsc::Receiver::recv()` until
//! the UI thread answers. The blocking wait is wrapped in
//! `tokio::task::block_in_place`, non-negotiably: without it, a user
//! taking even a few seconds to click a conflict button would starve the
//! queue's progress-sampler task exactly the way an un-`block_in_place`d
//! `server_side_copy` call did before this fix landed twice already this
//! session (see `crates/duet-ops/src/executor.rs`'s `copy_file_step`,
//! whose own doc comment has the full incident writeup -- this module's
//! own [`InteractiveConflictResolver::resolve`] is the identical shape,
//! just for a human's answer instead of a disk's).
//!
//! `response_tx`/`response_rx` are a plain `std::sync::mpsc` pair, not
//! `tokio::sync::oneshot` -- deliberately: the receiving half must be
//! callable from inside `block_in_place`'s synchronous closure, which
//! cannot `.await` a tokio channel. `request_tx` (the prompt-bound half)
//! *is* a `tokio::sync::mpsc::UnboundedSender`, since its consumer lives
//! inside a GPUI `cx.spawn` async loop and needs a real `.recv().await`.
//!
//! # Why the dialog can't be constructed with a live `Window` at open time
//!
//! Every *other* overlay in this crate opens from a synchronous GPUI
//! action handler (F5/F6, Ctrl+D, Ctrl+O, ...), which always has a live
//! `Window`. A conflict, by contrast, is detected on a background executor
//! thread at an unpredictable moment -- `Workspace::new`'s conflict-
//! request consumer loop (a `cx.spawn` async block) is the only place
//! that can react to it, and -- like the pre-existing `queue_events_rx`
//! loop right above it -- that block has no `Window` at all, only
//! `this.update(cx, |this, cx| ...)`. `Workspace::pending_notice`/
//! `pending_focus_restore`/`pending_panel_refresh` are this codebase's own
//! established answer to exactly this shape of problem ("deferred,
//! drained on the next `Render::render` call, which *does* have a live
//! `Window`") -- `Workspace::pending_conflict_focus` (declared in
//! `workspace.rs`, alongside those three) is the same pattern for this
//! dialog's initial open. One consequence: [`ConflictDialogState::new`]
//! cannot eagerly construct the `RenameTarget` sub-field's `InputState`
//! the way `CopyMoveDialogState::new` does --
//! `duet_widgets::input::InputState::new` itself requires `&mut Window`.
//! It's built lazily instead, the first time `Alt+R` is actually pressed,
//! which *does* run from a live-`Window` action handler (every `on_action`
//! callback in this codebase receives one).
//!
//! # Overlay architecture
//!
//! Mirrors `crate::operation_manager`'s `OperationManagerState` shape (see
//! that module's own doc comment), not `crate::copy_move_dialog`'s
//! input-delegate one: this dialog's *primary* focus target is its own
//! root (`focus_handle`, `.track_focus`'d), since most of it is a set of
//! keyboard-driven policy choices, not one text field. The `RenameTarget`
//! sub-field is the one conditional exception -- see above. `Workspace`
//! owns `Option<Entity<ConflictDialogState>>` plus a small queue of
//! not-yet-shown `ConflictRequest`s (concurrent jobs can each hit their
//! own first conflict around the same time; none can simply be dropped --
//! see `Workspace::pending_conflict_requests`'s own doc comment).
//! `workspace::conflict_dialog_overlay` builds the backdrop/card chrome,
//! mirroring every other overlay's `.occlude()` pattern (see
//! `workspace::command_palette_overlay`'s own doc comment for the real
//! regression that exists to avoid) with one deliberate difference: no
//! `.on_mouse_down_out` close handler. Every other overlay in this crate
//! treats an outside click as "never mind, dismiss this" -- but a
//! conflict dialog has no "never mind": the executor thread behind it is
//! genuinely blocked waiting for an answer, and there is no non-answer
//! that unblocks it. Closing without picking a policy isn't a safe no-op
//! here the way it is everywhere else, so the option isn't offered.
//!
//! # Keybindings (FR-OPS-04, `docs/keymap-tc.csv`'s "dialog" rows)
//!
//! Six of the seven [`ConflictPolicy`] variants get two bindings each --
//! "this conflict only" and "apply to all remaining":
//!
//! | Policy                      | This only      | All remaining        |
//! |------------------------------|----------------|----------------------|
//! | `Overwrite`                  | `Alt+Y`        | `Alt+A`               |
//! | `Skip`                       | `Alt+N`/`Esc`  | `Alt+S`               |
//! | `OverwriteIfOlder`            | `Alt+O`        | `Alt+Shift+O`         |
//! | `OverwriteIfDifferentSize`    | `Alt+D`        | `Alt+Shift+D`         |
//! | `AutoRename`                  | `Alt+U`        | `Alt+Shift+U`         |
//! | `Abort`                       | `Alt+B`        | `Alt+Shift+B`         |
//!
//! `Alt+Y`/`Alt+A`/`Alt+N`/`Esc`/`Alt+S` are `docs/keymap-tc.csv`'s own
//! verified rows (`dlg.overwrite_yes`/`overwrite_all`/`overwrite_no`
//! (x2)/`overwrite_skip_all`). `OverwriteIfOlder`/`OverwriteIfDifferentSize`/
//! `AutoRename`/`Abort` have no TC-documented key at all (the CSV's own
//! survey doesn't cover them) -- `Alt+O`/`Alt+D`/`Alt+U`/`Alt+B` are this
//! codebase's own reasonable, disclosed mnemonic choices ("Older",
//! "Different", "aUto", "aBort" -- plain `B`, not `Alt+A`, since that's
//! already `Overwrite`/all-remaining), each paired with its own
//! `Alt+Shift+<letter>` for "apply to all remaining" -- a self-consistent
//! generalisation of the pattern the two TC-documented pairs already
//! establish (`Alt+<letter>` this-only, a related-but-different letter
//! for all-remaining), since TC's own irregular letter-per-policy pairing
//! doesn't extend cleanly to four more policies it never defined.
//!
//! `RenameTarget` (`Alt+R`, `docs/keymap-tc.csv`'s `dlg.rename` row) is
//! the one policy *without* an all-remaining pairing -- a disclosed,
//! deliberate omission, not an oversight: `RenameTarget`'s whole
//! definition is "a new name chosen by the user for *this* conflict" (see
//! `duet_ops::conflict::ConflictResolution::alternate`'s own doc comment);
//! there is no single alternate name that could sensibly apply to every
//! remaining, differently-named conflict in the job. `AutoRename` (a
//! blanket, engine-chosen renaming scheme with no per-conflict name
//! needed) is the policy this dialog offers instead for "keep renaming
//! everything from here on."
//!
//! Two more on-demand actions, with no TC precedent (hashing was not part
//! of Total Commander's own conflict dialog): `Alt+H` hashes the source
//! side, `Alt+Shift+H` the destination side.
//!
//! `Tab`/`Shift+Tab` (`docs/keymap-tc.csv`'s `dlg.next_field`/
//! `dlg.prev_field`, "known" confidence) move focus between this dialog's
//! two focusable regions -- its own root (where every policy shortcut
//! above fires) and the `RenameTarget` sub-field, once it exists. With
//! only two regions, both directions perform the same toggle -- a
//! disclosed simplification, not a bug: there is nothing a "third field"
//! distinction would add here. Reached the same way
//! `crate::copy_move_dialog`'s own Tab-completion is: `InputState`'s own
//! `"tab"`/`"shift-tab"` bindings (`gpui-component-0.5.1/src/input/
//! state.rs`) resolve to `IndentInline`/`OutdentInline` while the rename
//! field has focus, whose handlers `cx.propagate()` immediately for a
//! single-line field (confirmed by reading `gpui-component-0.5.1/src/
//! input/indent.rs`) -- this dialog's root catches the bubbled action the
//! same way `copy_move_dialog.rs` already catches a bubbled `IndentInline`
//! for its own Tab-completion.
//!
//! # On-demand hashing
//!
//! No reusable "hash one path" helper exists at the right visibility --
//! `duet_ops::executor`'s own `hash_file`/`HashAttempt` are private to
//! that module and expect an `ExecutionControl` this dialog has no reason
//! to carry. [`hash_one_path`] below is this module's own small,
//! streaming (not whole-file-buffering) equivalent, reading through the
//! same `FileSystem::open_read` trait method `hash_file` itself uses, in
//! [`HASH_CHUNK_BYTES`]-sized chunks (mirroring `duet_ops::executor`'s own
//! `COPY_BUFFER_BYTES` for consistency, not reusable directly -- that
//! constant is private to a different crate). Spawned via
//! `self.tokio_handle.spawn(...)`, with the result delivered back onto
//! this entity through a `tokio::sync::oneshot` + `cx.spawn` bridge --
//! the exact shape `copy_move_dialog.rs`'s own `confirm()` already
//! establishes for off-UI-thread work whose result needs to land back on
//! a GPUI entity.
//!
//! # The "10k-conflict run is survivable using apply-to-all" AC
//!
//! This is a statement about the *existing*, already-tested sticky
//! `ConflictScope::AllRemaining` mechanism
//! (`duet_ops::executor::resolve_conflict`'s own `ctx.sticky_conflict`)
//! answering every conflict after the first without ever consulting this
//! resolver again -- not something this module has to build. What this
//! module *must not* do is undermine it: nothing here keeps an
//! unboundedly-growing history of past conflicts (`ConflictDialogState`
//! only ever represents the *current* prompt; `Workspace::
//! pending_conflict_requests`'s own doc comment explains why its queue
//! depth is bounded by concurrently-running-job count, not by
//! conflict-per-job count).

use std::sync::Arc;

use duet_ops::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, ConflictScope,
};
use duet_types::VPath;
use duet_vfs::{FileSystem, LocalFs};
use duet_widgets::input::{Escape, IndentInline, Input, InputEvent, InputState, OutdentInline};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _, Render,
    Styled as _, Subscription, WeakEntity, Window, actions, div, px,
};
use tokio::io::AsyncReadExt;

use crate::file_table::{write_byte_count, write_date};
use crate::workspace::Workspace;

/// One conflict awaiting a live answer -- the request half of
/// [`InteractiveConflictResolver`]'s bridge (see the module doc comment).
/// `response_tx` is a plain, blocking `std::sync::mpsc::Sender`, not a
/// `tokio` channel -- see the module doc comment's "Bridging" section for
/// why.
pub(crate) struct ConflictRequest {
    pub(crate) prompt: ConflictPrompt,
    pub(crate) response_tx: std::sync::mpsc::Sender<ConflictResolution>,
}

/// The live, UI-backed [`ConflictResolver`] `Workspace::new` constructs
/// once and threads into every copy/move dialog's `QueueManager::enqueue`
/// call (`CopyMoveDialogState::confirm` passes
/// `Some(Arc::clone(&self.conflict_resolver))` instead of T-5.2.1's
/// original, always-`None` placeholder). See the module doc comment for
/// the full request/response bridge this implements.
pub(crate) struct InteractiveConflictResolver {
    request_tx: tokio::sync::mpsc::UnboundedSender<ConflictRequest>,
}

impl InteractiveConflictResolver {
    pub(crate) fn new(request_tx: tokio::sync::mpsc::UnboundedSender<ConflictRequest>) -> Self {
        Self { request_tx }
    }
}

impl ConflictResolver for InteractiveConflictResolver {
    fn resolve(&self, prompt: &ConflictPrompt) -> ConflictResolution {
        let (response_tx, response_rx) = std::sync::mpsc::channel();
        let request = ConflictRequest {
            prompt: prompt.clone(),
            response_tx,
        };
        if self.request_tx.send(request).is_err() {
            // The UI-side consumer is gone (the app is shutting down, or
            // `Workspace` itself has already been torn down) -- fail safe
            // rather than hang the executor thread forever with nothing
            // left to ever answer it.
            return ConflictResolution::once(ConflictPolicy::Skip);
        }
        tokio::task::block_in_place(|| {
            response_rx
                .recv()
                .unwrap_or_else(|_| ConflictResolution::once(ConflictPolicy::Skip))
        })
    }
}

// See the module doc comment's "Keybindings" section for the full table
// and the reasoning behind every non-TC-documented choice.
actions!(
    duet_conflict_dialog,
    [
        ConflictOverwriteThis,
        ConflictOverwriteAll,
        ConflictSkipThis,
        ConflictSkipAll,
        ConflictRename,
        ConflictOverwriteIfOlderThis,
        ConflictOverwriteIfOlderAll,
        ConflictOverwriteIfDifferentSizeThis,
        ConflictOverwriteIfDifferentSizeAll,
        ConflictAutoRenameThis,
        ConflictAutoRenameAll,
        ConflictAbortThis,
        ConflictAbortAll,
        ConflictHashSource,
        ConflictHashDest,
        ConflictFocusNext,
        ConflictFocusPrev,
    ]
);

/// Registers this dialog's own keybindings, scoped to `"ConflictDialog"`
/// (set on [`ConflictDialogState::render`]'s own root `div`). Called once
/// from `workspace::run` (and from the test module's own `with_workspace`
/// harness), alongside every other `bind_*_keys` function.
pub(crate) fn bind_conflict_dialog_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("alt-y", ConflictOverwriteThis, Some("ConflictDialog")),
        KeyBinding::new("alt-a", ConflictOverwriteAll, Some("ConflictDialog")),
        KeyBinding::new("alt-n", ConflictSkipThis, Some("ConflictDialog")),
        KeyBinding::new("escape", ConflictSkipThis, Some("ConflictDialog")),
        KeyBinding::new("alt-s", ConflictSkipAll, Some("ConflictDialog")),
        KeyBinding::new("alt-r", ConflictRename, Some("ConflictDialog")),
        KeyBinding::new(
            "alt-o",
            ConflictOverwriteIfOlderThis,
            Some("ConflictDialog"),
        ),
        KeyBinding::new(
            "alt-shift-o",
            ConflictOverwriteIfOlderAll,
            Some("ConflictDialog"),
        ),
        KeyBinding::new(
            "alt-d",
            ConflictOverwriteIfDifferentSizeThis,
            Some("ConflictDialog"),
        ),
        KeyBinding::new(
            "alt-shift-d",
            ConflictOverwriteIfDifferentSizeAll,
            Some("ConflictDialog"),
        ),
        KeyBinding::new("alt-u", ConflictAutoRenameThis, Some("ConflictDialog")),
        KeyBinding::new("alt-shift-u", ConflictAutoRenameAll, Some("ConflictDialog")),
        KeyBinding::new("alt-b", ConflictAbortThis, Some("ConflictDialog")),
        KeyBinding::new("alt-shift-b", ConflictAbortAll, Some("ConflictDialog")),
        KeyBinding::new("alt-h", ConflictHashSource, Some("ConflictDialog")),
        KeyBinding::new("alt-shift-h", ConflictHashDest, Some("ConflictDialog")),
        KeyBinding::new("tab", ConflictFocusNext, Some("ConflictDialog")),
        KeyBinding::new("shift-tab", ConflictFocusPrev, Some("ConflictDialog")),
    ]);
}

/// Chunk size for [`hash_one_path`] -- mirrors `duet_ops::executor`'s own
/// `COPY_BUFFER_BYTES` (not reusable directly, see the module doc
/// comment's "On-demand hashing" section): 1 MiB, large enough to amortise
/// syscall overhead, small enough that a multi-gigabyte file never needs a
/// multi-gigabyte allocation just to be hashed.
const HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// Streams `path` through a BLAKE3 hasher in [`HASH_CHUNK_BYTES`] chunks,
/// via the same `FileSystem::open_read` trait method `duet_ops::executor
/// ::hash_file` itself uses. A free, `FileSystem`-generic async function
/// (not a method) so it's directly unit-testable against a real `LocalFs`
/// and a tempfile with no GPUI/`ConflictDialogState` involved at all.
async fn hash_one_path(fs: &dyn FileSystem, path: &VPath) -> Result<blake3::Hash, String> {
    let mut reader = fs.open_read(path).await.map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    loop {
        let n = reader.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Which side of the conflict an on-demand hash request/state applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Source,
    Dest,
}

/// One side's on-demand hash state -- see the module doc comment's
/// "On-demand hashing" section.
#[derive(Debug, Clone)]
enum HashState {
    Idle,
    Hashing,
    Done(blake3::Hash),
    Failed(String),
}

/// Builds the `RenameTarget` alternate path from the conflicting
/// destination's parent directory plus a user-typed file name -- pure
/// logic, factored out so it's unit-testable with no GPUI/`InputState`
/// involved, same reasoning `copy_move_dialog::split_parent_prefix`/
/// `complete_against_model` are their own free functions for. Rejects an
/// empty name and one containing `/` (which would silently escape the
/// destination's own directory, changing which conflict is even being
/// resolved) before ever reaching `VPath::join`'s own, stricter
/// validation.
fn rename_alternate(dest: &VPath, new_name: &str) -> Result<VPath, String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err("Enter a name for the new destination.".to_string());
    }
    if trimmed.contains('/') {
        return Err("The name can't contain \"/\".".to_string());
    }
    let Some(parent) = dest.parent() else {
        return Err("The destination has no parent directory to rename within.".to_string());
    };
    parent
        .join(trimmed)
        .map_err(|e| format!("\u{201c}{trimmed}\u{201d} isn't a valid name: {e}"))
}

/// T-5.2.3's conflict dialog: side-by-side source/destination metadata, an
/// on-demand hash per side, and every [`ConflictPolicy`] reachable by
/// keyboard (and mouse). See the module doc comment for the full
/// architecture.
pub(crate) struct ConflictDialogState {
    prompt: ConflictPrompt,
    /// Sends this dialog's one answer back to the blocked executor thread
    /// -- see [`InteractiveConflictResolver::resolve`]. `Option` so
    /// [`Self::answer`] can `.take()` it, making a second answer
    /// impossible even if some future bug fired two action handlers for
    /// the same dialog (the underlying `std::sync::mpsc::Sender` doesn't
    /// itself forbid a second `send`, it would just be silently ignored
    /// by a `Receiver` that already got its one value -- `.take()` turns
    /// that into a compile-time-checked impossibility instead).
    response_tx: Option<std::sync::mpsc::Sender<ConflictResolution>>,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    focus_handle: FocusHandle,
    source_hash: HashState,
    dest_hash: HashState,
    /// `true` once `Alt+R` has been pressed at least once -- the
    /// `RenameTarget` sub-field (`rename_input`) is shown exactly when
    /// this is `true`. Never reset back to `false` by anything other than
    /// this whole dialog closing (there's no "cancel just the rename
    /// sub-mode" action -- Escape resolves the *whole* conflict as `Skip`,
    /// per the module doc comment's keybinding table).
    renaming: bool,
    /// Lazily constructed the first time `Alt+R` fires -- see the module
    /// doc comment's "Why the dialog can't be constructed with a live
    /// `Window`" section for why this can't simply be built in `Self::new`.
    rename_input: Option<Entity<InputState>>,
    rename_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// A plain-`fn` policy/scope handler ([`ConflictDialogState::overwrite_this`]
/// and its eleven siblings) -- named so [`ConflictDialogState::action_span`]/
/// [`ConflictDialogState::policy_row`]'s own signatures stay readable
/// (clippy's `type_complexity` lint flags the bare, unaliased form).
type PolicyHandler = fn(&mut ConflictDialogState, &mut Window, &mut Context<ConflictDialogState>);

impl ConflictDialogState {
    pub(crate) fn new(
        prompt: ConflictPrompt,
        response_tx: std::sync::mpsc::Sender<ConflictResolution>,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            prompt,
            response_tx: Some(response_tx),
            workspace,
            tokio_handle,
            focus_handle: cx.focus_handle(),
            source_hash: HashState::Idle,
            dest_hash: HashState::Idle,
            renaming: false,
            rename_input: None,
            rename_error: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Test-only accessors -- same reasoning as `CopyMoveDialogState`'s
    /// own `#[cfg(test)]` block: `workspace.rs`'s tests need to read this
    /// otherwise-private state without a public API surface production
    /// code would ever need.
    #[cfg(test)]
    pub(crate) fn prompt(&self) -> &ConflictPrompt {
        &self.prompt
    }

    #[cfg(test)]
    pub(crate) fn renaming(&self) -> bool {
        self.renaming
    }

    #[cfg(test)]
    pub(crate) fn rename_input(&self) -> Option<&Entity<InputState>> {
        self.rename_input.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn rename_error(&self) -> Option<&str> {
        self.rename_error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn source_hash_digest(&self) -> Option<blake3::Hash> {
        match &self.source_hash {
            HashState::Done(h) => Some(*h),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn dest_hash_digest(&self) -> Option<blake3::Hash> {
        match &self.dest_hash {
            HashState::Done(h) => Some(*h),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn source_hash_is_pending(&self) -> bool {
        matches!(self.source_hash, HashState::Hashing)
    }

    /// The single send-the-answer-and-close path every policy handler
    /// below funnels through. A no-op if this dialog has already answered
    /// (see `response_tx`'s own doc comment for why that's an
    /// impossibility this guards defensively, not an expected path).
    fn answer(
        &mut self,
        policy: ConflictPolicy,
        scope: ConflictScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.answer_with(
            ConflictResolution {
                policy,
                scope,
                alternate: None,
            },
            window,
            cx,
        );
    }

    fn answer_with(
        &mut self,
        resolution: ConflictResolution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(response_tx) = self.response_tx.take() else {
            return;
        };
        let _ = response_tx.send(resolution);
        self.close(window, cx);
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_conflict_dialog(window, cx);
        });
    }

    // -- The twelve this-only/all-remaining policy handlers -------------

    fn overwrite_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::Overwrite,
            ConflictScope::ThisOnly,
            window,
            cx,
        );
    }
    fn overwrite_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::Overwrite,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }
    fn skip_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(ConflictPolicy::Skip, ConflictScope::ThisOnly, window, cx);
    }
    fn skip_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::Skip,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }
    fn overwrite_if_older_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::OverwriteIfOlder,
            ConflictScope::ThisOnly,
            window,
            cx,
        );
    }
    fn overwrite_if_older_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::OverwriteIfOlder,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }
    fn overwrite_if_different_size_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::OverwriteIfDifferentSize,
            ConflictScope::ThisOnly,
            window,
            cx,
        );
    }
    fn overwrite_if_different_size_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::OverwriteIfDifferentSize,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }
    fn auto_rename_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::AutoRename,
            ConflictScope::ThisOnly,
            window,
            cx,
        );
    }
    fn auto_rename_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::AutoRename,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }
    fn abort_this(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(ConflictPolicy::Abort, ConflictScope::ThisOnly, window, cx);
    }
    fn abort_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer(
            ConflictPolicy::Abort,
            ConflictScope::AllRemaining,
            window,
            cx,
        );
    }

    // -- RenameTarget -----------------------------------------------------

    /// `Alt+R`: shows (constructing on first use) the rename sub-field and
    /// focuses it, defaulting its text to the conflicting destination's
    /// current file name -- a reasonable starting point for editing, not
    /// a real answer on its own (nothing is sent until Enter).
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename_input.is_none() {
            let default_name = self
                .prompt
                .dest
                .inner()
                .file_name()
                .unwrap_or_default()
                .to_string();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(default_name)
                    .placeholder("New name")
            });
            let subscription = cx.subscribe_in(&input, window, Self::on_rename_event);
            self._subscriptions.push(subscription);
            self.rename_input = Some(input);
        }
        self.renaming = true;
        self.rename_error = None;
        if let Some(input) = self.rename_input.clone() {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn on_rename_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.confirm_rename(window, cx);
        }
    }

    fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.rename_input.clone() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        match rename_alternate(&self.prompt.dest, &text) {
            Ok(alternate) => self.answer_with(ConflictResolution::rename_to(alternate), window, cx),
            Err(message) => {
                self.rename_error = Some(message);
                cx.notify();
            }
        }
    }

    // -- Tab / Shift+Tab: toggle focus between the root and the rename field

    /// Both `Tab` and `Shift+Tab` perform the same toggle -- see the
    /// module doc comment's "Keybindings" section for why that's a
    /// disclosed simplification, not a bug, with only two focusable
    /// regions in this dialog.
    fn cycle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.rename_input.clone() else {
            return;
        };
        if self.focus_handle.is_focused(window) {
            input.update(cx, |state, cx| state.focus(window, cx));
        } else {
            window.focus(&self.focus_handle);
        }
    }

    // -- On-demand hashing --------------------------------------------------

    fn hash_source(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.start_hash(Side::Source, cx);
    }

    fn hash_dest(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.start_hash(Side::Dest, cx);
    }

    fn start_hash(&mut self, side: Side, cx: &mut Context<Self>) {
        let already_hashing = matches!(
            match side {
                Side::Source => &self.source_hash,
                Side::Dest => &self.dest_hash,
            },
            HashState::Hashing
        );
        if already_hashing {
            return;
        }
        match side {
            Side::Source => self.source_hash = HashState::Hashing,
            Side::Dest => self.dest_hash = HashState::Hashing,
        }
        cx.notify();

        let path = match side {
            Side::Source => self.prompt.source.clone(),
            Side::Dest => self.prompt.dest.clone(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let result = hash_one_path(fs.as_ref(), &path).await;
            let _ = tx.send(result);
        });

        let this_entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = rx.await;
            let _ = this_entity.update(cx, |this, cx| {
                let state = match result {
                    Ok(Ok(hash)) => HashState::Done(hash),
                    Ok(Err(message)) => HashState::Failed(message),
                    Err(_) => {
                        HashState::Failed("the hashing task was dropped before completing".into())
                    }
                };
                match side {
                    Side::Source => this.source_hash = state,
                    Side::Dest => this.dest_hash = state,
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// One clickable text span, wired to both a mouse click and (via
    /// [`bind_conflict_dialog_keys`]) a keyboard shortcut shown in
    /// `label` -- the "policy buttons" the module doc comment/this task's
    /// own AC calls for. A plain associated function (not a method) so it
    /// can take `cx: &mut Context<Self>` without also needing a `&self`
    /// borrow that would conflict with it.
    fn action_span(
        cx: &mut Context<Self>,
        id: (&'static str, usize),
        label: String,
        handler: PolicyHandler,
    ) -> impl IntoElement {
        div().id(id).cursor_pointer().child(label).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, window, cx| handler(this, window, cx)),
        )
    }

    /// One policy's full row: its name, plus one [`Self::action_span`] per
    /// scope it supports (one for `RenameTarget`/`AutoRename`'s
    /// this-only-only callers via `all_hint: None`, two otherwise).
    #[allow(clippy::too_many_arguments)]
    fn policy_row(
        cx: &mut Context<Self>,
        id_prefix: &'static str,
        name: &str,
        this_hint: &str,
        this_handler: PolicyHandler,
        all: Option<(&str, PolicyHandler)>,
    ) -> impl IntoElement {
        let this_span = Self::action_span(cx, (id_prefix, 0), this_hint.to_string(), this_handler);
        let mut controls = div().flex().gap_3().child(this_span);
        if let Some((all_hint, all_handler)) = all {
            controls = controls.child(Self::action_span(
                cx,
                (id_prefix, 1),
                all_hint.to_string(),
                all_handler,
            ));
        }
        div()
            .flex()
            .justify_between()
            .gap_3()
            .child(div().child(name.to_string()))
            .child(controls)
    }
}

impl Focusable for ConflictDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn hash_line(label: &str, state: &HashState) -> String {
    match state {
        HashState::Idle => format!("{label}: (not computed)"),
        HashState::Hashing => format!("{label}: computing\u{2026}"),
        HashState::Done(hash) => format!("{label}: {hash}"),
        HashState::Failed(message) => format!("{label}: failed \u{2014} {message}"),
    }
}

fn metadata_lines(meta: &duet_types::Metadata) -> (String, String) {
    let mut size = String::new();
    write_byte_count(&mut size, meta.size);
    let mut modified = String::new();
    write_date(&mut modified, meta.modified.map(|t| t.secs).unwrap_or(0));
    (size, modified)
}

impl Render for ConflictDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Extracted as plain `Hsla` values (not a held `&TokenPalette`)
        // specifically so this doesn't keep an immutable borrow of `cx`
        // alive across the `cx.listener(...)` calls further down --
        // `TokenPalette::current` borrows `cx`, and (unlike
        // `copy_move_dialog.rs`'s own single, early use of it) this
        // render body's last read of a token color comes *after* several
        // `cx.listener`-needing calls, so holding onto the reference
        // itself would conflict with them under NLL.
        let error_color = TokenPalette::current(cx).color.error;
        let statusbar_fg = TokenPalette::current(cx).color.statusbar_fg;
        let dest_name = self
            .prompt
            .dest
            .inner()
            .file_name()
            .unwrap_or("")
            .to_string();

        let (source_size, source_modified) = metadata_lines(&self.prompt.source_meta);
        let (dest_size, dest_modified) = metadata_lines(&self.prompt.dest_meta);
        let source_path = self.prompt.source.to_string();
        let dest_path = self.prompt.dest.to_string();
        let source_hash_line = hash_line("Hash", &self.source_hash);
        let dest_hash_line = hash_line("Hash", &self.dest_hash);

        let metadata_row = div()
            .flex()
            .gap_4()
            .text_size(px(11.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .flex_1()
                    .child(div().font_weight(FontWeight::BOLD).child("Source"))
                    .child(div().child(source_path))
                    .child(div().child(format!("Size: {source_size}")))
                    .child(div().child(format!("Modified: {source_modified}")))
                    .child(div().child(source_hash_line))
                    .child(Self::action_span(
                        cx,
                        ("conflict-hash", 0),
                        "Hash source (Alt+H)".to_string(),
                        Self::hash_source,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .flex_1()
                    .child(div().font_weight(FontWeight::BOLD).child("Destination"))
                    .child(div().child(dest_path))
                    .child(div().child(format!("Size: {dest_size}")))
                    .child(div().child(format!("Modified: {dest_modified}")))
                    .child(div().child(dest_hash_line))
                    .child(Self::action_span(
                        cx,
                        ("conflict-hash", 1),
                        "Hash destination (Alt+Shift+H)".to_string(),
                        Self::hash_dest,
                    )),
            );

        let policy_rows = div()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(11.))
            .child(Self::policy_row(
                cx,
                "conflict-overwrite",
                "Overwrite",
                "Alt+Y",
                Self::overwrite_this,
                Some(("Alt+A (all)", Self::overwrite_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-skip",
                "Skip",
                "Alt+N / Esc",
                Self::skip_this,
                Some(("Alt+S (all)", Self::skip_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-older",
                "Overwrite if older",
                "Alt+O",
                Self::overwrite_if_older_this,
                Some(("Alt+Shift+O (all)", Self::overwrite_if_older_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-diffsize",
                "Overwrite if different size",
                "Alt+D",
                Self::overwrite_if_different_size_this,
                Some(("Alt+Shift+D (all)", Self::overwrite_if_different_size_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-autorename",
                "Auto-rename",
                "Alt+U",
                Self::auto_rename_this,
                Some(("Alt+Shift+U (all)", Self::auto_rename_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-abort",
                "Abort job",
                "Alt+B",
                Self::abort_this,
                Some(("Alt+Shift+B (all)", Self::abort_all)),
            ))
            .child(Self::policy_row(
                cx,
                "conflict-rename",
                "Rename target (this conflict only)",
                "Alt+R",
                Self::start_rename,
                None,
            ));

        div()
            .id("conflict-dialog")
            .key_context("ConflictDialog")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(format!("\u{201c}{dest_name}\u{201d} already exists")),
            )
            .child(metadata_row)
            .child(policy_rows)
            .when(self.renaming, |this| {
                let Some(input) = self.rename_input.clone() else {
                    return this;
                };
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(11.))
                                .child("New name (Enter to confirm):"),
                        )
                        .child(Input::new(&input))
                        .when_some(self.rename_error.clone(), |this, message| {
                            this.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(error_color)
                                    .child(message),
                            )
                        }),
                )
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(statusbar_fg)
                    .child("Tab/Shift+Tab moves focus \u{2022} Enter confirms a rename"),
            )
            .on_action(cx.listener(|this, _: &ConflictOverwriteThis, window, cx| {
                this.overwrite_this(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictOverwriteAll, window, cx| {
                this.overwrite_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictSkipThis, window, cx| {
                this.skip_this(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictSkipAll, window, cx| {
                this.skip_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictRename, window, cx| {
                this.start_rename(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ConflictOverwriteIfOlderThis, window, cx| {
                    this.overwrite_if_older_this(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &ConflictOverwriteIfOlderAll, window, cx| {
                    this.overwrite_if_older_all(window, cx);
                }),
            )
            .on_action(cx.listener(
                |this, _: &ConflictOverwriteIfDifferentSizeThis, window, cx| {
                    this.overwrite_if_different_size_this(window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &ConflictOverwriteIfDifferentSizeAll, window, cx| {
                    this.overwrite_if_different_size_all(window, cx);
                },
            ))
            .on_action(cx.listener(|this, _: &ConflictAutoRenameThis, window, cx| {
                this.auto_rename_this(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictAutoRenameAll, window, cx| {
                this.auto_rename_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictAbortThis, window, cx| {
                this.abort_this(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictAbortAll, window, cx| {
                this.abort_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictHashSource, window, cx| {
                this.hash_source(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictHashDest, window, cx| {
                this.hash_dest(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictFocusNext, window, cx| {
                this.cycle_focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ConflictFocusPrev, window, cx| {
                this.cycle_focus(window, cx);
            }))
            // Bubbled from `rename_input`'s own "Input"-context bindings
            // once it has focus -- see the module doc comment's
            // "Keybindings" section for why Tab/Shift+Tab/Escape need both
            // a direct binding (root focused) and this bubbled-action
            // catch (rename field focused).
            .on_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.cycle_focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                this.cycle_focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                this.skip_this(window, cx);
            }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use duet_types::{EntryKind, Metadata, Timestamp, UnixPathBuf};

    use super::*;

    fn sample_prompt() -> ConflictPrompt {
        let mut source_meta = Metadata::minimal(EntryKind::File);
        source_meta.size = 10;
        source_meta.modified = Some(Timestamp::new(1_700_000_000, 0));
        let mut dest_meta = Metadata::minimal(EntryKind::File);
        dest_meta.size = 20;
        dest_meta.modified = Some(Timestamp::new(1_600_000_000, 0));
        ConflictPrompt {
            step_index: 0,
            source: VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
            dest: VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
            source_meta,
            dest_meta,
        }
    }

    // -- InteractiveConflictResolver::resolve ------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_blocks_the_caller_until_a_response_arrives() {
        let (request_tx, mut request_rx) =
            tokio::sync::mpsc::unbounded_channel::<ConflictRequest>();
        let resolver = Arc::new(InteractiveConflictResolver::new(request_tx));
        let prompt = sample_prompt();
        let answered = Arc::new(AtomicBool::new(false));
        let answered_for_task = answered.clone();

        let resolver_for_call = resolver.clone();
        let handle = tokio::spawn(async move {
            let resolution = resolver_for_call.resolve(&prompt);
            answered_for_task.store(true, Ordering::SeqCst);
            resolution
        });

        let request = request_rx.recv().await.expect("a request must arrive");
        // Generous margin against scheduler jitter, not a tight race --
        // proves `resolve()` is still blocked with no response sent yet.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !answered.load(Ordering::SeqCst),
            "resolve() must still be blocked before a response is sent"
        );

        let _ = request
            .response_tx
            .send(ConflictResolution::apply_to_all(ConflictPolicy::Overwrite));

        let resolution = handle.await.expect("the resolving task must not panic");
        assert_eq!(resolution.policy, ConflictPolicy::Overwrite);
        assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        assert!(answered.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_falls_back_to_skip_if_the_response_side_is_dropped_without_answering() {
        let (request_tx, mut request_rx) =
            tokio::sync::mpsc::unbounded_channel::<ConflictRequest>();
        let resolver = InteractiveConflictResolver::new(request_tx);
        let prompt = sample_prompt();

        let handle = tokio::spawn(async move { resolver.resolve(&prompt) });
        let request = request_rx.recv().await.expect("a request must arrive");
        drop(request.response_tx); // never answered

        let resolution = handle
            .await
            .expect("resolve() must not panic on a dropped response sender");
        assert_eq!(resolution, ConflictResolution::once(ConflictPolicy::Skip));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_falls_back_to_skip_if_no_ui_consumer_is_listening() {
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel::<ConflictRequest>();
        drop(request_rx);
        let resolver = InteractiveConflictResolver::new(request_tx);
        let prompt = sample_prompt();

        let resolution = tokio::spawn(async move { resolver.resolve(&prompt) })
            .await
            .expect("resolve() must not panic with no listener at all");
        assert_eq!(resolution, ConflictResolution::once(ConflictPolicy::Skip));
    }

    // -- hash_one_path -------------------------------------------------------

    #[tokio::test]
    async fn hash_one_path_matches_a_directly_computed_blake3_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known.bin");
        let content = b"the quick brown fox jumps over the lazy dog".repeat(1000);
        std::fs::write(&path, &content).unwrap();

        let fs = LocalFs;
        let vpath = crate::file_table::local_vpath(&path).unwrap();
        let digest = hash_one_path(&fs, &vpath)
            .await
            .expect("hashing a real, readable file must succeed");

        assert_eq!(digest, blake3::hash(&content));
    }

    #[tokio::test]
    async fn hash_one_path_reports_an_error_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.bin");
        let fs = LocalFs;
        let vpath = crate::file_table::local_vpath(&path).unwrap();
        assert!(hash_one_path(&fs, &vpath).await.is_err());
    }

    // -- rename_alternate ------------------------------------------------

    #[test]
    fn rename_alternate_builds_a_sibling_path_from_a_plain_name() {
        let dest = VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap());
        let alternate = rename_alternate(&dest, "b.txt").unwrap();
        assert_eq!(alternate.to_string(), "file:///tmp/dst/b.txt");
    }

    #[test]
    fn rename_alternate_rejects_an_empty_name() {
        let dest = VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap());
        assert!(rename_alternate(&dest, "   ").is_err());
    }

    #[test]
    fn rename_alternate_rejects_a_name_containing_a_slash() {
        let dest = VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap());
        assert!(rename_alternate(&dest, "sub/b.txt").is_err());
    }

    #[test]
    fn rename_alternate_trims_surrounding_whitespace() {
        let dest = VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap());
        let alternate = rename_alternate(&dest, "  b.txt  ").unwrap();
        assert_eq!(alternate.to_string(), "file:///tmp/dst/b.txt");
    }

    // -- hash_line / metadata_lines --------------------------------------

    #[test]
    fn hash_line_reports_each_state_distinctly() {
        assert_eq!(hash_line("Hash", &HashState::Idle), "Hash: (not computed)");
        assert_eq!(
            hash_line("Hash", &HashState::Hashing),
            "Hash: computing\u{2026}"
        );
        assert_eq!(
            hash_line("Hash", &HashState::Failed("boom".to_string())),
            "Hash: failed \u{2014} boom"
        );
    }
}
