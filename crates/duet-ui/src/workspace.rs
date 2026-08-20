// SPDX-License-Identifier: MIT
//! The application-bootstrap root view: window, theme, and the
//! Tokio-to-GPUI executor bridge demo (T-4.1.1), built out into the real
//! workspace shell by T-4.1.4/T-4.1.5: a draggable/keyboard-resizable
//! dual-pane splitter, a function-key bar, a status bar, and a
//! command-line row, all themed by [`crate::theme_controller`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use duet_commands::keymap::{self, tc_csv};
use duet_commands::palette::PaletteIndex;
use duet_commands::{CommandId, CommandRegistry, register_builtin_commands};
use duet_config::{HotlistEntry, SessionTab};
use duet_ops::{JobEvent, JobId, JobKind, JobOutcome, JobReport, ProgressSnapshot, QueueManager};
use duet_types::{UnixPathBuf, VPath};
use duet_vfs::{FileSystem, ListOpts, LocalFs};
use duet_widgets::{
    input::{Input, InputState},
    layout::{Root, WindowExt, h_flex, v_flex},
    list::{IndexPath, List, ListState},
    resizable::{ResizableState, h_resizable, resizable_panel},
    theme::{ActiveTheme as _, TokenPalette},
    toast::Notification,
};
use futures_util::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, px, size,
};

use crate::command_palette::CommandPaletteDelegate;
use crate::copy_move_dialog::{CopyMoveDialogState, bind_copy_move_dialog_keys};
use crate::file_table::{
    FileTable, FileTableSettings, MouseMode, QuickSearchMode, write_byte_count,
};
use crate::function_bar::{self, FKeySlot};
use crate::hotlist::HotlistDelegate;
use crate::operation_manager::{OperationManagerState, bind_operation_manager_keys};
use crate::panel::{Panel, bind_panel_keys};
use crate::theme_controller::ThemeController;

// FR-NAV-01's "keyboard resize": while the workspace has focus, adjust the
// splitter ratio without touching the mouse. Bound below to `ctrl-left`/
// `ctrl-right`. `gpui-component`'s `ResizablePanelGroup` (T-4.1.2's
// `duet_widgets::resizable` façade) has no keyboard-resize API of its own
// to call into (verified by reading `gpui-component-0.5.1`'s
// `resizable/mod.rs`: every size-mutating method is `pub(crate)`, so even
// this façade crate cannot reach it) -- this is the "or add a reasonable
// one" half of the task brief.
//
// `FocusOtherPanel` (T-4.3.2, FR-NAV-02's "Tab switches"): handled here,
// not in `panel.rs`, because answering "which panel isn't focused" needs
// both panels at once -- something neither `Panel` nor `FileTable` has
// any reason to know about the other. Bound to plain `Tab` in the
// `"FileTable"` context (see `bind_workspace_keys`) rather than
// `"Workspace"`/`"Panel"`: it only makes sense to fire while a panel's
// table genuinely holds focus, the same reasoning `docs/keymap-tc.csv`
// gives (`focus.other_panel`'s context column is `panel`).
//
// `OpenCommandPalette` (T-4.3.6, FR-TOOL-11): `Ctrl+Shift+P` is my own
// reasonable-default choice, not a verified TC binding -- Total Commander
// predates the "command palette" UX pattern entirely (this app's own
// `docs/keymap-tc.csv` survey has no row for it, same situation
// `NavigateHome`'s Alt+Home and `TabReopenClosed`'s Ctrl+Shift+T were
// already in). Chosen for being the same chord VSCode/Sublime/most
// Electron-era editors already use for this exact feature, and unclaimed
// by anything else in this app's keymap.
actions!(
    duet_workspace,
    [
        ResizeSplitterLeft,
        ResizeSplitterRight,
        FocusOtherPanel,
        OpenCommandPalette
    ]
);

// T-4.3.5's directory hotlist (FR-NAV-08). `OpenHotlist` (`Ctrl+D`) is the
// one binding `docs/keymap-tc.csv` actually documents (`hotlist.open`,
// "known" confidence). `AddCurrentDirToHotlist` (`Ctrl+Shift+D`),
// `HotlistRemoveEntry` (`Delete`), `HotlistMoveUp`/`HotlistMoveDown`
// (`Ctrl+Up`/`Ctrl+Down`) are this codebase's own reasonable defaults --
// `docs/commands.md`'s `hotlist.add`/`remove`/`reorder` rows exist but
// carry no keybinding anywhere in the repo (confirmed: neither
// `keymap-tc.csv` nor design.md's own keymap appendix names one). The
// Ctrl+Shift+`<letter>` pairing mirrors this app's own established
// convention (Ctrl+T new tab / Ctrl+Shift+T reopen closed tab): the
// shift variant is the more consequential sibling of the same key.
// Ctrl+Up/Ctrl+Down for reorder is deliberately *not* plain arrow keys --
// confirmed by reading `gpui-component-0.5.1/src/list/list.rs`'s own
// `list::init` that `List`'s internal `SelectUp`/`SelectDown` navigation
// is bound to the bare, unmodified `"up"`/`"down"` keystrokes, so a
// `Ctrl+Up`/`Ctrl+Down` binding is a genuinely different keystroke that
// never competes with it.
actions!(
    duet_workspace,
    [
        OpenHotlist,
        AddCurrentDirToHotlist,
        HotlistRemoveEntry,
        HotlistMoveUp,
        HotlistMoveDown
    ]
);

// T-5.2.1's F5/F6 copy/move dialog (FR-OPS-01). `docs/keymap-tc.csv` rows
// 5 and 7 (`ops.copy`/`ops.move_or_rename`) are both "known" TC bindings
// -- unlike `OpenCommandPalette`/the hotlist bindings above, these two are
// verified, not this codebase's own reasonable default. `Shift+F5`
// ("copy into the same dir, prompting for a new name") and `Shift+F6`
// ("rename in place, no dialog") are separate, narrower commands this
// task's own scope doesn't cover -- see `crate::copy_move_dialog`'s
// module doc comment for the full list of what this dialog does and
// doesn't do.
actions!(duet_workspace, [CopyDialog, MoveDialog]);

// T-5.2.2's expandable operation manager (FR-OPS-02/03, design.md §9.3).
// `OpenOperationManager` is this codebase's own reasonable-default choice
// -- TC itself has no single canonical keystroke for "open the background
// transfer manager" (`docs/keymap-tc.csv` has no row for it, same
// situation `OpenCommandPalette`'s `Ctrl+Shift+P` and the hotlist
// bindings above were already in), and `Ctrl+O` ("Operations") is
// unclaimed by any other `KeyBinding::new` call site in this crate.
// Everything the overlay itself needs once it's already open
// (`CloseOperationManager`, the cursor/pause/resume/cancel actions) is
// declared in `crate::operation_manager`, not here -- same split
// `CopyDialog`/`MoveDialog` (open-the-overlay, here) versus
// `copy_move_dialog::SetConflictSkip`/friends (overlay-internal, there)
// already establishes.
actions!(duet_workspace, [OpenOperationManager]);

/// Registers the workspace's own keybindings. Called once from [`run`],
/// before any window opens. `Some("Workspace")` scopes the splitter
/// bindings to elements tagged with that key context -- see the root
/// view's `.key_context("Workspace")` in [`Workspace::render`].
fn bind_workspace_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-left", ResizeSplitterLeft, Some("Workspace")),
        KeyBinding::new("ctrl-right", ResizeSplitterRight, Some("Workspace")),
        KeyBinding::new("tab", FocusOtherPanel, Some("FileTable")),
        KeyBinding::new("ctrl-shift-p", OpenCommandPalette, Some("Workspace")),
        KeyBinding::new("ctrl-d", OpenHotlist, Some("Workspace")),
        KeyBinding::new("ctrl-shift-d", AddCurrentDirToHotlist, Some("Workspace")),
        // Scoped to "HotlistOverlay" (the overlay card's own key context,
        // not "Workspace") -- these three should only ever fire while the
        // overlay is actually open and its list has an entry to act on.
        KeyBinding::new("delete", HotlistRemoveEntry, Some("HotlistOverlay")),
        KeyBinding::new("ctrl-up", HotlistMoveUp, Some("HotlistOverlay")),
        KeyBinding::new("ctrl-down", HotlistMoveDown, Some("HotlistOverlay")),
        KeyBinding::new("f5", CopyDialog, Some("Workspace")),
        KeyBinding::new("f6", MoveDialog, Some("Workspace")),
        KeyBinding::new("ctrl-o", OpenOperationManager, Some("Workspace")),
    ]);
}

