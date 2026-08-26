// SPDX-License-Identifier: MIT
//! T-5.2.7's two link-creation dialogs (FR-OPS-01,
//! `docs/commands.md`'s `ops.create_symlink`/`ops.create_hardlink` rows):
//! one [`LinkDialogState`] type driving both, distinguished by a
//! [`LinkKind`] captured at open time.
//!
//! One type rather than two because the two dialogs differ in exactly
//! three places -- their title, which planner runs, and which `JobKind`
//! gets enqueued -- and are identical in every other respect (a single
//! source entry, a single editable link path, the same Enter/Escape
//! handling, the same off-thread plan-and-enqueue). Two near-identical
//! `Render` impls would have been a copy, not an abstraction.
//!
//! # Keybindings: this codebase's own defaults, not verified TC chords
//!
//! `docs/commands.md` catalogues both commands, but `docs/keymap-tc.csv`
//! -- the TC keymap *survey*, and the only source of "known" bindings in
//! this repo -- has no row for either, so there is no Total Commander
//! chord to match here. `Ctrl+Shift+S` (Symlink) and `Ctrl+Shift+H`
//! (Hardlink) are this codebase's own reasonable defaults, chosen for the
//! obvious mnemonic and confirmed unclaimed against every other
//! `KeyBinding::new` call site in this crate. Same disclosed-default
//! pattern `NavigateHome`'s `Alt+Home`, `OpenCommandPalette`'s
//! `Ctrl+Shift+P` and the hotlist's own `Ctrl+Shift+D` already establish.
//! The bindings themselves live in `workspace::bind_workspace_keys`
//! alongside every other overlay-opening chord.
//!
//! # Disclosed scope boundary: one entry, not a selection
//!
//! Both dialogs operate on the **cursor entry** only
//! (`FileTableDelegate::cursor_entry_name()`), not on a multi-selection --
//! deliberately, and worth stating rather than leaving as a silent
//! limitation. `duet_ops::plan_symlink`/`plan_hardlink` are single-step
//! planners with no multi-source concept; "one link per selected file"
//! would mean batching several `Step::Symlink`/`Step::Link` into one
//! `Plan` and deciding each link's own name, which is real design work
//! this task's AC (which does not mention links at all beyond "create
//! symlink/hardlink dialogs") does not ask for. A later task can widen
//! this without changing anything here except how `source` is resolved.
//!
//! # The symlink target is a plain absolute path
//!
//! `plan_symlink` stores its `target` string in the link *verbatim*, never
//! resolving it -- so the choice of what string to store is this dialog's,
//! and it is simply the source entry's own absolute path
//! (`VPath::inner().as_str()`, this codebase's established plain-path
//! rendering of a `VPath`; `VPath`'s `Display` impl produces the URI form
//! `file:///...`, which is right for error messages and wrong for a
//! symlink target). No relative-path computation and no
//! absolute/relative toggle: the simplest thing that is unambiguously
//! correct wherever the link ends up. A "relative link" option is a
//! reasonable later addition, not something this task's AC asks for.
//!
//! # Overlay architecture and keyboard handling
//!
//! Identical to `crate::mkdir_dialog`'s -- see that module's doc comment,
//! and `copy_move_dialog.rs`'s for the full "why Enter/Escape need no
//! `KeyBinding` of this module's own" reasoning.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use duet_ops::{ConflictResolver, JobKind, QueueManager, plan_hardlink, plan_symlink};
use duet_types::VPath;
use duet_widgets::input::{Escape, Input, InputEvent, InputState};
use duet_widgets::theme::TokenPalette;
use gpui::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, div, px,
};

use crate::dialog_job::{report_job_outcome, spawn_plan_and_enqueue};
use crate::file_table::local_vpath;
use crate::workspace::{NoticeLevel, Workspace};

/// Which of the two link commands opened this dialog. Fixed at open time
/// -- there is deliberately no in-dialog toggle between the two: a symlink
/// and a hardlink are different enough operations (different constraints,
/// different failure modes, a directory source legal for one and rejected
/// at plan time by the other) that flipping between them mid-dialog would
/// be a footgun, not a convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkKind {
    Symlink,
    Hardlink,
}

impl LinkKind {
    fn noun(self) -> &'static str {
        match self {
            LinkKind::Symlink => "symlink",
            LinkKind::Hardlink => "hardlink",
        }
    }

    fn job_kind(self) -> JobKind {
        match self {
            LinkKind::Symlink => JobKind::CreateSymlink,
            LinkKind::Hardlink => JobKind::CreateHardlink,
        }
    }
}

