// SPDX-License-Identifier: MIT
//! T-5.2.8's `Ctrl+A` attributes/permissions dialog (FR-OPS-12).
//!
//! # `Ctrl+A` is a verified TC binding, and deliberately not "select all"
//!
//! `docs/keymap-tc.csv` row 16 catalogues `Ctrl+A` -> `ops.change_attributes`
//! at "known" confidence, with the note "Single most-cited keybinding
//! 'gotcha' in TC". Unlike T-5.2.7's symlink/hardlink chords, this is not
//! this codebase's own reasonable default -- it is the surveyed Total
//! Commander chord, and it is bound in `workspace::bind_workspace_keys`'s
//! `"Workspace"` context alongside every other overlay-opening chord.
//!
//! Nothing is displaced by it: this app binds "select all" to `Ctrl++`
//! (`file_table::SelectAll`), precisely to leave `Ctrl+A` free for the TC
//! meaning. `gpui-component`'s own `InputState` *does* bind `Ctrl+A` to a
//! select-all inside its `"Input"` key context, which is both harmless and
//! correct: that context is strictly deeper than `"Workspace"`, so while a
//! text field has focus `Ctrl+A` selects that field's text (including
//! inside this very dialog), and only a focused *panel* opens the dialog.
//!
//! `docs/commands.md` catalogues the same command as `file.attributes`
//! (an id mismatch with the CSV's `ops.change_attributes` that is a known,
//! pre-existing inconsistency elsewhere in this repo, not something this
//! module tries to reconcile). Its two neighbouring rows,
//! `file.chmod_recursive` ("runs through the operation queue") and
//! `file.set_timestamps`, are this same dialog's own internal behaviour --
//! the recursive toggle and the two timestamp fields -- not separate
//! commands needing top-level chords of their own.
//!
//! # Octal and symbolic agree, live, in both directions
//!
//! This task's AC, verbatim: "octal and symbolic entry agree." Typing
//! `644` in the octal field updates the symbolic field to `rw-r--r--` as
//! you type, and vice versa. Both directions are `cx.subscribe_in`
//! subscriptions on `InputEvent::Change`, running the four free functions
//! below ([`parse_octal_mode`]/[`format_symbolic_mode`] one way,
//! [`parse_symbolic_mode`]/[`format_octal_mode`] the other).
//!
//! ## Why the loop guard is value-based and not a `syncing: bool`
//!
//! `InputState::set_value` emits `InputEvent::Change` itself (verified by
//! reading `gpui-component-0.5.1/src/input/state.rs`: `set_value` ->
//! `replace_text` -> `replace_text_in_range`, which ends in an
//! unconditional `cx.emit(InputEvent::Change)`; the `silent` in
//! `replace_text_in_range_silent` suppresses only the completion trigger,
//! not the event). So writing the derived value into the *other* field
//! re-enters the mirror subscription, and without a guard the two would
//! ping-pong forever.
//!
//! The obvious guard -- set a `syncing: bool` before `set_value`, clear it
//! after, bail out at the top of both handlers -- **does not work in
//! GPUI**, and it is worth stating why rather than shipping a flag that
//! silently isn't doing anything. `cx.emit` does not call subscribers
//! synchronously; it pushes an effect that GPUI delivers later, during the
//! same `flush_effects` pass that is already delivering the event we are
//! currently handling. The mirror handler therefore runs *after* the
//! `syncing = false` line, with the flag already back to `false`.
//!
//! What is used instead is idempotence: a handler computes the value the
//! other field *should* hold and writes it only if the other field does
//! not already hold exactly that. Termination is guaranteed, not merely
//! likely, because both renderings are canonical -- `format_octal_mode`
//! and `format_symbolic_mode` are fixed points of their own
//! parse/format round trip (`format(parse(format(x))) == format(x)`, unit
//! -tested below), so the second propagation hop always finds the other
//! field already equal and stops. Worst case is two hops per keystroke.
//!
//! Garbage never propagates: a field whose text doesn't parse leaves the
//! other one exactly as it was, so a half-typed `rw-` doesn't blank out a
//! perfectly good octal value. Clearing a field *does* propagate, though,
//! and deliberately: an empty field means "leave this alone" (see below),
//! and the two mode fields must not disagree about that.
//!
//! # Empty means "don't touch", for every field
//!
//! `MetaPatch`'s own convention is that an absent field is "don't change
//! it," never "clear it" -- so a blank field here maps to `None` in the
//! patch, and a patch with nothing in it is a no-op close rather than an
//! enqueued job (`crate::rename_dialog`'s "confirming with nothing changed
//! just closes" precedent). That one rule covers both the single-target
//! case (the user cleared a pre-filled field) and the multi-target case
//! (there was never a single current value to pre-fill from -- see
//! [`AttributesDialogState::new`]), so `confirm` needs no multi-versus-
//! single branch of its own.
//!
//! # Scope: mode and timestamps, not ownership or xattrs
//!
//! `MetaPatch` also carries `uid`/`gid`/`set_xattrs`/`remove_xattrs`. This
//! dialog leaves all four untouched (i.e. `None`/empty, i.e. unchanged),
//! matching this task's own AC, which names only "octal and symbolic
//! entry" and "timestamp editing". A `chown` UI has its own real design
//! questions (name-versus-uid resolution, the privilege story) and an
//! xattr editor is a different surface again; neither is smuggled in here.