/// Opens the Duet application window.
pub fn run() {
    // Kept alive for the whole process lifetime by living in this
    // function's stack frame, which does not return until
    // `Application::run` does (i.e. until the app quits).
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::cmp::min(
            8,
            std::thread::available_parallelism().map_or(4, |n| n.get()),
        ))
        .enable_all()
        .thread_name("duet-io")
        .build()
        .expect("failed to start the core's Tokio runtime");
    let tokio_handle = tokio_rt.handle().clone();

    Application::new().run(move |cx: &mut App| {
        duet_widgets::init(cx);
        bind_workspace_keys(cx);
        crate::file_table::bind_file_table_keys(cx);
        bind_panel_keys(cx);
        bind_copy_move_dialog_keys(cx);
        bind_operation_manager_keys(cx);

        let bounds = Bounds::centered(None, size(px(1024.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Duet")),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.0), px(420.0))),
                app_id: Some("duet".into()),
                ..Default::default()
            },
            |window, cx| {
                // T-4.1.1's pre-window-existed sync (inside `duet_widgets::init`)
                // plus this post-window re-sync, per `compat::sync_theme_with_window`'s
                // doc comment (the Linux `App::window_appearance()` reliability
                // caveat). `ThemeController::install` (T-4.1.5) takes over from
                // here for the *live* follow-system + theme-file-hot-reload
                // behaviour this one-shot call cannot provide on its own.
                duet_widgets::compat::sync_theme_with_window(window, cx);

                let workspace = cx.new(|cx| Workspace::new(window, cx, tokio_handle.clone()));
                let theme = ThemeController::install(window, cx, workspace.clone());
                workspace.update(cx, |ws, cx| {
                    ws.theme = Some(theme);
                    cx.notify();
                });

                spawn_entry_count_demo(tokio_handle.clone(), workspace.clone(), cx);
                // Focuses the left panel's active tab directly, not the
                // workspace root -- T-4.2.2's cursor movement is bound to
                // `FileTable`'s own key context, and this is the only way
                // to reach it before any click lands (T-4.3.8's mouse
                // support). `Workspace`'s own "Workspace"-context bindings
                // (Ctrl+Left/Right splitter resize) still fire from here:
                // GPUI's action dispatch walks the focused element's whole
                // ancestor chain, and `Workspace`'s root div stays an
                // ancestor of the left panel regardless of which of the
                // two (or which tab within it) holds focus.
                let left_panel = workspace.read(cx).left_panel.clone();
                let handle = left_panel.read(cx).active_focus_handle(cx);
                window.focus(&handle);

                // `gpui-component` widgets (the command-line `Input` among
                // them -- see `duet_widgets::layout::Root`'s doc comment)
                // call into `Root::read`/`Root::update` internally and
                // panic if the window's actual root view isn't one, so the
                // real render root wraps `workspace`, not `workspace`
                // itself.
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("failed to open the Duet window");
    });
}

/// The splitter ratio never collapses a panel entirely -- keeps at least
/// 15% of the workspace width visible on either side, mirroring the
/// underlying widget's own `PANEL_MIN_SIZE` floor in spirit (a fixed pixel
/// floor would fight a very narrow window; a ratio floor scales with it).
const SPLITTER_MIN_RATIO: f32 = 0.15;
const SPLITTER_MAX_RATIO: f32 = 0.85;
/// `Ctrl+Left`/`Ctrl+Right`'s step size per keypress.
const SPLITTER_KEYBOARD_STEP: f32 = 0.02;

/// T-4.3.7's "kill -9 restores the full workspace": cursor position and
/// sort state change far more often (every arrow key, every header
/// click) than tab structure does. Hooking a dedicated event into every
/// single cursor-moving/sort-changing call site across `FileTable` (the
/// way `FileTableEvent::DirectoryChanged` does for directory changes)
/// would be a lot of invasive surface area for what's fundamentally a
/// "don't lose more than a few seconds of scrolling" guarantee, not a
/// pixel-perfect one. A periodic re-save (see [`Workspace::new`]'s
/// spawned loop) covers exactly that gap cheaply instead: every few
/// seconds, unconditionally re-persist whatever the current cursor/sort
/// state is, alongside the already-eager, event-driven saves structural
/// tab changes and real directory changes get. Short enough that a kill
/// -9 loses at most a few seconds of cursor movement, not imperceptible
/// enough to matter for a background file write nobody's watching.
const SESSION_PERIODIC_SAVE_INTERVAL: Duration = Duration::from_secs(3);

/// A reasonable, documented placeholder for how many T-5.2.1 copy/move
/// jobs the workspace's `QueueManager` runs at once -- there's no
/// user-facing concurrency setting yet (a future task's job, not this
/// one's); jobs beyond this bound simply wait `Queued` in priority order
/// (`QueueManager`'s own module doc comment), so this is a throughput
/// knob, not a correctness one.
const COPY_MOVE_QUEUE_MAX_CONCURRENT: usize = 2;

/// The root workspace view: the dual-pane splitter, the function-key bar,
/// the status bar, and the command-line row (T-4.1.4), themed live by
/// [`ThemeController`] (T-4.1.5).
pub struct Workspace {
    demo: DemoState,
    focus_handle: FocusHandle,

    /// The dual-pane splitter's current left-panel fraction of the
    /// workspace width, `[SPLITTER_MIN_RATIO, SPLITTER_MAX_RATIO]`.
    /// Authoritative source of truth for the ratio; `resizable_state`
    /// below is rebuilt from it on every *programmatic* (keyboard) change
    /// -- see that field's doc comment for why.
    splitter_ratio: f32,
    /// Backing state for `duet_widgets::resizable`'s `ResizablePanelGroup`.
    ///
    /// A mouse drag mutates this entity's internal per-panel pixel sizes
    /// directly (that part of the upstream widget works exactly as
    /// intended -- see `on_resize` in [`Self::dual_pane`], which reads the
    /// post-drag sizes back into `splitter_ratio`). A *keyboard* resize,
    /// however, has no upstream entry point to call: every size-mutating
    /// method on the upstream `gpui-component` crate's `resizable`
    /// module's `ResizableState` is `pub(crate)` to that crate (confirmed
    /// by reading `gpui-component-0.5.1/src/resizable/mod.rs`), so
    /// nothing outside it -- not even this façade -- can push a new size
    /// into an existing entity. The workaround: replace this field with a **fresh**
    /// `ResizableState` entity whenever `splitter_ratio` changes by
    /// keyboard. A brand-new entity's panels start with `size: None`
    /// (`ResizableState::sync_panels_count`'s default), so the next
    /// render's explicit `ResizablePanel::size(...)` (computed from the
    /// new `splitter_ratio`) actually takes effect instead of being
    /// silently overridden by a stale internal size -- see
    /// `gpui-component-0.5.1/src/resizable/panel.rs`'s render, where
    /// `panel_state.size` (once `Some`) always wins over the `size()`
    /// builder argument.
    resizable_state: Entity<ResizableState>,

    /// T-4.2.1/T-4.3.2: both panels, each a real, independent tab
    /// container (`crate::panel::Panel`) over the virtualised directory
    /// table (`duet_index::DirectoryModel`/`EntryStore` backed). Neither
    /// is a placeholder any more -- see [`Self::new`]'s doc comment for
    /// why making the right panel real landed as part of T-4.3.2 rather
    /// than its own task.
    left_panel: Entity<Panel>,
    right_panel: Entity<Panel>,

    function_keys: Vec<FKeySlot>,
    command_line: Entity<InputState>,

    /// `~/.config/duet/settings.toml` (or `None` if `$HOME`/
    /// `$XDG_CONFIG_HOME` can't be resolved -- splitter-ratio persistence
    /// is then skipped, not fatal).
    settings_path: Option<PathBuf>,

    /// `~/.local/state/duet/session.json` (or `None` for the same reason
    /// `settings_path` can be) -- T-4.3.2's tab-list persistence. Every
    /// structural tab change and every real directory change in either
    /// panel re-saves this (see [`Self::new`]'s `cx.observe` calls and
    /// `crate::file_table::FileTableEvent::DirectoryChanged`'s doc
    /// comment), plus a periodic re-save regardless of any event
    /// (`SESSION_PERIODIC_SAVE_INTERVAL`) for state that changes too
    /// often to hook individually (cursor position, sort -- T-4.3.7). Not
    /// just at graceful shutdown -- the AC ("kill -9 then restart
    /// restores the full workspace") only holds if saves are already
    /// this eager.
    session_path: Option<PathBuf>,

    /// Deferred toasts to surface via `window.push_notification` on the
    /// next render, then cleared -- T-4.3.7's "a corrupt session file
    /// degrades to defaults with a notice" originally needed only one of
    /// these (there was no `Window` yet at [`Self::new`] time --
    /// `gpui-component`'s `Root`, what `WindowExt::push_notification`
    /// routes through, doesn't exist until `Root::new(workspace, ..)`
    /// wraps `workspace` *after* `Workspace::new` returns, the same
    /// one-frame gap `theme` bridges for `ThemeController`). T-5.2.1
    /// added a second source with the identical "no live `Window`"
    /// problem -- the copy/move dialog's `QueueManager` event-consumer
    /// task (spawned in [`Self::new`], runs for the app's whole
    /// lifetime) -- and a *queue*, not the original `Option<String>`,
    /// because that second source can fire more than once between
    /// renders (two jobs finishing in the same tick, `max_concurrent >
    /// 1`): an `Option` would silently drop every notice but the last.
    /// [`Self::push_pending_notice`] is the one push site; [`Self::render`]
    /// drains and fires all of them, in order, every render.
    pending_notice: Vec<PendingNotice>,
    /// A `FocusHandle` to restore via `window.focus` on the next render,
    /// then cleared -- the exact same "no live `Window`" problem
    /// `pending_notice` documents, for the one close path that hits it:
    /// [`Self::close_copy_move_dialog_deferred`], called from the copy/
    /// move dialog's async plan/enqueue success callback. The ordinary
    /// Escape/Enter-with-a-live-`Window` close path
    /// ([`Self::close_copy_move_dialog`]) restores focus immediately and
    /// never touches this field.
    pending_focus_restore: Option<FocusHandle>,
    /// `true` when a T-5.2.1 copy/move job has finished since the last
    /// render and both panels need to re-list their current directory to
    /// reflect what it actually did on disk -- same "no live `Window`"
    /// problem `pending_notice`/`pending_focus_restore` document (the
    /// `QueueManager` event-consumer task that sets this has neither),
    /// drained by [`Self::render`] the same way. Deliberately not scoped
    /// to *which* directories the finished job touched: `JobEvent::
    /// Finished` carries a `JobReport` (counts/bytes), not the plan's
    /// source/dest paths, and re-listing whichever directory each panel
    /// already happens to be showing is always safe -- a panel unrelated
    /// to the job just re-reads the same, unchanged listing. Set on
    /// *every* `JobEvent::Finished`, regardless of outcome (even a
    /// `Failed`/`Cancelled` job can have made real, partial progress
    /// before stopping).
    pending_panel_refresh: bool,

    /// T-4.3.6's command palette: the fuzzy-searchable index over every
    /// registered command (`docs/commands.md`'s 302-entry catalogue) plus
    /// its currently resolved keybinding(s). Built once, here, rather
    /// than on every `Ctrl+Shift+P` -- parsing the catalogue and the TC
    /// keymap CSV isn't free, and "opening is instant with 200+ commands"
    /// is this task's own AC; commands/bindings never change at runtime
    /// in this app yet, so there's nothing that would ever need this
    /// rebuilt later. `Rc`, not a bare value, so each palette-open can
    /// hand a cheap clone to a fresh `CommandPaletteDelegate` without
    /// `Workspace` giving up ownership.
    palette_index: Rc<PaletteIndex>,
    /// `Some` while the palette overlay is open -- constructed fresh on
    /// every `open_command_palette` (so a reopened palette always starts
    /// with an empty query, matching every other command palette's
    /// convention) and dropped on close. The `Entity` itself owns the
    /// live search state (query text, current matches, selection).
    command_palette: Option<Entity<ListState<CommandPaletteDelegate>>>,
    /// Saved by `open_command_palette`, restored and cleared by
    /// `close_command_palette` -- so closing the palette (Escape, or
    /// after invoking a command) gives keyboard focus back to whichever
    /// panel had it before, rather than leaving focus stranded on an
    /// overlay that no longer exists.
    palette_previous_focus: Option<FocusHandle>,
    /// Which panel a palette-invoked tab command applies to -- captured
    /// once, at `open_command_palette` time (before focus moves onto the
    /// palette's own query input, at which point neither panel would
    /// read as focused any more). Defaults to the left panel if,
    /// somehow, neither panel had focus when the palette opened (e.g. it
    /// was invoked while the command line had focus).
    palette_target_panel: PanelSide,

    /// `~/.config/duet/hotlist.toml`, or `None` if `$HOME`/
    /// `$XDG_CONFIG_HOME` can't be resolved -- hotlist persistence is then
    /// skipped, not fatal, same tolerance every other config path in this
    /// struct already has.
    hotlist_path: Option<PathBuf>,
    /// T-4.3.5's directory hotlist (FR-NAV-08): the canonical, in-memory,
    /// persisted list of bookmarks. Loaded once at startup; every
    /// add/remove/reorder updates this field *and* writes it back to
    /// `hotlist_path` immediately (`persist_hotlist`) -- eager, matching
    /// `session.json`'s own "don't lose more than the last action"
    /// convention, not just-at-shutdown.
    hotlist_entries: Vec<HotlistEntry>,
    /// `Some` while the hotlist overlay is open -- constructed fresh on
    /// every `open_hotlist` from the current `hotlist_entries`, dropped on
    /// close. Mirrors `command_palette`'s own field exactly.
    hotlist: Option<Entity<ListState<HotlistDelegate>>>,
    /// Saved by `open_hotlist`, restored and cleared by `close_hotlist` --
    /// same reasoning as `palette_previous_focus`.
    hotlist_previous_focus: Option<FocusHandle>,
    /// Which panel `hotlist.navigate`/`AddCurrentDirToHotlist` apply to --
    /// same capture-at-open-time reasoning as `palette_target_panel`.
    /// Reused for `AddCurrentDirToHotlist` too even though that action
    /// doesn't open the overlay, since it needs the exact same "which
    /// panel is the user actually working in" answer.
    hotlist_target_panel: PanelSide,

    /// `Some` while the F5/F6 copy/move dialog (T-5.2.1, FR-OPS-01) is
    /// open -- constructed fresh on every `open_copy_move_dialog`, dropped
    /// on close. Mirrors `hotlist`/`command_palette`'s own fields, except
    /// there's no upstream `ListState<D>` to wrap: see
    /// `crate::copy_move_dialog`'s module doc comment for why this is a
    /// small, hand-rolled `Render`-implementing view instead.
    copy_move_dialog: Option<Entity<CopyMoveDialogState>>,
    /// Saved by `open_copy_move_dialog`, restored and cleared by
    /// `close_copy_move_dialog`/`close_copy_move_dialog_deferred` -- same
    /// reasoning as `hotlist_previous_focus`.
    copy_move_dialog_previous_focus: Option<FocusHandle>,
    /// The core's Tokio runtime handle, threaded down from [`run`] into
    /// `Panel`/`FileTable` (each keeps its own clone for directory
    /// listings) -- retained here too, as of T-5.2.1, since the copy/move
    /// dialog is the first thing constructed *after* `Workspace::new`
    /// returns (in response to a later F5/F6 keypress) that still needs
    /// to spawn real background I/O (`plan_copy`/`plan_move`/
    /// `QueueManager::enqueue`) and had no other way to reach a handle.
    tokio_handle: tokio::runtime::Handle,
    /// T-5.2.1: the in-memory, real, running multi-job scheduler every
    /// copy/move dialog confirmation ultimately calls `enqueue` on.
    /// Constructed once, here, with its own dedicated `JobEvent` channel
    /// (`Self::new`'s consumer loop is the only reader). `Arc`, not a bare
    /// value: `CopyMoveDialogState::confirm` needs to call `enqueue` (a
    /// `&self` method) from inside a spawned Tokio task on a different
    /// thread, and `QueueManager` itself isn't `Clone` -- an `Arc` around
    /// it is the standard way to share a `&self`-only handle across
    /// threads without giving every caller its own independent scheduler.
    queue: Arc<QueueManager>,
    /// `~/.local/state/duet`, resolved once here -- `None` under the same
    /// rare XDG-resolution failure `settings_path`/`session_path`/
    /// `hotlist_path` already tolerate. Passed to every
    /// `CopyMoveDialogState` this workspace opens; `confirm` refuses to
    /// enqueue (with a toast) rather than guessing a job journal location
    /// when this is `None`.
    state_dir: Option<PathBuf>,
    /// T-5.2.2: the latest [`ProgressSnapshot`] sample for every job that
    /// has ever emitted a `JobEvent::Progress` and not yet finished --
    /// populated by [`Self::new`]'s `QueueManager` event-consumer loop
    /// (broadened, as of this task, beyond just `Finished`), read by the
    /// status-bar tray and `crate::operation_manager` for the byte-level
    /// numbers (`bytes_done`, `throughput_bytes_per_sec`, `eta_secs`, ...)
    /// that `duet_ops::Job`/`JobState` themselves deliberately do not
    /// carry (see `duet_ops::job`'s own module doc comment: `Job` is the
    /// queue-visible snapshot type, `ProgressSnapshot` is the separate,
    /// higher-frequency sample). This is the *only* place that data
    /// lives -- `crate::operation_manager` never duplicates it into a
    /// second cache, it only ever reads through
    /// [`Self::job_progress_snapshot`], so there is exactly one source of
    /// truth for "what did the last 100 ms sample say" and one for "what
    /// is this job's lifecycle state" (`self.queue.snapshot()`), matching
    /// this task's own instruction not to let the two responsibilities
    /// blur together. A finished job's entry is removed the moment its
    /// `Finished` event lands (same loop) -- without that eviction this
    /// map would grow for as long as the process runs, one entry per job
    /// ever enqueued, never freed.
    job_progress: HashMap<JobId, ProgressSnapshot>,

    /// `Some` while the T-5.2.2 operation manager overlay (FR-OPS-02/03)
    /// is open -- constructed fresh on every `open_operation_manager`,
    /// dropped on close. Mirrors `copy_move_dialog`'s own field exactly,
    /// including the "no upstream `ListState<D>` to wrap" reasoning --
    /// see `crate::operation_manager`'s module doc comment.
    operation_manager: Option<Entity<OperationManagerState>>,
    /// Saved by `open_operation_manager`, restored and cleared by
    /// `close_operation_manager` -- same reasoning as
    /// `copy_move_dialog_previous_focus`.
    operation_manager_previous_focus: Option<FocusHandle>,

    /// Set once, right after construction, by [`run`] (needs a `Window`
    /// and this view's own `Entity` to exist first -- see
    /// `ThemeController::install`'s doc comment). `Option` only to bridge
    /// that one-frame gap; every render after startup sees `Some`.
    theme: Option<ThemeController>,
}

/// Severity for one deferred toast in [`Workspace::pending_notice`] --
/// mirrors three of `duet_widgets::toast::Notification`'s four
/// constructors (everything but `info`, which no current caller of
/// `push_pending_notice` needs -- the "Nothing selected." case has a live
/// `Window` already and calls `window.push_notification` directly rather
/// than going through this deferred queue at all), so [`Workspace::render`]'s
/// drain loop can pick the right one without guessing from the message
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeLevel {
    Success,
    Warning,
    Error,
}

/// One deferred toast -- see [`Workspace::pending_notice`]'s doc comment
/// for why these queue instead of overwriting.
pub(crate) struct PendingNotice {
    level: NoticeLevel,
    message: String,
}

/// Which of the two panels a palette-dispatched tab command should apply
/// to -- see the `palette_target_panel` field's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelSide {
    Left,
    Right,
}

/// Progress of the background Tokio task that lists the current
/// directory -- exists purely to demonstrate the executor bridge
/// (T-4.1.1's AC), not as a real feature. Folded into the status bar's
/// left slot.
enum DemoState {
    Loading,
    Ready { dir: String, entry_count: usize },
    Failed(String),
}

impl Workspace {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        tokio_handle: tokio::runtime::Handle,
    ) -> Self {
        let settings_path = duet_config::paths::settings_path().ok();
        let splitter_ratio = settings_path
            .as_deref()
            .map(load_splitter_ratio)
            .unwrap_or(0.5)
            .clamp(SPLITTER_MIN_RATIO, SPLITTER_MAX_RATIO);
        let file_table_settings = FileTableSettings {
            mouse_mode: settings_path
                .as_deref()
                .map(load_mouse_mode)
                .unwrap_or_default(),
            quick_search_default_mode: settings_path
                .as_deref()
                .map(load_quick_search_default_mode)
                .unwrap_or_default(),
            quick_search_idle_timeout: settings_path
                .as_deref()
                .map(load_quick_search_idle_timeout)
                .unwrap_or(Duration::from_millis(1200)),
        };

        let command_line = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Command line (not wired to a shell yet -- T-5.3.5)")
        });

        // T-4.2.1: the process's current directory is every fallback tab's
        // fallback directory -- same directory the T-4.1.1 executor-wiring
        // demo below counts, so the status bar's "N entries in <dir>" line
        // and a freshly-installed left panel's actual row count are
        // checkable against each other.
        let initial_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // T-4.3.2: both panels are real now (the right one was still
        // `placeholder_panel` through T-4.2.x -- closing that gap landed
        // here rather than its own task since "each panel hosts N tabs"
        // can't be demonstrated on a panel that doesn't exist yet).
        // `session.json`'s tab list restores both from the last run;
        // `resolve_panel_session` degrades to one fresh tab at
        // `initial_dir` on first launch, a missing/corrupt file, or every
        // saved tab's directory having since vanished.
        //
        // T-4.3.7: a missing file (first launch, or `session.json` never
        // written yet) is the ordinary case and gets no user-facing
        // notice, only a log line -- but a file that *exists* and still
        // failed to load (corrupt JSON, a schema version this build
        // predates, permission denied, ...) is a real "we lost your
        // session" event, surfaced via `pending_notice` once a window
        // exists to show it in (see that field's doc comment).
        let session_path = duet_config::paths::session_path().ok();
        let (session, pending_notice) = session_path
            .as_deref()
            .map(load_session_with_notice)
            .unwrap_or((None, None));
        let (left_tabs, left_active) =
            resolve_panel_session(session.as_ref().map(|s| &s.left), &initial_dir);
        let (right_tabs, right_active) =
            resolve_panel_session(session.as_ref().map(|s| &s.right), &initial_dir);

        let left_panel = cx.new(|cx| {
            Panel::new(
                left_tabs,
                left_active,
                tokio_handle.clone(),
                file_table_settings,
                window,
                cx,
            )
        });
        let right_panel = cx.new(|cx| {
            Panel::new(
                right_tabs,
                right_active,
                tokio_handle.clone(),
                file_table_settings,
                window,
                cx,
            )
        });
        // Every structural tab change (`Panel::new_tab`/`close_active`/...)
        // and every real per-tab directory change (via each `FileTable`'s
        // `DirectoryChanged` event, which `Panel` already re-notifies on --
        // see `Panel::add_tab_entry`'s doc comment) calls `cx.notify()` on
        // the panel entity, which is exactly what these observers fire on.
        cx.observe(&left_panel, |this, _panel, cx| this.persist_session(cx))
            .detach();
        cx.observe(&right_panel, |this, _panel, cx| this.persist_session(cx))
            .detach();

        // T-4.3.7's periodic catch-up save -- see
        // `SESSION_PERIODIC_SAVE_INTERVAL`'s doc comment for why this
        // exists alongside the event-driven saves above rather than
        // instead of them. Runs for the process's whole lifetime (there's
        // no "stop" -- it simply stops being polled once `Workspace` is
        // dropped, at which point `this.update` starts failing and the
        // loop exits on its own).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(SESSION_PERIODIC_SAVE_INTERVAL)
                    .await;
                if this
                    .update(cx, |this, cx| this.persist_session(cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        // T-4.3.6: see the `palette_index` field's doc comment for why
        // this is built once, here, rather than per-open. Same
        // catalogue-plus-TC-keymap-CSV pattern `function_bar.rs`'s
        // `build_function_bar` already establishes.
        let palette_index = {
            let mut registry = CommandRegistry::new();
            register_builtin_commands(&mut registry).expect(
                "docs/commands.md's catalogue is embedded at compile time and covered by \
                 duet-commands' own parse tests -- registration failing here would mean the \
                 checked-in document itself is malformed",
            );
            let loaded = tc_csv::load();
            let resolved = keymap::resolve_with_locations([loaded.layer]);
            Rc::new(PaletteIndex::build(&registry, &resolved))
        };

        // T-4.3.5: loaded once, here, same "no live-reload path yet"
        // story as `palette_index` -- every add/remove/reorder updates
        // `hotlist_entries` and `hotlist_path` in lockstep from then on
        // (see `Self::persist_hotlist`), so there's nothing to re-read.
        let hotlist_path = duet_config::paths::hotlist_path().ok();
        let hotlist_entries = hotlist_path
            .as_deref()
            .map(load_hotlist_entries)
            .unwrap_or_default();

        // T-4.3.7's original notice (see `pending_notice`'s doc comment)
        // becomes the queue's first, possible entry.
        let pending_notice: Vec<PendingNotice> = pending_notice
            .into_iter()
            .map(|message| PendingNotice {
                level: NoticeLevel::Warning,
                message,
            })
            .collect();

        // T-5.2.1: the copy/move dialog's `QueueManager` and the event
        // channel every job it enqueues reports through. `state_dir`
        // shares `settings_path`/`session_path`/`hotlist_path`'s own "best
        // -effort, `None` under a rare XDG failure" tolerance -- see that
        // field's own doc comment for what happens when it's `None`.
        let (queue_events_tx, mut queue_events_rx) =
            tokio::sync::mpsc::unbounded_channel::<JobEvent>();
        let queue = Arc::new(QueueManager::new(
            COPY_MOVE_QUEUE_MAX_CONCURRENT,
            queue_events_tx,
        ));
        let state_dir = duet_config::paths::duet_state_dir().ok();

        // The queue's event-consumer loop: drains every job's `JobEvent`s
        // in one `this.update` per event (not one per handled variant --
        // see the loop body's own comment) and always ends with
        // `cx.notify()`, so the tray/manager's next render always reflects
        // whatever just happened. T-5.2.1 originally handled only
        // `Finished` here (toast + `pending_panel_refresh`, see that
        // field's own doc comment for why *every* `Finished`, not just a
        // successful one, triggers a refresh); T-5.2.2 broadens it to also
        // cache `Progress` samples (`job_progress`) for the tray/manager's
        // live byte-level numbers. Every other variant (`Queued`,
        // `Started`, `Paused`, `StepStarted`, `ConflictDetected`, ...)
        // still needs no *state* update of its own -- `self.queue.
        // snapshot()` already reflects a job's current `JobState` the
        // moment the event that caused it was sent (the executor updates
        // `Job::state` before emitting the corresponding event) -- but
        // still triggers the unconditional `cx.notify()` below, since
        // that's what actually wakes a redraw to pick the new snapshot up.
        // Runs for the app's whole lifetime, same "stops polling once
        // `Workspace` is dropped" shape as the periodic session-save loop
        // just above.
        cx.spawn(async move |this, cx| {
            while let Some(event) = queue_events_rx.recv().await {
                let updated = this.update(cx, |this, cx| {
                    match event {
                        JobEvent::Progress { job_id, snapshot } => {
                            this.job_progress.insert(job_id, snapshot);
                        }
                        JobEvent::Finished {
                            job_id,
                            outcome,
                            report,
                        } => {
                            let notice = summarize_job_finished(outcome, &report);
                            if let Some((level, message)) = notice {
                                this.push_pending_notice(level, message, cx);
                            }
                            this.pending_panel_refresh = true;
                            // A finished job stops sampling `Progress` for
                            // good -- see `job_progress`'s own doc comment
                            // for why this eviction matters (an
                            // unboundedly growing map otherwise).
                            this.job_progress.remove(&job_id);
                        }
                        JobEvent::ConflictDetected { job_id, prompt } => {
                            // A real interactive answer is T-5.2.3's own
                            // scope, not this task's -- logged (deliberately
                            // just the step index and source path, not the
                            // full `Metadata` payload `prompt` also
                            // carries) so a conflict occurring today isn't
                            // entirely silent in the meantime.
                            tracing::debug!(
                                target: "duet_ui::workspace",
                                job_id = job_id.0,
                                step_index = prompt.step_index,
                                source = %prompt.source,
                                "conflict detected with no live prompt UI yet (T-5.2.3)"
                            );
                        }
                        _ => {}
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();

        Self {
            demo: DemoState::Loading,
            focus_handle: cx.focus_handle(),
            splitter_ratio,
            resizable_state: cx.new(|_| ResizableState::default()),
            left_panel,
            right_panel,
            function_keys: function_bar::build_function_bar(),
            command_line,
            settings_path,
            session_path,
            pending_notice,
            pending_focus_restore: None,
            pending_panel_refresh: false,
            palette_index,
            command_palette: None,
            palette_previous_focus: None,
            palette_target_panel: PanelSide::Left,
            hotlist_path,
            hotlist_entries,
            hotlist: None,
            hotlist_previous_focus: None,
            hotlist_target_panel: PanelSide::Left,
            copy_move_dialog: None,
            copy_move_dialog_previous_focus: None,
            tokio_handle: tokio_handle.clone(),
            queue,
            state_dir,
            job_progress: HashMap::new(),
            operation_manager: None,
            operation_manager_previous_focus: None,
            theme: None,
        }
    }

    /// `ws.theme_mut()` is used only by [`crate::theme_controller`]'s live
    /// callbacks; panics before the first render completes theme
    /// installation (see the `theme` field's doc comment).
    pub(crate) fn theme_mut(&mut self) -> &mut ThemeController {
        self.theme
            .as_mut()
            .expect("ThemeController::install must run before any live theme callback fires")
    }

    /// `Ctrl+Left`/`Ctrl+Right`: nudges `splitter_ratio` by
    /// `SPLITTER_KEYBOARD_STEP` and forces a fresh `resizable_state` (see
    /// that field's doc comment for why a fresh entity is required).
    fn resize_splitter_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        let new_ratio = (self.splitter_ratio + delta).clamp(SPLITTER_MIN_RATIO, SPLITTER_MAX_RATIO);
        if (new_ratio - self.splitter_ratio).abs() < f32::EPSILON {
            return;
        }
        self.splitter_ratio = new_ratio;
        self.resizable_state = cx.new(|_| ResizableState::default());
        self.persist_splitter_ratio(cx);
        cx.notify();
    }

    /// After a mouse drag completes, reconcile `splitter_ratio` from the
    /// widget's own post-drag pixel sizes (no entity swap needed here --
    /// the drag already mutated `resizable_state` correctly; this just
    /// updates our own authoritative ratio to match, and persists it).
    fn sync_ratio_from_drag(&mut self, state: &Entity<ResizableState>, cx: &mut Context<Self>) {
        let sizes = state.read(cx).sizes().clone();
        if let [left, right] = sizes[..] {
            let total = left + right;
            if f32::from(total) > 0.0 {
                self.splitter_ratio = (left / total).clamp(SPLITTER_MIN_RATIO, SPLITTER_MAX_RATIO);
                self.persist_splitter_ratio(cx);
            }
        }
    }

    /// Saves `splitter_ratio` to `settings.toml` off the UI thread
    /// (design.md §8.2: "main thread does no I/O, ever"). Best-effort: a
    /// failure is logged, never surfaced as a crash -- losing the
    /// persisted ratio for one session is not worth interrupting the user
    /// over.
    fn persist_splitter_ratio(&self, cx: &mut Context<Self>) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let ratio = self.splitter_ratio;
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = save_splitter_ratio(&path, ratio) {
                    tracing::warn!(
                        target: "duet_ui::workspace",
                        "failed to persist splitter ratio: {err}"
                    );
                }
            })
            .detach();
    }

    /// `Tab` (`focus.other_panel`): moves keyboard focus to whichever
    /// panel doesn't currently have it. "Doesn't currently have it" is
    /// derived from real focus state (`left_panel`'s active tab's
    /// `FocusHandle`), not a separately tracked "which side is active"
    /// field -- same reasoning as [`Self::dual_pane`]'s `left_active`/
    /// `right_active`, and for the same reason: one source of truth,
    /// nothing to drift out of sync. Defaults to focusing the left panel
    /// if, somehow, neither currently holds focus (e.g. the command line
    /// does) -- an arbitrary but reasonable landing spot, not a state that
    /// should be reachable in practice since nothing else binds `Tab`.
    fn focus_other_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let left_focused = self
            .left_panel
            .read(cx)
            .active_focus_handle(cx)
            .is_focused(window);
        let target = if left_focused {
            &self.right_panel
        } else {
            &self.left_panel
        };
        let handle = target.read(cx).active_focus_handle(cx);
        window.focus(&handle);
    }

    /// `Ctrl+Shift+P` (`OpenCommandPalette`, T-4.3.6): opens the command
    /// palette overlay. A no-op if it's already open (`Ctrl+Shift+P`
    /// twice shouldn't stack a second one, or discard whatever query the
    /// user already typed by rebuilding from scratch). Captures which
    /// panel currently has focus (before this moves focus onto the
    /// palette's own query input, at which point neither panel would read
    /// as focused any more -- see `palette_target_panel`'s doc comment)
    /// and the focus to restore on close.
    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            return;
        }

        self.palette_target_panel = if self
            .right_panel
            .read(cx)
            .active_focus_handle(cx)
            .is_focused(window)
        {
            PanelSide::Right
        } else {
            PanelSide::Left
        };
        self.palette_previous_focus = window.focused(cx);

        let index = self.palette_index.clone();
        let weak_workspace = cx.entity().downgrade();
        let state = cx.new(|cx| {
            ListState::new(
                CommandPaletteDelegate::new(index, weak_workspace),
                window,
                cx,
            )
            .searchable(true)
        });
        state.update(cx, |state, cx| state.focus(window, cx));
        self.command_palette = Some(state);
        cx.notify();
    }

    /// Closes the command palette overlay (Escape, a click outside it, or
    /// right after a command is dispatched) and restores keyboard focus
    /// to whatever had it before the palette opened.
    pub(crate) fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.command_palette = None;
        if let Some(handle) = self.palette_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Runs the confirmed palette entry, then closes the palette -- see
    /// `command_palette.rs`'s module doc comment for why this is a small,
    /// explicit `match` rather than going through `Command::handler`
    /// (every built-in handler is an intentional stub). Anything not
    /// covered here is a real registered command with no implementation
    /// yet, reported via a toast rather than silently doing nothing.
    pub(crate) fn dispatch_palette_command(
        &mut self,
        id: &CommandId,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = match self.palette_target_panel {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };
        let handled = match id.as_str() {
            "tab.new" => {
                panel.update(cx, |panel, cx| panel.new_tab(window, cx));
                true
            }
            "tab.close" => {
                panel.update(cx, |panel, cx| panel.close_active(window, cx));
                true
            }
            "tab.next" => {
                panel.update(cx, |panel, cx| panel.next_tab(window, cx));
                true
            }
            "tab.prev" => {
                panel.update(cx, |panel, cx| panel.prev_tab(window, cx));
                true
            }
            "tab.duplicate" => {
                panel.update(cx, |panel, cx| panel.duplicate_active(window, cx));
                true
            }
            "tab.close_others" => {
                panel.update(cx, |panel, cx| panel.close_others(window, cx));
                true
            }
            "tab.reopen_closed" => {
                panel.update(cx, |panel, cx| panel.reopen_closed(window, cx));
                true
            }
            "tab.lock" => {
                panel.update(cx, |panel, cx| panel.toggle_lock(cx));
                true
            }
            "tab.lock_dir_change" => {
                panel.update(cx, |panel, cx| panel.toggle_lock_dir_change(cx));
                true
            }
            "tab.move_left" => {
                panel.update(cx, |panel, cx| panel.move_active_left(cx));
                true
            }
            "tab.move_right" => {
                panel.update(cx, |panel, cx| panel.move_active_right(cx));
                true
            }
            "focus.other_panel" => {
                self.focus_other_panel(window, cx);
                true
            }
            "hotlist.open" => {
                self.open_hotlist_for_panel(self.palette_target_panel, window, cx);
                true
            }
            "hotlist.add" => {
                self.add_dir_to_hotlist_for_panel(self.palette_target_panel, window, cx);
                true
            }
            _ => false,
        };
        if !handled {
            window.push_notification(
                Notification::info(format!("\u{201c}{title}\u{201d} isn't wired up yet.")),
                cx,
            );
        }
        self.close_command_palette(window, cx);
    }

    /// Which panel is currently focused -- the same capture-at-invocation
    /// logic `open_command_palette` already established for
    /// `palette_target_panel`, reused here for both `open_hotlist` and
    /// `AddCurrentDirToHotlist` (which needs the same answer without
    /// opening the overlay at all).
    fn focused_panel_side(&self, window: &Window, cx: &App) -> PanelSide {
        if self
            .right_panel
            .read(cx)
            .active_focus_handle(cx)
            .is_focused(window)
        {
            PanelSide::Right
        } else {
            PanelSide::Left
        }
    }

    /// `Ctrl+D` (`OpenHotlist`, T-4.3.5, FR-NAV-08): opens the directory
    /// hotlist overlay, targeting whichever panel currently has focus.
    fn open_hotlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.focused_panel_side(window, cx);
        self.open_hotlist_for_panel(target, window, cx);
    }

    /// The `hotlist.open` half of [`Self::dispatch_palette_command`] and
    /// [`Self::open_hotlist`]'s shared implementation, taking `target`
    /// explicitly rather than deriving it from window focus: when this
    /// runs from the palette, focus is still on the palette's own list at
    /// this point (the palette closes *after* dispatch returns), so
    /// [`Self::focused_panel_side`] would see the palette itself, not the
    /// panel the user actually meant -- `dispatch_palette_command` already
    /// knows the right answer via `palette_target_panel` (captured back
    /// when the palette *opened*, before it stole focus) and passes that
    /// straight through instead.
    ///
    /// A no-op if the overlay is already open (same reasoning as
    /// `open_command_palette`: reopening shouldn't stack a second one).
    fn open_hotlist_for_panel(
        &mut self,
        target: PanelSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hotlist.is_some() {
            return;
        }

        self.hotlist_target_panel = target;
        self.hotlist_previous_focus = window.focused(cx);

        let entries = self.hotlist_entries.clone();
        let weak_workspace = cx.entity().downgrade();
        let state =
            cx.new(|cx| ListState::new(HotlistDelegate::new(entries, weak_workspace), window, cx));
        state.update(cx, |state, cx| state.focus(window, cx));
        self.hotlist = Some(state);
        cx.notify();
    }

    /// Closes the hotlist overlay (Escape, a click outside it, or right
    /// after Enter navigates) and restores keyboard focus to whatever had
    /// it before it opened. Mirrors `close_command_palette` exactly.
    pub(crate) fn close_hotlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hotlist = None;
        if let Some(handle) = self.hotlist_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// `hotlist.navigate` (Enter, inside the overlay, via
    /// `HotlistDelegate::confirm`): navigates the captured target panel's
    /// active tab to `dir`, then closes the overlay. Goes through
    /// `FileTable::navigate_to_path` (T-4.3.5's own new entry point --
    /// `navigate_to` itself is private to `file_table`'s module).
    pub(crate) fn navigate_to_hotlist_entry(
        &mut self,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = match self.hotlist_target_panel {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };
        let table = panel.read(cx).active_table().clone();
        table.update(cx, |table, cx| {
            table.navigate_to_path(PathBuf::from(dir), window, cx);
        });
        self.close_hotlist(window, cx);
    }

    /// `Ctrl+Shift+D` (`AddCurrentDirToHotlist`, `hotlist.add`):
    /// bookmarks whichever panel currently has focus's active tab's
    /// current directory.
    fn add_current_dir_to_hotlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.focused_panel_side(window, cx);
        self.add_dir_to_hotlist_for_panel(target, window, cx);
    }

    /// The `hotlist.add` half of [`Self::dispatch_palette_command`] and
    /// [`Self::add_current_dir_to_hotlist`]'s shared implementation -- see
    /// [`Self::open_hotlist_for_panel`]'s doc comment for why `target` is
    /// taken explicitly rather than re-derived from window focus here. A
    /// no-op (with an explanatory toast, not silence) if `target`'s
    /// directory is already bookmarked -- TC's own hotlist doesn't allow
    /// duplicate entries either.
    fn add_dir_to_hotlist_for_panel(
        &mut self,
        target: PanelSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = match target {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        };
        let dir = panel
            .read(cx)
            .active_table()
            .read(cx)
            .current_dir()
            .to_string_lossy()
            .into_owned();

        if self.hotlist_entries.iter().any(|e| e.path == dir) {
            window.push_notification(
                Notification::info(format!("{dir} is already in the hotlist.")),
                cx,
            );
            return;
        }

        self.hotlist_entries.push(HotlistEntry {
            path: dir.clone(),
            label: None,
        });
        self.persist_hotlist(cx);
        window.push_notification(Notification::success(format!("Bookmarked {dir}")), cx);
    }

    /// `Delete` (`HotlistRemoveEntry`, `hotlist.remove`, inside the
    /// overlay): removes the selected entry, moves the selection to
    /// whatever now sits at (or nearest to) the same position, and
    /// persists the change immediately. A no-op if nothing is selected
    /// (an empty hotlist).
    fn remove_selected_hotlist_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.hotlist.clone() else {
            return;
        };
        let entries = state.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            let Some(selected) = delegate.selected else {
                return delegate.entries.clone();
            };
            delegate.entries.remove(selected);
            let new_selected = if delegate.entries.is_empty() {
                None
            } else {
                Some(selected.min(delegate.entries.len() - 1))
            };
            let entries = delegate.entries.clone();
            // `ListState::set_selected_index` (not writing
            // `delegate.selected` by hand) is what actually moves the
            // *rendered* highlight -- `ListState` tracks the real
            // selected index itself and only notifies the delegate of
            // changes via `ListDelegate::set_selected_index`, it doesn't
            // read the delegate's own copy back.
            state.set_selected_index(new_selected.map(IndexPath::new), window, cx);
            entries
        });
        self.hotlist_entries = entries;
        self.persist_hotlist(cx);
    }

    /// `Ctrl+Up`/`Ctrl+Down` (`HotlistMoveUp`/`HotlistMoveDown`,
    /// `hotlist.reorder`, inside the overlay): swaps the selected entry
    /// with its neighbor in `direction` (`-1` = up, `1` = down), a no-op
    /// at either end of the list or if nothing is selected. Persists
    /// immediately.
    fn move_selected_hotlist_entry(
        &mut self,
        direction: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.hotlist.clone() else {
            return;
        };
        let entries = state.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            let Some(selected) = delegate.selected else {
                return delegate.entries.clone();
            };
            let target = selected as isize + direction;
            if target < 0 || target as usize >= delegate.entries.len() {
                return delegate.entries.clone();
            }
            let target = target as usize;
            delegate.entries.swap(selected, target);
            let entries = delegate.entries.clone();
            state.set_selected_index(Some(IndexPath::new(target)), window, cx);
            entries
        });
        self.hotlist_entries = entries;
        self.persist_hotlist(cx);
    }

    /// Writes `self.hotlist_entries` to `hotlist_path` off the UI thread,
    /// matching `persist_session`'s own "best-effort, log on failure,
    /// never a crash" pattern. Called after every add/remove/reorder.
    fn persist_hotlist(&self, cx: &mut Context<Self>) {
        let Some(path) = self.hotlist_path.clone() else {
            return;
        };
        let entries = self.hotlist_entries.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = save_hotlist_entries(&path, &entries) {
                    tracing::warn!(
                        target: "duet_ui::workspace",
                        "failed to save hotlist.toml: {err}"
                    );
                }
            })
            .detach();
    }

    /// Gathers both panels' *live* tab lists (real current directories,
    /// not whatever was last saved -- see `Panel::snapshot`'s doc comment)
    /// and writes them to `session.json` off the UI thread, matching
    /// [`Self::persist_splitter_ratio`]'s pattern exactly. Called by the
    /// `cx.observe` subscriptions [`Self::new`] sets up on both panels, so
    /// this fires on every structural tab change and every real directory
    /// change in either panel -- see the `session_path` field's doc
    /// comment for why that eagerness matters. Best-effort: a failure is
    /// logged, never surfaced as a crash.
    fn persist_session(&self, cx: &mut Context<Self>) {
        let Some(path) = self.session_path.clone() else {
            return;
        };
        let session = duet_config::Session {
            schema_version: duet_config::session::SESSION_SCHEMA_VERSION,
            left: self.left_panel.read(cx).snapshot(cx),
            right: self.right_panel.read(cx).snapshot(cx),
        };
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = duet_config::session::save(&path, &session) {
                    tracing::warn!(
                        target: "duet_ui::workspace",
                        "failed to persist session: {err}"
                    );
                }
            })
            .detach();
    }

    /// Pushes one deferred toast and wakes the next render -- the
    /// `cx.notify()` here is load-bearing, not decorative: every caller of
    /// this method runs from a background-completion callback with no
    /// live `Window` (see `pending_notice`'s doc comment), so nothing else
    /// would otherwise schedule the render that actually fires the toast.
    pub(crate) fn push_pending_notice(
        &mut self,
        level: NoticeLevel,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.pending_notice.push(PendingNotice {
            level,
            message: message.into(),
        });
        cx.notify();
    }

    /// F5 (`CopyDialog`) / F6 (`MoveDialog`), T-5.2.1: resolves what to
    /// operate on from whichever panel currently has focus (selection, or
    /// the cursor row if nothing's selected -- `resolve_source_names`),
    /// defaults the destination to the *other* panel's current directory
    /// (`docs/keymap-tc.csv`'s own "F5 ... to the other panel's
    /// directory"), and opens the dialog. A no-op (with an explanatory
    /// toast) if there is nothing to operate on -- an empty directory with
    /// nothing selected and no cursor row to fall back to. A no-op,
    /// silently, if the dialog is already open (same "reopening shouldn't
    /// stack a second one" reasoning as `open_hotlist_for_panel`/
    /// `open_command_palette`).
    fn open_copy_move_dialog(
        &mut self,
        kind: JobKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.copy_move_dialog.is_some() {
            return;
        }

        let source_side = self.focused_panel_side(window, cx);
        let dest_side = match source_side {
            PanelSide::Left => PanelSide::Right,
            PanelSide::Right => PanelSide::Left,
        };
        let source_panel = match source_side {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };
        let dest_panel = match dest_side {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };

        let source_table = source_panel.read(cx).active_table().clone();
        let current_dir = source_table.read(cx).current_dir().to_path_buf();
        let names = {
            let table_state = source_table.read(cx).state().read(cx);
            crate::copy_move_dialog::resolve_source_names(table_state.delegate())
        };
        if names.is_empty() {
            window.push_notification(Notification::info("Nothing selected."), cx);
            return;
        }
        let sources: Vec<VPath> = names
            .iter()
            .filter_map(|name| crate::file_table::local_vpath(&current_dir.join(name)).ok())
            .collect();
        if sources.is_empty() {
            window.push_notification(
                Notification::warning("The selected item(s) don't have a valid path."),
                cx,
            );
            return;
        }

        let dest_dir = dest_panel.read(cx).active_table().read(cx).current_dir();
        let initial_destination = dest_dir.to_string_lossy().into_owned();

        self.copy_move_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let state = cx.new(|cx| {
            CopyMoveDialogState::new(
                kind,
                sources,
                initial_destination,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                window,
                cx,
            )
        });
        self.copy_move_dialog = Some(state);
        cx.notify();
    }

    /// Closes the copy/move dialog (Escape, or a click outside it) and
    /// restores keyboard focus to whatever had it before it opened.
    /// Mirrors `close_hotlist` exactly -- always has a live `Window`.
    pub(crate) fn close_copy_move_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_move_dialog = None;
        if let Some(handle) = self.copy_move_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// The one close path with no live `Window` -- `CopyMoveDialogState::
    /// confirm`'s async plan/enqueue success callback. See
    /// `pending_focus_restore`'s doc comment: the actual `window.focus`
    /// call happens on `Self::render`'s next pass instead.
    pub(crate) fn close_copy_move_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.copy_move_dialog = None;
        self.pending_focus_restore = self.copy_move_dialog_previous_focus.take();
        cx.notify();
    }

    /// [`crate::copy_move_dialog::CopyMoveDialogState::try_complete_destination`]'s
    /// "does `parent` match either panel's already-loaded directory" half
    /// -- see that method's own doc comment for the full picture (T-5.2.1's
    /// deliberately narrow Tab-completion). The actual name-matching logic
    /// (`complete_against_model`) is factored into `copy_move_dialog` so
    /// it stays unit-testable against a plain `DirectoryModel`, with no
    /// live `Workspace`/panel needed.
    pub(crate) fn completion_candidate(
        &self,
        parent: &Path,
        prefix: &str,
        cx: &App,
    ) -> Option<String> {
        for panel in [&self.left_panel, &self.right_panel] {
            let table_entity = panel.read(cx).active_table();
            let table = table_entity.read(cx);
            if table.current_dir() == parent {
                let model = table.state().read(cx).delegate().model();
                return crate::copy_move_dialog::complete_against_model(model, parent, prefix);
            }
        }
        None
    }

    /// `Ctrl+O` (`OpenOperationManager`, T-5.2.2, FR-OPS-02/03): opens the
    /// expandable operation manager overlay listing every T-5.1.13 queue
    /// job (`self.queue.snapshot()`, read directly by the overlay itself
    /// once open) with live progress and per-job pause/resume/cancel --
    /// see `crate::operation_manager`'s module doc comment for the
    /// overlay's own architecture. A no-op if already open, mirroring
    /// every other overlay's "reopening shouldn't stack a second one"
    /// convention (`open_hotlist_for_panel`/`open_command_palette`/
    /// `open_copy_move_dialog`).
    fn open_operation_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation_manager.is_some() {
            return;
        }

        self.operation_manager_previous_focus = window.focused(cx);
        let weak_workspace = cx.entity().downgrade();
        let queue = self.queue.clone();
        let state = cx.new(|cx| OperationManagerState::new(weak_workspace, queue, cx));
        let handle = state.read(cx).focus_handle(cx);
        window.focus(&handle);
        self.operation_manager = Some(state);
        cx.notify();
    }

    /// Closes the operation manager overlay (Escape, or a click outside
    /// it) and restores keyboard focus to whatever had it before it
    /// opened. Mirrors `close_hotlist`/`close_copy_move_dialog` exactly --
    /// always has a live `Window` (nothing about closing this overlay
    /// needs the deferred, no-`Window` path `close_copy_move_dialog_
    /// deferred` exists for).
    pub(crate) fn close_operation_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.operation_manager = None;
        if let Some(handle) = self.operation_manager_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// A single job's latest T-5.1.11 progress sample, if any has landed
    /// yet -- see `job_progress`'s own doc comment. `None` covers both
    /// "not currently `Running`/`Paused`" and the real window between
    /// `JobEvent::Started` and that job's first 100 ms sample;
    /// `crate::operation_manager` treats both the same way ("no progress
    /// data yet") since there is nothing to synthesize for either.
    pub(crate) fn job_progress_snapshot(&self, id: JobId) -> Option<ProgressSnapshot> {
        self.job_progress.get(&id).copied()
    }

    /// The status-bar tray's own aggregate line (T-5.2.2, FR-OPS-02:
    /// "a tray in the status bar showing aggregate progress"). `None`
    /// renders nothing at all -- see [`crate::operation_manager::
    /// tray_summary`]'s own doc comment for the full aggregation rule
    /// and why "nothing," not a "0 operations" placeholder, is this
    /// task's own reading of "unobtrusive." `self.queue.snapshot()` is
    /// the same `O(job count)` call the operation manager itself makes on
    /// every render -- no separate aggregate is cached here, so there is
    /// nothing that could drift out of sync with the queue's own truth.
    fn operations_tray_text(&self) -> Option<String> {
        crate::operation_manager::tray_summary(&self.queue.snapshot(), &self.job_progress)
    }

    fn dual_pane(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let theme = cx.theme();
        let total = px(900.); // A reasonable initial estimate; the widget's own
        // canvas-driven `adjust_to_container_size` immediately corrects this to
        // the real measured width on first layout and on every subsequent
        // window resize (see `gpui-component-0.5.1/src/resizable/mod.rs`), so
        // this only affects the very first frame before layout has happened.
        let left_w = total * self.splitter_ratio;
        let right_w = total * (1.0 - self.splitter_ratio);

        // FR-NAV-02's "active panel indicated by cursor rendering and
        // header treatment": derived directly from real keyboard focus
        // (`FocusHandle::is_focused`) rather than a separately-tracked
        // `active_panel` field, so there's exactly one source of truth
        // and no way for the two to drift apart. Both panels are real now
        // (T-4.3.2) -- the header/footer text is always the *active tab's*
        // path/stats within whichever panel, since that's the only thing
        // meaningfully "this panel's" state once a panel can hold more
        // than one directory at a time.
        let (left_header, left_footer, left_active) =
            panel_header_footer_active(&self.left_panel, window, cx);
        let (right_header, right_footer, right_active) =
            panel_header_footer_active(&self.right_panel, window, cx);

        h_resizable("workspace-splitter")
            .with_state(&self.resizable_state)
            .child(
                resizable_panel()
                    .size(left_w)
                    .size_range(px(160.)..Pixels::MAX)
                    .child(panel_view(
                        &self.left_panel,
                        left_header,
                        left_footer,
                        left_active,
                        tokens,
                        theme.border,
                    )),
            )
            .child(
                resizable_panel()
                    .size(right_w)
                    .size_range(px(160.)..Pixels::MAX)
                    .child(panel_view(
                        &self.right_panel,
                        right_header,
                        right_footer,
                        right_active,
                        tokens,
                        theme.border,
                    )),
            )
            .on_resize(
                cx.listener(|this, state: &Entity<ResizableState>, _window, cx| {
                    this.sync_ratio_from_drag(state, cx);
                }),
            )
    }

    fn command_line_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .bg(tokens.color.panel_bg_active)
            .border_t_1()
            .border_color(tokens.color.border_default)
            .items_center()
            .child(
                gpui::div()
                    .text_color(tokens.color.accent)
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("$"),
            )
            .child(gpui::div().flex_1().child(Input::new(&self.command_line)))
    }

    fn status_bar_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let status_text: SharedString = match &self.demo {
            DemoState::Loading => "Reading current directory via the core Tokio runtime...".into(),
            DemoState::Ready { dir, entry_count } => {
                format!("core -> UI bridge OK: {entry_count} entries in {dir}").into()
            }
            DemoState::Failed(err) => format!("core -> UI bridge error: {err}").into(),
        };

        let theme_text: SharedString = match &self.theme {
            Some(theme) => {
                let mode = if theme.mode().is_dark() {
                    "dark"
                } else {
                    "light"
                };
                match theme.active_file() {
                    Some(path) => format!(
                        "theme: {mode} ({})",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                    )
                    .into(),
                    None => format!("theme: {mode} (built-in)").into(),
                }
            }
            None => "theme: (initializing)".into(),
        };

        // T-5.2.2's status-bar tray (FR-OPS-02): `None` while the queue is
        // idle, so nothing renders at all -- see `operations_tray_text`'s
        // own doc comment. Clicking it is a nice-to-have shortcut to the
        // same overlay `Ctrl+O` opens, not a replacement for the
        // keybinding (this task's own instruction).
        let tray_text = self.operations_tray_text();

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .justify_between()
            .bg(tokens.color.statusbar_bg)
            .text_color(tokens.color.statusbar_fg)
            .text_size(px(12.))
            .child(gpui::div().child(status_text))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .children(tray_text.map(|text| {
                        gpui::div()
                            .id("operations-tray")
                            .cursor_pointer()
                            .child(text)
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.open_operation_manager(window, cx);
                            }))
                    }))
                    .child(gpui::div().child(theme_text)),
            )
    }

    fn function_key_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        h_flex()
            .w_full()
            .gap_px()
            .bg(tokens.color.statusbar_bg)
            .border_t_1()
            .border_color(tokens.color.border_default)
            .children(self.function_keys.iter().map(|slot| {
                h_flex()
                    .flex_1()
                    .justify_center()
                    .items_center()
                    .gap_1()
                    .py_1()
                    .child(
                        gpui::div()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(tokens.color.accent)
                            .child(slot.key),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.))
                            .text_color(tokens.color.statusbar_fg)
                            .child(if slot.label.is_empty() {
                                SharedString::from("—")
                            } else {
                                SharedString::from(slot.label.clone())
                            }),
                    )
            }))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.background;
        let fg = theme.foreground;

        // T-5.2.1: the one deferred close path with no live `Window` --
        // see `pending_focus_restore`'s doc comment. Drained before the
        // notice queue below on general principle (restoring focus first
        // reads more naturally than the other order), though the two are
        // otherwise independent.
        if let Some(handle) = self.pending_focus_restore.take() {
            window.focus(&handle);
        }

        // T-4.3.7 / T-5.2.1: every deferred toast queued since the last
        // render -- see `pending_notice`'s doc comment for why this is a
        // drain-everything loop rather than a single `.take()` (T-4.3.7's
        // original shape): a background job-completion callback can push
        // more than one of these before the next render ever runs.
        for notice in self.pending_notice.drain(..) {
            let toast = match notice.level {
                NoticeLevel::Success => Notification::success(notice.message),
                NoticeLevel::Warning => Notification::warning(notice.message),
                NoticeLevel::Error => Notification::error(notice.message),
            };
            window.push_notification(toast, cx);
        }

        // T-5.2.1 (post-UAT): a finished copy/move job means either panel
        // may now be showing stale rows -- sources that moved away,
        // destinations that just appeared -- until the user manually
        // re-navigates. See `pending_panel_refresh`'s own doc comment for
        // why this always refreshes both panels rather than trying to
        // track exactly which directories a given job touched.
        if std::mem::take(&mut self.pending_panel_refresh) {
            for panel in [&self.left_panel, &self.right_panel] {
                let table = panel.read(cx).active_table().clone();
                table.update(cx, |table, cx| table.reread_current_dir(cx));
            }
        }

        v_flex()
            .id("workspace-root")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .on_action(cx.listener(|this, _: &ResizeSplitterLeft, _window, cx| {
                this.resize_splitter_by(-SPLITTER_KEYBOARD_STEP, cx);
            }))
            .on_action(cx.listener(|this, _: &ResizeSplitterRight, _window, cx| {
                this.resize_splitter_by(SPLITTER_KEYBOARD_STEP, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusOtherPanel, window, cx| {
                this.focus_other_panel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.open_command_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenHotlist, window, cx| {
                this.open_hotlist(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AddCurrentDirToHotlist, window, cx| {
                this.add_current_dir_to_hotlist(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyDialog, window, cx| {
                this.open_copy_move_dialog(JobKind::Copy, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveDialog, window, cx| {
                this.open_copy_move_dialog(JobKind::Move, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenOperationManager, window, cx| {
                this.open_operation_manager(window, cx);
            }))
            .child(gpui::div().flex_1().p_2().child(self.dual_pane(window, cx)))
            .child(self.command_line_row(cx))
            .child(self.status_bar_row(cx))
            .child(self.function_key_bar(cx))
            .when_some(self.command_palette.clone(), |this, state| {
                this.child(command_palette_overlay(&state, cx))
            })
            .when_some(self.hotlist.clone(), |this, state| {
                this.child(hotlist_overlay(&state, cx))
            })
            .when_some(self.copy_move_dialog.clone(), |this, state| {
                this.child(copy_move_dialog_overlay(&state, cx))
            })
            .when_some(self.operation_manager.clone(), |this, state| {
                this.child(operation_manager_overlay(&state, cx))
            })
    }
}

/// T-4.3.6: the palette's overlay chrome -- a full-window backdrop behind
/// a centered card wrapping the real `duet_widgets::list::List` widget,
/// which already owns the query input, the virtualised results list, and
/// all of Up/Down/Enter/Escape's keyboard handling (`CommandPaletteDelegate`
/// supplies the data and the `confirm`/`cancel` callbacks). `.absolute()`
/// positions this against the nearest positioned ancestor, which is why
/// `Workspace::render`'s root carries `.relative()`.
///
/// `max_h` goes on `List::new(state)` itself, not the wrapping card --
/// matching `gpui-component`'s own established usage
/// (`select.rs`'s dropdown: `List::new(&self.list)...max_h(rems(20.))`
/// on the `List` directly, wrapped in a plain, sizing-unconstrained
/// `v_flex()`). `List::render` explicitly pulls `max_size.height` out of
/// its *own* style into `options.max_height`, which is what actually
/// bounds the internal virtualized results view -- setting it on an
/// ancestor div instead (an earlier bug here: UAT reported the palette
/// opening but search always returning nothing, and typing feeling
/// stuttery) leaves that bound unset, so the virtualized list has no
/// definite height to lay out into at all.
///
/// `.occlude()` on both the backdrop and the card, plus `.on_mouse_down_out`
/// on the card, mirror `select.rs`'s own popup exactly -- without the
/// backdrop's `.occlude()`, a click anywhere on it falls straight through
/// to whatever panel is underneath (UAT: "click on a panel while the
/// palette is open activates that panel, the palette doesn't close, and
/// controls freeze" -- clicking moved real GPUI focus onto the panel
/// while `command_palette` stayed `Some`, leaving the still-rendered
/// overlay visually on top but no longer the thing anything was actually
/// talking to). `on_mouse_down_out` listens window-wide regardless of the
/// card's own size (it's a capture-phase, `window.mouse_position()`-based
/// check, not scoped to the backdrop's bounds), so a click on the
/// backdrop -- now that it can't reach the panel underneath either --
/// closes the palette the same way Escape does.
fn command_palette_overlay(
    state: &Entity<ListState<CommandPaletteDelegate>>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("command-palette-backdrop")
        .absolute()
        .size_full()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(96.))
        .bg(gpui::hsla(0., 0., 0., 0.5))
        .child(
            gpui::div()
                .id("command-palette-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(List::new(state).max_h(px(420.)))
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_command_palette(window, cx);
                })),
        )
}

/// T-4.3.5's hotlist overlay -- same `.occlude()`-backdrop/card shape as
/// `command_palette_overlay` (see that function's own doc comment for the
/// full reasoning), plus a `"HotlistOverlay"` key context and three extra
/// `.on_action` handlers the palette never needed: this overlay is
/// editable (`Delete`/`Ctrl+Up`/`Ctrl+Down`), not read-only.
fn hotlist_overlay(
    state: &Entity<ListState<HotlistDelegate>>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("hotlist-backdrop")
        .absolute()
        .size_full()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(96.))
        .bg(gpui::hsla(0., 0., 0., 0.5))
        .child(
            gpui::div()
                .id("hotlist-card")
                .key_context("HotlistOverlay")
                .occlude()
                .w(px(480.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(List::new(state).max_h(px(360.)))
                .on_action(cx.listener(|this, _: &HotlistRemoveEntry, window, cx| {
                    this.remove_selected_hotlist_entry(window, cx);
                }))
                .on_action(cx.listener(|this, _: &HotlistMoveUp, window, cx| {
                    this.move_selected_hotlist_entry(-1, window, cx);
                }))
                .on_action(cx.listener(|this, _: &HotlistMoveDown, window, cx| {
                    this.move_selected_hotlist_entry(1, window, cx);
                }))
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_hotlist(window, cx);
                })),
        )
}

/// T-5.2.1's copy/move dialog overlay -- same `.occlude()`-backdrop/card
/// shape as `hotlist_overlay`/`command_palette_overlay` (see
/// `command_palette_overlay`'s own doc comment for the full reasoning,
/// including the real regression this pattern exists to avoid). Unlike
/// those two, the card's body is `state.clone()` directly rather than a
/// `duet_widgets::list::List` -- `CopyMoveDialogState` is its own
/// `Render`-implementing view (see `crate::copy_move_dialog`'s module doc
/// comment), and an `Entity<V: Render>` is `IntoElement` on its own, the
/// same way `panel_view` already embeds `Entity<Panel>` directly. The
/// card sets no `key_context` of its own here -- `CopyMoveDialogState::
/// render` already sets `"CopyMoveDialog"` on its own root, which is an
/// equally valid ancestor for key-context resolution purposes.
fn copy_move_dialog_overlay(
    state: &Entity<CopyMoveDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("copy-move-dialog-backdrop")
        .absolute()
        .size_full()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(96.))
        .bg(gpui::hsla(0., 0., 0., 0.5))
        .child(
            gpui::div()
                .id("copy-move-dialog-card")
                .occlude()
                .w(px(480.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_copy_move_dialog(window, cx);
                })),
        )
}

/// T-5.2.2's operation manager overlay -- same `.occlude()`-backdrop/card
/// shape as `copy_move_dialog_overlay` (see `command_palette_overlay`'s
/// own doc comment for the full reasoning, including the real regression
/// this pattern exists to avoid). Wider than the copy/move dialog's own
/// card (`560px` vs `480px`): a job row's kind/state/progress line is
/// naturally wider than a destination path plus three option toggles.
/// The card sets no `key_context` of its own here, same reasoning as
/// `copy_move_dialog_overlay`'s: `OperationManagerState::render` already
/// sets `"OperationManager"` on its own root.
fn operation_manager_overlay(
    state: &Entity<OperationManagerState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("operation-manager-backdrop")
        .absolute()
        .size_full()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(96.))
        .bg(gpui::hsla(0., 0., 0., 0.5))
        .child(
            gpui::div()
                .id("operation-manager-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_operation_manager(window, cx);
                })),
        )
}