/// `Ctrl+Shift+S` / `Ctrl+Shift+H`'s dialog. See the module doc comment
/// for the full architecture; `Workspace::open_link_dialog` is the only
/// constructor call site and has already resolved everything below.
pub(crate) struct LinkDialogState {
    kind: LinkKind,
    /// The cursor entry's already-resolved absolute path, captured at open
    /// time and not editable -- there is nothing to browse for here, it is
    /// always whatever the cursor was on.
    source: VPath,
    /// The source's own basename, for the title line.
    source_name: String,
    /// The editable half: where the new link itself goes. Pre-filled with
    /// the *other* panel's current directory plus `source_name`, matching
    /// F5/F6's own "destination defaults to the other panel" convention.
    link_path: Entity<InputState>,
    planning_in_progress: bool,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    queue: Arc<QueueManager>,
    state_dir: Option<PathBuf>,
    conflict_resolver: Arc<dyn ConflictResolver>,
    _subscriptions: Vec<Subscription>,
}

impl LinkDialogState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: LinkKind,
        source: VPath,
        source_name: String,
        initial_link_path: String,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        state_dir: Option<PathBuf>,
        conflict_resolver: Arc<dyn ConflictResolver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let link_path = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial_link_path)
                .placeholder("Path of the new link")
        });
        let _subscriptions = vec![cx.subscribe_in(&link_path, window, Self::on_link_path_event)];
        link_path.update(cx, |state, cx| state.focus(window, cx));

        Self {
            kind,
            source,
            source_name,
            link_path,
            planning_in_progress: false,
            workspace,
            tokio_handle,
            queue,
            state_dir,
            conflict_resolver,
            _subscriptions,
        }
    }

    /// Test-only accessors -- same reasoning as `DeleteDialogState`'s own
    /// `#[cfg(test)]` block.
    #[cfg(test)]
    pub(crate) fn kind(&self) -> LinkKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &VPath {
        &self.source
    }

    #[cfg(test)]
    pub(crate) fn link_path_value(&self, cx: &gpui::App) -> String {
        self.link_path.read(cx).value().to_string()
    }

    /// Test-only: see `MkdirDialogState::set_destination_value`'s doc
    /// comment for why the tests set the field this way rather than
    /// simulating keystrokes.
    #[cfg(test)]
    pub(crate) fn set_link_path_value(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.link_path.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
        });
    }

    fn on_link_path_event(
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

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_link_dialog(window, cx);
        });
    }

    /// Enter: plans and enqueues the link, off the UI thread. The two
    /// kinds diverge only here -- `plan_symlink` is pure, synchronous and
    /// infallible (the target string is stored verbatim, so there is
    /// nothing to validate), while `plan_hardlink` `stat`s the source so it
    /// can reject a directory before a doomed job ever reaches the
    /// executor.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.planning_in_progress {
            return;
        }

        let text = self.link_path.read(cx).value().to_string();
        let Ok(link_path) = local_vpath(Path::new(text.as_str())) else {
            let workspace = self.workspace.clone();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.push_pending_notice(
                    NoticeLevel::Error,
                    format!("\u{201c}{text}\u{201d} isn't a valid path for the new link."),
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

        let kind = self.kind;
        let source = self.source.clone();
        let rx = spawn_plan_and_enqueue(
            &self.tokio_handle,
            kind.job_kind(),
            self.queue.clone(),
            state_dir,
            self.conflict_resolver.clone(),
            move |fs| async move {
                match kind {
                    // See the module doc comment: a plain absolute path,
                    // not the `file://` URI `VPath`'s `Display` renders.
                    LinkKind::Symlink => {
                        Ok(plan_symlink(source.inner().as_str().to_string(), link_path))
                    }
                    LinkKind::Hardlink => plan_hardlink(fs.as_ref(), &source, &link_path).await,
                }
            },
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
                    workspace.close_link_dialog_deferred(cx);
                });
            }
        })
        .detach();
    }
}

impl Render for LinkDialogState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let title = format!(
            "Create {} to \u{201c}{}\u{201d} at:",
            self.kind.noun(),
            self.source_name
        );
        let detail = match self.kind {
            LinkKind::Symlink => format!("Target: {}", self.source.inner().as_str()),
            LinkKind::Hardlink => format!("Same inode as: {}", self.source.inner().as_str()),
        };

        div()
            .id("link-dialog")
            .key_context("LinkDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(Input::new(&self.link_path))
            .child(
                div()
                    .text_size(px(11.))
                    // `min_w(0)` for the same reason `delete_dialog.rs`'s
                    // warning line carries it: a long, unbroken path would
                    // otherwise widen this flex child past the card
                    // instead of wrapping inside it.
                    .min_w(px(0.))
                    .text_color(tokens.color.statusbar_fg)
                    .child(detail),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tokens.color.statusbar_fg)
                    .child("Enter to confirm, Esc to cancel"),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
    }
}