use std::path::PathBuf;
use std::sync::Arc;

use duet_ops::{ConflictResolver, JobKind, QueueManager, plan_attributes};
use duet_types::{MetaPatch, Timestamp, VPath};
use duet_widgets::input::{Escape, Input, InputEvent, InputState};
use duet_widgets::theme::TokenPalette;
use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity,
    Window, actions, div, px,
};

use crate::dialog_job::{report_job_outcome, spawn_plan_and_enqueue};
use crate::file_table::{civil_from_unix, unix_from_civil, write_date};
use crate::workspace::{NoticeLevel, Workspace};

// This dialog's one option. `Ctrl+R` is **this codebase's own reasonable
// default, not a verified TC chord**: `docs/keymap-tc.csv` has no row for
// a recursive-apply toggle at all (`docs/commands.md`'s
// `file.chmod_recursive` row carries no keybinding either), so there is no
// TC binding to match -- the same disclosed-default situation
// `copy_move_dialog`'s own `Ctrl+1/2/3`/`Ctrl+K`/`Ctrl+J` and
// `delete_dialog`'s `Ctrl+T` are already in. Chosen for the obvious
// mnemonic ("Recursive") and confirmed unclaimed against every other
// `KeyBinding::new` call site in this crate.
//
// Scoped to `"AttributesDialog"` (this view's own key context), so it
// could not collide with anything even if it wanted to: `docs/keymap-tc.csv`
// row 15 gives `Ctrl+R` to `panel.reread`, which -- if it is ever bound --
// belongs in the `"FileTable"`/`"Panel"` context, i.e. never in the
// dispatch path while this overlay holds focus. `InputState`'s own
// `"Input"` context, the one context *deeper* than this dialog's, does not
// bind `Ctrl+R` (verified by reading `gpui-component-0.5.1/src/input/
// state.rs`'s `init`), so the chord reaches this dialog even with a text
// field focused -- which is the only way it is ever used.
actions!(duet_attributes_dialog, [ToggleRecursiveApply]);

/// Registers this dialog's own keybinding, scoped to `"AttributesDialog"`
/// (set on [`AttributesDialogState::render`]'s own root `div`). Called once
/// from `workspace::run`, alongside every other `bind_*_keys` function.
/// Enter and Escape need no binding here, for the reason
/// `copy_move_dialog.rs`'s module doc comment sets out at length.
pub(crate) fn bind_attributes_dialog_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "ctrl-r",
        ToggleRecursiveApply,
        Some("AttributesDialog"),
    )]);
}

/// The single-target `stat` result [`Workspace::open_attributes_dialog`]
/// fetches off the UI thread so the dialog can open showing the entry's
/// *current* attributes rather than four blank fields.
///
/// Plain `Option`-of-scalars rather than a whole `duet_types::Metadata`:
/// these three values are all this dialog can display or edit, and keeping
/// the type this narrow means the async continuation carries nothing it
/// doesn't use.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AttributesPrefill {
    /// Full POSIX mode as `stat` reported it, file-type bits included --
    /// [`AttributesDialogState::new`] masks it down to `0o7777` itself.
    pub(crate) mode: Option<u32>,
    pub(crate) modified_secs: Option<i64>,
    pub(crate) accessed_secs: Option<i64>,
}

/// `Ctrl+A`'s dialog. See the module doc comment for the full architecture;
/// `Workspace::open_attributes_dialog` is the only constructor call site
/// and has already resolved everything below.
pub(crate) struct AttributesDialogState {
    /// Every selected entry (or the cursor row, if nothing is selected) --
    /// `crate::copy_move_dialog::resolve_source_names`' own
    /// selection-or-cursor fallback, the same one F5/F6/F8 use, matching
    /// `docs/commands.md`'s `panel && selection.nonempty` precondition for
    /// `file.attributes`.
    targets: Vec<VPath>,
    /// The cursor/selection's own basename when there is exactly one
    /// target, for the title line. `None` for a multi-selection, which the
    /// title renders as a count instead.
    single_name: Option<String>,
    /// Whether at least one target is a directory -- read out of the
    /// panel's already-loaded listing at open time (no I/O), purely so the
    /// recursive hint line can be honest about whether the toggle would
    /// currently do anything.
    has_directory_target: bool,
    /// `0o7777`-masked octal, e.g. `644` or `4755`. See
    /// [`format_octal_mode`] for the three-versus-four-digit rule.
    mode_octal: Entity<InputState>,
    /// The same bits as an `ls -l`-style nine-character `rwxr-xr-x`. Kept
    /// in sync with `mode_octal` in both directions -- see the module doc
    /// comment.
    mode_symbolic: Entity<InputState>,
    /// `YYYY-MM-DD HH:MM`, the exact format `file_table::write_date`
    /// renders the panel's own Date column in.
    modified: Entity<InputState>,
    accessed: Entity<InputState>,
    /// `Ctrl+R`. Applies the patch to every entry beneath each directory
    /// target too, via `duet_ops::plan_attributes`' own walk -- which means
    /// a bigger `Plan` run by the same queue, not a synchronous loop (this
    /// task's AC: "recursive apply runs through the operation queue, not
    /// synchronously").
    recursive: bool,
    /// Guards against a double-Enter re-entering [`Self::confirm`] while a
    /// previous `plan_attributes` is still running off the UI thread --
    /// same reasoning as `CopyMoveDialogState::planning_in_progress`.
    planning_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    state_dir: Option<PathBuf>,
    conflict_resolver: Arc<dyn ConflictResolver>,
    /// Keeps all six subscriptions (a `PressEnter` on each of the four
    /// fields, plus the two mirrored `Change` syncs) alive for as long as
    /// this view exists.
    _subscriptions: Vec<Subscription>,
}