/// T-4.2.7's per-panel footer text: `FR-SEL-05`'s "n of m files selected,
/// x of y bytes" (T-4.2.3's stats, relocated here from a one-off slot in
/// the *global* status bar -- TC's own footer is per-panel, not
/// window-wide, and now that there's real per-panel chrome to put it in
/// this is where it belongs) plus, once the first `volume_stats` query
/// lands, the free-space figure the AC asks for. Reads `table`'s live
/// state directly rather than caching a copy -- `selection_stats()`/
/// `total_bytes_in_view()` are already `O(1)`/cached, so there's nothing
/// to gain by duplicating either here. Liveness needs no explicit
/// `cx.observe`: GPUI's own per-view accessed-entity tracking (confirmed
/// by reading `gpui-0.2.2/src/view.rs`) means `Workspace` reading
/// `table`'s entity during render is enough for `table`'s `cx.notify()`
/// (after a selection command, or once the volume-stats query completes)
/// to re-render this text.
fn panel_footer_text(table: &FileTable, cx: &App) -> SharedString {
    let state = table.state().read(cx);
    let model = state.delegate().model();
    let stats = model.selection_stats();

    let mut selected_bytes = String::new();
    write_byte_count(&mut selected_bytes, stats.total_bytes);
    let mut total_bytes = String::new();
    write_byte_count(&mut total_bytes, state.delegate().total_bytes_in_view());

    let mut text = format!(
        "{} of {} files selected, {selected_bytes} of {total_bytes} bytes",
        stats.count,
        model.order().len(),
    );

    if let Some(vol) = table.volume_stats() {
        let mut available = String::new();
        write_byte_count(&mut available, vol.available_bytes);
        let mut total = String::new();
        write_byte_count(&mut total, vol.total_bytes);
        text.push_str(&format!(" \u{2014} {available} free of {total}"));
    }

    // T-4.3.3 (FR-NAV-07/FR-NAV-13): the quick-search/quick-filter
    // indicator, appended here rather than as a separate overlay --
    // `panel_footer_text` already reads `table`'s live state on every
    // render with no explicit `cx.observe` needed (see this function's
    // own doc comment), and design.md itself leaves the indicator's
    // placement open ("anchored to the panel's footer or near the
    // cursor row").
    if let Some(indicator) = table.quick_search_indicator_text(cx) {
        text.push_str(&format!(" \u{2014} {indicator}"));
    }

    text.into()
}

/// The path/free-space chrome common to every panel (T-4.2.7) -- a header
/// (the current path) above the panel's real content and a footer
/// (selection stats + free space) below it, both switching color with
/// `active` the same way the panel's own body background already does
/// ([`panel_view`]), plus a colored bottom-border "underline" on the
/// header specifically: with only a
/// background-brightness difference between active/inactive, a panel
/// showing few or muted colors (some themes, most content) could still
/// leave "which one is active" genuinely ambiguous at a glance -- the
/// AC's actual bar. The underline is a second, independent signal that
/// doesn't depend on the theme's brightness contrast being strong enough
/// on its own.
fn panel_chrome(
    header_text: impl Into<SharedString>,
    footer_text: impl Into<SharedString>,
    active: bool,
    tokens: &TokenPalette,
    body: impl IntoElement,
) -> impl IntoElement {
    let underline = if active {
        tokens.color.border_focus
    } else {
        tokens.color.border_default
    };
    v_flex()
        .size_full()
        .child(
            gpui::div()
                .w_full()
                .px_2()
                .py_1()
                .text_size(px(11.))
                .text_color(tokens.color.header_fg)
                .bg(tokens.color.header_bg)
                .border_b_1()
                .border_color(underline)
                .truncate()
                .child(header_text.into()),
        )
        .child(gpui::div().flex_1().min_h(px(0.)).child(body))
        .child(
            gpui::div()
                .w_full()
                .px_2()
                .py_1()
                .text_size(px(11.))
                .text_color(tokens.color.statusbar_fg)
                .bg(tokens.color.statusbar_bg)
                .border_t_1()
                .border_color(tokens.color.border_default)
                .truncate()
                .child(footer_text.into()),
        )
}