impl AttributesDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        targets: Vec<VPath>,
        single_name: Option<String>,
        has_directory_target: bool,
        prefill: Option<AttributesPrefill>,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Multi-target: no pre-fill at all. There is no single "current
        // value" to show when several entries may each have a different
        // one, and showing one of them would invite the user to
        // accidentally flatten the rest onto whichever happened to be
        // sampled. Blank + a "(unchanged)" placeholder says exactly what
        // an untouched field will do.
        let prefill = prefill.unwrap_or_default();
        let permission_bits = prefill.mode.map(|m| m & 0o7777);
        let octal_initial = permission_bits.map(format_octal_mode).unwrap_or_default();
        let symbolic_initial = permission_bits
            .map(format_symbolic_mode)
            .unwrap_or_default();
        let modified_initial = prefill
            .modified_secs
            .map(format_date_time)
            .unwrap_or_default();
        let accessed_initial = prefill
            .accessed_secs
            .map(format_date_time)
            .unwrap_or_default();

        let mode_octal = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(octal_initial)
                .placeholder("Octal, e.g. 755 (unchanged if blank)")
        });
        let mode_symbolic = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(symbolic_initial)
                .placeholder("Symbolic, e.g. rwxr-xr-x (unchanged if blank)")
        });
        let modified = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(modified_initial)
                .placeholder("YYYY-MM-DD HH:MM (unchanged if blank)")
        });
        let accessed = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(accessed_initial)
                .placeholder("YYYY-MM-DD HH:MM (unchanged if blank)")
        });

        let _subscriptions = vec![
            cx.subscribe_in(&mode_octal, window, Self::on_octal_event),
            cx.subscribe_in(&mode_symbolic, window, Self::on_symbolic_event),
            cx.subscribe_in(&modified, window, Self::on_plain_field_event),
            cx.subscribe_in(&accessed, window, Self::on_plain_field_event),
        ];
        // Focus goes on the octal field -- the one a user reaching for
        // "chmod" is overwhelmingly likely to want. `InputState::focus`
        // rather than `dialog_job::focus_at_end`: unlike every pre-filled
        // field in the T-5.2.7 dialogs, a permission value is something
        // the user *replaces* wholesale (`755`), not something they extend,
        // and `Ctrl+A` (select-all, inside `InputState`'s own key context)
        // is right there for doing so.
        mode_octal.update(cx, |state, cx| state.focus(window, cx));

        Self {
            targets,
            single_name,
            has_directory_target,
            mode_octal,
            mode_symbolic,
            modified,
            accessed,
            recursive: false,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
            _subscriptions,
        }
    }

    /// Test-only accessors -- same reasoning as `LinkDialogState`'s own
    /// `#[cfg(test)]` block.
    #[cfg(test)]
    pub(crate) fn targets(&self) -> &[VPath] {
        &self.targets
    }

    #[cfg(test)]
    pub(crate) fn recursive(&self) -> bool {
        self.recursive
    }

    #[cfg(test)]
    pub(crate) fn mode_octal_value(&self, cx: &App) -> String {
        self.mode_octal.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn mode_symbolic_value(&self, cx: &App) -> String {
        self.mode_symbolic.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn modified_value(&self, cx: &App) -> String {
        self.modified.read(cx).value().to_string()
    }

    /// Test-only: types into a field the way a user would, through the real
    /// `InputState::set_value` (so the real `InputEvent::Change` -> mirror
    /// -sync path runs) rather than `simulate_keystrokes`, which trips an
    /// unrelated upstream `gpui` panic against a focused, non-empty
    /// `InputState` -- documented in full at `workspace.rs`'s own
    /// `f5_copy_end_to_end_copies_a_real_file_to_the_other_panels_directory`.
    #[cfg(test)]
    pub(crate) fn set_mode_octal_value(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode_octal.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn set_mode_symbolic_value(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode_symbolic.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn set_modified_value(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.modified.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    fn on_octal_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.confirm(cx),
            InputEvent::Change => self.sync_symbolic_from_octal(window, cx),
            _ => {}
        }
    }

    fn on_symbolic_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.confirm(cx),
            InputEvent::Change => self.sync_octal_from_symbolic(window, cx),
            _ => {}
        }
    }

    /// The two timestamp fields have no mirror to keep in sync -- Enter is
    /// all they need.
    fn on_plain_field_event(
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

    fn sync_symbolic_from_octal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.mode_octal.read(cx).value().trim().to_string();
        let derived = if text.is_empty() {
            String::new()
        } else if let Some(bits) = parse_octal_mode(&text) {
            format_symbolic_mode(bits)
        } else {
            // Unparseable (mid-edit, or genuinely wrong): leave the other
            // field exactly as it is rather than propagating garbage.
            return;
        };
        write_if_different(&self.mode_symbolic, derived, window, cx);
    }

    fn sync_octal_from_symbolic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.mode_symbolic.read(cx).value().trim().to_string();
        let derived = if text.is_empty() {
            String::new()
        } else if let Some(bits) = parse_symbolic_mode(&text) {
            format_octal_mode(bits)
        } else {
            return;
        };
        write_if_different(&self.mode_octal, derived, window, cx);
    }

    fn toggle_recursive(&mut self, cx: &mut Context<Self>) {
        self.recursive = !self.recursive;
        cx.notify();
    }

    /// Escape: cancel without doing anything, exactly like
    /// `CopyMoveDialogState::cancel`.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_attributes_dialog(window, cx);
        });
    }

    /// Enter: assembles a [`MetaPatch`] from whichever fields are non-empty
    /// and hands it to `duet_ops::plan_attributes` off the UI thread.
    ///
    /// # Which mode field wins
    ///
    /// The octal field is authoritative; the symbolic field is consulted
    /// only when the octal one is blank. In practice this almost never
    /// matters -- the live sync means both fields already agree by the time
    /// Enter is pressed, and clearing either one clears the other -- so the
    /// rule exists to make the one remaining case unambiguous rather than
    /// to arbitrate a real disagreement.
    ///
    /// A field that is non-empty but doesn't parse is a *refusal*, with a
    /// toast and the dialog left open, not a silent "treat it as blank":
    /// silently ignoring text a user deliberately typed would apply a patch
    /// they didn't ask for. (The live sync makes an unparseable field
    /// reachable only by typing into one and pressing Enter before fixing
    /// it -- but that is exactly when a clear message matters.)
    ///
    /// A patch that ends up empty -- every field blank, nothing edited --
    /// closes the dialog without enqueuing anything, mirroring
    /// `crate::rename_dialog`'s own "confirming with nothing changed just
    /// closes" precedent. `plan_attributes` would produce a valid zero-step
    /// plan for it anyway; there is simply no reason to journal a job whose
    /// every step would be a no-op.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
            return;
        }

        let octal_text = self.mode_octal.read(cx).value().trim().to_string();
        let symbolic_text = self.mode_symbolic.read(cx).value().trim().to_string();
        let mode = if !octal_text.is_empty() {
            match parse_octal_mode(&octal_text) {
                Some(bits) => Some(bits),
                None => {
                    self.reject(
                        format!(
                            "\u{201c}{octal_text}\u{201d} isn't a valid octal permission value \
                             (three or four digits, 0-7, e.g. 755)."
                        ),
                        cx,
                    );
                    return;
                }
            }
        } else if !symbolic_text.is_empty() {
            match parse_symbolic_mode(&symbolic_text) {
                Some(bits) => Some(bits),
                None => {
                    self.reject(
                        format!(
                            "\u{201c}{symbolic_text}\u{201d} isn't a valid symbolic permission \
                             value (nine characters, e.g. rwxr-xr-x)."
                        ),
                        cx,
                    );
                    return;
                }
            }
        } else {
            None
        };

        let modified = match self.parse_timestamp_field(&self.modified, "Modified", cx) {
            Ok(t) => t,
            Err(()) => return,
        };
        let accessed = match self.parse_timestamp_field(&self.accessed, "Accessed", cx) {
            Ok(t) => t,
            Err(()) => return,
        };

        let patch = MetaPatch {
            mode,
            modified,
            accessed,
            // Ownership and xattrs are deliberately out of this dialog's
            // scope -- see the module doc comment. `None`/empty means
            // "leave unchanged", which is exactly right.
            ..MetaPatch::default()
        };
        if patch.is_empty() {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.close_attributes_dialog_deferred(cx);
            });
            return;
        }

        let Some(state_dir) = self.state_dir.clone() else {
            self.reject(
                "Can't run the operation: no writable state directory found \
                 (is $HOME/$XDG_STATE_HOME set?)."
                    .to_string(),
                cx,
            );
            return;
        };

        self.planning_in_progress = true;

        let targets = self.targets.clone();
        let recursive = self.recursive;
        let rx = spawn_plan_and_enqueue(
            &self.tokio_handle,
            JobKind::ChangeAttributes,
            self.queue.clone(),
            state_dir,
            // A `SetMeta` step has no conflict concept today (the executor
            // never builds a `ConflictPrompt` for one), so this resolver
            // will not currently be consulted. Passed anyway, for
            // consistency with every other dialog -- special-casing `None`
            // here would only mean this call site silently missing out if
            // `SetMeta` ever grows one.
            self.conflict_resolver.clone(),
            move |fs| async move { plan_attributes(fs.as_ref(), &targets, patch, recursive).await },
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
                    workspace.close_attributes_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }

    /// `Ok(None)` for a blank field ("leave it alone"), `Ok(Some(t))` for a
    /// parseable one, `Err(())` after toasting an unparseable one -- in
    /// which case the caller returns and the dialog stays open.
    fn parse_timestamp_field(
        &self,
        field: &Entity<InputState>,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Result<Option<Timestamp>, ()> {
        let text = field.read(cx).value().trim().to_string();
        if text.is_empty() {
            return Ok(None);
        }
        match parse_date_time(&text) {
            Some(t) => Ok(Some(t)),
            None => {
                self.reject(
                    format!(
                        "{label}: \u{201c}{text}\u{201d} isn't a valid date and time \
                         (expected YYYY-MM-DD HH:MM)."
                    ),
                    cx,
                );
                Err(())
            }
        }
    }

    /// Surfaces a validation failure as a toast, leaving the dialog open so
    /// the user can fix what they typed. Goes through
    /// `Workspace::push_pending_notice` for the same reason every sibling
    /// dialog does -- see that method's own doc comment.
    fn reject(&self, message: String, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.push_pending_notice(NoticeLevel::Warning, message, cx);
        });
    }
}