/// Reads whichever `FileTable` is currently `panel`'s active tab and
/// derives the three things [`Workspace::dual_pane`] needs from it: the
/// header text (its path), the footer text ([`panel_footer_text`]), and
/// whether it holds real keyboard focus. A standalone function (not a
/// `Panel` method) because it needs `Window` for the focus check, which
/// `Panel`'s own read-only accessors deliberately don't take (nothing
/// inside `panel.rs` itself needs to know about focus).
fn panel_header_footer_active(
    panel: &Entity<Panel>,
    window: &Window,
    cx: &App,
) -> (String, SharedString, bool) {
    let panel = panel.read(cx);
    let table = panel.active_table().read(cx);
    let active = table.focus_handle(cx).is_focused(window);
    let header = table.current_dir().display().to_string();
    let footer = panel_footer_text(table, cx);
    (header, footer, active)
}

/// Wraps a real [`Panel`] (T-4.3.2 -- both sides, now that the right panel
/// is no longer a placeholder) in [`panel_chrome`]. One function for both
/// sides: nothing here is left/right-specific, only which `Entity<Panel>`
/// and pre-computed header/footer/active values the caller passes in.
fn panel_view(
    panel: &Entity<Panel>,
    header_text: impl Into<SharedString>,
    footer_text: impl Into<SharedString>,
    active: bool,
    tokens: &TokenPalette,
    border: gpui::Hsla,
) -> impl IntoElement {
    let (bg, _fg) = if active {
        (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
    } else {
        (
            tokens.color.panel_bg_inactive,
            tokens.color.panel_fg_inactive,
        )
    };
    gpui::div()
        .size_full()
        .bg(bg)
        .border_1()
        .border_color(if active {
            tokens.color.border_focus
        } else {
            border
        })
        .rounded_md()
        .child(panel_chrome(
            header_text,
            footer_text,
            active,
            tokens,
            panel.clone(),
        ))
}

/// Loads `session.json` at `path`, distinguishing "no file yet" (the
/// ordinary first-launch case, gets only a log line) from "a file exists
/// but failed to load" (corrupt JSON, a schema version this build
/// predates, permission denied, ... -- a real "we lost your session"
/// event, T-4.3.7's "degrades to defaults with a notice" AC). Returns
/// `(session, notice)`: `session` is `None` in both failure cases either
/// way (there's nothing to restore from), `notice` is `Some` only for the
/// second one, meant for `Workspace::pending_notice`. A pure wrapper
/// around `duet_config::session::load` (aside from the two `tracing`
/// calls) specifically so this branching is unit-testable without a real
/// `Window`/`Workspace`, same reasoning as [`resolve_panel_session`].
fn load_session_with_notice(path: &Path) -> (Option<duet_config::Session>, Option<String>) {
    match duet_config::session::load(path) {
        Ok(session) => (Some(session), None),
        Err(duet_config::ConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::info!(
                target: "duet_ui::workspace",
                "no existing session at {path:?}; starting fresh"
            );
            (None, None)
        }
        Err(err) => {
            tracing::warn!(
                target: "duet_ui::workspace",
                "session.json failed to load ({path:?}): {err}; starting fresh"
            );
            let notice = format!("Couldn't restore your last session ({err}) -- starting fresh.");
            (None, Some(notice))
        }
    }
}

/// Resolves one panel's initial tab list at startup: `session`'s saved
/// tabs (T-4.3.2), filtered down to the ones whose directory still exists
/// (a saved tab pointing at a since-deleted directory is silently dropped,
/// not surfaced as an error -- opening to a broken tab would be worse than
/// just not restoring it), or a single fresh tab at `fallback_dir` if
/// `session` is `None` (first launch, missing/corrupt `session.json`) or
/// every saved tab got filtered out. `session.active_tab` is clamped into
/// the *filtered* list, which can point at a different tab than originally
/// saved if entries before it were dropped -- an accepted imprecision for
/// a rare edge case (a tab's directory vanishing between runs), not worth
/// re-deriving which original tab the saved index meant.
fn resolve_panel_session(
    session: Option<&duet_config::SessionPanel>,
    fallback_dir: &Path,
) -> (Vec<SessionTab>, usize) {
    if let Some(session) = session {
        let tabs: Vec<SessionTab> = session
            .tabs
            .iter()
            .filter(|t| t.dir.is_dir())
            .cloned()
            .collect();
        if !tabs.is_empty() {
            let active = session.active_tab.min(tabs.len() - 1);
            return (tabs, active);
        }
    }
    (
        vec![SessionTab {
            dir: fallback_dir.to_path_buf(),
            locked: false,
            lock_dir_change: false,
            cursor_name: None,
            sort_column: duet_config::SessionSortColumn::Name,
            sort_ascending: true,
        }],
        0,
    )
}

/// Reads `panels.splitter_ratio` from `settings.toml` at `path`. Any
/// failure (missing file on first run, malformed TOML, ...) falls back to
/// `Settings::default()`'s documented `0.5` -- this is the *initial*,
/// synchronous, before-the-window-opens read `duet-config`'s own docs
/// carve out as safe for the UI thread (a settings-UI-triggered reload
/// after startup would instead go through `duet_config::watch`, not this
/// function).
fn load_splitter_ratio(path: &std::path::Path) -> f32 {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| settings.panels.splitter_ratio)
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using default splitter ratio (settings.toml not loaded yet: {err})"
            );
            duet_config::Settings::default().panels.splitter_ratio
        })
}

/// Reads `selection.mouse_mode` (FR-SEL-06) from `settings.toml` at
/// `path`, same "missing/malformed file falls back to
/// `Settings::default()`" tolerance as [`load_splitter_ratio`] --
/// there's nothing to persist here (unlike `splitter_ratio`, this never
/// changes at runtime yet), so this is the only place it's ever read.
fn load_mouse_mode(path: &std::path::Path) -> MouseMode {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| MouseMode::from_settings_str(&settings.selection.mouse_mode))
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using default mouse selection mode (settings.toml not loaded yet: {err})"
            );
            MouseMode::from_settings_str(&duet_config::Settings::default().selection.mouse_mode)
        })
}

/// Reads `navigation.quick_search_mode` (FR-NAV-07) from `settings.toml`
/// at `path`, same fallback tolerance as [`load_mouse_mode`].
fn load_quick_search_default_mode(path: &std::path::Path) -> QuickSearchMode {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| QuickSearchMode::from_settings_str(&settings.navigation.quick_search_mode))
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using default quick-search mode (settings.toml not loaded yet: {err})"
            );
            QuickSearchMode::from_settings_str(
                &duet_config::Settings::default()
                    .navigation
                    .quick_search_mode,
            )
        })
}

/// Reads `navigation.quick_search_idle_timeout_ms` (FR-NAV-13) from
/// `settings.toml` at `path`, same fallback tolerance as
/// [`load_mouse_mode`]. Clamped to `docs/config-schema.md`'s documented
/// `200..=5000` range so a hand-edited out-of-range value can't produce a
/// timer that fires instantly or never.
fn load_quick_search_idle_timeout(path: &std::path::Path) -> Duration {
    let ms = duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| settings.navigation.quick_search_idle_timeout_ms)
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using default quick-search idle timeout (settings.toml not loaded yet: {err})"
            );
            duet_config::Settings::default()
                .navigation
                .quick_search_idle_timeout_ms
        });
    Duration::from_millis(ms.clamp(200, 5000) as u64)
}

/// Reads `hotlist.toml`'s `entries` from `path` (T-4.3.5, FR-NAV-08).
/// Same fallback tolerance as [`load_mouse_mode`] -- a missing file (no
/// bookmarks saved yet, the ordinary case on first launch) or a malformed
/// one both degrade to an empty hotlist rather than failing startup.
fn load_hotlist_entries(path: &std::path::Path) -> Vec<HotlistEntry> {
    duet_config::hotlist::load(path)
        .and_then(|file| file.typed())
        .map(|hotlist| hotlist.entries)
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using an empty hotlist (hotlist.toml not loaded yet: {err})"
            );
            Vec::new()
        })
}

/// Writes `entries` to `hotlist.toml` at `path`, creating the file (with
/// `schema_version` at the documented current version) if this is the
/// first write. Round-trip preserving for every other key, per
/// `duet-config`'s `ConfigFile::set` contract -- same pattern
/// `save_splitter_ratio` already establishes for `settings.toml`.
fn save_hotlist_entries(
    path: &std::path::Path,
    entries: &[HotlistEntry],
) -> duet_config::Result<()> {
    let mut file = match duet_config::hotlist::load(path) {
        Ok(file) => file,
        Err(_) => duet_config::HotlistFile::from_str(
            path,
            "schema_version = 1\nentries = []\n",
            &duet_config::MigrationRegistry::generic_v0_to_v1(),
            duet_config::hotlist::HOTLIST_SCHEMA_VERSION,
        )?,
    };
    file.set(
        &["entries"],
        duet_config::Hotlist::entries_to_toml_array(entries),
    );
    file.save()
}

/// Writes `panels.splitter_ratio = ratio` to `settings.toml` at `path`,
/// creating the file (with every other field at its documented default)
/// if this is the first write. Round-trip preserving for every *other*
/// key, per `duet-config`'s `ConfigFile::set` contract.
fn save_splitter_ratio(path: &std::path::Path, ratio: f32) -> duet_config::Result<()> {
    let mut file = match duet_config::settings::load(path) {
        Ok(file) => file,
        Err(_) => duet_config::SettingsFile::from_str(
            path,
            "schema_version = 1\n",
            &duet_config::MigrationRegistry::settings(),
            duet_config::settings::SETTINGS_SCHEMA_VERSION,
        )?,
    };
    file.set(&["panels", "splitter_ratio"], ratio as f64);
    file.save()
}

/// Turns a finished T-5.2.1 copy/move job's outcome/report into one
/// human-readable toast, for `Workspace::new`'s `QueueManager` event-
/// consumer loop. `Cancelled` gets no toast at all -- the user asked for
/// it, it's not noteworthy. Every other `JobEvent` variant is ignored by
/// that loop entirely; a live progress UI (T-5.2.2) is a separate, later
/// task this one deliberately doesn't attempt.
fn summarize_job_finished(
    outcome: JobOutcome,
    report: &JobReport,
) -> Option<(NoticeLevel, String)> {
    match outcome {
        JobOutcome::Completed => {
            let mut bytes = String::new();
            write_byte_count(&mut bytes, report.bytes_completed);
            Some((
                NoticeLevel::Success,
                format!("Finished: {} file(s), {bytes}.", report.files_completed),
            ))
        }
        JobOutcome::CompletedWithSkips => Some((
            NoticeLevel::Warning,
            format!(
                "Finished with {} skipped: {} file(s) completed.",
                report.skipped.len(),
                report.files_completed
            ),
        )),
        JobOutcome::Failed => {
            let first = report
                .errors
                .first()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            let suffix = if report.errors.len() > 1 {
                format!(" ({} errors total)", report.errors.len())
            } else {
                String::new()
            };
            Some((NoticeLevel::Error, format!("Failed: {first}{suffix}")))
        }
        JobOutcome::Cancelled => None,
    }
}