/// The mirror-sync write itself: set `field` to `value` only if it isn't
/// already exactly that. This inequality check *is* the loop guard -- see
/// the module doc comment for why a `syncing: bool` around `set_value`
/// cannot work under GPUI's deferred event delivery.
fn write_if_different(
    field: &Entity<InputState>,
    value: String,
    window: &mut Window,
    cx: &mut App,
) {
    if field.read(cx).value().as_ref() == value.as_str() {
        return;
    }
    field.update(cx, |state, cx| {
        state.set_value(SharedString::from(value), window, cx);
    });
}

// -- the four mode parse/format helpers, all GPUI-free ---------------------

/// `"644"`/`"4755"` -> the mode bits. `None` for anything that isn't one to
/// four octal digits, or that would exceed `0o7777` (the permission bits
/// `chmod` owns -- the nine `rwx` bits plus setuid/setgid/sticky).
///
/// Leading/trailing whitespace is *not* tolerated here: callers trim first,
/// so accepting it again would just be a second, redundant policy.
pub(crate) fn parse_octal_mode(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 4 {
        return None;
    }
    if !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    let bits = u32::from_str_radix(text, 8).ok()?;
    (bits <= 0o7777).then_some(bits)
}

/// The inverse: mode bits -> the canonical octal string this dialog shows.
///
/// **Three digits when setuid/setgid/sticky are all clear, four when any of
/// them is set** -- so an ordinary `644` reads as `644` rather than a
/// pointlessly padded `0644`, while a setgid directory can't silently lose
/// its `2` in the rendering. (Always-four would be equally defensible; this
/// way round matches how `ls -l`-adjacent tooling and people actually write
/// modes.) Canonical in the fixed-point sense the sync loop depends on:
/// `format_octal_mode(parse_octal_mode(format_octal_mode(x)))` is
/// `format_octal_mode(x)`.
pub(crate) fn format_octal_mode(mode: u32) -> String {
    let bits = mode & 0o7777;
    if bits & 0o7000 != 0 {
        format!("{bits:04o}")
    } else {
        format!("{bits:03o}")
    }
}