/// Spawns the T-4.1.1 executor-wiring demo: a background task on the
/// core's Tokio runtime lists the current directory through the real VFS
/// (`duet_vfs::local::LocalFs`), then hands its result back to GPUI's
/// foreground executor via a `tokio::sync::oneshot` channel so the root
/// view can be updated and repainted -- proving a core async task can
/// drive a UI update through the foreground executor, not just that both
/// executors happen to exist side by side.
fn spawn_entry_count_demo(
    tokio_handle: tokio::runtime::Handle,
    workspace: Entity<Workspace>,
    cx: &mut App,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    // 1. The core's async task: runs on the Tokio runtime, does real I/O
    //    (never on the GPUI/UI thread -- design.md §8.2's "main thread
    //    does no I/O, ever").
    tokio_handle.spawn(async move {
        let result = count_current_dir_entries().await;
        let _ = tx.send(result);
    });

    // 2. The bridge: GPUI's foreground executor awaits the Tokio task's
    //    result and applies it to the view. `cx.spawn`'s future runs on
    //    GPUI's own executor, but the `oneshot::Receiver` wakes it the
    //    moment the Tokio-side `send` completes, regardless of which
    //    runtime polls it -- this is the concrete "core async task drives
    //    a UI update through the foreground executor" the AC asks for.
    cx.spawn(async move |cx| {
        let outcome = match rx.await {
            Ok(Ok((dir, count))) => DemoState::Ready {
                dir,
                entry_count: count,
            },
            Ok(Err(err)) => DemoState::Failed(err),
            Err(_) => {
                DemoState::Failed("background task was dropped before completing".to_string())
            }
        };

        let log_msg = match &outcome {
            DemoState::Ready { dir, entry_count } => {
                format!("{entry_count} entries in {dir}")
            }
            DemoState::Failed(err) => format!("error: {err}"),
            DemoState::Loading => "still loading".to_string(),
        };

        let updated = workspace.update(cx, |workspace, cx| {
            workspace.demo = outcome;
            cx.notify();
        });

        if updated.is_ok() {
            tracing::info!(
                target: "duet_ui::workspace",
                "executor-wiring demo completed and view updated: {log_msg}"
            );
        } else {
            tracing::warn!(
                target: "duet_ui::workspace",
                "executor-wiring demo finished ({log_msg}) after the workspace view was dropped"
            );
        }
    })
    .detach();
}

/// Lists the process's current directory through the real local VFS
/// backend and returns `(directory, entry_count)`. Runs entirely on the
/// caller's (Tokio) executor -- this is the "core" side of the
/// executor-wiring demo, deliberately using the same `FileSystem` trait
/// object path production code will use, not a shortcut.
async fn count_current_dir_entries() -> Result<(String, usize), String> {
    let cwd: PathBuf = std::env::current_dir().map_err(|e| format!("current_dir: {e}"))?;
    let dir_display = cwd.display().to_string();
    let path_str = cwd
        .to_str()
        .ok_or_else(|| "current directory is not valid UTF-8".to_string())?;
    let vpath = VPath::local(
        UnixPathBuf::new(path_str).map_err(|e| format!("invalid path {path_str:?}: {e}"))?,
    );

    let fs = LocalFs;
    let mut stream = fs.read_dir(&vpath, ListOpts::names_only());
    let mut count = 0usize;
    while let Some(chunk) = stream.next().await {
        let entries = chunk.map_err(|e| format!("read_dir: {e}"))?;
        count += entries.len();
    }
    Ok((dir_display, count))
}

#[cfg(test)]
mod tests {
    use duet_commands::CommandId;
    // T-5.2.2: `JobState` itself is only ever inspected by this test
    // module's own assertions (`workspace.rs`'s non-test code only ever
    // reads a job's `JobState` indirectly, through `crate::
    // operation_manager`) -- imported here, not at the top of the file,
    // so a plain `cargo build` (no `cfg(test)`) doesn't warn about an
    // otherwise-unused import.
    use duet_ops::JobState;
    use duet_widgets::layout::Root;
    // T-5.2.2's own overlay-internal actions -- declared in
    // `crate::operation_manager`, not `workspace.rs`, so (unlike
    // `OpenOperationManager`, defined right in this file and already
    // reachable via `use super::*;` below) they need naming explicitly.
    use crate::operation_manager::{
        OperationManagerCursorDown, OperationManagerCursorUp, OperationManagerPauseSelected,
        OperationManagerResumeSelected,
    };
    use duet_widgets::list::ListDelegate as _;
    use duet_widgets::table::TableDelegate as _;
    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    /// Serializes every test that touches `$XDG_CONFIG_HOME`/
    /// `$XDG_STATE_HOME` (env vars are process-global state, and `cargo
    /// test` runs tests concurrently across threads by default) --
    /// mirrors `duet-config`'s own `paths::tests::temp_env` helper, which
    /// this crate doesn't have direct access to (different crate).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Builds a real `Workspace` (both panels, the palette index, the
    /// works) inside a real test window, with `$XDG_CONFIG_HOME`/
    /// `$XDG_STATE_HOME` pointed at a fresh tempdir for the duration of
    /// `f` -- without this, `Workspace::new`'s `settings_path()`/
    /// `session_path()` calls would read whatever the *real* machine
    /// running the test happens to have at `~/.config/duet`/
    /// `~/.local/state/duet`, making the test's behavior depend on the
    /// environment it happens to run in. Same real-multi-thread-Tokio-
    /// runtime rationale as `panel.rs`'s `with_panel`: both panels'
    /// `FileTable`s spawn real background listing loads.
    fn with_workspace(
        cx: &mut TestAppContext,
        f: impl FnOnce(Entity<Workspace>, &mut VisualTestContext),
    ) {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config_dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let prev_config = std::env::var_os("XDG_CONFIG_HOME");
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: serialized by ENV_LOCK above; no other thread in this
        // test binary reads these specific vars concurrently.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
            std::env::set_var("XDG_STATE_HOME", state_dir.path());
        }

        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("failed to start a test Tokio runtime");
        let tokio_handle = tokio_rt.handle().clone();

        cx.update(|cx| {
            duet_widgets::init(cx);
            duet_widgets::theme::TokenPalette::built_in(duet_widgets::theme::ThemeMode::Dark)
                .install(cx);
            bind_workspace_keys(cx);
            crate::file_table::bind_file_table_keys(cx);
            bind_panel_keys(cx);
            bind_copy_move_dialog_keys(cx);
            bind_operation_manager_keys(cx);
        });

        let mut workspace_cell: Option<Entity<Workspace>> = None;
        let (_root, vcx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, tokio_handle.clone()));
            workspace_cell = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = workspace_cell.expect("the window-build closure always constructs one");

        f(workspace, vcx);

        // SAFETY: still serialized by ENV_LOCK.
        unsafe {
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_state {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }

    #[gpui::test]
    fn open_command_palette_captures_the_focused_panel_and_focuses_the_palette(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(ws.palette_target_panel, PanelSide::Left);
                assert!(ws.command_palette.is_some());
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            vcx.update(|window, _cx| {
                assert!(
                    !left_handle.is_focused(window),
                    "focus must move onto the palette's own query input, not stay on the panel"
                );
            });
        });
    }

    #[gpui::test]
    fn close_command_palette_restores_previous_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));
            workspace.update_in(vcx, |ws, window, cx| ws.close_command_palette(window, cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.command_palette.is_none()));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "closing the palette must restore focus to whatever had it before"
                );
            });
        });
    }

    /// UAT regression: a real mouse click on the backdrop (well outside
    /// the centered card -- the palette's own top padding is 96px, so a
    /// click near the window's top-left corner is always on the backdrop
    /// regardless of window size) must close the palette, not fall
    /// through and activate whatever panel is underneath. Drives this
    /// through a real simulated click (`VisualTestContext::simulate_click`),
    /// not a direct method call -- this is specifically what the
    /// `.occlude()`/`on_mouse_down_out` wiring exists to guarantee.
    #[gpui::test]
    fn clicking_the_backdrop_closes_the_palette_instead_of_reaching_the_panel_underneath(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.simulate_click(gpui::point(px(5.), px(5.)), gpui::Modifiers::default());
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.command_palette.is_none(),
                    "a click on the backdrop must close the palette"
                );
            });
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "the click must not have reached the panel underneath -- closing the \
                     palette should simply restore focus to what had it before, not leave \
                     the panel independently focused via a click that fell through"
                );
            });
        });
    }

    #[gpui::test]
    fn dispatch_palette_command_for_a_wired_id_runs_the_real_panel_method(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let tabs_before =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).snapshot(cx).tabs.len());

            workspace.update_in(vcx, |ws, window, cx| {
                ws.palette_target_panel = PanelSide::Left;
                ws.dispatch_palette_command(
                    &CommandId::new("tab.new").unwrap(),
                    "Open a new tab",
                    window,
                    cx,
                );
            });

            workspace.read_with(vcx, |ws, cx| {
                assert_eq!(
                    ws.left_panel.read(cx).snapshot(cx).tabs.len(),
                    tabs_before + 1,
                    "tab.new must actually open a new tab on the target panel"
                );
                assert!(
                    ws.command_palette.is_none(),
                    "dispatching a command must close the palette"
                );
            });
        });
    }

    /// Regression test for a UAT-reported bug: `hotlist.open`/`hotlist.add`
    /// were missing from `dispatch_palette_command`'s match entirely (so
    /// selecting either from the palette just showed the "isn't wired up
    /// yet" notice), and once wired naively (calling `open_hotlist`/
    /// `add_current_dir_to_hotlist`, which both re-derive the target panel
    /// from *live window focus*) they'd silently target the wrong panel:
    /// at the moment `dispatch_palette_command` runs, focus is still on
    /// the palette's own list (the palette only closes *after* dispatch
    /// returns), so `focused_panel_side` sees neither panel focused and
    /// falls back to `PanelSide::Left` regardless of which panel the user
    /// actually meant. This drives the real `CommandPaletteDelegate::
    /// confirm` path (not `dispatch_palette_command` called directly, so
    /// focus is genuinely on the palette when it runs) with the *right*
    /// panel focused beforehand, and confirms `hotlist.open` still targets
    /// the right panel.
    #[gpui::test]
    fn dispatch_palette_command_for_hotlist_open_targets_the_captured_palette_panel(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let right_handle = workspace.read_with(vcx, |ws, cx| {
                ws.right_panel.read(cx).active_focus_handle(cx)
            });
            vcx.update(|window, _cx| window.focus(&right_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));
            let state = workspace
                .read_with(vcx, |ws, _| ws.command_palette.clone())
                .expect("just opened");

            state.update_in(vcx, |state, window, cx| {
                state
                    .delegate_mut()
                    .perform_search("hotlist.open", window, cx)
                    .detach();
            });
            vcx.run_until_parked();
            state.update_in(vcx, |state, window, cx| {
                state.delegate_mut().set_selected_index(
                    Some(duet_widgets::list::IndexPath::new(0)),
                    window,
                    cx,
                );
                state.delegate_mut().confirm(false, window, cx);
            });

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(
                    ws.hotlist_target_panel,
                    PanelSide::Right,
                    "hotlist.open dispatched from the palette must target the panel that \
                     was focused when the palette *opened*, not whatever has focus at \
                     dispatch time (the palette itself)"
                );
                assert!(ws.hotlist.is_some());
                assert!(ws.command_palette.is_none());
            });
        });
    }

    /// Same regression as the test above, for `hotlist.add`: bookmarks
    /// whichever panel was focused *before* the palette opened, not
    /// wherever `focused_panel_side` lands when called from inside
    /// dispatch (with focus still on the palette).
    #[gpui::test]
    fn dispatch_palette_command_for_hotlist_add_bookmarks_the_captured_palette_panel(
        cx: &mut TestAppContext,
    ) {
        let right_dir = tempfile::tempdir().unwrap();
        with_workspace(cx, |workspace, vcx| {
            let left_dir = workspace.read_with(vcx, |ws, cx| {
                ws.left_panel
                    .read(cx)
                    .active_table()
                    .read(cx)
                    .current_dir()
                    .to_path_buf()
            });
            let right_table =
                workspace.read_with(vcx, |ws, cx| ws.right_panel.read(cx).active_table().clone());
            right_table.update_in(vcx, |table, window, cx| {
                table.navigate_to_path(right_dir.path().to_path_buf(), window, cx);
            });
            vcx.run_until_parked();

            let right_handle = workspace.read_with(vcx, |ws, cx| {
                ws.right_panel.read(cx).active_focus_handle(cx)
            });
            vcx.update(|window, _cx| window.focus(&right_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));
            let state = workspace
                .read_with(vcx, |ws, _| ws.command_palette.clone())
                .expect("just opened");

            state.update_in(vcx, |state, window, cx| {
                state
                    .delegate_mut()
                    .perform_search("hotlist.add", window, cx)
                    .detach();
            });
            vcx.run_until_parked();
            state.update_in(vcx, |state, window, cx| {
                state.delegate_mut().set_selected_index(
                    Some(duet_widgets::list::IndexPath::new(0)),
                    window,
                    cx,
                );
                state.delegate_mut().confirm(false, window, cx);
            });

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(
                    ws.hotlist_entries.len(),
                    1,
                    "hotlist.add dispatched from the palette must actually bookmark \
                     something, not just show the \"isn't wired up yet\" notice"
                );
                assert_eq!(
                    ws.hotlist_entries[0].path,
                    right_dir.path().to_string_lossy(),
                    "must bookmark the panel that was focused when the palette opened \
                     ({:?}), not the left panel's directory ({left_dir:?})",
                    right_dir.path()
                );
            });
        });
    }

    #[gpui::test]
    fn dispatch_palette_command_for_an_unwired_id_shows_a_notice_and_still_closes(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| ws.open_command_palette(window, cx));

            let notifications_before = vcx.update(|window, cx| window.notifications(cx).len());

            workspace.update_in(vcx, |ws, window, cx| {
                // `ops.copy` is a real, registered catalogue command with
                // no real implementation to dispatch to yet.
                ws.dispatch_palette_command(
                    &CommandId::new("ops.copy").unwrap(),
                    "Copy selection to the target panel",
                    window,
                    cx,
                );
            });

            let notifications_after = vcx.update(|window, cx| window.notifications(cx).len());
            assert!(
                notifications_after > notifications_before,
                "an unwired command must surface a notice, not silently no-op"
            );
            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.command_palette.is_none(),
                    "the palette still closes even for an unwired command"
                );
            });
        });
    }

    #[gpui::test]
    fn palette_index_covers_the_full_catalogue(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let len = workspace.read_with(vcx, |ws, _| ws.palette_index.len());
            assert!(
                len >= 200,
                "T-4.3.6's AC names \"200+ commands\" explicitly; got {len}"
            );
        });
    }

    /// Exercises the palette's `ListDelegate` impl directly (bypassing
    /// real keystroke simulation, the same "drive the same logic the
    /// trait methods call" approach `file_table.rs`'s own delegate tests
    /// already use) -- confirms `perform_search`, `confirm`, and `cancel`
    /// all reach `Workspace` correctly through the whole real wiring, not
    /// just `dispatch_palette_command` called directly as the tests
    /// above do.
    #[gpui::test]
    fn confirming_a_selected_row_through_the_list_delegate_dispatches_it(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let tabs_before =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).snapshot(cx).tabs.len());

            workspace.update_in(vcx, |ws, window, cx| {
                ws.palette_target_panel = PanelSide::Left;
                ws.open_command_palette(window, cx);
            });
            let state = workspace
                .read_with(vcx, |ws, _| ws.command_palette.clone())
                .expect("just opened");

            state.update_in(vcx, |state, window, cx| {
                state
                    .delegate_mut()
                    .perform_search("tab.new", window, cx)
                    .detach();
            });
            vcx.run_until_parked();
            state.update_in(vcx, |state, window, cx| {
                state.delegate_mut().set_selected_index(
                    Some(duet_widgets::list::IndexPath::new(0)),
                    window,
                    cx,
                );
                state.delegate_mut().confirm(false, window, cx);
            });

            workspace.read_with(vcx, |ws, cx| {
                assert_eq!(
                    ws.left_panel.read(cx).snapshot(cx).tabs.len(),
                    tabs_before + 1,
                    "confirming the top \"tab.new\" search result must dispatch it for real"
                );
                assert!(ws.command_palette.is_none());
            });
        });
    }

    // -- T-4.3.5 directory hotlist --------------------------------------

    #[gpui::test]
    fn add_current_dir_to_hotlist_bookmarks_the_focused_panels_directory(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            let dir = workspace.read_with(vcx, |ws, cx| {
                ws.left_panel
                    .read(cx)
                    .active_table()
                    .read(cx)
                    .current_dir()
                    .to_string_lossy()
                    .into_owned()
            });

            workspace.update_in(vcx, |ws, window, cx| {
                ws.add_current_dir_to_hotlist(window, cx);
            });

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(ws.hotlist_entries.len(), 1);
                assert_eq!(ws.hotlist_entries[0].path, dir);
                assert_eq!(ws.hotlist_entries[0].label, None);
            });
        });
    }

    #[gpui::test]
    fn add_current_dir_to_hotlist_is_a_noop_for_an_already_bookmarked_directory(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.add_current_dir_to_hotlist(window, cx);
                ws.add_current_dir_to_hotlist(window, cx);
            });
            workspace.read_with(vcx, |ws, _| {
                assert_eq!(
                    ws.hotlist_entries.len(),
                    1,
                    "adding the same directory twice must not create a duplicate entry"
                );
            });
        });
    }

    #[gpui::test]
    fn open_hotlist_captures_the_focused_panel_and_focuses_the_overlay(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_hotlist(window, cx));

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(ws.hotlist_target_panel, PanelSide::Left);
                assert!(ws.hotlist.is_some());
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            vcx.update(|window, _cx| {
                assert!(
                    !left_handle.is_focused(window),
                    "focus must move onto the hotlist overlay, not stay on the panel"
                );
            });
        });
    }

    #[gpui::test]
    fn close_hotlist_restores_previous_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.update_in(vcx, |ws, window, cx| ws.open_hotlist(window, cx));
            workspace.update_in(vcx, |ws, window, cx| ws.close_hotlist(window, cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.hotlist.is_none()));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "closing the hotlist must restore focus to whatever had it before"
                );
            });
        });
    }

    /// Exercises `HotlistDelegate::confirm` directly (same "drive the
    /// same logic the trait methods call" approach the palette's own
    /// `confirming_a_selected_row...` test uses) -- confirms Enter on a
    /// bookmarked entry actually navigates the captured target panel, not
    /// just that `navigate_to_hotlist_entry` works when called directly.
    #[gpui::test]
    fn confirming_a_hotlist_entry_navigates_the_target_panel_and_closes(cx: &mut TestAppContext) {
        let target = tempfile::tempdir().unwrap();
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.hotlist_entries.push(HotlistEntry {
                    path: target.path().to_string_lossy().into_owned(),
                    label: None,
                });
                ws.hotlist_target_panel = PanelSide::Left;
                ws.open_hotlist(window, cx);
            });
            let state = workspace
                .read_with(vcx, |ws, _| ws.hotlist.clone())
                .expect("just opened");

            state.update_in(vcx, |state, window, cx| {
                state.delegate_mut().set_selected_index(
                    Some(duet_widgets::list::IndexPath::new(0)),
                    window,
                    cx,
                );
                state.delegate_mut().confirm(false, window, cx);
            });
            vcx.run_until_parked();

            workspace.read_with(vcx, |ws, cx| {
                assert_eq!(
                    ws.left_panel.read(cx).active_table().read(cx).current_dir(),
                    target.path(),
                    "confirming the entry must navigate the target panel there"
                );
                assert!(
                    ws.hotlist.is_none(),
                    "confirming must also close the overlay"
                );
            });
        });
    }

    #[gpui::test]
    fn remove_selected_hotlist_entry_removes_it_and_persists(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.hotlist_entries = vec![
                    HotlistEntry {
                        path: "/a".into(),
                        label: None,
                    },
                    HotlistEntry {
                        path: "/b".into(),
                        label: None,
                    },
                ];
                ws.open_hotlist(window, cx);
            });

            workspace.update_in(vcx, |ws, window, cx| {
                ws.remove_selected_hotlist_entry(window, cx);
            });
            vcx.run_until_parked();

            let (entries, path) = workspace.read_with(vcx, |ws, _| {
                (ws.hotlist_entries.clone(), ws.hotlist_path.clone())
            });
            assert_eq!(
                entries,
                vec![HotlistEntry {
                    path: "/b".into(),
                    label: None
                }]
            );

            let on_disk = duet_config::hotlist::load(&path.unwrap())
                .unwrap()
                .typed()
                .unwrap()
                .entries;
            assert_eq!(
                on_disk, entries,
                "the removal must be persisted to hotlist.toml, not just in memory"
            );
        });
    }

    #[gpui::test]
    fn move_selected_hotlist_entry_swaps_with_its_neighbor_and_persists(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.hotlist_entries = vec![
                    HotlistEntry {
                        path: "/a".into(),
                        label: None,
                    },
                    HotlistEntry {
                        path: "/b".into(),
                        label: None,
                    },
                    HotlistEntry {
                        path: "/c".into(),
                        label: None,
                    },
                ];
                ws.open_hotlist(window, cx);
            });
            // The overlay's `HotlistDelegate` starts selected on index 0
            // ("/a") -- move it down once.
            workspace.update_in(vcx, |ws, window, cx| {
                ws.move_selected_hotlist_entry(1, window, cx);
            });
            vcx.run_until_parked();

            let (entries, path) = workspace.read_with(vcx, |ws, _| {
                (ws.hotlist_entries.clone(), ws.hotlist_path.clone())
            });
            assert_eq!(
                entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
                vec!["/b", "/a", "/c"]
            );

            let on_disk = duet_config::hotlist::load(&path.unwrap())
                .unwrap()
                .typed()
                .unwrap()
                .entries;
            assert_eq!(on_disk, entries, "reorder must persist to hotlist.toml too");
        });
    }

    #[gpui::test]
    fn move_selected_hotlist_entry_up_from_the_top_is_a_noop(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.hotlist_entries = vec![
                    HotlistEntry {
                        path: "/a".into(),
                        label: None,
                    },
                    HotlistEntry {
                        path: "/b".into(),
                        label: None,
                    },
                ];
                ws.open_hotlist(window, cx);
            });
            // Selected starts at index 0 -- moving *up* must be a no-op.
            workspace.update_in(vcx, |ws, window, cx| {
                ws.move_selected_hotlist_entry(-1, window, cx);
            });
            vcx.run_until_parked();

            workspace.read_with(vcx, |ws, _| {
                assert_eq!(
                    ws.hotlist_entries
                        .iter()
                        .map(|e| e.path.as_str())
                        .collect::<Vec<_>>(),
                    vec!["/a", "/b"]
                );
            });
        });
    }

    /// FR-NAV-08's "entries persist" AC, exercised end to end against a
    /// real file: `add_current_dir_to_hotlist` writes to `hotlist.toml`
    /// off the UI thread, and a fresh read of that same file sees exactly
    /// what was added -- not just an in-memory assertion.
    #[gpui::test]
    fn hotlist_entries_persist_to_a_real_hotlist_toml_file(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            workspace.update_in(vcx, |ws, window, cx| {
                ws.add_current_dir_to_hotlist(window, cx);
            });
            vcx.run_until_parked();

            let (entries, path) = workspace.read_with(vcx, |ws, _| {
                (ws.hotlist_entries.clone(), ws.hotlist_path.clone())
            });
            assert_eq!(entries.len(), 1);

            let on_disk = duet_config::hotlist::load(&path.unwrap())
                .unwrap()
                .typed()
                .unwrap()
                .entries;
            assert_eq!(on_disk, entries);
        });
    }

    /// FR-NAV-01's "ratio persists per session", exercised end to end
    /// against real files: no `settings.toml` exists yet (fresh install,
    /// matching a real first launch), a ratio is saved, and a fresh load
    /// sees exactly that value -- not the manual, log-based verification
    /// this task's report otherwise relies on for the interactive
    /// (drag/keyboard) half of resizing, since this sandbox has no input
    /// -injection tool to drive that live.
    #[test]
    fn splitter_ratio_round_trips_through_settings_toml_from_a_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert!(!path.exists(), "starting from a fresh install, no file yet");

        assert_eq!(load_splitter_ratio(&path), 0.5, "documented default");

        save_splitter_ratio(&path, 0.27).expect("first save must create the file");
        assert_eq!(load_splitter_ratio(&path), 0.27);

        // A second save (the "user dragged again" case) must not lose the
        // first write or any other section's defaults.
        save_splitter_ratio(&path, 0.63).expect("second save must succeed");
        assert_eq!(load_splitter_ratio(&path), 0.63);

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("splitter_ratio"), "{on_disk}");
    }

    /// Saving a ratio must not disturb the rest of an existing, hand
    /// -edited `settings.toml` -- the same round-trip-preservation
    /// contract `duet-config::document::ConfigFile` guarantees generally,
    /// exercised here through this module's actual save path.
    #[test]
    fn saving_ratio_preserves_other_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n\n[panels]\nshow_hidden = true\nsplitter_ratio = 0.5\n",
        )
        .unwrap();

        save_splitter_ratio(&path, 0.8).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("show_hidden = true"), "{on_disk}");
        assert!(on_disk.contains("splitter_ratio = 0.8"), "{on_disk}");
    }

    fn session_tab(dir: PathBuf, locked: bool) -> SessionTab {
        SessionTab {
            dir,
            locked,
            lock_dir_change: false,
            cursor_name: None,
            sort_column: duet_config::SessionSortColumn::Name,
            sort_ascending: true,
        }
    }

    /// T-4.3.7: a missing `session.json` (first launch) is silent -- no
    /// notice, since there's nothing wrong to report.
    #[test]
    fn load_session_with_notice_is_silent_when_the_file_is_simply_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let (session, notice) = load_session_with_notice(&path);
        assert!(session.is_none());
        assert!(notice.is_none(), "a fresh install must not nag the user");
    }

    /// T-4.3.7's AC: "a corrupt session file degrades to defaults with a
    /// notice" -- unlike the missing-file case, a file that exists but
    /// fails to parse must produce a user-facing notice.
    #[test]
    fn load_session_with_notice_surfaces_a_notice_for_a_corrupt_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "not valid json { [ }").unwrap();
        let (session, notice) = load_session_with_notice(&path);
        assert!(session.is_none());
        assert!(
            notice.is_some(),
            "an existing-but-corrupt file must be reported, not silently swallowed"
        );
    }

    /// A well-formed `session.json` loads with no notice at all.
    #[test]
    fn load_session_with_notice_is_silent_on_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let real = tempfile::tempdir().unwrap();
        let written = duet_config::Session {
            schema_version: duet_config::session::SESSION_SCHEMA_VERSION,
            left: duet_config::SessionPanel {
                tabs: vec![session_tab(real.path().to_path_buf(), false)],
                active_tab: 0,
            },
            right: duet_config::SessionPanel {
                tabs: vec![session_tab(real.path().to_path_buf(), false)],
                active_tab: 0,
            },
        };
        duet_config::session::save(&path, &written).unwrap();

        let (session, notice) = load_session_with_notice(&path);
        assert!(notice.is_none());
        assert_eq!(session, Some(written));
    }

    /// T-4.3.2: `resolve_panel_session` is the pure (GPUI-free) half of
    /// startup session-loading -- everything about it that doesn't need a
    /// real `Panel`/`FileTable`/window to exercise, unlike `Panel`'s own
    /// tab-command tests (`panel.rs`, which need `gpui::TestAppContext`).
    #[test]
    fn resolve_panel_session_falls_back_to_a_single_tab_at_fallback_dir_when_session_is_none() {
        let fallback = PathBuf::from("/tmp");
        let (tabs, active) = resolve_panel_session(None, &fallback);
        assert_eq!(tabs, vec![session_tab(fallback, false)]);
        assert_eq!(active, 0);
    }

    #[test]
    fn resolve_panel_session_filters_out_tabs_whose_directory_no_longer_exists() {
        let real = tempfile::tempdir().unwrap();
        let gone = real.path().join("this-directory-was-deleted");
        let session = duet_config::SessionPanel {
            tabs: vec![
                session_tab(gone, false),
                session_tab(real.path().to_path_buf(), true),
            ],
            active_tab: 1,
        };
        let (tabs, active) = resolve_panel_session(Some(&session), Path::new("/tmp"));
        assert_eq!(tabs.len(), 1, "the deleted-directory tab must be dropped");
        assert_eq!(tabs[0].dir, real.path());
        assert!(tabs[0].locked, "surviving tabs keep their lock flags");
        assert_eq!(active, 0, "re-clamped into the filtered list");
    }

    #[test]
    fn resolve_panel_session_falls_back_when_every_saved_tab_dir_is_gone() {
        let real = tempfile::tempdir().unwrap();
        let gone = real.path().join("nope");
        let session = duet_config::SessionPanel {
            tabs: vec![session_tab(gone, false)],
            active_tab: 0,
        };
        let fallback = PathBuf::from("/tmp");
        let (tabs, active) = resolve_panel_session(Some(&session), &fallback);
        assert_eq!(tabs, vec![session_tab(fallback, false)]);
        assert_eq!(active, 0);
    }

    #[test]
    fn resolve_panel_session_clamps_active_tab_into_range() {
        let real = tempfile::tempdir().unwrap();
        let session = duet_config::SessionPanel {
            tabs: vec![session_tab(real.path().to_path_buf(), false)],
            active_tab: 99,
        };
        let (tabs, active) = resolve_panel_session(Some(&session), Path::new("/tmp"));
        assert_eq!(tabs.len(), 1);
        assert_eq!(active, 0);
    }

    // -- T-5.2.1 copy/move dialog -----------------------------------------

    /// Waits for `condition` to become true, alternating `run_until_parked`
    /// with a short real sleep -- identical reasoning and shape to
    /// `panel.rs`'s own private `wait_until` (that module's copy isn't
    /// reachable from here): a single `run_until_parked()` isn't enough
    /// for anything that depends on real background Tokio work (directory
    /// listings, `plan_copy`/`QueueManager::enqueue`/`execute()`), all of
    /// which cross onto the real, if minimal, Tokio runtime `with_workspace`
    /// builds. Panics with a clear message rather than hanging a test run
    /// indefinitely.
    fn wait_until(
        vcx: &mut VisualTestContext,
        mut condition: impl FnMut(&mut VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            vcx.run_until_parked();
            // T-5.2.1 (post-UAT): `Workspace::render`-only state (e.g.
            // `pending_panel_refresh`) only actually gets drained by a
            // real draw pass, not merely by `run_until_parked` letting
            // background tasks progress -- without this, a condition
            // that depends on `Render::render` having run at least once
            // since the state it's waiting on was set would hang until
            // the deadline below, not because the app is broken but
            // because nothing here ever asked it to draw.
            let _ = vcx.update(|window, cx| window.draw(cx));
            if condition(vcx) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("wait_until: condition did not become true within 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Navigates `table` to `dir` and waits for the resulting background
    /// directory listing (`spawn_directory_load`) to finish -- `loading()`
    /// flips back to `false` once it has, real content or not (an empty
    /// destination directory is a real, expected case for the E2E test
    /// below, so this can't just wait for a non-empty `order()`).
    fn navigate_panel_to(vcx: &mut VisualTestContext, table: &Entity<FileTable>, dir: PathBuf) {
        table.update_in(vcx, |table, window, cx| {
            table.navigate_to_path(dir, window, cx);
        });
        wait_until(vcx, |vcx| {
            table.read_with(vcx, |table, cx| {
                !table.state().read(cx).delegate().loading(cx)
            })
        });
    }

    #[gpui::test]
    fn f5_opens_the_copy_dialog_defaulting_destination_to_the_other_panels_directory(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("hello.txt"), b"hi").unwrap();

            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            let right_table =
                workspace.read_with(vcx, |ws, cx| ws.right_panel.read(cx).active_table().clone());
            navigate_panel_to(vcx, &left_table, source_dir.path().to_path_buf());
            navigate_panel_to(vcx, &right_table, dest_dir.path().to_path_buf());

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(CopyDialog);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, cx| {
                let state = ws
                    .copy_move_dialog
                    .clone()
                    .expect("F5 must open the copy/move dialog");
                state.read_with(cx, |state, cx| {
                    assert_eq!(state.kind(), JobKind::Copy);
                    assert_eq!(
                        state.destination_value(cx),
                        dest_dir.path().to_string_lossy(),
                        "destination must default to the *other* (right) panel's directory"
                    );
                    assert_eq!(
                        state.sources(),
                        &[
                            crate::file_table::local_vpath(&source_dir.path().join("hello.txt"))
                                .unwrap()
                        ],
                        "with nothing explicitly selected, the sole cursor-row entry is used"
                    );
                });
            });
        });
    }

    #[gpui::test]
    fn f6_opens_the_move_dialog(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("a.txt"), b"a").unwrap();
            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            navigate_panel_to(vcx, &left_table, source_dir.path().to_path_buf());

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(MoveDialog);

            workspace.read_with(vcx, |ws, cx| {
                let state = ws
                    .copy_move_dialog
                    .clone()
                    .expect("F6 must open the copy/move dialog");
                state.read_with(cx, |state, _cx| assert_eq!(state.kind(), JobKind::Move));
            });
        });
    }

    #[gpui::test]
    fn f5_opens_the_dialog_with_nothing_to_operate_on_shows_a_notice_instead(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            // An empty source directory: nothing selected, no cursor row
            // to fall back to either.
            let empty_dir = tempfile::tempdir().unwrap();
            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            navigate_panel_to(vcx, &left_table, empty_dir.path().to_path_buf());

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(CopyDialog);

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.copy_move_dialog.is_none(),
                    "an empty directory with nothing selected must not open the dialog"
                );
            });
        });
    }

    /// The most valuable test in this module: real tempdirs, the real
    /// `LocalFs`, F5 to open the dialog, a real Enter keystroke inside the
    /// destination field to confirm, and -- via the real off-thread
    /// `plan_copy` -> `QueueManager::enqueue` -> `execute()` path, no
    /// shortcuts -- the file actually landing on real disk at the
    /// destination. This is the first proof the T-5.2.1 wiring works
    /// end to end, not just that individual pieces return plausible
    /// values in isolation.
    #[gpui::test]
    fn f5_copy_end_to_end_copies_a_real_file_to_the_other_panels_directory(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("hello.txt"), b"hello world").unwrap();

            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            let right_table =
                workspace.read_with(vcx, |ws, cx| ws.right_panel.read(cx).active_table().clone());
            navigate_panel_to(vcx, &left_table, source_dir.path().to_path_buf());
            navigate_panel_to(vcx, &right_table, dest_dir.path().to_path_buf());

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(CopyDialog);
            workspace.read_with(vcx, |ws, _| assert!(ws.copy_move_dialog.is_some()));

            // The destination field already has real keyboard focus (see
            // `CopyMoveDialogState::new`). Dispatches the resolved
            // `duet_widgets::input::Enter` action directly rather than
            // `vcx.simulate_keystrokes("enter")` -- the latter drives
            // GPUI's synthetic IME/text-input pipeline, which hits an
            // unrelated upstream panic (`gpui`'s own `shape_line`
            // debug_assert) the moment a real, focused, non-empty
            // `InputState` receives a simulated keystroke; confirmed by
            // isolating it against `dispatch_action(CopyDialog)` (fine)
            // and `dispatch_action(Enter{..})` (also fine) -- only
            // `simulate_keystrokes` trips it. Dispatching the action
            // directly still exercises the real thing this test cares
            // about: `InputState::enter`'s own `cx.emit(PressEnter)` ->
            // this dialog's `cx.subscribe_in` -> `confirm`, with no
            // shortcut through any of this module's own private methods.
            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });

            let dest_file = dest_dir.path().join("hello.txt");
            // Waits for both the file landing on disk *and* the dialog
            // closing in the same poll -- these are two independently
            // eventually-consistent outcomes of the same background job
            // (the file can land before the dialog's own close callback
            // gets scheduled, or vice versa), so asserting the dialog is
            // closed as a separate, immediate check right after only the
            // first one is racy.
            wait_until(vcx, |vcx| {
                dest_file.is_file()
                    && workspace.read_with(vcx, |ws, _| ws.copy_move_dialog.is_none())
            });

            assert_eq!(std::fs::read(&dest_file).unwrap(), b"hello world");
        });
    }

    fn model_has_name(vcx: &mut VisualTestContext, table: &Entity<FileTable>, name: &str) -> bool {
        table.read_with(vcx, |table, cx| {
            table
                .state()
                .read(cx)
                .delegate()
                .model()
                .ordered_names()
                .any(|(_, n)| n == name)
        })
    }

    /// Post-UAT (T-5.2.1): a real F6 move must leave *both* panels
    /// showing the true, current state of disk without the user manually
    /// re-navigating -- the source no longer listing the moved file, the
    /// destination now listing it. Deliberately never calls
    /// `navigate_panel_to` a second time after the move; the whole point
    /// is that `Workspace`'s own `pending_panel_refresh` drain
    /// (`Render::render`) does this automatically once the job's
    /// `Finished` event lands, not that a manual re-navigation would
    /// paper over a real gap.
    #[gpui::test]
    fn f6_move_end_to_end_refreshes_both_panels_without_manual_renavigation(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("move_me.txt"), b"move me").unwrap();

            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            let right_table =
                workspace.read_with(vcx, |ws, cx| ws.right_panel.read(cx).active_table().clone());
            navigate_panel_to(vcx, &left_table, source_dir.path().to_path_buf());
            navigate_panel_to(vcx, &right_table, dest_dir.path().to_path_buf());
            assert!(model_has_name(vcx, &left_table, "move_me.txt"));
            assert!(!model_has_name(vcx, &right_table, "move_me.txt"));

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| window.focus(&left_handle));
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(MoveDialog);
            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });

            let dest_file = dest_dir.path().join("move_me.txt");
            wait_until(vcx, |_vcx| dest_file.is_file());

            wait_until(vcx, |vcx| {
                !model_has_name(vcx, &left_table, "move_me.txt")
                    && model_has_name(vcx, &right_table, "move_me.txt")
            });
        });
    }

    // -- T-5.2.2 operation manager -------------------------------------------

    /// A `FileSystem` test double wrapping a real `LocalFs`, delaying
    /// every `open_read` by a fixed duration -- mirrors `duet-ops`'s own
    /// `SlowFs` (`crates/duet-ops/src/queue.rs`'s test module), private to
    /// that crate and so not reachable from here. This module's own E2E
    /// test below needs a job that is still genuinely `Running` by the
    /// time it opens the manager and drives Pause/Resume through it -- a
    /// real (fast, tmpfs-backed) copy of a couple of small files finishes
    /// too quickly for that window to be reliably observable otherwise.
    struct SlowLocalFs {
        inner: LocalFs,
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl FileSystem for SlowLocalFs {
        fn scheme(&self) -> &'static str {
            self.inner.scheme()
        }
        fn caps(&self) -> duet_types::Caps {
            self.inner.caps()
        }
        fn read_dir(
            &self,
            p: &VPath,
            opts: ListOpts,
        ) -> futures_util::stream::BoxStream<'_, duet_types::Result<Vec<duet_vfs::DirEntry>>>
        {
            self.inner.read_dir(p, opts)
        }
        async fn stat(&self, p: &VPath, follow: bool) -> duet_types::Result<duet_types::Metadata> {
            self.inner.stat(p, follow).await
        }
        async fn volume_stats(&self, p: &VPath) -> duet_types::Result<duet_vfs::VolumeStats> {
            self.inner.volume_stats(p).await
        }
        async fn open_read(
            &self,
            p: &VPath,
        ) -> duet_types::Result<Box<dyn duet_vfs::AsyncReadSeek>> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.inner.open_read(p).await
        }
        async fn open_write(
            &self,
            p: &VPath,
            o: duet_vfs::WriteOpts,
        ) -> duet_types::Result<Box<dyn duet_vfs::AsyncWriteCommit>> {
            self.inner.open_write(p, o).await
        }
        async fn create_dir(
            &self,
            p: &VPath,
            mode: Option<duet_vfs::Mode>,
        ) -> duet_types::Result<()> {
            self.inner.create_dir(p, mode).await
        }
        async fn remove(&self, p: &VPath, kind: duet_vfs::RemoveKind) -> duet_types::Result<()> {
            self.inner.remove(p, kind).await
        }
        async fn rename(
            &self,
            from: &VPath,
            to: &VPath,
            flags: duet_vfs::RenameFlags,
        ) -> duet_types::Result<()> {
            self.inner.rename(from, to, flags).await
        }
        async fn link(&self, source: &VPath, dest: &VPath) -> duet_types::Result<()> {
            self.inner.link(source, dest).await
        }
        async fn set_meta(&self, p: &VPath, m: &duet_types::MetaPatch) -> duet_types::Result<()> {
            self.inner.set_meta(p, m).await
        }
        fn watch(
            &self,
            p: &VPath,
        ) -> duet_types::Result<futures_util::stream::BoxStream<'_, duet_vfs::ChangeEvent>>
        {
            self.inner.watch(p)
        }
        async fn server_side_copy(
            &self,
            _from: &VPath,
            _to: &VPath,
            _should_cancel: &(dyn Fn() -> bool + Send + Sync),
        ) -> duet_types::Result<duet_vfs::CopyOutcome> {
            // Always `Unsupported` -- forces every copy through the
            // naive path, whose first call is `open_read`, so `delay`
            // above is actually reached, same reasoning as `duet-ops`'s
            // own `SlowFs`.
            Ok(duet_vfs::CopyOutcome::Unsupported)
        }
    }

    /// Plans and enqueues a real `JobKind::Copy` job (`src` -> `dst_dir`)
    /// against `fs` directly through `duet_ops::plan_copy`/
    /// `QueueManager::enqueue` -- the same off-UI-thread path
    /// `CopyMoveDialogState::confirm` uses, but driven here without a live
    /// dialog so the test can inject `SlowLocalFs` (`confirm` hardcodes a
    /// plain `LocalFs`, see its own doc comment). Blocks on a
    /// `std::sync::mpsc` channel, not GPUI polling -- `tokio_handle` is a
    /// real multi-threaded runtime already running on its own OS thread
    /// (`with_workspace`'s own setup), so the spawned task makes real
    /// progress independent of anything GPUI does.
    fn enqueue_slow_copy(
        tokio_handle: &tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        fs: Arc<dyn FileSystem>,
        src: VPath,
        dst: VPath,
        state_dir: PathBuf,
    ) -> duet_ops::JobId {
        let (tx, rx) = std::sync::mpsc::channel();
        let fs_for_plan = fs.clone();
        tokio_handle.spawn(async move {
            let cancel = duet_ops::CancelToken::new();
            let plan = duet_ops::plan_copy(
                fs_for_plan.as_ref(),
                &[src],
                &dst,
                duet_ops::PlanOptions::default(),
                &cancel,
            )
            .await
            .expect("plan_copy over a real tempdir must succeed");
            let id = queue.enqueue(JobKind::Copy, plan, 0, fs_for_plan, state_dir, 1, None);
            let _ = tx.send(id);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("plan_copy + enqueue must complete quickly even with a slowed FileSystem")
    }

    /// (a) The tray's own "hidden when idle, shown when active" logic,
    /// exercised against a real `Workspace`/`QueueManager`, not just
    /// `operation_manager::tray_summary`'s own pure-logic unit tests: idle
    /// at start, `Some` while a real slowed job is genuinely `Running`,
    /// back to `None` once it finishes.
    #[gpui::test]
    fn operations_tray_text_is_hidden_when_idle_and_shown_while_a_job_is_active(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            workspace.read_with(vcx, |ws, _| {
                assert_eq!(
                    ws.operations_tray_text(),
                    None,
                    "an idle queue must render nothing in the tray"
                );
            });

            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("a.bin"), vec![0u8; 256]).unwrap();

            let (queue, state_dir, tokio_handle) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                )
            });
            let fs: Arc<dyn FileSystem> = Arc::new(SlowLocalFs {
                inner: LocalFs,
                delay: Duration::from_millis(200),
            });
            let src = crate::file_table::local_vpath(source_dir.path()).unwrap();
            let dst = crate::file_table::local_vpath(dest_dir.path()).unwrap();
            let job_id = enqueue_slow_copy(&tokio_handle, queue.clone(), fs, src, dst, state_dir);

            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.operations_tray_text().is_some())
            });

            wait_until(vcx, |_vcx| {
                matches!(
                    queue.job(job_id).map(|j| j.state),
                    Some(JobState::Terminal { .. })
                )
            });
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.operations_tray_text().is_none())
            });
        });
    }

    /// `dispatch_action` routes an action by walking up from whatever
    /// element currently holds keyboard focus -- with nothing focused at
    /// all (the state a freshly built `with_workspace` window starts in),
    /// there is no dispatch path for it to walk, so `OpenOperationManager`
    /// (bound on the workspace root, same as every other `Open*` action)
    /// never reaches its handler. Every existing F5/F6 test already
    /// established this same "focus the left panel first" step for
    /// exactly this reason; factored out here since three T-5.2.2 tests
    /// need it too.
    fn focus_left_panel(workspace: &Entity<Workspace>, vcx: &mut VisualTestContext) {
        let left_handle =
            workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
        vcx.update(|window, _cx| window.focus(&left_handle));
        let _ = vcx.update(|window, cx| window.draw(cx));
    }

    /// (b) `Ctrl+O` opens the manager, and it lists a real job from
    /// `queue.snapshot()` -- kind, id, and state all coming from the real
    /// queue, not a stub.
    #[gpui::test]
    fn operation_manager_opens_via_its_keybinding_and_lists_real_queue_jobs(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("b.bin"), vec![0u8; 256]).unwrap();

            let (queue, state_dir, tokio_handle) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                )
            });
            let fs: Arc<dyn FileSystem> = Arc::new(SlowLocalFs {
                inner: LocalFs,
                delay: Duration::from_millis(200),
            });
            let src = crate::file_table::local_vpath(source_dir.path()).unwrap();
            let dst = crate::file_table::local_vpath(dest_dir.path()).unwrap();
            let job_id = enqueue_slow_copy(&tokio_handle, queue, fs, src, dst, state_dir);

            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenOperationManager);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.operation_manager.is_some(),
                    "Ctrl+O must open the operation manager"
                );
            });

            let state = workspace
                .read_with(vcx, |ws, _| ws.operation_manager.clone())
                .unwrap();
            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| {
                    s.jobs_for_test()
                        .iter()
                        .any(|j| j.id == job_id && j.kind == JobKind::Copy)
                })
            });
        });
    }

    /// Real keyboard-only row navigation (Up/Down) between two rows --
    /// see `crate::operation_manager`'s module doc comment for the
    /// cursor-index design this exercises. Two independent slow jobs
    /// (separate source subdirectories) so `sorted_jobs`'s own
    /// most-recently-enqueued-first order is deterministic: the second
    /// job enqueued must be row 0, the first row 1.
    #[gpui::test]
    fn operation_manager_cursor_navigates_rows_with_up_and_down(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let (queue, state_dir, tokio_handle) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                )
            });

            // Two independent source/dest tempdir pairs, kept alive for
            // this whole test body (not just a loop iteration) -- each
            // still-`Running` slowed copy needs its own source directory
            // to keep reading from until this test explicitly cancels it
            // below.
            let source_dir_a = tempfile::tempdir().unwrap();
            let dest_dir_a = tempfile::tempdir().unwrap();
            let source_dir_b = tempfile::tempdir().unwrap();
            let dest_dir_b = tempfile::tempdir().unwrap();
            std::fs::write(source_dir_a.path().join("c.bin"), vec![0u8; 256]).unwrap();
            std::fs::write(source_dir_b.path().join("c.bin"), vec![0u8; 256]).unwrap();

            let mut job_ids = Vec::new();
            for (source_dir, dest_dir) in
                [(&source_dir_a, &dest_dir_a), (&source_dir_b, &dest_dir_b)]
            {
                let fs: Arc<dyn FileSystem> = Arc::new(SlowLocalFs {
                    inner: LocalFs,
                    delay: Duration::from_millis(300),
                });
                let src = crate::file_table::local_vpath(source_dir.path()).unwrap();
                let dst = crate::file_table::local_vpath(dest_dir.path()).unwrap();
                let id = enqueue_slow_copy(
                    &tokio_handle,
                    queue.clone(),
                    fs,
                    src,
                    dst,
                    state_dir.clone(),
                );
                job_ids.push(id);
            }

            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenOperationManager);
            let _ = vcx.update(|window, cx| window.draw(cx));
            let state = workspace
                .read_with(vcx, |ws, _| ws.operation_manager.clone())
                .expect("Ctrl+O must open the manager");

            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| s.jobs_for_test().len() == 2)
            });

            state.read_with(vcx, |s, _| {
                assert_eq!(s.cursor_for_test(), 0, "the cursor must start on row 0");
            });

            vcx.dispatch_action(OperationManagerCursorDown);
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |s, _| {
                assert_eq!(s.cursor_for_test(), 1, "Down must move the cursor to row 1");
            });

            // Down again, past the last row, must clamp rather than go
            // out of bounds.
            vcx.dispatch_action(OperationManagerCursorDown);
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |s, _| {
                assert_eq!(s.cursor_for_test(), 1, "Down past the last row must clamp");
            });

            vcx.dispatch_action(OperationManagerCursorUp);
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |s, _| {
                assert_eq!(
                    s.cursor_for_test(),
                    0,
                    "Up must move the cursor back to row 0"
                );
            });

            // Up again, past the first row, must clamp too.
            vcx.dispatch_action(OperationManagerCursorUp);
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |s, _| {
                assert_eq!(s.cursor_for_test(), 0, "Up past the first row must clamp");
            });

            // Drain both jobs so the test doesn't leave background work
            // running past its own scope.
            for id in job_ids {
                let _ = queue.cancel(id);
            }
        });
    }

    /// (c) The real end-to-end path: enqueue a genuinely slow job, open
    /// the manager, confirm it shows live progress, Pause it via the
    /// manager's own keyboard control and confirm `queue.job(id)`'s state
    /// actually becomes `Paused`, then Resume and let it finish,
    /// confirming the manager reflects `Terminal` -- and that
    /// `Workspace::job_progress` evicted the finished job's entry (see
    /// that field's own doc comment).
    #[gpui::test]
    fn operation_manager_end_to_end_pause_resume_via_keyboard(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            for i in 0..3 {
                std::fs::write(source_dir.path().join(format!("f{i}.bin")), vec![0u8; 4096])
                    .unwrap();
            }

            let (queue, state_dir, tokio_handle) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                )
            });
            let fs: Arc<dyn FileSystem> = Arc::new(SlowLocalFs {
                inner: LocalFs,
                delay: Duration::from_millis(400),
            });
            let src = crate::file_table::local_vpath(source_dir.path()).unwrap();
            let dst = crate::file_table::local_vpath(dest_dir.path()).unwrap();
            let job_id = enqueue_slow_copy(&tokio_handle, queue.clone(), fs, src, dst, state_dir);

            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenOperationManager);
            let _ = vcx.update(|window, cx| window.draw(cx));
            let state = workspace
                .read_with(vcx, |ws, _| ws.operation_manager.clone())
                .expect("Ctrl+O must open the manager");

            // The manager must observe the job actually `Running` ...
            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| {
                    s.jobs_for_test()
                        .iter()
                        .any(|j| j.id == job_id && matches!(j.state, JobState::Running { .. }))
                })
            });
            // ... and, separately, that live progress has actually landed
            // (there is a real window between `Started` and the first
            // 100ms sample -- see `job_progress`'s own doc comment).
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.job_progress_snapshot(job_id).is_some())
            });

            // Pause via the manager's own keyboard control -- the cursor
            // defaults to row 0, and this is the only job in the queue.
            vcx.dispatch_action(OperationManagerPauseSelected);
            wait_until(vcx, |_vcx| {
                matches!(
                    queue.job(job_id).map(|j| j.state),
                    Some(JobState::Paused { .. })
                )
            });

            // Resume, and let it run to completion.
            vcx.dispatch_action(OperationManagerResumeSelected);
            wait_until(vcx, |_vcx| {
                matches!(
                    queue.job(job_id).map(|j| j.state),
                    Some(JobState::Terminal { .. })
                )
            });

            // The manager's own next render must reflect the terminal
            // state too, not just `QueueManager`'s internal one.
            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| {
                    s.jobs_for_test()
                        .iter()
                        .any(|j| j.id == job_id && j.state.is_terminal())
                })
            });

            // And `job_progress` must have evicted this job's entry once
            // it finished -- see that field's own doc comment.
            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.job_progress_snapshot(job_id).is_none(),
                    "a finished job's progress sample must be evicted, not linger forever"
                );
            });

            // `plan_copy` copies a directory *source* as a subdirectory of
            // `dest_dir` (named after the source's own basename), not by
            // flattening its contents directly into `dest_dir` -- see
            // `planner::plan_copy`'s own `dest_dir.join(name)` where `name`
            // is `source`'s file name.
            let copied_subdir = dest_dir.path().join(source_dir.path().file_name().unwrap());
            let dest_files_copied = (0..3)
                .filter(|i| copied_subdir.join(format!("f{i}.bin")).is_file())
                .count();
            assert_eq!(
                dest_files_copied, 3,
                "pausing and resuming must not have dropped any of the three files"
            );
        });
    }
}