/// `"rwxr-xr-x"` -> the mode bits, `ls -l`-style, setuid/setgid/sticky
/// included (`s`/`S` in the user and group execute columns, `t`/`T` in the
/// other column, exactly as `ls` renders them: lowercase when the execute
/// bit is *also* set, uppercase when it isn't).
///
/// `None` for anything that isn't exactly nine characters drawn from the
/// alphabet each column allows. Deliberately strict: a nine-character
/// string is either a complete permission spec or it is not, and there is
/// no partial reading of one worth guessing at. In particular this does
/// *not* accept an `ls -l` line's leading file-type character (`-rw-r--r--`
/// is ten characters, and the type isn't a permission).
pub(crate) fn parse_symbolic_mode(text: &str) -> Option<u32> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() != 9 {
        return None;
    }
    let mut bits = 0u32;
    // (read bit, write bit, execute bit, special bit) per triad, in
    // user/group/other order.
    const TRIADS: [(u32, u32, u32, u32); 3] = [
        (0o400, 0o200, 0o100, 0o4000),
        (0o040, 0o020, 0o010, 0o2000),
        (0o004, 0o002, 0o001, 0o1000),
    ];
    for (i, &(r, w, x, special)) in TRIADS.iter().enumerate() {
        match chars[i * 3] {
            'r' => bits |= r,
            '-' => {}
            _ => return None,
        }
        match chars[i * 3 + 1] {
            'w' => bits |= w,
            '-' => {}
            _ => return None,
        }
        // The sticky bit's own letter is `t`, setuid's and setgid's is `s`
        // -- the only place the three triads differ.
        let (special_lower, special_upper) = if i == 2 { ('t', 'T') } else { ('s', 'S') };
        match chars[i * 3 + 2] {
            'x' => bits |= x,
            c if c == special_lower => bits |= x | special,
            c if c == special_upper => bits |= special,
            '-' => {}
            _ => return None,
        }
    }
    Some(bits)
}

/// The inverse: mode bits -> the canonical nine-character rendering.
/// Canonical in the same fixed-point sense [`format_octal_mode`] is, which
/// is what makes the two-way live sync terminate.
pub(crate) fn format_symbolic_mode(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    const TRIADS: [(u32, u32, u32, u32); 3] = [
        (0o400, 0o200, 0o100, 0o4000),
        (0o040, 0o020, 0o010, 0o2000),
        (0o004, 0o002, 0o001, 0o1000),
    ];
    for (i, &(r, w, x, special)) in TRIADS.iter().enumerate() {
        out.push(if mode & r != 0 { 'r' } else { '-' });
        out.push(if mode & w != 0 { 'w' } else { '-' });
        let (special_lower, special_upper) = if i == 2 { ('t', 'T') } else { ('s', 'S') };
        out.push(match (mode & x != 0, mode & special != 0) {
            (true, true) => special_lower,
            (false, true) => special_upper,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    out
}

// -- the two timestamp helpers ---------------------------------------------

/// Unix seconds -> the `YYYY-MM-DD HH:MM` string the timestamp fields are
/// pre-filled with -- `file_table::write_date`'s exact format (it *is*
/// `write_date`), so the value the dialog shows is byte-for-byte the one
/// the panel's own Date column shows for the same entry.
fn format_date_time(secs: i64) -> String {
    let mut out = String::new();
    write_date(&mut out, secs);
    out
}

/// The inverse of [`format_date_time`]: `"2023-11-14 22:13"` -> a
/// [`Timestamp`] at second `0` of that minute (the displayed format has no
/// seconds field, so there is nothing finer to recover).
///
/// Every component is validated, and an impossible date is rejected without
/// this function owning a month-length table of its own: it round-trips its
/// own answer back through `file_table::civil_from_unix` and requires the
/// parts to come back unchanged. `2023-02-30` normalises to March 2nd and
/// is therefore rejected; `2024-02-29` (a real leap day) round-trips and is
/// accepted. Timestamps are interpreted as UTC, matching
/// `civil_from_unix`/`write_date`, which do no timezone conversion either
/// -- so a value read out of the panel and written straight back is a
/// genuine no-op rather than a silent shift.
pub(crate) fn parse_date_time(text: &str) -> Option<Timestamp> {
    let (date, time) = text.split_once(' ')?;
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let m: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }
    let (hh, mm) = time.split_once(':')?;
    let hh: u32 = hh.parse().ok()?;
    let mm: u32 = mm.parse().ok()?;

    let secs = unix_from_civil(y, m, d, hh, mm);
    // The whole validation, in one line: anything out of range (month 13,
    // day 30 of February, hour 24, ...) normalises into a *different*
    // civil time on the way through, and so fails to come back.
    if civil_from_unix(secs) != (y, m, d, hh, mm) {
        return None;
    }
    Some(Timestamp::new(secs, 0))
}

impl Render for AttributesDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = match &self.single_name {
            Some(name) => format!("Change attributes of \u{201c}{name}\u{201d}:"),
            None => format!(
                "Change attributes of {} item{}:",
                self.targets.len(),
                if self.targets.len() == 1 { "" } else { "s" }
            ),
        };
        let recursive_hint = if self.recursive {
            "Recursive: on (Ctrl+R) \u{2014} applies to everything inside the selected \
             director\u{200b}ies too"
        } else if self.has_directory_target {
            "Recursive: off (Ctrl+R) \u{2014} only the selected entries themselves"
        } else {
            "Recursive: off (Ctrl+R) \u{2014} nothing selected is a directory, so this \
             changes nothing"
        };

        div()
            .id("attributes-dialog")
            .key_context("AttributesDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(field_row("Permissions (octal)", &self.mode_octal, tokens))
            .child(field_row("Symbolic", &self.mode_symbolic, tokens))
            .child(field_row("Modified", &self.modified, tokens))
            .child(field_row("Accessed", &self.accessed, tokens))
            .child(
                div()
                    .text_size(px(11.))
                    .min_w(px(0.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(recursive_hint),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .min_w(px(0.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("A blank field is left unchanged. Enter to confirm, Esc to cancel."),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleRecursiveApply, _window, cx| {
                this.toggle_recursive(cx);
            }))
    }
}

/// One labelled field. A fixed-width label column so the four fields' own
/// inputs line up with each other -- this dialog has four, where every
/// sibling dialog has one and could get away with no label at all.
fn field_row(
    label: &'static str,
    field: &Entity<InputState>,
    tokens: &TokenPalette,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_shrink_0()
                .w(px(140.))
                .text_size(px(11.))
                .text_color(tokens.color.statusbar_fg)
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.)).child(Input::new(field)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_octal_mode / format_octal_mode ------------------------------

    #[test]
    fn parses_a_three_digit_octal_mode() {
        assert_eq!(parse_octal_mode("644"), Some(0o644));
        assert_eq!(parse_octal_mode("755"), Some(0o755));
        assert_eq!(parse_octal_mode("000"), Some(0));
        assert_eq!(parse_octal_mode("777"), Some(0o777));
    }

    #[test]
    fn parses_a_four_digit_octal_mode_with_the_special_bits() {
        assert_eq!(parse_octal_mode("4755"), Some(0o4755));
        assert_eq!(parse_octal_mode("2775"), Some(0o2775));
        assert_eq!(parse_octal_mode("1777"), Some(0o1777));
        assert_eq!(parse_octal_mode("7777"), Some(0o7777));
    }

    /// A shorter spec is a legitimate thing to type mid-edit (and `chmod`
    /// itself accepts `chmod 7 file`), so it parses rather than forcing the
    /// live sync to go blank while the user is still typing.
    #[test]
    fn parses_a_one_or_two_digit_octal_mode() {
        assert_eq!(parse_octal_mode("7"), Some(0o7));
        assert_eq!(parse_octal_mode("44"), Some(0o44));
    }

    #[test]
    fn rejects_malformed_octal_input() {
        for bad in [
            "",      // blank means "unchanged", never a mode
            "8",     // not an octal digit
            "9",     //
            "64a",   // not a digit at all
            "rw-",   // symbolic text in the octal field
            "07777", // five digits: past the 0o7777 chmod owns
            "12345", //
            " 644",  // callers trim; this function does not
            "644 ",  //
            "-644",  //
            "6.4",   //
        ] {
            assert_eq!(parse_octal_mode(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn formats_three_digits_without_special_bits_and_four_with() {
        assert_eq!(format_octal_mode(0o644), "644");
        assert_eq!(format_octal_mode(0o000), "000");
        assert_eq!(format_octal_mode(0o4755), "4755");
        assert_eq!(format_octal_mode(0o1777), "1777");
    }

    /// `Metadata::mode` carries the file-type bits in the high bits; the
    /// dialog only ever shows the permission bits.
    #[test]
    fn format_octal_mode_masks_off_the_file_type_bits() {
        // 0o100644 is a regular file with mode 644.
        assert_eq!(format_octal_mode(0o100_644), "644");
        // 0o040755 is a directory with mode 755.
        assert_eq!(format_octal_mode(0o040_755), "755");
    }

    // -- parse_symbolic_mode / format_symbolic_mode ------------------------

    #[test]
    fn parses_an_ordinary_symbolic_mode() {
        assert_eq!(parse_symbolic_mode("rw-r--r--"), Some(0o644));
        assert_eq!(parse_symbolic_mode("rwxr-xr-x"), Some(0o755));
        assert_eq!(parse_symbolic_mode("---------"), Some(0));
        assert_eq!(parse_symbolic_mode("rwxrwxrwx"), Some(0o777));
    }

    /// `ls -l`'s own convention: lowercase when the execute bit is set too,
    /// uppercase when the special bit is set on its own.
    #[test]
    fn parses_setuid_setgid_and_sticky_in_both_cases() {
        assert_eq!(parse_symbolic_mode("rwsr-xr-x"), Some(0o4755));
        assert_eq!(parse_symbolic_mode("rwSr-xr-x"), Some(0o4655));
        assert_eq!(parse_symbolic_mode("rwxr-sr-x"), Some(0o2755));
        assert_eq!(parse_symbolic_mode("rwxr-Sr-x"), Some(0o2745));
        assert_eq!(parse_symbolic_mode("rwxrwxrwt"), Some(0o1777));
        assert_eq!(parse_symbolic_mode("rwxrwxrwT"), Some(0o1776));
    }

    #[test]
    fn rejects_malformed_symbolic_input() {
        for bad in [
            "",           // blank means "unchanged"
            "rw-r--r",    // eight characters
            "rw-r--r---", // ten
            "-rw-r--r--", // an `ls -l` line's leading type character
            "644",        // octal text in the symbolic field
            "xw-r--r--",  // `x` in the read column
            "rr-r--r--",  // `r` in the write column
            "rw-r--r--z", //
            "rwtr-xr-x",  // sticky's letter in the *user* triad
            "rwxr-xr-s",  // setuid's letter in the *other* triad
            "RW-R--R--",  // uppercase r/w are not a thing
        ] {
            assert_eq!(parse_symbolic_mode(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn formats_the_special_bits_the_way_ls_does() {
        assert_eq!(format_symbolic_mode(0o644), "rw-r--r--");
        assert_eq!(format_symbolic_mode(0o755), "rwxr-xr-x");
        assert_eq!(format_symbolic_mode(0o4755), "rwsr-xr-x");
        assert_eq!(format_symbolic_mode(0o4655), "rwSr-xr-x");
        assert_eq!(format_symbolic_mode(0o2775), "rwxrwsr-x");
        assert_eq!(format_symbolic_mode(0o1777), "rwxrwxrwt");
        assert_eq!(format_symbolic_mode(0o1776), "rwxrwxrwT");
    }

    /// The AC's "octal and symbolic entry agree", proven exhaustively at
    /// the level the two live-sync handlers actually work at: every one of
    /// the 4096 permission values must survive a full round trip through
    /// both renderings, in both directions.
    #[test]
    fn every_mode_round_trips_through_both_renderings() {
        for bits in 0..=0o7777u32 {
            assert_eq!(
                parse_octal_mode(&format_octal_mode(bits)),
                Some(bits),
                "octal round trip failed for {bits:o}"
            );
            assert_eq!(
                parse_symbolic_mode(&format_symbolic_mode(bits)),
                Some(bits),
                "symbolic round trip failed for {bits:o}"
            );
            // ... and crossing between them, which is exactly what one
            // live-sync hop does.
            assert_eq!(
                parse_symbolic_mode(&format_symbolic_mode(
                    parse_octal_mode(&format_octal_mode(bits)).unwrap()
                )),
                Some(bits)
            );
        }
    }

    /// What makes the two mirrored `InputEvent::Change` subscriptions
    /// terminate instead of ping-ponging forever (see the module doc
    /// comment): both renderings are fixed points of their own round trip,
    /// so the second propagation hop always finds the other field already
    /// holding exactly the value it would write.
    #[test]
    fn both_renderings_are_fixed_points_so_the_live_sync_terminates() {
        for bits in 0..=0o7777u32 {
            let octal = format_octal_mode(bits);
            assert_eq!(
                format_octal_mode(parse_octal_mode(&octal).unwrap()),
                octal,
                "octal rendering is not canonical for {bits:o}"
            );
            let symbolic = format_symbolic_mode(bits);
            assert_eq!(
                format_symbolic_mode(parse_symbolic_mode(&symbolic).unwrap()),
                symbolic,
                "symbolic rendering is not canonical for {bits:o}"
            );
        }
    }

    // -- parse_date_time ---------------------------------------------------

    #[test]
    fn parses_the_format_the_panel_itself_displays() {
        let t = parse_date_time("2023-11-14 22:13").unwrap();
        assert_eq!(t.secs, 1_700_000_000 - 20, "22:13:00, not 22:13:20");
        assert_eq!(t.nanos, 0);
        assert_eq!(format_date_time(t.secs), "2023-11-14 22:13");
    }

    #[test]
    fn a_displayed_timestamp_round_trips_back_to_the_same_minute() {
        for secs in [1_700_000_000i64, 981_173_100, 951_782_400, 2_147_483_647] {
            let truncated = secs - secs.rem_euclid(60);
            let rendered = format_date_time(truncated);
            assert_eq!(
                parse_date_time(&rendered).map(|t| t.secs),
                Some(truncated),
                "{rendered} did not round-trip"
            );
        }
    }

    #[test]
    fn accepts_a_real_leap_day() {
        assert!(parse_date_time("2024-02-29 12:00").is_some());
        assert!(parse_date_time("2000-02-29 00:00").is_some());
    }

    #[test]
    fn rejects_malformed_or_impossible_dates_and_times() {
        for bad in [
            "",                    // blank means "unchanged"
            "2023-11-14",          // no time half
            "22:13",               // no date half
            "2023-11-14T22:13",    // ISO `T` separator, not this format
            "2023-11 22:13",       // two date components
            "2023-11-14-01 22:13", // four
            "2023-13-01 00:00",    // month 13
            "2023-00-01 00:00",    // month 0
            "2023-02-30 00:00",    // February 30th
            "2023-11-31 00:00",    // November has 30 days
            "2023-02-29 00:00",    // 2023 is not a leap year
            "2023-11-14 24:00",    // hour 24
            "2023-11-14 22:60",    // minute 60
            "2023-11-14 22",       // no minute
            "-",                   // `write_date`'s own "no timestamp" rendering
            "not a date at all",
        ] {
            assert_eq!(parse_date_time(bad), None, "{bad:?} must not parse");
        }
    }
}
