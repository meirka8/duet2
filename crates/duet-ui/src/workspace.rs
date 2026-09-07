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
use duet_ops::{
    ConflictResolver, JobEvent, JobId, JobKind, JobOutcome, JobReport, JournalReader, MountScan,
    ProgressSnapshot, QueueManager, RecoveryReport, TrashEntry, list_trash_entries_with_mounts,
};
use duet_types::{UnixPathBuf, VPath};
use duet_vfs::{FileSystem, ListOpts, LocalFs};
use duet_widgets::{
    input::{Input, InputState},
    layout::{Root, TitleBar, WindowExt, h_flex, v_flex},
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
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, WindowBounds,
    WindowOptions, actions, px, size,
};

use crate::attributes_dialog::{
    AttributesDialogState, AttributesPrefill, bind_attributes_dialog_keys,
};
use crate::columns::{ColumnLayout, ColumnLayoutStore};
use crate::command_palette::CommandPaletteDelegate;
use crate::conflict_dialog::{
    ConflictDialogState, ConflictRequest, InteractiveConflictResolver, bind_conflict_dialog_keys,
};
use crate::copy_move_dialog::{CopyMoveDialogState, bind_copy_move_dialog_keys};
use crate::delete_dialog::{DeleteDialogState, bind_delete_dialog_keys};
use crate::file_table::{
    FileTable, FileTableSettings, MouseMode, QuickSearchMode, write_byte_count,
};
use crate::function_bar::{self, FKeySlot};
use crate::hotlist::HotlistDelegate;
use crate::job_report_dialog::{JobReportDialogState, bind_job_report_dialog_keys};
use crate::link_dialog::{LinkDialogState, LinkKind};
use crate::mkdir_dialog::MkdirDialogState;
use crate::operation_manager::{OperationManagerState, bind_operation_manager_keys};
use crate::panel::{Panel, bind_panel_keys};
use crate::recovery_dialog::{RecoveryDialogState, bind_recovery_dialog_keys};
use crate::rename_dialog::RenameDialogState;
use crate::theme_controller::ThemeController;
use crate::trash_dialog::{TrashDialogState, bind_trash_dialog_keys};

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

// T-5.2.6's F8/Shift+F8 delete confirmation (FR-OPS-01).
// `docs/keymap-tc.csv` rows 10-13 (`ops.delete` on both `F8` and `Delete`,
// `ops.delete_permanent` on both `Shift+F8` and `Shift+Delete`) are all
// "known" TC bindings, not this codebase's own defaults -- so all four are
// bound, each pair to the same action. The `"Workspace"` scope matches
// `CopyDialog`/`MoveDialog`'s: these open an overlay that isn't a panel's
// or a table's concern. `Delete` is also bound (to `HotlistRemoveEntry`)
// in the deeper `"HotlistOverlay"` context, which therefore wins whenever
// that overlay actually holds focus -- exactly the context-scoping
// `docs/keymap-tc.csv`'s own row 143 comment calls out as the point of
// design.md §9.4.
actions!(duet_workspace, [DeleteDialog, DeletePermanentDialog]);

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

// T-5.2.7's four "create one thing" dialogs (FR-OPS-01). Two of the four
// chords are verified TC bindings from `docs/keymap-tc.csv`: row 9
// (`ops.mkdir`, `F7`) and row 8 (`ops.rename_in_place`, `Shift+F6`), both
// "known" confidence.
//
// The other two are **this codebase's own reasonable defaults, not
// verified TC chords** -- `docs/commands.md` catalogues
// `ops.create_symlink`/`ops.create_hardlink`, but `docs/keymap-tc.csv`
// (the TC survey, and the only source of "known" bindings here) has no row
// for either, so there is no Total Commander binding to match.
// `Ctrl+Shift+S` (Symlink) / `Ctrl+Shift+H` (Hardlink) are chosen for the
// obvious mnemonic and confirmed unclaimed against every other
// `KeyBinding::new` call site in this crate -- the same disclosed-default
// situation `OpenCommandPalette`'s `Ctrl+Shift+P`, `NavigateHome`'s
// `Alt+Home` and the hotlist's `Ctrl+D`/`Ctrl+Shift+D` are already in. See
// `crate::link_dialog`'s module doc comment for the rest of that
// reasoning, including the disclosed "cursor entry only, not a
// multi-selection" scope boundary both link dialogs sit inside.
//
// The `"Workspace"` scope matches `CopyDialog`/`DeleteDialog`'s: all four
// open an overlay that is neither a panel's nor a table's concern.
// Everything the overlays need once open (Enter, Escape) needs no binding
// from this crate at all -- see `crate::mkdir_dialog`'s module doc comment
// for why, which is also why there is no `bind_mkdir_dialog_keys`/
// `bind_rename_dialog_keys`/`bind_link_dialog_keys` alongside
// `bind_copy_move_dialog_keys`/`bind_delete_dialog_keys` below.
actions!(
    duet_workspace,
    [MkdirDialog, RenameDialog, SymlinkDialog, HardlinkDialog]
);

// T-5.2.8's Ctrl+A attributes/permissions dialog (FR-OPS-12). Unlike the
// two link chords right above, this one is a **verified TC binding**:
// `docs/keymap-tc.csv` row 16 (`ops.change_attributes`, "known"
// confidence) with the note "Single most-cited keybinding 'gotcha' in TC"
// -- in Total Commander, Ctrl+A opens Change Attributes, it does *not*
// select all. This app deliberately follows that, which costs nothing
// here: "select all" is `Ctrl++` (`file_table::SelectAll`, per
// `docs/keymap-tc.csv`'s own `sel.all` row), so Ctrl+A was genuinely
// unclaimed by every `KeyBinding::new` call site in this crate.
//
// `gpui-component`'s `InputState` binds Ctrl+A to a select-all inside its
// own `"Input"` key context, which is strictly deeper than `"Workspace"`
// -- so a focused text field still gets select-all, and only a focused
// panel opens this dialog. That is the correct behaviour for both, not a
// conflict.
//
// `docs/commands.md` catalogues the same command as `file.attributes`
// (`panel && selection.nonempty`) -- a known, pre-existing id mismatch
// with the CSV's `ops.change_attributes`, not something this task
// reconciles. Its `file.chmod_recursive`/`file.set_timestamps` neighbours
// are this same dialog's own internals (the Ctrl+R toggle, the two
// timestamp fields), not separate top-level commands.
actions!(duet_workspace, [AttributesDialog]);

// T-5.3.2 phase 2's trash browser (`trash.open_browser`, `docs/commands.md`'s
// `app`-context row -- no panel focus required, same reasoning `Ctrl+D`
// (`OpenHotlist`)/`Ctrl+Shift+P` (`OpenCommandPalette`) already establish
// for a Workspace-scoped trigger that must fire regardless of which panel
// currently holds focus). `Alt+T` is this module's own disclosed default:
// Total Commander has no comparable trash-browser feature, so
// `docs/keymap-tc.csv` has no row to follow, and `Alt+T` is unclaimed by
// every `KeyBinding::new` call site in this crate at *any* scope (checked
// directly, not just at `"Workspace"`) -- unlike, say, `Ctrl+T` (`Panel`'s
// own "new tab"), which would silently never fire here while a panel holds
// focus, since the nearer context's binding always wins first.
actions!(duet_workspace, [OpenTrashDialog]);

// T-4.3.4's path bar (`nav.goto_path`, `docs/commands.md`'s Navigation
// row, FR-NAV-09): focus the active panel's header as an editable path
// input. `Ctrl+L` is this module's own disclosed default -- Total Commander
// has no equivalent (its title bar isn't editable), so `docs/keymap-tc.csv`
// has no row to follow, and `Ctrl+L` is what every GTK file chooser,
// Nautilus, Dolphin and every browser use for exactly this; it is unclaimed
// by every `KeyBinding::new` call site in this crate at any scope. Bound in
// the `FileTable` context (the panel), not `Workspace`, so it cannot fire
// from inside a dialog or the command line.
actions!(duet_workspace, [GotoPath]);

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
        KeyBinding::new("f8", DeleteDialog, Some("Workspace")),
        KeyBinding::new("delete", DeleteDialog, Some("Workspace")),
        KeyBinding::new("shift-f8", DeletePermanentDialog, Some("Workspace")),
        KeyBinding::new("shift-delete", DeletePermanentDialog, Some("Workspace")),
        KeyBinding::new("f7", MkdirDialog, Some("Workspace")),
        KeyBinding::new("shift-f6", RenameDialog, Some("Workspace")),
        KeyBinding::new("ctrl-shift-s", SymlinkDialog, Some("Workspace")),
        KeyBinding::new("ctrl-shift-h", HardlinkDialog, Some("Workspace")),
        KeyBinding::new("ctrl-a", AttributesDialog, Some("Workspace")),
        KeyBinding::new("alt-t", OpenTrashDialog, Some("Workspace")),
        KeyBinding::new("ctrl-l", GotoPath, Some("FileTable")),
    ]);
}

/// Opens the Duet application window.
pub fn run() {
    // T-3.1.6's UI-thread blocking guard, armed. `duet-vfs`'s every
    // `LocalFs` syscall wrapper `debug_assert!`s that it is *not* on the
    // thread flagged here, so a directory listing, `stat`, or copy that
    // accidentally runs on the GPUI thread panics in a debug build
    // instead of silently stalling the UI (zero cost in release). This is
    // the one call the guard's own doc comment says "lands wherever the
    // GPUI shell boots" -- and until 2026-09-04 it never had: the 28
    // assertion sites existed, nothing ever armed them. `Application::run`
    // drives the UI on this same thread, so marking it here is marking
    // the UI thread.
    duet_vfs::local::mark_ui_thread();

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
        bind_delete_dialog_keys(cx);
        bind_operation_manager_keys(cx);
        bind_job_report_dialog_keys(cx);
        bind_conflict_dialog_keys(cx);
        bind_attributes_dialog_keys(cx);
        bind_recovery_dialog_keys(cx);
        bind_trash_dialog_keys(cx);

        let bounds = Bounds::centered(None, size(px(1024.0), px(700.0)), cx);
        // The window's own titlebar text/traffic-light metadata --
        // `TitleBar::title_bar_options()`'s own defaults
        // (`appears_transparent`/`traffic_light_position`) are tuned to
        // match the `duet_widgets::layout::TitleBar` element this window
        // renders as its own root child below; only `title` is overridden
        // here since that default is deliberately `None` (a bare-bones
        // titlebar embedder has nowhere to put a title string, so the
        // element itself renders one instead -- see this window's root
        // `TitleBar::new().child(...)` call).
        let mut titlebar = TitleBar::title_bar_options();
        titlebar.title = Some(SharedString::from("Duet"));
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar),
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

                spawn_window_chrome_cycle_if_requested(window, cx);

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

/// Diagnostics-only hook for the Wayland out-of-date swapchain freeze
/// (ADR-007, `vendor/README.md`'s verification recipe): when
/// `DUET_DEBUG_CHROME_CYCLE_MS=<ms>` is set, the window toggles fullscreen
/// on, off, then maximize, restore, every `<ms>` milliseconds for the life
/// of the process. Each of those transitions makes GNOME/Mutter re-send
/// the surface's dmabuf feedback, which is what retires the swapchain --
/// the actual trigger otherwise depends on pointer/focus timing that is
/// awkward to drive by hand, so this exists to make the bump gate (ADR-003)
/// reproducible without clicking. Inert unless the variable is set; the
/// value is parsed leniently (anything unparsable means 4000ms) because a
/// diagnostics switch that panics on a typo helps nobody.
fn spawn_window_chrome_cycle_if_requested(window: &mut Window, cx: &mut App) {
    let Ok(raw) = std::env::var("DUET_DEBUG_CHROME_CYCLE_MS") else {
        return;
    };
    let period = Duration::from_millis(raw.trim().parse().unwrap_or(4000));
    tracing::warn!(
        ?period,
        "DUET_DEBUG_CHROME_CYCLE_MS set: cycling fullscreen/maximize for diagnostics"
    );
    window
        .spawn(cx, async move |cx| {
            let mut step: u32 = 0;
            loop {
                cx.background_executor().timer(period).await;
                let fullscreen = step % 4 < 2;
                tracing::warn!(
                    step,
                    action = if fullscreen {
                        "toggle_fullscreen"
                    } else {
                        "zoom_window"
                    },
                    "DEBUG: step {step}: window chrome cycle"
                );
                let updated = cx.update(|window, _cx| {
                    if fullscreen {
                        window.toggle_fullscreen();
                    } else {
                        window.zoom_window();
                    }
                });
                if updated.is_err() {
                    // The window is gone; nothing left to cycle.
                    return;
                }
                step = step.wrapping_add(1);
            }
        })
        .detach();
}

/// A one-shot cooperative yield: returns `Pending` exactly once (waking
/// itself immediately), so a long-running async loop hands control back to
/// its executor between iterations. See `Workspace::new`'s queue-event
/// consumer for the concrete case this exists for. GPUI's executors have
/// no `yield_now` of their own; this is the minimal, executor-agnostic
/// equivalent.
fn yield_once() -> impl std::future::Future<Output = ()> {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
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
    /// Where the trash browser (`Alt+T`, [`Self::open_trash_dialog`]) looks
    /// for per-mount trash roots. `MountScan::System` -- the real mount
    /// table -- everywhere except tests, which set `MountScan::Explicit`
    /// (see `with_configured_workspace_inner`) for the same reason they
    /// redirect `$XDG_DATA_HOME`: a test must never see, let alone act on,
    /// the developer's real trash. This can't be an environment redirect
    /// like the XDG ones because there is no environment knob for
    /// `/proc/self/mountinfo`; it has to be an explicit policy.
    trash_mount_scan: MountScan,
    /// T-4.3.4: the path bar in its editing face, when open -- at most one
    /// across both panels (`PathBarState::side` says which); `None` means
    /// both headers show their breadcrumb. See `crate::path_bar`.
    path_bar: Option<Entity<crate::path_bar::PathBarState>>,
    /// Whatever had focus before the path bar opened (the panel's table,
    /// normally), restored by `close_path_bar`. Held here rather than in
    /// the bar so closing from inside one of the bar's own listeners never
    /// has to read the bar entity while it is leased.
    path_bar_previous_focus: Option<FocusHandle>,

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

    /// `Some` while the F8/Shift+F8 delete confirmation (T-5.2.6,
    /// FR-OPS-01) is open -- constructed fresh on every
    /// `open_delete_dialog` that actually decides to confirm, dropped on
    /// close. Mirrors `copy_move_dialog`'s own field exactly; see
    /// `crate::delete_dialog`'s module doc comment for the overlay's own
    /// architecture. Stays `None` for the whole life of a delete run under
    /// `confirm_delete = "never"` (and under `"non_empty_dirs"` when
    /// nothing warned about was found) -- those paths enqueue directly.
    delete_dialog: Option<Entity<DeleteDialogState>>,
    /// Saved by `open_delete_dialog`, restored and cleared by
    /// `close_delete_dialog`/`close_delete_dialog_deferred` -- same
    /// reasoning as `copy_move_dialog_previous_focus`. Only ever set on the
    /// path that actually shows the dialog: there is no previous focus to
    /// restore when nothing took focus away in the first place.
    delete_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.2.7's F7 "create directory" dialog is open.
    /// Mirrors `copy_move_dialog`'s own field exactly; see
    /// `crate::mkdir_dialog`'s module doc comment.
    mkdir_dialog: Option<Entity<MkdirDialogState>>,
    /// Saved by `open_mkdir_dialog`, restored and cleared by
    /// `close_mkdir_dialog`/`close_mkdir_dialog_deferred` -- same reasoning
    /// as `copy_move_dialog_previous_focus`.
    mkdir_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.2.7's Shift+F6 in-place rename dialog is open.
    /// See `crate::rename_dialog`'s module doc comment, in particular for
    /// why the extension is a fixed label rather than unselected text.
    rename_dialog: Option<Entity<RenameDialogState>>,
    /// Saved by `open_rename_dialog`, restored and cleared by
    /// `close_rename_dialog`/`close_rename_dialog_deferred`.
    rename_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.2.7's symlink *or* hardlink dialog is open -- one
    /// field for both, since one `LinkDialogState` type serves both
    /// commands (see `crate::link_dialog`'s module doc comment) and, like
    /// every other overlay here, only one can be open at a time anyway.
    link_dialog: Option<Entity<LinkDialogState>>,
    /// Saved by `open_link_dialog`, restored and cleared by
    /// `close_link_dialog`/`close_link_dialog_deferred`.
    link_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.2.8's `Ctrl+A` attributes/permissions dialog is
    /// open. Mirrors `copy_move_dialog`'s own field exactly; see
    /// `crate::attributes_dialog`'s module doc comment, in particular for
    /// why `Ctrl+A` is Change Attributes here and not "select all".
    attributes_dialog: Option<Entity<AttributesDialogState>>,
    /// Saved by `open_attributes_dialog` on the path that actually shows
    /// the dialog, restored and cleared by `close_attributes_dialog`/
    /// `close_attributes_dialog_deferred`.
    attributes_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.2.5's startup interrupted-operation recovery
    /// dialog is open. Unlike every other dialog above, this one is never
    /// opened by a user keystroke -- `Self::new` constructs it directly
    /// (see that method's own startup-scan section) when a fresh
    /// `JournalReader::scan` finds at least one report still needing
    /// attention, and it is otherwise `None` for the rest of the process's
    /// life once closed. See `crate::recovery_dialog`'s module doc comment
    /// for the full architecture.
    recovery_dialog: Option<Entity<RecoveryDialogState>>,
    /// Saved by `Self::new` on the path that actually shows the dialog,
    /// restored and cleared by `close_recovery_dialog` -- this dialog has
    /// no `_deferred` sibling; see `crate::recovery_dialog`'s module doc
    /// comment for why its own async continuations never need one.
    recovery_dialog_previous_focus: Option<FocusHandle>,

    /// `Some` while T-5.3.2 phase 2's trash browser is open. See
    /// `crate::trash_dialog`'s module doc comment for the full
    /// architecture, including why (unlike `recovery_dialog` above) it has
    /// no `_deferred` close sibling.
    trash_dialog: Option<Entity<TrashDialogState>>,
    /// Saved by `open_trash_dialog`'s continuation on the path that
    /// actually shows the dialog, restored and cleared by
    /// `close_trash_dialog`.
    trash_dialog_previous_focus: Option<FocusHandle>,

    /// `operations.confirm_delete` (`"always"` | `"non_empty_dirs"` |
    /// `"never"`), read once at startup the same way every other
    /// `settings.toml`-derived field in this struct is (see
    /// `load_confirm_delete_policy`). Kept as the raw string, matching
    /// `duet_config::Operations`'s own representation -- there is no
    /// `duet-ui`-side enum for it and one value's worth of `match` doesn't
    /// earn one.
    confirm_delete: String,
    /// The effective default for a plain F8's trash-versus-permanent
    /// choice: `operations.delete_default == "permanent"`, *or*
    /// `!trash.enabled` (`docs/config-schema.md`: "when `trash.enabled ==
    /// false`, `delete_default` cannot be `trash`" -- treated here as a
    /// defensive fallback to permanent, not an error). Irrelevant to
    /// Shift+F8, which forces permanent regardless.
    delete_default_permanent: bool,

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

    /// `Some` while T-5.2.4's error/skip report overlay is open -- opened
    /// from the operation manager's own Enter/`O` on a terminal row
    /// (`ops.queue.show_errors`), which this app deliberately treats as
    /// *replacing* the manager rather than stacking on top of it; see
    /// `crate::job_report_dialog`'s module doc comment for that disclosed
    /// judgment call and the two concrete reasons behind it.
    job_report_dialog: Option<Entity<JobReportDialogState>>,
    /// Saved by `open_job_report_dialog`, restored and cleared by
    /// `close_job_report_dialog`/`close_job_report_dialog_deferred`. Since
    /// opening the report closes the operation manager underneath it, this
    /// inherits the *manager's* own saved handle (the panel Ctrl+O was
    /// pressed in) rather than the manager's about-to-be-dropped one --
    /// see `open_job_report_dialog`.
    job_report_dialog_previous_focus: Option<FocusHandle>,

    /// T-5.2.3's live conflict dialog (FR-OPS-04): `Some` while a real,
    /// unresolved conflict from any running copy/move job is waiting on a
    /// human answer. See `crate::conflict_dialog`'s module doc comment for
    /// the full request/response bridge. Constructed by
    /// [`Self::spawn_conflict_dialog_entity`] (called either from the
    /// conflict-request consumer loop below, with no live `Window`, or
    /// from [`Self::close_conflict_dialog`] itself when advancing to the
    /// next queued conflict, which does have one), dropped on close.
    conflict_dialog: Option<Entity<ConflictDialogState>>,
    /// Captured the moment a conflict dialog first takes over keyboard
    /// focus -- [`Self::render`]'s `pending_conflict_focus` drain is the
    /// only place with both a live `Window` and the guarantee that focus
    /// hasn't moved yet, so *that* is where this is set, not at
    /// construction time (unlike every other `*_previous_focus` field in
    /// this struct). Restored once every queued conflict has been
    /// answered and `conflict_dialog` goes back to `None` with nothing
    /// left in `pending_conflict_requests`.
    conflict_dialog_previous_focus: Option<FocusHandle>,
    /// A `FocusHandle` to focus via `window.focus` on the next render,
    /// then cleared -- the same "no live `Window`" problem
    /// `pending_focus_restore` documents (see that field's own doc
    /// comment), for the one *open* path that hits it: a conflict is
    /// detected on a background executor thread at an unpredictable
    /// moment, so `Self::new`'s conflict-request consumer loop (a
    /// `cx.spawn` async block, no `Window`) is the only place that can
    /// react to it.
    pending_conflict_focus: Option<FocusHandle>,
    /// Every [`ConflictRequest`] that arrived while a conflict dialog was
    /// already open -- concurrent jobs (up to
    /// `COPY_MOVE_QUEUE_MAX_CONCURRENT` of them) can each hit their own
    /// first conflict around the same time, and each is genuinely blocked
    /// (`InteractiveConflictResolver::resolve`'s `block_in_place`) until
    /// answered, so none can simply be dropped. Bounded in practice by the
    /// number of concurrently *running* jobs (a small constant), not by
    /// conflict count within one job -- T-5.2.3's own AC ("a 10k-conflict
    /// run is survivable using apply-to-all") is a statement about the
    /// *existing*, already-tested sticky `ConflictScope::AllRemaining`
    /// mechanism (`duet_ops::executor::resolve_conflict`'s own
    /// `sticky_conflict`) answering every conflict after the first
    /// without ever consulting this resolver -- and therefore this queue
    /// -- again, not about this field's own size.
    pending_conflict_requests: std::collections::VecDeque<ConflictRequest>,
    /// The single, live [`duet_ops::ConflictResolver`] shared by every
    /// copy/move dialog this workspace opens
    /// (`CopyMoveDialogState::confirm` passes
    /// `Some(Arc::clone(&self.conflict_resolver))` instead of T-5.2.1's
    /// original, always-`None` placeholder) -- constructed once, here,
    /// alongside `queue`, with its own dedicated request channel
    /// (`conflict_request_rx`'s consumer loop, in [`Self::new`], is the
    /// only reader).
    conflict_resolver: Arc<InteractiveConflictResolver>,

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
pub(crate) enum PanelSide {
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
        // T-4.2.4: the one column layout every tab in both panels renders
        // from (see `columns.rs`), read once from `[panels.layouts.full]`
        // (built-in default when absent or unusable). Owned by the two
        // `Panel`s (every tab holds a handle); observed here to persist
        // each change any table publishes to `settings.toml`, off the UI
        // thread, exactly like `splitter_ratio`.
        let column_layouts = cx.new(|_| {
            ColumnLayoutStore::new(
                settings_path
                    .as_deref()
                    .map(load_column_layout)
                    .unwrap_or_default(),
            )
        });
        cx.observe(&column_layouts, |this, store, cx| {
            let layout = store.read(cx).full().to_config();
            this.persist_column_layout(layout, cx);
        })
        .detach();
        // T-5.2.6: the two `[operations]`/`[trash]` values F8 needs, read
        // once here alongside every other `settings.toml`-derived default.
        let confirm_delete = settings_path
            .as_deref()
            .map(load_confirm_delete_policy)
            .unwrap_or_else(|| duet_config::Settings::default().operations.confirm_delete);
        let delete_default_permanent = settings_path
            .as_deref()
            .map(load_delete_default_permanent)
            .unwrap_or(false);

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
                column_layouts.clone(),
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
                column_layouts.clone(),
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
        let mut pending_notice: Vec<PendingNotice> = pending_notice
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

        // T-5.2.5: the startup interrupted-operation recovery scan
        // (FR-OPS-07) -- synchronous, like every other startup-time load
        // in this constructor (`load_session_with_notice`,
        // `load_hotlist_entries`, ...): `JournalReader::scan` only ever
        // reads a handful of small journal files, and there is no live
        // `Window` yet for an async round-trip to hand a result back into
        // anyway. Folded into `pending_notice` exactly the way T-4.3.7's
        // own `session.json` load failure is -- "a file that exists and
        // failed to load is a real event" applies just as much to a
        // corrupt journal as to a corrupt session file.
        let (recovery_reports, recovery_scan_notice) = state_dir
            .as_deref()
            .map(scan_startup_recovery_reports)
            .unwrap_or_default();
        if let Some(message) = recovery_scan_notice {
            pending_notice.push(PendingNotice {
                level: NoticeLevel::Error,
                message,
            });
        }

        // T-5.2.3: the live conflict dialog's own request channel --
        // `InteractiveConflictResolver::resolve` (called synchronously
        // from deep inside a `tokio::spawn`'d executor task, potentially
        // blocking) sends here; the consumer loop just below is the only
        // reader, running on GPUI's own executor via `cx.spawn` -- see
        // `crate::conflict_dialog`'s module doc comment for the full
        // bridge this implements.
        let (conflict_request_tx, mut conflict_request_rx) =
            tokio::sync::mpsc::unbounded_channel::<ConflictRequest>();
        let conflict_resolver = Arc::new(InteractiveConflictResolver::new(conflict_request_tx));

        // T-5.2.5: if the scan above found anything needing attention,
        // build the dialog right here -- a live `Window`/`Context<Self>`
        // are both already in scope (unlike `pending_notice`'s own
        // `push_notification` path, this constructs an `Entity` directly
        // rather than calling anything that needs `gpui-component`'s
        // `Root`, so there is no "no live Window yet" problem to work
        // around). `window.focused(cx)` is almost certainly `None` this
        // early in construction -- that is fine and expected, the same
        // way every other `close_*_dialog`'s `Option<FocusHandle>` already
        // tolerates a `None` previous focus.
        let recovery_dialog_previous_focus = window.focused(cx);
        let recovery_dialog = if recovery_reports.is_empty() {
            None
        } else {
            // `recovery_reports` is only ever non-empty when `state_dir`
            // resolved to `Some` -- `scan_startup_recovery_reports` only
            // ran against a real path in that case (see just above). This
            // documents an invariant, not a guess.
            let dir = state_dir
                .clone()
                .expect("a non-empty recovery scan implies state_dir resolved");
            let recovery_workspace = cx.entity().downgrade();
            let recovery_tokio_handle = tokio_handle.clone();
            let recovery_queue = queue.clone();
            let recovery_conflict_resolver = conflict_resolver.clone();
            Some(cx.new(|cx| {
                RecoveryDialogState::new(
                    recovery_reports,
                    recovery_workspace,
                    recovery_tokio_handle,
                    recovery_queue,
                    dir,
                    recovery_conflict_resolver,
                    window,
                    cx,
                )
            }))
        };

        // The conflict-request consumer loop: mirrors `queue_events_rx`'s
        // own loop below in every structural respect (a `cx.spawn` task
        // with no live `Window`, `this.update` per message, ends when
        // `Workspace` is dropped) but handles a request/response
        // exchange rather than a fire-and-forget event --
        // `Self::open_conflict_dialog_deferred` is the one place that
        // either opens the dialog immediately (setting
        // `pending_conflict_focus` for `Self::render` to drain) or, if
        // one is already open, queues this request in
        // `pending_conflict_requests` instead.
        cx.spawn(async move |this, cx| {
            while let Some(request) = conflict_request_rx.recv().await {
                let updated = this.update(cx, |this, cx| {
                    this.open_conflict_dialog_deferred(request, cx);
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();

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
                // Hand the executor back between events. `recv().await`
                // resolves immediately while events are queued, so without
                // this the loop above runs every queued event inside a
                // single poll -- harmless in the real app (a notify is
                // cheap; the platform draws once per frame), but GPUI's
                // test-mode `flush_effects` draws the whole window on
                // every `update`, and a progress sample lands every 100ms
                // while a job runs. On a machine where a debug-build draw
                // takes longer than that, the poll never ends, the test
                // executor never returns to the test, and anything
                // waiting on the UI (a conflict answer, say) waits
                // forever: `two_concurrent_conflicts_are_both_served_not_
                // dropped` hung this way on a 4-core CI runner and on this
                // workstation pinned to 2 cores (2026-09-04, thread dump).
                yield_once().await;
            }
        })
        .detach();

        Self {
            demo: DemoState::Loading,
            focus_handle: cx.focus_handle(),
            trash_mount_scan: MountScan::System,
            path_bar: None,
            path_bar_previous_focus: None,
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
            delete_dialog: None,
            delete_dialog_previous_focus: None,
            mkdir_dialog: None,
            mkdir_dialog_previous_focus: None,
            rename_dialog: None,
            rename_dialog_previous_focus: None,
            link_dialog: None,
            link_dialog_previous_focus: None,
            attributes_dialog: None,
            attributes_dialog_previous_focus: None,
            recovery_dialog,
            recovery_dialog_previous_focus,
            trash_dialog: None,
            trash_dialog_previous_focus: None,
            confirm_delete,
            delete_default_permanent,
            tokio_handle: tokio_handle.clone(),
            queue,
            state_dir,
            job_progress: HashMap::new(),
            operation_manager: None,
            operation_manager_previous_focus: None,
            job_report_dialog: None,
            job_report_dialog_previous_focus: None,
            conflict_dialog: None,
            conflict_dialog_previous_focus: None,
            pending_conflict_focus: None,
            pending_conflict_requests: std::collections::VecDeque::new(),
            conflict_resolver,
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

    /// Saves the Full-view column layout to `settings.toml` off the UI
    /// thread -- same best-effort contract as
    /// [`Self::persist_splitter_ratio`].
    fn persist_column_layout(&self, layout: duet_config::ColumnLayout, cx: &mut Context<Self>) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = save_column_layout(&path, crate::columns::FULL_VIEW, &layout) {
                    tracing::warn!(
                        target: "duet_ui::workspace",
                        "failed to persist column layout: {err}"
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
        // T-5.2.3: the live, interactive resolver replaces T-5.2.1's
        // original always-`None` placeholder -- every conflict this job
        // hits that neither a pre-resolved `Step` nor an already-
        // established "apply to all" answer already covers now gets a
        // real, live prompt instead of silently falling back to
        // `default_conflict`. `Arc<InteractiveConflictResolver>` ->
        // `Arc<dyn ConflictResolver>` is an unsized coercion, not a clone
        // of the resolver's own state -- every dialog this workspace
        // opens shares the exact same resolver instance (and therefore
        // the same request channel back to this workspace's own consumer
        // loop).
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            CopyMoveDialogState::new(
                kind,
                sources,
                initial_destination,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
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

    /// F8 (`DeleteDialog`) / Shift+F8 (`DeletePermanentDialog`), T-5.2.6:
    /// resolves what to delete from whichever panel has focus (the same
    /// `resolve_source_names` selection-or-cursor-fallback F5/F6 use), then
    /// applies `operations.confirm_delete`:
    ///
    /// - `"never"` -- no dialog at all; the job starts immediately.
    /// - `"non_empty_dirs"` -- a dialog only if at least one directory
    ///   target turns out to be non-empty; otherwise the job starts
    ///   immediately, exactly like `"never"`. Empty directories and plain
    ///   files therefore need no confirmation under this policy, which is
    ///   the whole point of it.
    /// - anything else (`"always"`, the shipped default) -- always a
    ///   dialog.
    ///
    /// **Judgment call, disclosed:** `"always"` *also* runs the non-empty
    /// check when there's at least one directory target, purely to
    /// populate the warning line. It doesn't need the answer to decide
    /// whether to show the dialog, but this task's own AC asks for a
    /// "non-empty directory warning," and gating that on the
    /// `"non_empty_dirs"` policy alone would leave it dead code under the
    /// default configuration. The cost is one `getdents64` per directory
    /// target (`delete_dialog::non_empty_directory_names` stops at the
    /// first entry), off the UI thread.
    ///
    /// Unlike [`Self::open_copy_move_dialog`], which is synchronous top to
    /// bottom, that check makes this method's tail asynchronous
    /// (`tokio_handle.spawn` + a `oneshot`, bridged back through
    /// `Window::spawn` so the continuation still has a live `Window` to
    /// take focus with -- the same `window.spawn`/`update_in` dance
    /// `crate::panel::Panel::apply_lock_state` documents). A no-op (with
    /// an explanatory toast) if there is nothing to delete, and silently
    /// if a dialog is already open.
    fn open_delete_dialog(
        &mut self,
        permanent_forced: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delete_dialog.is_some() {
            return;
        }

        let source_panel = match self.focused_panel_side(window, cx) {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };
        let source_table = source_panel.read(cx).active_table().clone();
        let current_dir = source_table.read(cx).current_dir().to_path_buf();
        let (names, dir_names) = {
            let table_state = source_table.read(cx).state().read(cx);
            let delegate = table_state.delegate();
            let names = crate::copy_move_dialog::resolve_source_names(delegate);
            // Every directory in the *already-loaded* model -- no I/O:
            // `EntryKind` is part of the listing this panel is already
            // showing, the same data `resolve_source_names` itself reads.
            let model = delegate.model();
            let dir_names: std::collections::HashSet<String> = model
                .ordered_names()
                .filter(|(id, _)| model.entries().kind(*id) == duet_types::EntryKind::Directory)
                .map(|(_, name)| name.to_string())
                .collect();
            (names, dir_names)
        };
        if names.is_empty() {
            window.push_notification(Notification::info("Nothing selected."), cx);
            return;
        }

        let mut targets: Vec<VPath> = Vec::with_capacity(names.len());
        let mut dir_targets: Vec<(String, VPath)> = Vec::new();
        for name in &names {
            let Ok(vpath) = crate::file_table::local_vpath(&current_dir.join(name)) else {
                continue;
            };
            if dir_names.contains(name) {
                dir_targets.push((name.clone(), vpath.clone()));
            }
            targets.push(vpath);
        }
        if targets.is_empty() {
            window.push_notification(
                Notification::warning("The selected item(s) don't have a valid path."),
                cx,
            );
            return;
        }

        let permanent = permanent_forced || self.delete_default_permanent;
        let policy = self.confirm_delete.clone();

        if policy == "never" {
            self.start_delete_job(targets, permanent, cx);
            return;
        }
        if dir_targets.is_empty() {
            if policy == "non_empty_dirs" {
                self.start_delete_job(targets, permanent, cx);
            } else {
                self.show_delete_dialog(
                    targets,
                    permanent,
                    permanent_forced,
                    Vec::new(),
                    window,
                    cx,
                );
            }
            return;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let found =
                crate::delete_dialog::non_empty_directory_names(fs.as_ref(), &dir_targets).await;
            let _ = tx.send(found);
        });
        let weak_workspace = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                // A dropped sender (the runtime shutting down mid-check)
                // degrades to "nothing found," which under `"always"` still
                // confirms and under `"non_empty_dirs"` still deletes --
                // the same outcome as a genuinely empty set, and never a
                // silently swallowed delete request.
                let non_empty = rx.await.unwrap_or_default();
                let _ = weak_workspace.update_in(cx, |this: &mut Workspace, window, cx| {
                    if non_empty.is_empty() && policy == "non_empty_dirs" {
                        this.start_delete_job(targets, permanent, cx);
                    } else {
                        this.show_delete_dialog(
                            targets,
                            permanent,
                            permanent_forced,
                            non_empty,
                            window,
                            cx,
                        );
                    }
                });
            })
            .detach();
    }

    /// Constructs, stores, and focuses the delete confirmation --
    /// [`Self::open_delete_dialog`]'s one "actually show it" path,
    /// reachable both synchronously (no directory targets to check) and
    /// from its spawned continuation. Captures the focus to restore here
    /// rather than in `open_delete_dialog` itself: the paths that never
    /// show a dialog never take focus away, so they have nothing to
    /// restore.
    fn show_delete_dialog(
        &mut self,
        targets: Vec<VPath>,
        permanent: bool,
        permanent_forced: bool,
        non_empty_dir_names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delete_dialog.is_some() {
            return;
        }
        self.delete_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let state = cx.new(|cx| {
            DeleteDialogState::new(
                targets,
                permanent,
                permanent_forced,
                non_empty_dir_names,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                window,
                cx,
            )
        });
        self.delete_dialog = Some(state);
        cx.notify();
    }

    /// The no-confirmation delete path (`confirm_delete = "never"`, and
    /// `"non_empty_dirs"` with nothing to warn about): the exact same
    /// off-UI-thread plan-and-enqueue `DeleteDialogState::confirm` runs,
    /// through the same shared [`crate::delete_dialog::spawn_delete_job`],
    /// just with no dialog to close afterward. Deliberately silent on
    /// success -- `CopyMoveDialogState` doesn't toast when a job starts
    /// either, and the status-bar tray (T-5.2.2) already shows a running
    /// job; a failure still surfaces as a toast.
    fn start_delete_job(&mut self, targets: Vec<VPath>, permanent: bool, cx: &mut Context<Self>) {
        let Some(state_dir) = self.state_dir.clone() else {
            self.push_pending_notice(
                NoticeLevel::Error,
                "Can't run the operation: no writable state directory found \
                 (is $HOME/$XDG_STATE_HOME set?).",
                cx,
            );
            return;
        };
        let rx = crate::delete_dialog::spawn_delete_job(
            &self.tokio_handle,
            targets,
            permanent,
            self.queue.clone(),
            state_dir,
        );
        let workspace = cx.entity().downgrade();
        cx.spawn(async move |_this, cx| {
            let outcome = rx.await;
            let _ = crate::dialog_job::report_job_outcome(outcome, &workspace, cx);
        })
        .detach();
    }

    /// Closes the delete confirmation (Escape, or a click outside it) and
    /// restores keyboard focus to whatever had it before it opened.
    /// Mirrors `close_copy_move_dialog` exactly -- always has a live
    /// `Window`.
    pub(crate) fn close_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_dialog = None;
        if let Some(handle) = self.delete_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// The one close path with no live `Window` --
    /// `DeleteDialogState::confirm`'s async plan/enqueue success callback.
    /// Mirrors `close_copy_move_dialog_deferred` exactly; see
    /// `pending_focus_restore`'s doc comment for why the actual
    /// `window.focus` call happens on `Self::render`'s next pass instead.
    pub(crate) fn close_delete_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.delete_dialog = None;
        self.pending_focus_restore = self.delete_dialog_previous_focus.take();
        cx.notify();
    }

    /// The focused panel's active tab's current directory -- the one
    /// question all three T-5.2.7 `open_*` methods below start from, and
    /// the piece `open_copy_move_dialog`/`open_delete_dialog` each spell
    /// out inline. Returns the `PanelSide` too, since the link dialog also
    /// needs the *other* side's directory for its default.
    fn focused_panel_dir(&self, window: &Window, cx: &App) -> (PanelSide, PathBuf) {
        let side = self.focused_panel_side(window, cx);
        let panel = match side {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        };
        let dir = panel
            .read(cx)
            .active_table()
            .read(cx)
            .current_dir()
            .to_path_buf();
        (side, dir)
    }

    /// The focused panel's active tab's *cursor* entry name, or `None` if
    /// there isn't one (an empty listing, or the cursor parked on the
    /// synthetic ".." row -- see `FileTableDelegate::cursor_entry_name`'s
    /// own doc comment). Shift+F6 and both link commands resolve their
    /// single target this way, deliberately *not* through
    /// `copy_move_dialog::resolve_source_names`' selection-or-cursor
    /// fallback: each of them acts on exactly one entry (see
    /// `crate::rename_dialog`/`crate::link_dialog`'s module doc comments).
    fn focused_cursor_entry_name(&self, window: &Window, cx: &App) -> Option<String> {
        let panel = match self.focused_panel_side(window, cx) {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        };
        panel.read(cx).active_table().read(cx).cursor_entry_name(cx)
    }

    /// F7 (`MkdirDialog`, T-5.2.7, `ops.mkdir`): opens the create-directory
    /// dialog pre-filled with the focused panel's current directory plus a
    /// trailing `/`, so the user types only the new segment(s) --
    /// `duet_ops::plan_mkdir` handles a nested `a/b/c` in one job, which is
    /// this task's own "as TC does" AC clause.
    ///
    /// Unlike every other `open_*` here there is no selection or cursor to
    /// resolve: F7 makes something new, so there is nothing to act *on*.
    /// A no-op, silently, if the dialog is already open (the same
    /// "reopening shouldn't stack a second one" convention every sibling
    /// overlay follows).
    fn open_mkdir_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mkdir_dialog.is_some() {
            return;
        }

        let (_side, dir) = self.focused_panel_dir(window, cx);
        let mut initial = dir.to_string_lossy().into_owned();
        if !initial.ends_with('/') {
            initial.push('/');
        }

        self.mkdir_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            MkdirDialogState::new(
                initial,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.mkdir_dialog = Some(state);
        cx.notify();
    }

    /// Mirrors `close_copy_move_dialog` exactly -- always has a live
    /// `Window`.
    pub(crate) fn close_mkdir_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mkdir_dialog = None;
        if let Some(handle) = self.mkdir_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Mirrors `close_copy_move_dialog_deferred` exactly; see
    /// `pending_focus_restore`'s doc comment for why the actual
    /// `window.focus` call happens on `Self::render`'s next pass instead.
    pub(crate) fn close_mkdir_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.mkdir_dialog = None;
        self.pending_focus_restore = self.mkdir_dialog_previous_focus.take();
        cx.notify();
    }

    /// Shift+F6 (`RenameDialog`, T-5.2.7, `ops.rename_in_place`): opens the
    /// in-place rename dialog on the focused panel's *cursor* entry. A
    /// no-op with an explanatory toast if there is no cursor entry to
    /// rename, and silently if the dialog is already open.
    fn open_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename_dialog.is_some() {
            return;
        }

        let (_side, dir) = self.focused_panel_dir(window, cx);
        let Some(name) = self.focused_cursor_entry_name(window, cx) else {
            window.push_notification(Notification::info("Nothing to rename."), cx);
            return;
        };
        let Ok(source) = crate::file_table::local_vpath(&dir.join(&name)) else {
            window.push_notification(
                Notification::warning("That entry doesn't have a valid path."),
                cx,
            );
            return;
        };

        self.rename_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            RenameDialogState::new(
                source,
                name,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.rename_dialog = Some(state);
        cx.notify();
    }

    pub(crate) fn close_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_dialog = None;
        if let Some(handle) = self.rename_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    pub(crate) fn close_rename_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.rename_dialog = None;
        self.pending_focus_restore = self.rename_dialog_previous_focus.take();
        cx.notify();
    }

    /// `Ctrl+Shift+S` (`SymlinkDialog`) / `Ctrl+Shift+H`
    /// (`HardlinkDialog`), T-5.2.7: opens the link-creation dialog on the
    /// focused panel's *cursor* entry, defaulting the new link's own path
    /// to the **other** panel's current directory plus the same basename --
    /// F5/F6's own "destination defaults to the other panel" convention
    /// (`open_copy_move_dialog`'s `dest_panel` resolution). A no-op with an
    /// explanatory toast if there is no cursor entry, and silently if the
    /// dialog is already open. See `crate::link_dialog`'s module doc
    /// comment for both the unverified-by-TC keybindings and the disclosed
    /// "one entry, not a selection" scope boundary.
    fn open_link_dialog(&mut self, kind: LinkKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.link_dialog.is_some() {
            return;
        }

        let (source_side, source_dir) = self.focused_panel_dir(window, cx);
        let Some(name) = self.focused_cursor_entry_name(window, cx) else {
            window.push_notification(Notification::info("Nothing to link to."), cx);
            return;
        };
        let Ok(source) = crate::file_table::local_vpath(&source_dir.join(&name)) else {
            window.push_notification(
                Notification::warning("That entry doesn't have a valid path."),
                cx,
            );
            return;
        };

        let dest_panel = match source_side {
            PanelSide::Left => self.right_panel.clone(),
            PanelSide::Right => self.left_panel.clone(),
        };
        let dest_dir = dest_panel.read(cx).active_table().read(cx).current_dir();
        let initial_link_path = dest_dir.join(&name).to_string_lossy().into_owned();

        self.link_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            LinkDialogState::new(
                kind,
                source,
                name,
                initial_link_path,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.link_dialog = Some(state);
        cx.notify();
    }

    pub(crate) fn close_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.link_dialog = None;
        if let Some(handle) = self.link_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    pub(crate) fn close_link_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.link_dialog = None;
        self.pending_focus_restore = self.link_dialog_previous_focus.take();
        cx.notify();
    }

    /// `Ctrl+A` (`AttributesDialog`), T-5.2.8: opens the attributes/
    /// permissions dialog on the focused panel's selection -- or the cursor
    /// row, if nothing is selected, via the same
    /// `copy_move_dialog::resolve_source_names` fallback F5/F6/F8 use and
    /// that `docs/commands.md`'s own `panel && selection.nonempty`
    /// precondition for `file.attributes` implies. Deliberately *not* the
    /// "exactly one entry" scope T-5.2.7's link dialogs chose: changing
    /// permissions across a multi-selection is an ordinary thing to want,
    /// and `duet_ops::plan_attributes` takes a `&[VPath]` for exactly that
    /// reason.
    ///
    /// A no-op with an explanatory toast if there is nothing to act on, and
    /// silently if the dialog is already open.
    ///
    /// # Why this one is asynchronous
    ///
    /// With exactly one target the dialog opens pre-filled with that
    /// entry's *current* mode and timestamps, which needs a real `stat`.
    /// `Metadata::mode` is not part of the panel's already-loaded listing
    /// (`FileTableSettings`' own `ListFields` don't request it), so unlike
    /// `open_delete_dialog`'s directory check this cannot be answered from
    /// memory. The `stat` therefore runs on the ops runtime and the dialog
    /// is constructed in the continuation -- the same "`tokio_handle.spawn`,
    /// a `oneshot`, then `window.spawn`/`update_in`" shape
    /// [`Self::open_delete_dialog`] already uses, so the continuation still
    /// has a live `Window` to take focus with.
    ///
    /// With more than one target there is nothing to pre-fill from (see
    /// `AttributesDialogState::new`), so that path skips the `stat` and
    /// shows the dialog synchronously.
    fn open_attributes_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.attributes_dialog.is_some() {
            return;
        }

        let source_panel = match self.focused_panel_side(window, cx) {
            PanelSide::Left => self.left_panel.clone(),
            PanelSide::Right => self.right_panel.clone(),
        };
        let source_table = source_panel.read(cx).active_table().clone();
        let current_dir = source_table.read(cx).current_dir().to_path_buf();
        let (names, dir_names) = {
            let table_state = source_table.read(cx).state().read(cx);
            let delegate = table_state.delegate();
            let names = crate::copy_move_dialog::resolve_source_names(delegate);
            // Directory-ness comes free out of the already-loaded model --
            // no I/O, exactly as `open_delete_dialog` reads it. Only used
            // to make the recursive hint line honest.
            let model = delegate.model();
            let dir_names: std::collections::HashSet<String> = model
                .ordered_names()
                .filter(|(id, _)| model.entries().kind(*id) == duet_types::EntryKind::Directory)
                .map(|(_, name)| name.to_string())
                .collect();
            (names, dir_names)
        };
        if names.is_empty() {
            window.push_notification(Notification::info("Nothing selected."), cx);
            return;
        }

        let mut targets: Vec<VPath> = Vec::with_capacity(names.len());
        let mut has_directory_target = false;
        for name in &names {
            let Ok(vpath) = crate::file_table::local_vpath(&current_dir.join(name)) else {
                continue;
            };
            has_directory_target |= dir_names.contains(name);
            targets.push(vpath);
        }
        if targets.is_empty() {
            window.push_notification(
                Notification::warning("The selected item(s) don't have a valid path."),
                cx,
            );
            return;
        }

        let single_name = (targets.len() == 1).then(|| names[0].clone());
        if targets.len() > 1 {
            self.show_attributes_dialog(
                targets,
                single_name,
                has_directory_target,
                None,
                window,
                cx,
            );
            return;
        }

        let target = targets[0].clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            // `follow_symlinks: false`: a cursor parked on a symlink should
            // show the link's *own* attributes, matching what the panel row
            // beside it already reports.
            let prefill = fs
                .stat(&target, false)
                .await
                .ok()
                .map(|meta| AttributesPrefill {
                    mode: meta.mode,
                    modified_secs: meta.modified.map(|t| t.secs),
                    accessed_secs: meta.accessed.map(|t| t.secs),
                });
            let _ = tx.send(prefill);
        });
        let weak_workspace = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                // A failed `stat` (or a dropped sender during runtime
                // shutdown) degrades to "no pre-fill" -- four blank fields,
                // every one of which means "leave unchanged". Never a
                // silently swallowed Ctrl+A.
                let prefill = rx.await.unwrap_or(None);
                let _ = weak_workspace.update_in(cx, |this: &mut Workspace, window, cx| {
                    this.show_attributes_dialog(
                        targets,
                        single_name,
                        has_directory_target,
                        prefill,
                        window,
                        cx,
                    );
                });
            })
            .detach();
    }

    /// Constructs, stores and focuses the attributes dialog --
    /// [`Self::open_attributes_dialog`]'s one "actually show it" path,
    /// reachable both synchronously (a multi-selection, nothing to
    /// pre-fill) and from its spawned `stat` continuation. Captures the
    /// focus to restore here rather than in `open_attributes_dialog`, for
    /// the same reason `show_delete_dialog` does: the paths that never show
    /// a dialog never take focus away, so they have nothing to restore.
    #[allow(clippy::too_many_arguments)]
    fn show_attributes_dialog(
        &mut self,
        targets: Vec<VPath>,
        single_name: Option<String>,
        has_directory_target: bool,
        prefill: Option<AttributesPrefill>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.attributes_dialog.is_some() {
            return;
        }
        self.attributes_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            AttributesDialogState::new(
                targets,
                single_name,
                has_directory_target,
                prefill,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.attributes_dialog = Some(state);
        cx.notify();
    }

    pub(crate) fn close_attributes_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attributes_dialog = None;
        if let Some(handle) = self.attributes_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    pub(crate) fn close_attributes_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.attributes_dialog = None;
        self.pending_focus_restore = self.attributes_dialog_previous_focus.take();
        cx.notify();
    }

    /// Closes T-5.2.5's recovery dialog -- Escape's path, and the one every
    /// resume/discard success path also uses directly (see `crate::
    /// recovery_dialog`'s module doc comment for why that dialog's own
    /// async continuations always have a live `&mut Window` by the time
    /// they'd call this, unlike every sibling dialog's `cx.spawn`-based
    /// continuations). There is deliberately no `close_recovery_dialog_
    /// deferred`: nothing in this dialog ever needs one.
    pub(crate) fn close_recovery_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recovery_dialog = None;
        if let Some(handle) = self.recovery_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// `trash.open_browser` (`Alt+T`): scans the real trash
    /// (`duet_ops::list_trash_entries`) off the UI thread and, once that
    /// succeeds, shows T-5.3.2 phase 2's browser dialog. See
    /// `crate::trash_dialog`'s module doc comment for why this is
    /// asynchronous (unlike `open_delete_dialog`'s synchronous directory
    /// check) and for why an empty scan still opens the dialog rather than
    /// silently declining to.
    ///
    /// A no-op if the dialog is already open, or if `$XDG_DATA_HOME` can't
    /// even be resolved (the same rare XDG-resolution failure every other
    /// path in this file tolerates) -- both surfaced the same way every
    /// sibling "can't even start" case is: a toast, not a panic.
    fn open_trash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trash_dialog.is_some() {
            return;
        }
        let xdg_data_home = match duet_config::paths::xdg_data_home() {
            Ok(dir) => dir,
            Err(err) => {
                window.push_notification(
                    Notification::error(format!("Can't locate the trash: {err}")),
                    cx,
                );
                return;
            }
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let mounts = self.trash_mount_scan.clone();
        self.tokio_handle.spawn(async move {
            let result = list_trash_entries_with_mounts(&xdg_data_home, &mounts);
            let _ = tx.send(result);
        });
        let weak_workspace = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let outcome = rx.await;
                let _ =
                    weak_workspace.update_in(
                        cx,
                        |this: &mut Workspace, window, cx| match outcome {
                            Ok(Ok(mut entries)) => {
                                entries.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
                                this.show_trash_dialog(entries, window, cx);
                            }
                            Ok(Err(err)) => {
                                this.push_pending_notice(
                                    NoticeLevel::Error,
                                    format!("Couldn't read the trash: {err}"),
                                    cx,
                                );
                            }
                            Err(_) => {
                                this.push_pending_notice(
                                    NoticeLevel::Error,
                                    "The trash scan task was dropped before completing."
                                        .to_string(),
                                    cx,
                                );
                            }
                        },
                    );
            })
            .detach();
    }

    /// Constructs, stores and focuses the trash dialog --
    /// [`Self::open_trash_dialog`]'s one "actually show it" path, reached
    /// only from that method's own scan continuation (which always has a
    /// live `Window` -- see `crate::trash_dialog`'s module doc comment).
    fn show_trash_dialog(
        &mut self,
        entries: Vec<TrashEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.trash_dialog.is_some() {
            return;
        }
        self.trash_dialog_previous_focus = window.focused(cx);
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            TrashDialogState::new(
                entries,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.trash_dialog = Some(state);
        cx.notify();
    }

    /// Closes T-5.3.2 phase 2's trash browser -- Escape's path. There is
    /// deliberately no `close_trash_dialog_deferred`; see
    /// `crate::trash_dialog`'s module doc comment for why nothing in that
    /// dialog ever needs one.
    pub(crate) fn close_trash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.trash_dialog = None;
        if let Some(handle) = self.trash_dialog_previous_focus.take() {
            window.focus(&handle);
        }
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

    /// T-4.3.4: one panel header, in whichever face applies -- the editing
    /// [`crate::path_bar::PathBarState`] if it is open on this side, else
    /// the breadcrumb whose segments navigate the panel and whose empty
    /// area (or pencil) opens the editor. See `crate::path_bar`.
    fn panel_header(
        &self,
        side: PanelSide,
        dir: &Path,
        tokens: &TokenPalette,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(bar) = &self.path_bar
            && bar.read(cx).side() == side
        {
            return bar.clone().into_any_element();
        }
        let weak = cx.entity().downgrade();
        let on_segment: crate::path_bar::SegmentHandler = Rc::new({
            let weak = weak.clone();
            move |target, window, cx| {
                let target = target.to_path_buf();
                let _ = weak.update(cx, |this, cx| {
                    this.navigate_panel_to_path(side, target, window, cx);
                });
            }
        });
        let on_edit: crate::path_bar::EditHandler = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.open_path_bar(side, window, cx));
        });
        crate::path_bar::breadcrumb_header(side, dir, tokens, on_segment, on_edit)
            .into_any_element()
    }

    /// `Ctrl+L` (`GotoPath`) or a click on the header: opens `side`'s path
    /// bar for editing, pre-filled with its current directory. Opening the
    /// other panel's bar while one is open moves the editor there (the
    /// original previous-focus is kept, so `Esc` still lands where the
    /// user started). A no-op if this side's bar is already open.
    pub(crate) fn open_path_bar(
        &mut self,
        side: PanelSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bar) = &self.path_bar
            && bar.read(cx).side() == side
        {
            return;
        }
        // Switching sides keeps the *original* previous focus, so `Esc`
        // still lands where the user started.
        let previous_focus = if self.path_bar.take().is_some() {
            self.path_bar_previous_focus.take()
        } else {
            window.focused(cx)
        };
        self.path_bar_previous_focus = previous_focus;
        let panel = match side {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        };
        let dir = panel
            .read(cx)
            .active_table()
            .read(cx)
            .current_dir()
            .to_path_buf();
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let bar = cx.new(|cx| {
            crate::path_bar::PathBarState::new(side, dir, workspace, tokio_handle, window, cx)
        });
        self.path_bar = Some(bar);
        cx.notify();
    }

    /// Closes the path bar (Enter, Esc, or a completed navigation) and
    /// hands focus back to whatever had it before it opened.
    pub(crate) fn close_path_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_bar = None;
        if let Some(handle) = self.path_bar_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Navigates `side`'s active tab to `dir` -- the same in-place
    /// navigation (history entry, locked-tab redirect, quick-search
    /// invalidation) any other "go to this directory" gesture uses.
    pub(crate) fn navigate_panel_to_path(
        &mut self,
        side: PanelSide,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = match side {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        };
        let table = panel.read(cx).active_table().clone();
        table.update(cx, |table, cx| table.navigate_to_path(dir, window, cx));
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

    /// T-5.2.4 (`ops.queue.show_errors`): opens the error/skip report for
    /// an already-finished `job` -- this task's own AC, "a job with 50
    /// permission errors ends with an actionable list, not 50 dialogs."
    /// Called only from `crate::operation_manager`'s Enter/`O` handler,
    /// which has already established that `job` is `Terminal` and that its
    /// report has something in it.
    ///
    /// **Opening the report closes the operation manager underneath it**
    /// -- a "drill down, replacing the view" feel rather than a stacked
    /// one. See `crate::job_report_dialog`'s module doc comment for the
    /// full disclosed reasoning (two stacked backdrops darken the
    /// workspace twice over, and the manager's window-wide
    /// `.on_mouse_down_out` would close it on the first click into this
    /// dialog anyway, stranding a saved focus handle). The manager's own
    /// saved previous focus is *inherited* rather than dropped, so Escape
    /// here lands back on the panel Ctrl+O was pressed in.
    ///
    /// A no-op, silently, if a report is already open -- the same
    /// "reopening shouldn't stack a second one" convention every sibling
    /// overlay follows.
    pub(crate) fn open_job_report_dialog(
        &mut self,
        job: duet_ops::Job,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.job_report_dialog.is_some() {
            return;
        }
        // Fully qualified rather than imported at the top of this file:
        // `workspace.rs`'s non-test code inspects a `JobState` in exactly
        // this one place (everywhere else it goes through
        // `crate::operation_manager`), and a bare `use` would be a
        // near-unused import -- see the test module's own note on the
        // same type.
        let duet_ops::JobState::Terminal { report, .. } = job.state else {
            return;
        };

        self.job_report_dialog_previous_focus = self
            .operation_manager_previous_focus
            .take()
            .or_else(|| window.focused(cx));
        self.operation_manager = None;

        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let queue = self.queue.clone();
        let state_dir = self.state_dir.clone();
        let conflict_resolver: Arc<dyn ConflictResolver> = self.conflict_resolver.clone();
        let state = cx.new(|cx| {
            JobReportDialogState::new(
                job.kind,
                job.plan,
                report,
                workspace,
                tokio_handle,
                queue,
                state_dir,
                conflict_resolver,
                window,
                cx,
            )
        });
        self.job_report_dialog = Some(state);
        cx.notify();
    }

    /// Mirrors `close_delete_dialog` exactly -- always has a live
    /// `Window`.
    pub(crate) fn close_job_report_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.job_report_dialog = None;
        if let Some(handle) = self.job_report_dialog_previous_focus.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    /// The one close path with no live `Window` --
    /// `JobReportDialogState::rerun`'s async enqueue success callback.
    /// Mirrors `close_delete_dialog_deferred` exactly; see
    /// `pending_focus_restore`'s doc comment for why the actual
    /// `window.focus` call happens on `Self::render`'s next pass instead.
    pub(crate) fn close_job_report_dialog_deferred(&mut self, cx: &mut Context<Self>) {
        self.job_report_dialog = None;
        self.pending_focus_restore = self.job_report_dialog_previous_focus.take();
        cx.notify();
    }

    /// Constructs a fresh [`ConflictDialogState`] entity for `request` and
    /// stores it as `self.conflict_dialog` -- the one piece of
    /// construction logic [`Self::open_conflict_dialog_deferred`] (no live
    /// `Window`) and [`Self::close_conflict_dialog`] (advancing to the
    /// next queued conflict, which *does* have one) both need. Doesn't
    /// touch focus itself -- callers differ on whether they can move focus
    /// immediately (a live `Window`) or must defer it
    /// (`pending_conflict_focus`), so that's left to them.
    fn spawn_conflict_dialog_entity(
        &mut self,
        request: ConflictRequest,
        cx: &mut Context<Self>,
    ) -> Entity<ConflictDialogState> {
        let workspace = cx.entity().downgrade();
        let tokio_handle = self.tokio_handle.clone();
        let state = cx.new(|cx| {
            ConflictDialogState::new(
                request.prompt,
                request.response_tx,
                workspace,
                tokio_handle,
                cx,
            )
        });
        self.conflict_dialog = Some(state.clone());
        state
    }

    /// T-5.2.3: the sole handler for the conflict-request consumer loop
    /// (`Self::new`) -- called with no live `Window` (see that field's own
    /// doc comment on `pending_conflict_focus`). Opens `request`
    /// immediately if no conflict dialog is already showing (deferring
    /// the actual `window.focus` call to `Self::render`'s next pass via
    /// `pending_conflict_focus`), or queues it in
    /// `pending_conflict_requests` otherwise -- see that field's own doc
    /// comment for why a second, concurrent conflict can't simply be
    /// dropped.
    fn open_conflict_dialog_deferred(&mut self, request: ConflictRequest, cx: &mut Context<Self>) {
        if self.conflict_dialog.is_some() {
            self.pending_conflict_requests.push_back(request);
            cx.notify();
            return;
        }
        let state = self.spawn_conflict_dialog_entity(request, cx);
        let handle = state.read(cx).focus_handle(cx);
        self.pending_conflict_focus = Some(handle);
        cx.notify();
    }

    /// Closes the conflict dialog (called from [`ConflictDialogState`]
    /// itself, once it has sent its one answer -- always has a live
    /// `Window`, unlike the open path above). If another conflict was
    /// already waiting (`pending_conflict_requests`), immediately opens
    /// and focuses it -- no need to defer via `pending_conflict_focus`
    /// here, since this call site already has a `Window`. Otherwise
    /// restores focus to whatever had it before the *first* conflict
    /// dialog in this burst took over (`conflict_dialog_previous_focus`,
    /// captured by `Self::render`'s `pending_conflict_focus` drain -- see
    /// that field's own doc comment for why capture happens there rather
    /// than at open time).
    pub(crate) fn close_conflict_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conflict_dialog = None;
        if let Some(request) = self.pending_conflict_requests.pop_front() {
            let state = self.spawn_conflict_dialog_entity(request, cx);
            let handle = state.read(cx).focus_handle(cx);
            window.focus(&handle);
        } else if let Some(handle) = self.conflict_dialog_previous_focus.take() {
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
        let (left_dir, left_footer, left_focused) =
            panel_header_footer_active(&self.left_panel, window, cx);
        let (right_dir, right_footer, right_focused) =
            panel_header_footer_active(&self.right_panel, window, cx);
        // T-4.3.4: while a panel's path bar is being edited, keyboard focus
        // is in the bar's input, not the table -- the panel is still the
        // active one as far as its chrome is concerned.
        let editing = self.path_bar.as_ref().map(|bar| bar.read(cx).side());
        let left_active = left_focused || editing == Some(PanelSide::Left);
        let right_active = right_focused || editing == Some(PanelSide::Right);
        let left_header = self.panel_header(PanelSide::Left, &left_dir, tokens, cx);
        let right_header = self.panel_header(PanelSide::Right, &right_dir, tokens, cx);

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

        // T-5.2.3: the conflict dialog's own no-live-`Window`-at-open-time
        // problem -- see `pending_conflict_focus`'s own doc comment. The
        // capture of `conflict_dialog_previous_focus` happens *here*,
        // right before focus actually moves, rather than at the original
        // (`Window`-less) open call -- this is the first point with both
        // a live `Window` and the guarantee that focus hasn't shifted yet.
        // Guarded by `is_none()` so a burst of several queued conflicts
        // (each handled via `Self::close_conflict_dialog`'s own,
        // `Window`-having path, not this one) never overwrites the
        // *original* previous focus with a conflict dialog's own handle.
        if let Some(handle) = self.pending_conflict_focus.take() {
            if self.conflict_dialog_previous_focus.is_none() {
                self.conflict_dialog_previous_focus = window.focused(cx);
            }
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
            .on_action(cx.listener(|this, _: &DeleteDialog, window, cx| {
                this.open_delete_dialog(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeletePermanentDialog, window, cx| {
                this.open_delete_dialog(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MkdirDialog, window, cx| {
                this.open_mkdir_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RenameDialog, window, cx| {
                this.open_rename_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SymlinkDialog, window, cx| {
                this.open_link_dialog(LinkKind::Symlink, window, cx);
            }))
            .on_action(cx.listener(|this, _: &HardlinkDialog, window, cx| {
                this.open_link_dialog(LinkKind::Hardlink, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AttributesDialog, window, cx| {
                this.open_attributes_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenOperationManager, window, cx| {
                this.open_operation_manager(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenTrashDialog, window, cx| {
                this.open_trash_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GotoPath, window, cx| {
                let side = this.focused_panel_side(window, cx);
                this.open_path_bar(side, window, cx);
            }))
            // Client-side window chrome: on Linux, GPUI defaults every
            // window to *client* decorations unless the compositor
            // explicitly negotiates server-side ones (most Wayland
            // compositors don't) -- without this, there is no drag-to-
            // move, minimize, or maximize at all, and the window can only
            // be resized via `Root`'s own already-decoration-aware
            // `window_border()` edge hit-testing (`duet_widgets::layout`'s
            // own doc comment has the full story). `TitleBar::new()`'s own
            // `WindowControls` no-ops into native traffic lights on macOS
            // and is Linux-only in practice for this project.
            // UAT regression: this root's own `.track_focus(..)` above makes
            // GPUI register an automatic "focus me on mouse-down" listener
            // for the whole root, and the titlebar is a plain child of it
            // with no focus story of its own -- so every click on the drag
            // region (a window move, or the double-click that maximizes on
            // Linux) bubbled up and handed keyboard focus to the workspace
            // root, silently deactivating whichever panel had it. GPUI's
            // documented opt-out is `prevent_default()` from a mouse-down
            // listener that runs before the root's (bubble phase runs
            // innermost-first, so this wrapper's listener does). It only
            // suppresses that focus transfer: `TitleBar`'s own listeners
            // (drag-to-move, double-click-to-maximize) and its control
            // icons' `on_click` handlers are explicit listeners, unaffected.
            .child(
                gpui::div()
                    .on_mouse_down(gpui::MouseButton::Left, |_event, window, _cx| {
                        window.prevent_default();
                    })
                    .child(TitleBar::new().child(gpui::div().px_2().child("Duet"))),
            )
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
            .when_some(self.delete_dialog.clone(), |this, state| {
                this.child(delete_dialog_overlay(&state, cx))
            })
            .when_some(self.mkdir_dialog.clone(), |this, state| {
                this.child(mkdir_dialog_overlay(&state, cx))
            })
            .when_some(self.rename_dialog.clone(), |this, state| {
                this.child(rename_dialog_overlay(&state, cx))
            })
            .when_some(self.link_dialog.clone(), |this, state| {
                this.child(link_dialog_overlay(&state, cx))
            })
            .when_some(self.attributes_dialog.clone(), |this, state| {
                this.child(attributes_dialog_overlay(&state, cx))
            })
            .when_some(self.operation_manager.clone(), |this, state| {
                this.child(operation_manager_overlay(&state, cx))
            })
            .when_some(self.job_report_dialog.clone(), |this, state| {
                this.child(job_report_dialog_overlay(&state, cx))
            })
            .when_some(self.conflict_dialog.clone(), |this, state| {
                this.child(conflict_dialog_overlay(&state, cx))
            })
            .when_some(self.recovery_dialog.clone(), |this, state| {
                this.child(recovery_dialog_overlay(&state, cx))
            })
            .when_some(self.trash_dialog.clone(), |this, state| {
                this.child(trash_dialog_overlay(&state, cx))
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

/// T-5.2.6's delete confirmation overlay -- same `.occlude()`-backdrop/card
/// shape as `copy_move_dialog_overlay` (see `command_palette_overlay`'s own
/// doc comment for the full reasoning, including the real regression this
/// pattern exists to avoid), including its `.on_mouse_down_out` close: an
/// outside click is a safe "never mind" here, since nothing has been
/// enqueued yet (unlike `conflict_dialog_overlay`, which deliberately omits
/// it because a blocked executor thread is waiting on an answer).
/// Narrower than the copy/move dialog's own card (`420px` vs `480px`):
/// there's no destination path field to fit, only a title, a mode line, and
/// at most one warning. The card sets no `key_context` of its own, same
/// reasoning as every sibling overlay function: `DeleteDialogState::render`
/// already sets `"DeleteDialog"` on its own root.
fn delete_dialog_overlay(
    state: &Entity<DeleteDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("delete-dialog-backdrop")
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
                .id("delete-dialog-card")
                .occlude()
                .w(px(420.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_delete_dialog(window, cx);
                })),
        )
}

/// T-5.2.7's F7 create-directory overlay -- same `.occlude()`-backdrop/card
/// shape and same `.on_mouse_down_out` close as `copy_move_dialog_overlay`
/// (see `command_palette_overlay`'s own doc comment for the full reasoning,
/// including the real regression this pattern exists to avoid). Nothing is
/// enqueued until Enter, so an outside click is always a safe "never mind"
/// here -- unlike `conflict_dialog_overlay`, which deliberately omits it.
/// Same `480px` width as the copy/move dialog: both are "one path field
/// plus a hint line". The card sets no `key_context` of its own, same
/// reasoning as every sibling overlay function.
fn mkdir_dialog_overlay(
    state: &Entity<MkdirDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("mkdir-dialog-backdrop")
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
                .id("mkdir-dialog-card")
                .occlude()
                .w(px(480.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_mkdir_dialog(window, cx);
                })),
        )
}

/// T-5.2.7's Shift+F6 rename overlay -- identical chrome to
/// [`mkdir_dialog_overlay`]; see that function's doc comment.
fn rename_dialog_overlay(
    state: &Entity<RenameDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("rename-dialog-backdrop")
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
                .id("rename-dialog-card")
                .occlude()
                .w(px(480.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_rename_dialog(window, cx);
                })),
        )
}

/// T-5.2.7's symlink/hardlink overlay (one function for both, same as the
/// one `LinkDialogState` type behind it) -- identical chrome to
/// [`mkdir_dialog_overlay`]; see that function's doc comment.
fn link_dialog_overlay(
    state: &Entity<LinkDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("link-dialog-backdrop")
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
                .id("link-dialog-card")
                .occlude()
                .w(px(480.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_link_dialog(window, cx);
                })),
        )
}

/// T-5.2.8's Ctrl+A attributes overlay -- same chrome as
/// [`mkdir_dialog_overlay`] (see that function's doc comment), just a wider
/// card: this dialog has four labelled fields where every sibling has one
/// unlabelled one, and a `140px` label column plus a usable input needs
/// more than `480px` to avoid squeezing `YYYY-MM-DD HH:MM` down to nothing.
fn attributes_dialog_overlay(
    state: &Entity<AttributesDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("attributes-dialog-backdrop")
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
                .id("attributes-dialog-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_attributes_dialog(window, cx);
                })),
        )
}

/// T-5.2.5's startup recovery overlay -- same `.occlude()`-backdrop/card
/// chrome as [`attributes_dialog_overlay`] (see `command_palette_overlay`'s
/// own doc comment for the full reasoning). A click on the backdrop closes
/// the dialog exactly like Escape does -- see `crate::recovery_dialog`'s
/// module doc comment for why that is a deliberate, disclosed "resolve
/// nothing, it reappears next launch" choice rather than a gap.
fn recovery_dialog_overlay(
    state: &Entity<RecoveryDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("recovery-dialog-backdrop")
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
                .id("recovery-dialog-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_recovery_dialog(window, cx);
                })),
        )
}

/// T-5.3.2 phase 2's trash browser overlay -- same `.occlude()`-backdrop/
/// card chrome as [`recovery_dialog_overlay`] (see `command_palette_overlay`'s
/// own doc comment for the full reasoning). A click on the backdrop closes
/// the dialog exactly like Escape does; unlike escaping the recovery
/// dialog, there is nothing this ever "resolves nothing" about -- see
/// `crate::trash_dialog`'s module doc comment.
fn trash_dialog_overlay(
    state: &Entity<TrashDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("trash-dialog-backdrop")
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
                .id("trash-dialog-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_trash_dialog(window, cx);
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

/// T-5.2.4's error/skip report overlay -- same `.occlude()`-backdrop/card
/// shape and same `.on_mouse_down_out` close as `operation_manager_overlay`
/// (see `command_palette_overlay`'s own doc comment for the full
/// reasoning, including the real regression this pattern exists to
/// avoid): an outside click is a safe "never mind" here, since nothing is
/// enqueued until R/Enter. Same `560px` width as the operation manager it
/// drills in from -- a path plus a classified error message is at least as
/// wide as a job row, and the two views read as one flow when they don't
/// jump size. The card sets no `key_context` of its own, same reasoning as
/// every sibling overlay function: `JobReportDialogState::render` already
/// sets `"JobReport"` on its own root.
fn job_report_dialog_overlay(
    state: &Entity<JobReportDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("job-report-dialog-backdrop")
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
                .id("job-report-dialog-card")
                .occlude()
                .w(px(560.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone())
                .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
                    this.close_job_report_dialog(window, cx);
                })),
        )
}

/// T-5.2.3's conflict dialog overlay -- same `.occlude()`-backdrop/card
/// shape as every other overlay in this crate (see
/// `command_palette_overlay`'s own doc comment for the real regression
/// this pattern exists to avoid), with one deliberate difference: **no
/// `.on_mouse_down_out` close handler.** Every other overlay treats an
/// outside click as "never mind, dismiss this" -- but a conflict dialog
/// has no "never mind": the executor thread behind it is genuinely
/// blocked (`InteractiveConflictResolver::resolve`'s `block_in_place`)
/// waiting for an answer, and there is no non-answer that unblocks it.
/// Closing without picking a policy isn't a safe no-op the way it is for
/// every other overlay, so the option isn't offered -- the backdrop's own
/// `.occlude()` still stops a stray click from falling through to the
/// panel underneath (`command_palette_overlay`'s own doc comment explains
/// why that alone matters), it just doesn't also close anything. Wider
/// than the copy/move dialog's own card (`600px` vs `480px`): a
/// side-by-side source/destination metadata block needs more horizontal
/// room than a single destination path. The card sets no `key_context` of
/// its own here, same reasoning as every sibling overlay function:
/// `ConflictDialogState::render` already sets `"ConflictDialog"` on its
/// own root.
fn conflict_dialog_overlay(
    state: &Entity<ConflictDialogState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let tokens = TokenPalette::current(cx);
    gpui::div()
        .id("conflict-dialog-backdrop")
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
                .id("conflict-dialog-card")
                .occlude()
                .w(px(600.))
                .bg(tokens.color.panel_bg_active)
                .border_1()
                .border_color(tokens.color.border_focus)
                .rounded_md()
                .child(state.clone()),
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
    header: gpui::AnyElement,
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
                .child(header),
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
) -> (PathBuf, SharedString, bool) {
    let panel = panel.read(cx);
    let table = panel.active_table().read(cx);
    let active = table.focus_handle(cx).is_focused(window);
    let header = table.current_dir().to_path_buf();
    let footer = panel_footer_text(table, cx);
    (header, footer, active)
}

/// Wraps a real [`Panel`] (T-4.3.2 -- both sides, now that the right panel
/// is no longer a placeholder) in [`panel_chrome`]. One function for both
/// sides: nothing here is left/right-specific, only which `Entity<Panel>`
/// and pre-computed header/footer/active values the caller passes in.
fn panel_view(
    panel: &Entity<Panel>,
    header: gpui::AnyElement,
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
            header,
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

/// T-5.2.5's startup recovery scan (FR-OPS-07): every [`RecoveryReport`]
/// `JournalReader::scan` finds under `dir` that still "needs attention" --
/// `!incomplete_steps.is_empty() || !orphaned_partials.is_empty()`,
/// deliberately **not** filtered on `last_outcome`. A journal whose last
/// record is `JobFinished { outcome: Completed, .. }` can still carry a
/// real orphaned partial (`JournalReader::scan`'s own doc comment: a crash
/// landing between that `Completion`/`JobFinished` pair and the last
/// partial's rename is exactly this case) -- filtering on `last_outcome`
/// instead would silently hide that leak, which is precisely the gap
/// `JournalReader::scan`'s own module doc comment says this method exists
/// to rule out.
///
/// Same "no file/dir yet is silent, a real read/parse failure becomes a
/// notice" split as [`load_session_with_notice`]: a `state_dir` with no
/// `jobs/` directory at all (nothing has ever run) is the ordinary case
/// and returns an empty list with no notice; a `state_dir` that exists but
/// whose scan genuinely failed (a corrupt journal -- a parse failure
/// anywhere but a torn trailing record, which `JournalReader::scan` itself
/// already tolerates) is a real "we couldn't check for interrupted
/// operations" event, meant for `Workspace::pending_notice`. A pure
/// wrapper around `JournalReader::scan` (aside from the filter) so this
/// branching is unit-testable without a real `Window`/`Workspace`, same
/// reasoning as [`load_session_with_notice`]/[`resolve_panel_session`].
fn scan_startup_recovery_reports(dir: &Path) -> (Vec<RecoveryReport>, Option<String>) {
    match JournalReader::scan(dir) {
        Ok(reports) => {
            let needing_attention = reports
                .into_iter()
                .filter(|r| !r.incomplete_steps.is_empty() || !r.orphaned_partials.is_empty())
                .collect();
            (needing_attention, None)
        }
        Err(err) => {
            tracing::warn!(
                target: "duet_ui::workspace",
                "startup recovery scan failed ({dir:?}): {err}"
            );
            (
                Vec::new(),
                Some(format!("Couldn't check for interrupted operations: {err}")),
            )
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

/// Reads `[panels.layouts.full]` (T-4.2.4) from `settings.toml` at
/// `path`, same fallback tolerance as [`load_splitter_ratio`]; a section
/// that is absent or unusable (see `ColumnLayout::from_config`) yields
/// the built-in default layout.
fn load_column_layout(path: &std::path::Path) -> ColumnLayout {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| {
            settings
                .panels
                .layouts
                .get(crate::columns::FULL_VIEW)
                .and_then(ColumnLayout::from_config)
                .unwrap_or_default()
        })
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using default column layout (settings.toml not loaded yet: {err})"
            );
            ColumnLayout::default()
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

/// Reads `operations.confirm_delete` (T-5.2.6, FR-OPS-01) from
/// `settings.toml` at `path`, same fallback tolerance as
/// [`load_mouse_mode`]. Returned as the raw string; an unrecognized value
/// is *not* rejected here -- `Workspace::open_delete_dialog`'s own `match`
/// treats anything that isn't `"never"`/`"non_empty_dirs"` as `"always"`,
/// the safest of the three (confirm rather than silently delete).
fn load_confirm_delete_policy(path: &std::path::Path) -> String {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| settings.operations.confirm_delete)
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using the default delete-confirmation policy (settings.toml not loaded yet: {err})"
            );
            duet_config::Settings::default().operations.confirm_delete
        })
}

/// Reads `operations.delete_default` (T-5.2.6, FR-OPS-01/FR-CFG-07) from
/// `settings.toml` at `path` and folds in `trash.enabled`, same fallback
/// tolerance as [`load_mouse_mode`]. `docs/config-schema.md` documents that
/// "when `trash.enabled == false`, `delete_default` cannot be `trash`" --
/// honored defensively here (that combination degrades to permanent)
/// rather than treated as a config error: a delete must still work when
/// someone hand-edits the two keys into disagreement.
fn load_delete_default_permanent(path: &std::path::Path) -> bool {
    duet_config::settings::load(path)
        .and_then(|file| file.typed())
        .map(|settings| {
            settings.operations.delete_default == "permanent" || !settings.trash.enabled
        })
        .unwrap_or_else(|err| {
            tracing::info!(
                target: "duet_ui::workspace",
                "using the default delete target (settings.toml not loaded yet: {err})"
            );
            let defaults = duet_config::Settings::default();
            defaults.operations.delete_default == "permanent" || !defaults.trash.enabled
        })
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

/// The column-layout counterpart of [`save_splitter_ratio`]: writes
/// `[panels.layouts.<view>]`, creating the file if needed and leaving
/// every other key alone.
fn save_column_layout(
    path: &std::path::Path,
    view: &str,
    layout: &duet_config::ColumnLayout,
) -> duet_config::Result<()> {
    let mut file = match duet_config::settings::load(path) {
        Ok(file) => file,
        Err(_) => duet_config::SettingsFile::from_str(
            path,
            "schema_version = 1\n",
            &duet_config::MigrationRegistry::settings(),
            duet_config::settings::SETTINGS_SCHEMA_VERSION,
        )?,
    };
    duet_config::settings::set_column_layout(&mut file, view, layout);
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
        OperationManagerCursorDown, OperationManagerCursorUp, OperationManagerOpenReport,
        OperationManagerPauseSelected, OperationManagerResumeSelected,
    };
    // T-5.2.4's own overlay-internal actions -- same reasoning as the
    // `operation_manager` import right above: declared in
    // `crate::job_report_dialog`, not here.
    use crate::job_report_dialog::{CloseJobReport, RerunFailedItems};
    use duet_widgets::list::ListDelegate as _;
    use duet_widgets::table::TableDelegate as _;
    use gpui::{TestAppContext, VisualTestContext};

    // T-5.2.3's own overlay-internal actions -- same reasoning as the
    // `operation_manager` import right above: declared in
    // `crate::conflict_dialog`, not `workspace.rs`, so they need naming
    // explicitly (unlike `OpenOperationManager`-style actions declared
    // right in this file).
    use crate::conflict_dialog::{
        ConflictAbortAll, ConflictAbortThis, ConflictAutoRenameAll, ConflictAutoRenameThis,
        ConflictFocusNext, ConflictFocusPrev, ConflictHashDest, ConflictHashSource,
        ConflictOverwriteAll, ConflictOverwriteIfDifferentSizeAll,
        ConflictOverwriteIfDifferentSizeThis, ConflictOverwriteIfOlderAll,
        ConflictOverwriteIfOlderThis, ConflictOverwriteThis, ConflictRename, ConflictSkipAll,
        ConflictSkipThis,
    };
    // T-5.2.6's own overlay-internal actions -- same reasoning as the two
    // imports above: declared in `crate::delete_dialog`, not here.
    use crate::delete_dialog::{CancelDelete, ConfirmDelete, ToggleTrashPermanent};
    // T-5.2.8's own overlay-internal action -- same reasoning as the three
    // imports above: declared in `crate::attributes_dialog`, not here.
    use crate::attributes_dialog::ToggleRecursiveApply;
    // T-5.2.5's own overlay-internal actions -- same reasoning as the four
    // imports above: declared in `crate::recovery_dialog`, not here.
    use crate::recovery_dialog::{
        CloseRecoveryDialog, RecoveryDialogCursorDown, RecoveryDialogCursorUp,
        RecoveryDialogDiscard, RecoveryDialogResume, RecoveryDialogToggleInspect,
    };
    // T-5.3.2 phase 2's own overlay-internal actions -- same reasoning as
    // the imports above: declared in `crate::trash_dialog`, not here.
    use crate::trash_dialog::{
        TrashDialogCursorDown, TrashDialogDeleteSelected, TrashDialogEmpty, TrashDialogRestore,
        TrashDialogToggleMark,
    };
    use duet_ops::{
        ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictScope, Journal, JournalRecord,
        StepOutcome,
    };
    use duet_types::{EntryKind, Metadata, Timestamp};
    use duet_widgets::input::IndentInline;

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
        with_configured_workspace(cx, None, |workspace, vcx, _data_dir| f(workspace, vcx));
    }

    /// [`with_workspace`] plus two things only T-5.2.6's own tests need:
    /// a `settings.toml` written into the temporary `$XDG_CONFIG_HOME`
    /// *before* `Workspace::new` reads it (the delete policy/default are
    /// loaded exactly once, at construction), and the temporary
    /// `$XDG_DATA_HOME` handed to the closure so a trash-mode test can
    /// assert against `duet_config::paths::trash_files_dir()`'s real
    /// result without ever touching the machine's own
    /// `~/.local/share/Trash`. `$XDG_DATA_HOME` is redirected for *every*
    /// test through this helper, not just the trash ones, for the same
    /// reason `$XDG_CONFIG_HOME`/`$XDG_STATE_HOME` already are: a test
    /// must not be able to write into the real user's data directory even
    /// by accident.
    fn with_configured_workspace(
        cx: &mut TestAppContext,
        settings_toml: Option<&str>,
        f: impl FnOnce(Entity<Workspace>, &mut VisualTestContext, &Path),
    ) {
        with_configured_workspace_inner(cx, settings_toml, |_duet_state_dir| {}, f);
    }

    /// [`with_configured_workspace`] plus one thing only T-5.2.5's own
    /// recovery tests need: a `pre_construct` hook that runs after
    /// `$XDG_STATE_HOME` is redirected but *before* `Workspace::new`'s own
    /// startup `JournalReader::scan` runs against it -- the one thing no
    /// existing helper offers, since every other test only ever needs to
    /// touch the redirected directories *after* construction (`with_
    /// configured_workspace`'s own `&Path` parameter to `f`). `pre_construct`
    /// receives `duet_config::paths::duet_state_dir()`'s own real result
    /// (not the bare `$XDG_STATE_HOME` tempdir root -- i.e. exactly the
    /// path `Journal::open`/`JournalReader::scan` themselves use), so a
    /// test can write real journal files with `duet_ops::Journal` directly
    /// and have `Workspace::new`'s scan see exactly what it wrote.
    ///
    /// A sibling function rather than a new parameter threaded through
    /// [`with_configured_workspace`] itself: every one of that function's
    /// thirteen existing call sites would otherwise need touching just to
    /// pass a no-op closure, for a hook only this one task's tests need.
    fn with_configured_workspace_and_recovery_seed(
        cx: &mut TestAppContext,
        pre_construct: impl FnOnce(&Path),
        f: impl FnOnce(Entity<Workspace>, &mut VisualTestContext, &Path),
    ) {
        with_configured_workspace_inner(cx, None, pre_construct, f);
    }

    fn with_configured_workspace_inner(
        cx: &mut TestAppContext,
        settings_toml: Option<&str>,
        pre_construct: impl FnOnce(&Path),
        f: impl FnOnce(Entity<Workspace>, &mut VisualTestContext, &Path),
    ) {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config_dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let prev_config = std::env::var_os("XDG_CONFIG_HOME");
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let prev_data = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: serialized by ENV_LOCK above; no other thread in this
        // test binary reads these specific vars concurrently.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
            std::env::set_var("XDG_STATE_HOME", state_dir.path());
            std::env::set_var("XDG_DATA_HOME", data_dir.path());
        }
        if let Some(body) = settings_toml {
            let dir = config_dir.path().join("duet");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.toml"), body).unwrap();
        }
        // Runs after the redirect above but before `Workspace::new`'s own
        // startup recovery scan -- see this function's own doc comment.
        // `duet_state_dir()` cannot fail here: `$XDG_STATE_HOME` was just
        // set, unconditionally, two lines above.
        pre_construct(&duet_config::paths::duet_state_dir().unwrap());

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
            bind_delete_dialog_keys(cx);
            bind_operation_manager_keys(cx);
            bind_job_report_dialog_keys(cx);
            bind_conflict_dialog_keys(cx);
            bind_attributes_dialog_keys(cx);
            bind_recovery_dialog_keys(cx);
            bind_trash_dialog_keys(cx);
        });

        let mut workspace_cell: Option<Entity<Workspace>> = None;
        let (_root, vcx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, tokio_handle.clone()));
            workspace_cell = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let workspace = workspace_cell.expect("the window-build closure always constructs one");
        // The per-mount half of the trash browser's scan reads the real
        // mount table, which no `$XDG_*` redirect above can touch -- so
        // it's switched off explicitly, the same way and for the same
        // reason the home-trash half is pointed at `data_dir`. Without
        // this, every trash-browser test picks up whatever the developer
        // actually has in a second drive's `.Trash-<uid>`.
        workspace.update(vcx, |ws, _cx| {
            ws.trash_mount_scan = MountScan::Explicit(Vec::new());
        });

        f(workspace, vcx, data_dir.path());

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
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
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

    /// UAT regression (window chrome): a click on the titlebar's drag
    /// region -- what starts a window move, and what a double-click
    /// maximizes through on Linux -- must not move keyboard focus off the
    /// active panel. `Workspace`'s root `.track_focus(..)` makes GPUI
    /// register an automatic "focus me on mouse-down" listener for the
    /// whole root, and the titlebar is a child of that root with no focus
    /// story of its own, so without an explicit opt-out every titlebar
    /// click bubbled up and handed focus to the workspace root instead.
    #[gpui::test]
    fn clicking_the_titlebar_drag_region_keeps_panel_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            focus_left_panel(&workspace, vcx);
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));

            // Well inside the bar (34px tall), far from the control icons
            // at its right edge.
            vcx.simulate_click(gpui::point(px(300.), px(17.)), gpui::Modifiers::default());
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "a titlebar click must leave keyboard focus on the panel that had it"
                );
            });
        });
    }

    // -- T-4.3.4 path bar --------------------------------------------------

    fn open_path_bar_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<crate::path_bar::PathBarState> {
        vcx.dispatch_action(GotoPath);
        let _ = vcx.update(|window, cx| window.draw(cx));
        workspace
            .read_with(vcx, |ws, _| ws.path_bar.clone())
            .expect("Ctrl+L must open the path bar")
    }

    /// `Ctrl+L` opens the focused panel's path bar, pre-filled with its
    /// directory, with keyboard focus in the field, and the panel's chrome
    /// still reads as active while the bar has focus.
    #[gpui::test]
    fn ctrl_l_opens_the_path_bar_prefilled_and_focused(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            let bar = open_path_bar_state(&workspace, vcx);
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.side(), PanelSide::Left);
                assert_eq!(bar.value(cx), dir.path().to_string_lossy());
            });
            vcx.update(|window, cx| {
                assert!(
                    bar.read(cx).is_focused(window, cx),
                    "the bar's input must take keyboard focus on open"
                );
            });
            // Opening it again on the same side is a no-op, not a second bar.
            vcx.dispatch_action(GotoPath);
            let _ = vcx.update(|window, cx| window.draw(cx));
            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.path_bar
                        .as_ref()
                        .is_some_and(|b| b.entity_id() == bar.entity_id())
                );
            });
        });
    }

    /// `Enter` on a real directory navigates the panel there, closes the
    /// bar, and returns focus to the panel's table.
    #[gpui::test]
    fn enter_in_the_path_bar_navigates_closes_and_restores_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("sub");
            std::fs::create_dir(&sub).unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));

            let bar = open_path_bar_state(&workspace, vcx);
            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&sub.to_string_lossy(), window, cx);
            });
            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });

            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            wait_until(vcx, |vcx| {
                left_table.read_with(vcx, |t, _| t.current_dir() == sub)
            });
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.path_bar.is_none())
            });
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "after navigating, focus must be back on the panel's table"
                );
            });
        });
    }

    /// A relative entry resolves against the panel's directory and `~`
    /// against `$HOME` -- exercised end to end through the same `Enter`.
    #[gpui::test]
    fn a_relative_path_in_the_bar_resolves_against_the_panel_directory(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let nested = dir.path().join("a").join("b");
            std::fs::create_dir_all(&nested).unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            let bar = open_path_bar_state(&workspace, vcx);
            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test("a/./b/", window, cx)
            });
            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });

            let left_table =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
            wait_until(vcx, |vcx| {
                left_table.read_with(vcx, |t, _| t.current_dir() == nested)
            });
        });
    }

    /// `Esc` closes the bar without navigating and restores focus.
    #[gpui::test]
    fn escape_in_the_path_bar_cancels_and_restores_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));

            let bar = open_path_bar_state(&workspace, vcx);
            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test("/nowhere", window, cx)
            });
            vcx.dispatch_action(duet_widgets::input::Escape);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, cx| {
                assert!(ws.path_bar.is_none(), "Esc must close the bar");
                assert_eq!(
                    ws.left_panel.read(cx).active_table().read(cx).current_dir(),
                    dir.path(),
                    "Esc must not navigate"
                );
            });
            vcx.update(|window, _cx| assert!(left_handle.is_focused(window)));
        });
    }

    /// `Enter` on something that is not a directory keeps the bar open and
    /// says why, inline; the next edit clears the message.
    #[gpui::test]
    fn a_non_directory_entry_keeps_the_path_bar_open_with_an_inline_error(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("plain.txt");
            std::fs::write(&file, b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            let bar = open_path_bar_state(&workspace, vcx);
            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&file.to_string_lossy(), window, cx);
            });
            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });
            wait_until(vcx, |vcx| {
                bar.read_with(vcx, |bar, _| bar.error().is_some())
            });
            workspace.read_with(vcx, |ws, _| assert!(ws.path_bar.is_some()));
            bar.read_with(vcx, |bar, _| {
                assert!(bar.error().unwrap().starts_with("Not a directory: "));
            });

            vcx.simulate_input("x");
            let _ = vcx.update(|window, cx| window.draw(cx));
            bar.read_with(vcx, |bar, _| {
                assert!(bar.error().is_none(), "an edit clears it")
            });
        });
    }

    /// `Tab` completes a unique directory (with a trailing `/`), and for an
    /// ambiguous prefix extends to the common prefix and lists the
    /// candidates -- against the real filesystem, off the UI thread.
    #[gpui::test]
    fn tab_in_the_path_bar_completes_directories(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            for name in ["alpha", "alphabet", "beta"] {
                std::fs::create_dir(dir.path().join(name)).unwrap();
            }
            std::fs::write(dir.path().join("betamax.txt"), b"not a dir").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());
            let bar = open_path_bar_state(&workspace, vcx);
            let root = dir.path().to_string_lossy().into_owned();

            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&format!("{root}/b"), window, cx);
            });
            vcx.dispatch_action(IndentInline);
            wait_until(vcx, |vcx| {
                bar.read_with(vcx, |bar, cx| bar.value(cx) == format!("{root}/beta/"))
            });
            bar.read_with(vcx, |bar, _| assert!(bar.candidates().is_empty()));

            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&format!("{root}/al"), window, cx);
            });
            vcx.dispatch_action(IndentInline);
            wait_until(vcx, |vcx| {
                bar.read_with(vcx, |bar, cx| bar.value(cx) == format!("{root}/alpha"))
            });
            bar.read_with(vcx, |bar, _| {
                assert_eq!(
                    bar.candidates(),
                    ["alpha".to_string(), "alphabet".to_string()]
                );
                assert_eq!(bar.selected(), None, "listed, nothing picked yet");
            });

            // Further Tabs cycle through the candidates like a shell's
            // menu completion, wrapping; Shift+Tab goes back.
            vcx.dispatch_action(IndentInline);
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.value(cx), format!("{root}/alpha/"));
                assert_eq!(bar.selected(), Some(0));
                assert_eq!(bar.candidates().len(), 2, "the hint stays up");
            });
            vcx.dispatch_action(IndentInline);
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.value(cx), format!("{root}/alphabet/"));
                assert_eq!(bar.selected(), Some(1));
            });
            vcx.dispatch_action(IndentInline);
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.value(cx), format!("{root}/alpha/"), "wraps");
            });
            vcx.dispatch_action(duet_widgets::input::OutdentInline);
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.value(cx), format!("{root}/alphabet/"), "backwards");
            });

            // A real edit ends the cycle; the next Tab completes afresh
            // from the new text (inside the chosen directory).
            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&format!("{root}/alphabet/x"), window, cx);
            });
            bar.read_with(vcx, |bar, _| {
                assert!(bar.candidates().is_empty());
                assert_eq!(bar.selected(), None);
            });
        });
    }

    /// The candidate hint shows a window of names; cycling past its end
    /// slides the window by one (the leftmost name gives way, `+N`
    /// shrinks by one), and wrapping around brings it back to the start.
    #[gpui::test]
    fn cycling_past_the_visible_hint_slides_the_window(cx: &mut TestAppContext) {
        use crate::path_bar::HINT_VISIBLE;
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let total = HINT_VISIBLE + 3;
            for i in 0..total {
                std::fs::create_dir(dir.path().join(format!("d{i:02}"))).unwrap();
            }
            focus_left_panel_at(&workspace, vcx, dir.path());
            let bar = open_path_bar_state(&workspace, vcx);
            let root = dir.path().to_string_lossy().into_owned();

            bar.update_in(vcx, |bar, window, cx| {
                bar.set_value_for_test(&format!("{root}/d"), window, cx);
            });
            vcx.dispatch_action(IndentInline);
            wait_until(vcx, |vcx| {
                bar.read_with(vcx, |bar, _| bar.candidates().len() == total)
            });
            bar.read_with(vcx, |bar, _| assert_eq!(bar.hint_window_start(), 0));

            // Tab up to the last visible candidate: the window holds.
            for _ in 0..HINT_VISIBLE {
                vcx.dispatch_action(IndentInline);
            }
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.selected(), Some(HINT_VISIBLE - 1));
                assert_eq!(bar.hint_window_start(), 0);
                assert_eq!(bar.value(cx), format!("{root}/d{:02}/", HINT_VISIBLE - 1));
            });
            // One more: the next candidate scrolls in, the first scrolls out.
            vcx.dispatch_action(IndentInline);
            bar.read_with(vcx, |bar, _| {
                assert_eq!(bar.selected(), Some(HINT_VISIBLE));
                assert_eq!(bar.hint_window_start(), 1);
            });
            // Through the end and around: back to the first, window reset.
            for _ in 0..3 {
                vcx.dispatch_action(IndentInline);
            }
            bar.read_with(vcx, |bar, cx| {
                assert_eq!(bar.selected(), Some(0));
                assert_eq!(bar.hint_window_start(), 0);
                assert_eq!(bar.value(cx), format!("{root}/d00/"));
            });
        });
    }

    // The maximize/minimize control icons themselves are deliberately not
    // clicked here: GPUI's test platform leaves `TestWindow::zoom` and
    // `::minimize` as `unimplemented!()`, so a simulated click on either
    // panics inside gpui rather than exercising anything of ours. Their
    // focus story is covered by reading, not by test: `gpui-component`'s
    // `ControlIcon` calls `window.prevent_default()` from its own
    // mouse-down listener (the same opt-out the wrapper in
    // `Workspace::render` uses for the drag region), so those clicks never
    // reached the root's focus-transfer listener even before this fix.

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

    #[test]
    fn column_layout_round_trips_through_settings_toml_from_a_fresh_install() {
        use crate::columns::{ColumnKind, FULL_VIEW};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert_eq!(load_column_layout(&path), ColumnLayout::default());

        let mut layout = ColumnLayout::default();
        layout.toggle(ColumnKind::Extension);
        layout.set_widths(&[0.0, 64.0, 120.0, 180.0]);
        save_column_layout(&path, FULL_VIEW, &layout.to_config()).unwrap();
        assert_eq!(load_column_layout(&path), layout);

        // A second save (the user dragged again) replaces, never appends.
        layout.toggle(ColumnKind::Extension);
        save_column_layout(&path, FULL_VIEW, &layout.to_config()).unwrap();
        assert_eq!(load_column_layout(&path), layout);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("\"ext\""), "{on_disk}");
        assert!(on_disk.contains("[panels.layouts.full]"), "{on_disk}");
    }

    #[test]
    fn saving_a_column_layout_preserves_other_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n\n[panels]\nshow_hidden = true\nsplitter_ratio = 0.4\n",
        )
        .unwrap();
        save_column_layout(
            &path,
            crate::columns::FULL_VIEW,
            &ColumnLayout::default().to_config(),
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("show_hidden = true"), "{on_disk}");
        assert!(on_disk.contains("splitter_ratio = 0.4"), "{on_disk}");
        assert_eq!(load_splitter_ratio(&path), 0.4);
    }

    /// T-4.2.4's fan-out: a column toggled from one table's header shows
    /// up in the other panel's table (they share one layout) and lands in
    /// `settings.toml` (so it survives a restart) without anything else
    /// having to be wired per tab.
    #[gpui::test]
    fn toggling_a_column_updates_every_table_and_persists_it(cx: &mut TestAppContext) {
        use crate::columns::ColumnKind;
        with_workspace(cx, |workspace, vcx| {
            let (left, right) = workspace.read_with(vcx, |ws, cx| {
                (
                    ws.left_panel.read(cx).active_table().clone(),
                    ws.right_panel.read(cx).active_table().clone(),
                )
            });
            let has_attrs = |table: &Entity<FileTable>, vcx: &mut VisualTestContext| {
                table.read_with(vcx, |table, cx| {
                    table
                        .state()
                        .read(cx)
                        .delegate()
                        .layout()
                        .contains(ColumnKind::Attributes)
                })
            };
            assert!(!has_attrs(&left, vcx));
            assert!(!has_attrs(&right, vcx));

            left.update(vcx, |table, cx| {
                let state = table.state().clone();
                state.update(cx, |state, cx| {
                    state
                        .delegate_mut()
                        .toggle_column(ColumnKind::Attributes, cx);
                });
            });
            vcx.run_until_parked();
            assert!(has_attrs(&left, vcx), "the toggling table itself");
            assert!(
                has_attrs(&right, vcx),
                "the other panel follows the shared layout"
            );

            let settings = duet_config::paths::settings_path().unwrap();
            wait_until(vcx, |_| {
                std::fs::read_to_string(&settings)
                    .map(|text| text.contains("attrs"))
                    .unwrap_or(false)
            });
            assert_eq!(
                load_column_layout(&settings),
                left.read_with(vcx, |t, cx| t.state().read(cx).delegate().layout().clone())
            );
        });
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
            // A *bounded* drain of GPUI's test executor, deliberately not
            // `run_until_parked()`: that keeps ticking for as long as any
            // task is runnable, and while a job is running the queue's
            // event consumer (`Workspace::new`'s `queue_events_rx` loop)
            // is re-woken by a progress sample every 100ms, each of which
            // ends in a full window draw (gpui's test-mode
            // `flush_effects` draws every dirty window). On a machine
            // where a debug-build draw takes longer than that -- a 4-core
            // CI runner, or this workstation pinned to 2 cores -- the
            // executor never parks, `condition` is never re-checked, the
            // conflict dialog is never answered, and the jobs keep
            // sampling forever: `two_concurrent_conflicts_are_both_
            // served_not_dropped` hung this way for the full nextest
            // timeout on 2026-09-04 (thread dump: the test thread mid-
            // draw under `run_until_parked`, both Tokio workers parked
            // in `ConflictResolver::resolve`). The bound is a *time*
            // budget, not a tick count: on a slow machine every tick can
            // be a full draw, and a fixed count of those blew straight
            // through the 5s deadline before `condition` was checked
            // even once (6/6 failures pinned to 4 cores). This way the
            // condition is re-checked at least every ~50ms of executor
            // work regardless of how fast the machine draws.
            let budget = std::time::Instant::now() + std::time::Duration::from_millis(50);
            while std::time::Instant::now() < budget && vcx.background_executor.tick() {}
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
        async fn symlink(&self, target: &str, link_path: &VPath) -> duet_types::Result<()> {
            self.inner.symlink(target, link_path).await
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
            _on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
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
                // Wide enough that a draw or two on a slow machine cannot
                // eat the whole first file's delay before the manager gets
                // to observe `Running`; small enough that the Pause/Resume
                // round trip (resume restarts the interrupted file) still
                // finishes well inside the 5s `wait_until` deadlines.
                delay: Duration::from_millis(700),
            });
            let src = crate::file_table::local_vpath(source_dir.path()).unwrap();
            let dst = crate::file_table::local_vpath(dest_dir.path()).unwrap();

            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenOperationManager);
            let _ = vcx.update(|window, cx| window.draw(cx));
            let state = workspace
                .read_with(vcx, |ws, _| ws.operation_manager.clone())
                .expect("Ctrl+O must open the manager");
            // Enqueue only now, with the manager already open: everything
            // above draws at least twice, and on a slow machine (a 4-core
            // CI runner; this workstation pinned to 4 cores) those draws
            // alone take longer than the fixture's whole artificial
            // delay, so a job enqueued first was already `Terminal` by the
            // first check below (instrumented 2026-09-04: `Completed, 3
            // files` at t=75ms). The manager reads the live queue snapshot
            // on every render, so a job that appears after it opened is
            // observed exactly the same way.
            let job_id = enqueue_slow_copy(&tokio_handle, queue.clone(), fs, src, dst, state_dir);

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
            // it finished -- see that field's own doc comment. Waited for,
            // not asserted outright: the two waits above observe the
            // *queue* (updated synchronously on the Tokio side), while the
            // eviction happens when the workspace's own event consumer
            // gets to the `Finished` event -- one event per executor tick,
            // behind whatever `Progress` samples are still queued ahead of
            // it. Once the UI has caught up, the entry must be gone and
            // must stay gone (the executor now awaits its aborted sampler
            // before sending `Finished`, so no trailing sample can
            // resurrect it).
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.job_progress_snapshot(job_id).is_none())
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
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

    // ======================================================================
    // T-5.2.3: the live conflict dialog
    // ======================================================================

    fn sample_conflict_prompt(
        source: VPath,
        dest: VPath,
        source_size: u64,
        dest_size: u64,
    ) -> ConflictPrompt {
        let mut source_meta = Metadata::minimal(EntryKind::File);
        source_meta.size = source_size;
        source_meta.modified = Some(Timestamp::new(1_700_000_000, 0));
        let mut dest_meta = Metadata::minimal(EntryKind::File);
        dest_meta.size = dest_size;
        dest_meta.modified = Some(Timestamp::new(1_600_000_000, 0));
        ConflictPrompt {
            step_index: 0,
            source,
            dest,
            source_meta,
            dest_meta,
        }
    }

    /// Opens a [`ConflictDialogState`] directly against `prompt`, bypassing
    /// the real executor/`InteractiveConflictResolver` round trip --
    /// exactly the shape [`Workspace::spawn_conflict_dialog_entity`]
    /// itself builds, just called from a test with a plain
    /// `std::sync::mpsc` pair standing in for the blocked executor thread
    /// on the other end. Used by the narrow, per-keybinding tests below,
    /// which only care "does this key produce this `ConflictResolution`,"
    /// not the full plan/queue/executor path a real conflict would need
    /// (that's what the end-to-end test further down is for).
    fn open_conflict_dialog_directly(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        prompt: ConflictPrompt,
    ) -> (
        Entity<ConflictDialogState>,
        std::sync::mpsc::Receiver<ConflictResolution>,
    ) {
        let (response_tx, response_rx) = std::sync::mpsc::channel();
        let state = workspace.update_in(vcx, |ws, window, cx| {
            let request = ConflictRequest {
                prompt,
                response_tx,
            };
            let state = ws.spawn_conflict_dialog_entity(request, cx);
            let handle = state.read(cx).focus_handle(cx);
            window.focus(&handle);
            state
        });
        let _ = vcx.update(|window, cx| window.draw(cx));
        (state, response_rx)
    }

    /// Plans and enqueues a real `JobKind::Copy` job through the
    /// workspace's own, real `conflict_resolver` (not `None`, unlike
    /// `enqueue_slow_copy` above) -- `src`'s destination is expected to
    /// already exist at `dst_dir`, producing a genuine conflict the
    /// executor must live-resolve. `fs` is caller-supplied so the
    /// concurrent-conflicts test below can inject `SlowLocalFs` the same
    /// way `enqueue_slow_copy` does, without duplicating this whole
    /// function just to swap the `FileSystem` impl.
    fn enqueue_conflicting_copy(
        tokio_handle: &tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        resolver: Arc<dyn ConflictResolver>,
        fs: Arc<dyn FileSystem>,
        src: VPath,
        dst_dir: VPath,
        state_dir: PathBuf,
    ) -> duet_ops::JobId {
        let (tx, rx) = std::sync::mpsc::channel();
        let fs_for_plan = fs.clone();
        tokio_handle.spawn(async move {
            let cancel = duet_ops::CancelToken::new();
            let plan = duet_ops::plan_copy(
                fs_for_plan.as_ref(),
                &[src],
                &dst_dir,
                duet_ops::PlanOptions::default(),
                &cancel,
            )
            .await
            .expect("plan_copy over a real tempdir must succeed");
            let id = queue.enqueue(
                JobKind::Copy,
                plan,
                0,
                fs_for_plan,
                state_dir,
                1,
                Some(resolver),
            );
            let _ = tx.send(id);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("plan_copy + enqueue must complete quickly")
    }

    /// End-to-end: a real conflicting copy job, live-resolved through the
    /// dialog this task built, start to finish -- enqueue -> the dialog
    /// opens with the right prompt data -> answering `Overwrite`/`ThisOnly`
    /// unblocks the executor thread -> the job completes with the
    /// destination actually overwritten. This is the one test in this
    /// module that exercises `InteractiveConflictResolver::resolve`'s real
    /// `block_in_place` wait from inside a real executor task, not just
    /// `conflict_dialog.rs`'s own narrower, resolver-only tests.
    #[gpui::test]
    fn f5_copy_with_a_real_conflict_opens_the_dialog_and_completes_via_overwrite(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            focus_left_panel(&workspace, vcx);

            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            let source_content = b"NEW CONTENT FROM SOURCE".to_vec();
            let dest_content = b"stale, different, older content".to_vec();
            std::fs::write(source_dir.path().join("a.bin"), &source_content).unwrap();
            std::fs::write(dest_dir.path().join("a.bin"), &dest_content).unwrap();

            let (queue, state_dir, tokio_handle, resolver) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                    Arc::clone(&ws.conflict_resolver) as Arc<dyn ConflictResolver>,
                )
            });
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let src = crate::file_table::local_vpath(&source_dir.path().join("a.bin")).unwrap();
            let dst_dir = crate::file_table::local_vpath(dest_dir.path()).unwrap();
            let job_id = enqueue_conflicting_copy(
                &tokio_handle,
                queue.clone(),
                resolver,
                fs,
                src.clone(),
                dst_dir,
                state_dir,
            );

            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.conflict_dialog.is_some())
            });

            let dst = crate::file_table::local_vpath(&dest_dir.path().join("a.bin")).unwrap();
            workspace.read_with(vcx, |ws, cx| {
                let dialog = ws.conflict_dialog.as_ref().unwrap().read(cx);
                let prompt = dialog.prompt();
                assert_eq!(
                    prompt.source, src,
                    "the dialog must show the real source path"
                );
                assert_eq!(prompt.dest, dst, "the dialog must show the real dest path");
                assert_eq!(
                    prompt.source_meta.size,
                    source_content.len() as u64,
                    "the dialog must show the real source size"
                );
                assert_eq!(
                    prompt.dest_meta.size,
                    dest_content.len() as u64,
                    "the dialog must show the real dest size"
                );
            });

            vcx.dispatch_action(ConflictOverwriteThis);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.conflict_dialog.is_none(),
                    "answering must close the dialog"
                );
            });

            wait_until(vcx, |_vcx| {
                matches!(
                    queue.job(job_id).map(|j| j.state),
                    Some(JobState::Terminal { .. })
                )
            });
            let final_state = queue.job(job_id).map(|j| j.state.clone());
            assert!(
                matches!(
                    final_state,
                    Some(JobState::Terminal {
                        outcome: JobOutcome::Completed,
                        ..
                    })
                ),
                "the job must complete cleanly once the conflict is answered: {final_state:?}"
            );
            assert_eq!(
                std::fs::read(dest_dir.path().join("a.bin")).unwrap(),
                source_content,
                "Overwrite must have replaced the stale destination with the source's content"
            );

            // Focus must return to whatever had it before the dialog took
            // over -- the left panel, per `focus_left_panel` above.
            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "closing the conflict dialog must restore the previously-focused panel"
                );
            });
        });
    }

    /// Two real, concurrently-running conflicting copy jobs
    /// (`COPY_MOVE_QUEUE_MAX_CONCURRENT` allows both to run at once):
    /// proves `pending_conflict_requests` genuinely holds the second
    /// conflict while the first dialog is still open, and that answering
    /// the first immediately opens and focuses the second (via
    /// `Self::close_conflict_dialog`'s own reopen path) rather than
    /// dropping it -- the concrete mechanism T-5.2.3's "10k-conflict run
    /// is survivable using apply-to-all" AC depends on not being
    /// undermined by.
    #[gpui::test]
    fn two_concurrent_conflicts_are_both_served_not_dropped(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            focus_left_panel(&workspace, vcx);

            let mut sources = Vec::new();
            let mut dest_dirs = Vec::new();
            let mut expected_content = Vec::new();
            for i in 0..2 {
                let source_dir = tempfile::tempdir().unwrap();
                let dest_dir = tempfile::tempdir().unwrap();
                let content = format!("source content {i}").into_bytes();
                std::fs::write(source_dir.path().join("a.bin"), &content).unwrap();
                std::fs::write(
                    dest_dir.path().join("a.bin"),
                    format!("stale dest content {i}"),
                )
                .unwrap();
                sources.push((source_dir, dest_dir.path().join("a.bin")));
                dest_dirs.push(dest_dir);
                expected_content.push(content);
            }

            let (queue, state_dir, tokio_handle, resolver) = workspace.read_with(vcx, |ws, _| {
                (
                    ws.queue.clone(),
                    ws.state_dir
                        .clone()
                        .expect("test env always resolves a state dir"),
                    ws.tokio_handle.clone(),
                    Arc::clone(&ws.conflict_resolver) as Arc<dyn ConflictResolver>,
                )
            });

            // A generous, fixed delay on every `open_read` -- staggers
            // both jobs' publish-time conflict detection widely enough
            // apart from "instant" that the test can reliably observe
            // both dialogs (open, then queued) rather than racing a
            // conflict that resolves before the test ever gets to look.
            let mut job_ids = Vec::new();
            for (i, (source_dir, dest_path)) in sources.iter().enumerate() {
                let fs: Arc<dyn FileSystem> = Arc::new(SlowLocalFs {
                    inner: LocalFs,
                    delay: Duration::from_millis(400),
                });
                let src = crate::file_table::local_vpath(&source_dir.path().join("a.bin")).unwrap();
                let dst_dir = crate::file_table::local_vpath(dest_dirs[i].path()).unwrap();
                let _ = dest_path;
                job_ids.push(enqueue_conflicting_copy(
                    &tokio_handle,
                    queue.clone(),
                    resolver.clone(),
                    fs,
                    src,
                    dst_dir,
                    state_dir.clone(),
                ));
            }

            // The first dialog opens...
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.conflict_dialog.is_some())
            });
            // ...and the second conflict lands in the pending queue while
            // it's still open, rather than being dropped or silently
            // stalling its own job forever.
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| !ws.pending_conflict_requests.is_empty())
            });

            // Answer whichever conflict is currently showing.
            vcx.dispatch_action(ConflictOverwriteThis);
            let _ = vcx.update(|window, cx| window.draw(cx));

            // The queued one must now be open (and focused) too -- not
            // still sitting unopened.
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.conflict_dialog.is_some())
            });
            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.pending_conflict_requests.is_empty(),
                    "the second conflict must have been dequeued into `conflict_dialog`"
                );
            });
            vcx.dispatch_action(ConflictOverwriteThis);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.conflict_dialog.is_none(),
                    "both conflicts must be answered by now"
                );
            });

            for id in &job_ids {
                wait_until(vcx, |_vcx| {
                    matches!(
                        queue.job(*id).map(|j| j.state),
                        Some(JobState::Terminal { .. })
                    )
                });
            }
            for (i, dest_dir) in dest_dirs.iter().enumerate() {
                assert_eq!(
                    std::fs::read(dest_dir.path().join("a.bin")).unwrap(),
                    expected_content[i],
                    "job {i}'s destination must have been overwritten with its own source"
                );
            }
        });
    }

    // -- Keyboard-completeness: every policy, both scopes -------------------

    #[gpui::test]
    fn overwrite_key_resolves_this_only_and_all_remaining(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictOverwriteThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Overwrite);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictOverwriteAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Overwrite);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    #[gpui::test]
    fn skip_key_resolves_this_only_and_all_remaining(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictSkipThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Skip);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictSkipAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Skip);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    /// Escape's *bubbled* path: while the rename field has focus,
    /// `InputState`'s own "escape" binding fires its internal `Escape`
    /// action first (its `clean_on_escape` default is `false`, so it
    /// propagates rather than clearing the field -- see the module doc
    /// comment's "Keybindings" section), which this dialog's root then
    /// catches via `.on_action::<Escape>()`. Distinct code path from the
    /// root-focused case (`KeyBinding::new("escape", ConflictSkipThis,
    /// ...)`, already covered by `skip_key_resolves_this_only_and_all_
    /// remaining` above) -- deserves its own test since `dispatch_action`
    /// bypasses keystroke-to-action resolution entirely (delivering the
    /// given action instance straight to `on_action` handlers, the same
    /// way every other keybinding test in this module already works), so
    /// only dispatching the actual bubbled action type exercises this
    /// handler.
    #[gpui::test]
    fn escape_while_the_rename_field_has_focus_still_resolves_skip_this_only(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictRename);
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |s, _| assert!(s.renaming()));

            vcx.dispatch_action(duet_widgets::input::Escape);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Skip);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);
            workspace.read_with(vcx, |ws, _| assert!(ws.conflict_dialog.is_none()));
        });
    }

    #[gpui::test]
    fn overwrite_if_older_key_resolves_this_only_and_all_remaining(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictOverwriteIfOlderThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::OverwriteIfOlder);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictOverwriteIfOlderAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::OverwriteIfOlder);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    #[gpui::test]
    fn overwrite_if_different_size_key_resolves_this_only_and_all_remaining(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictOverwriteIfDifferentSizeThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::OverwriteIfDifferentSize);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictOverwriteIfDifferentSizeAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::OverwriteIfDifferentSize);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    #[gpui::test]
    fn auto_rename_key_resolves_this_only_and_all_remaining(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictAutoRenameThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::AutoRename);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictAutoRenameAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::AutoRename);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    #[gpui::test]
    fn abort_key_resolves_this_only_and_all_remaining(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt.clone());
            vcx.dispatch_action(ConflictAbortThis);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Abort);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);

            let (_state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictAbortAll);
            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::Abort);
            assert_eq!(resolution.scope, ConflictScope::AllRemaining);
        });
    }

    // -- RenameTarget + Tab/Shift+Tab focus cycling --------------------------

    #[gpui::test]
    fn alt_r_shows_and_focuses_the_rename_field_defaulted_to_the_dest_name(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (state, _rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);

            state.read_with(vcx, |s, _| assert!(!s.renaming()));
            vcx.dispatch_action(ConflictRename);
            let _ = vcx.update(|window, cx| window.draw(cx));

            state.read_with(vcx, |s, _| assert!(s.renaming()));
            let input = state.read_with(vcx, |s, _| s.rename_input().cloned().unwrap());
            let default_value = input.read_with(vcx, |s, _| s.value().to_string());
            assert_eq!(default_value, "a.txt");

            let input_handle = input.read_with(vcx, |s, cx| s.focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    input_handle.is_focused(window),
                    "Alt+R must focus the rename field, not just show it"
                );
            });
        });
    }

    #[gpui::test]
    fn tab_and_shift_tab_toggle_focus_between_the_root_and_the_rename_field(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (state, _rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictRename);
            let _ = vcx.update(|window, cx| window.draw(cx));

            let root_handle = state.read_with(vcx, |s, cx| s.focus_handle(cx));
            let input = state.read_with(vcx, |s, _| s.rename_input().cloned().unwrap());
            let input_handle = input.read_with(vcx, |s, cx| s.focus_handle(cx));

            vcx.update(|window, _cx| assert!(input_handle.is_focused(window)));

            // Tab, while the rename field has focus, bubbles as
            // `IndentInline` (see the module doc comment's "Keybindings"
            // section) -- dispatch that action directly rather than the
            // dialog's own `ConflictFocusNext`, matching what a real
            // keypress with the field focused would actually produce.
            vcx.dispatch_action(IndentInline);
            let _ = vcx.update(|window, cx| window.draw(cx));
            vcx.update(|window, _cx| {
                assert!(
                    root_handle.is_focused(window),
                    "Tab must move focus back to the dialog root"
                );
            });

            // From the root, `ConflictFocusNext`/`ConflictFocusPrev` (the
            // direct bindings) move focus back onto the rename field.
            vcx.dispatch_action(ConflictFocusPrev);
            let _ = vcx.update(|window, cx| window.draw(cx));
            vcx.update(|window, _cx| {
                assert!(input_handle.is_focused(window));
            });

            vcx.dispatch_action(ConflictFocusNext);
            let _ = vcx.update(|window, cx| window.draw(cx));
            vcx.update(|window, _cx| {
                assert!(root_handle.is_focused(window));
            });
        });
    }

    #[gpui::test]
    fn enter_in_the_rename_field_confirms_rename_target_with_the_typed_name(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dest = VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap());
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                dest.clone(),
                10,
                20,
            );
            let (state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictRename);
            let _ = vcx.update(|window, cx| window.draw(cx));

            let input = state.read_with(vcx, |s, _| s.rename_input().cloned().unwrap());
            input.update_in(vcx, |input_state, window, cx| {
                input_state.set_value("renamed.txt", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });
            let _ = vcx.update(|window, cx| window.draw(cx));

            let resolution = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(resolution.policy, ConflictPolicy::RenameTarget);
            assert_eq!(resolution.scope, ConflictScope::ThisOnly);
            assert_eq!(
                resolution.alternate,
                Some(VPath::local(
                    UnixPathBuf::new("/tmp/dst/renamed.txt").unwrap()
                ))
            );
            workspace.read_with(vcx, |ws, _| assert!(ws.conflict_dialog.is_none()));
        });
    }

    #[gpui::test]
    fn enter_in_the_rename_field_with_an_invalid_name_shows_an_error_and_stays_open(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let prompt = sample_conflict_prompt(
                VPath::local(UnixPathBuf::new("/tmp/src/a.txt").unwrap()),
                VPath::local(UnixPathBuf::new("/tmp/dst/a.txt").unwrap()),
                10,
                20,
            );
            let (state, rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);
            vcx.dispatch_action(ConflictRename);
            let _ = vcx.update(|window, cx| window.draw(cx));

            let input = state.read_with(vcx, |s, _| s.rename_input().cloned().unwrap());
            input.update_in(vcx, |input_state, window, cx| {
                input_state.set_value("", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });
            let _ = vcx.update(|window, cx| window.draw(cx));

            assert!(
                rx.try_recv().is_err(),
                "an invalid name must not send any resolution"
            );
            state.read_with(vcx, |s, _| assert!(s.rename_error().is_some()));
            workspace.read_with(vcx, |ws, _| assert!(ws.conflict_dialog.is_some()));
        });
    }

    // -- On-demand hashing ----------------------------------------------------

    #[gpui::test]
    fn hashing_each_side_shows_a_spinner_then_the_correct_blake3_digest(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            let source_content = b"source side content for hashing".repeat(100);
            let dest_content = b"destination side content, different".repeat(100);
            let source_path = source_dir.path().join("a.bin");
            let dest_path = dest_dir.path().join("a.bin");
            std::fs::write(&source_path, &source_content).unwrap();
            std::fs::write(&dest_path, &dest_content).unwrap();

            let source_vpath = crate::file_table::local_vpath(&source_path).unwrap();
            let dest_vpath = crate::file_table::local_vpath(&dest_path).unwrap();
            let prompt = sample_conflict_prompt(
                source_vpath,
                dest_vpath,
                source_content.len() as u64,
                dest_content.len() as u64,
            );
            let (state, _rx) = open_conflict_dialog_directly(&workspace, vcx, prompt);

            state.read_with(vcx, |s, _| {
                assert!(s.source_hash_digest().is_none());
            });

            // `TestAppContext::dispatch_action` (what `vcx.dispatch_action`
            // calls) runs a `background_executor.run_until_parked()` of
            // its own right after dispatching -- for a file this small,
            // that's enough to let the whole hash round trip finish before
            // this call even returns, so checking for "still pending"
            // afterward would be asserting on a race that's already lost.
            // `window.dispatch_action` directly (the raw, `Window`-level
            // call `vcx.dispatch_action` itself wraps) queues the action
            // as a deferred effect that flushes when *this* `vcx.update`
            // call returns -- `start_hash`'s synchronous `HashState::
            // Hashing` write happens then, but nothing has forced the
            // background executor to make progress yet, so the spinner
            // state is genuinely observable right here.
            vcx.update(|window, cx| {
                window.dispatch_action(Box::new(ConflictHashSource), cx);
            });
            state.read_with(vcx, |s, _| assert!(s.source_hash_is_pending()));

            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| s.source_hash_digest().is_some())
            });
            state.read_with(vcx, |s, _| {
                assert_eq!(s.source_hash_digest(), Some(blake3::hash(&source_content)));
            });

            vcx.dispatch_action(ConflictHashDest);
            wait_until(vcx, |vcx| {
                state.read_with(vcx, |s, _| s.dest_hash_digest().is_some())
            });
            state.read_with(vcx, |s, _| {
                assert_eq!(s.dest_hash_digest(), Some(blake3::hash(&dest_content)));
            });
        });
    }

    // -- T-5.2.6 delete confirmation ---------------------------------------

    /// A `settings.toml` body pinning exactly the three keys T-5.2.6 reads
    /// (`operations.delete_default`, `operations.confirm_delete`,
    /// `trash.enabled`). Every other key stays absent and therefore at its
    /// documented default -- `duet_config::Settings`'s own
    /// `#[serde(default)]` shape, so a partial file is a real, supported
    /// input, not a test-only shortcut.
    fn delete_settings(delete_default: &str, confirm_delete: &str, trash_enabled: bool) -> String {
        format!(
            "schema_version = 1\n\n[operations]\ndelete_default = \"{delete_default}\"\n\
             confirm_delete = \"{confirm_delete}\"\n\n[trash]\nenabled = {trash_enabled}\n"
        )
    }

    /// Points the left panel at `dir`, focuses it, and draws -- the shared
    /// preamble of every delete test below (each of which then dispatches
    /// `DeleteDialog`/`DeletePermanentDialog` with the cursor on `dir`'s
    /// sole entry, exercising `resolve_source_names`' cursor fallback).
    fn focus_left_panel_at(workspace: &Entity<Workspace>, vcx: &mut VisualTestContext, dir: &Path) {
        let left_table =
            workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_table().clone());
        navigate_panel_to(vcx, &left_table, dir.to_path_buf());
        focus_left_panel(workspace, vcx);
    }

    fn open_delete_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<DeleteDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.delete_dialog.clone())
            .expect("the delete dialog must be open by now")
    }

    #[gpui::test]
    fn f8_opens_the_delete_dialog_in_trash_mode_by_default(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let state = open_delete_dialog_state(&workspace, vcx);

                state.read_with(vcx, |state, _| {
                    assert!(
                        !state.permanent(),
                        "delete_default = trash with the trash enabled must start in trash mode"
                    );
                    assert_eq!(state.title_text(), "Move 1 item to trash?");
                    assert_eq!(
                        state.targets(),
                        &[crate::file_table::local_vpath(&dir.path().join("doomed.txt")).unwrap()],
                        "with nothing explicitly selected, the sole cursor-row entry is the target"
                    );
                    assert!(
                        state.non_empty_dir_names().is_empty(),
                        "a plain file target has no directory to warn about"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn f8_opens_the_delete_dialog_in_permanent_mode_when_configured(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let state = open_delete_dialog_state(&workspace, vcx);

                state.read_with(vcx, |state, _| {
                    assert!(state.permanent());
                    assert_eq!(state.title_text(), "Delete 1 item permanently?");
                });
            },
        );
    }

    /// The most valuable delete test: real tempdirs, the real `LocalFs`,
    /// F8 to confirm, and -- through the real off-thread `plan_delete` ->
    /// `QueueManager::enqueue` -> `execute()` path, no shortcuts -- the
    /// file genuinely gone from disk.
    #[gpui::test]
    fn confirming_a_permanent_delete_actually_removes_the_file(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("doomed.txt");
                std::fs::write(&victim, b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let _state = open_delete_dialog_state(&workspace, vcx);

                vcx.dispatch_action(ConfirmDelete);
                wait_until(vcx, |vcx| {
                    !victim.exists() && workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_none())
                });
            },
        );
    }

    /// The trash half of the same end-to-end path: the file must be gone
    /// from where it was *and* really present under
    /// `duet_config::paths::trash_files_dir()` -- resolved against the
    /// temporary `$XDG_DATA_HOME` `with_configured_workspace` installs, so
    /// this never touches the machine's own `~/.local/share/Trash`.
    #[gpui::test]
    fn confirming_a_trash_delete_moves_the_file_into_the_trash_directory(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("doomed.txt");
                std::fs::write(&victim, b"trash me").unwrap();
                let trashed = data_dir.join("Trash").join("files").join("doomed.txt");
                assert_eq!(
                    duet_config::paths::trash_files_dir().unwrap(),
                    data_dir.join("Trash").join("files"),
                    "the test's own expectation must match what the app resolves"
                );
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let _state = open_delete_dialog_state(&workspace, vcx);

                vcx.dispatch_action(ConfirmDelete);
                wait_until(vcx, |_vcx| trashed.is_file());
                assert!(!victim.exists(), "the original must be gone, not copied");
                assert_eq!(std::fs::read(&trashed).unwrap(), b"trash me");
            },
        );
    }

    #[gpui::test]
    fn escaping_the_delete_dialog_leaves_the_file_alone_and_restores_focus(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let survivor = dir.path().join("keep.txt");
                std::fs::write(&survivor, b"still here").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let _state = open_delete_dialog_state(&workspace, vcx);

                vcx.dispatch_action(CancelDelete);
                let _ = vcx.update(|window, cx| window.draw(cx));

                workspace.read_with(vcx, |ws, _| assert!(ws.delete_dialog.is_none()));
                assert_eq!(std::fs::read(&survivor).unwrap(), b"still here");

                let left_handle = workspace
                    .read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
                vcx.update(|window, _cx| {
                    assert!(
                        left_handle.is_focused(window),
                        "cancelling must restore focus to the panel the delete started from"
                    );
                });
            },
        );
    }

    /// This task's own AC clause: "Shift+Del bypasses trash with an
    /// explicit confirmation" -- forced permanent even though
    /// `delete_default = "trash"`, and with no way back to trash from
    /// inside the dialog.
    #[gpui::test]
    fn shift_f8_forces_permanent_and_ctrl_t_cannot_undo_it(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeletePermanentDialog);
                let state = open_delete_dialog_state(&workspace, vcx);
                state.read_with(vcx, |state, _| {
                    assert!(
                        state.permanent(),
                        "Shift+F8 must force permanent regardless of delete_default"
                    );
                });

                vcx.dispatch_action(ToggleTrashPermanent);
                let _ = vcx.update(|window, cx| window.draw(cx));
                state.read_with(vcx, |state, _| {
                    assert!(
                        state.permanent(),
                        "Ctrl+T must be a no-op once permanent was explicitly forced"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn ctrl_t_toggles_trash_and_permanent_in_the_ordinary_case(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let state = open_delete_dialog_state(&workspace, vcx);
                state.read_with(vcx, |s, _| assert!(!s.permanent()));

                vcx.dispatch_action(ToggleTrashPermanent);
                let _ = vcx.update(|window, cx| window.draw(cx));
                state.read_with(vcx, |s, _| {
                    assert!(s.permanent());
                    assert_eq!(s.title_text(), "Delete 1 item permanently?");
                });

                vcx.dispatch_action(ToggleTrashPermanent);
                let _ = vcx.update(|window, cx| window.draw(cx));
                state.read_with(vcx, |s, _| {
                    assert!(!s.permanent());
                    assert_eq!(s.title_text(), "Move 1 item to trash?");
                });
            },
        );
    }

    #[gpui::test]
    fn confirm_delete_never_deletes_immediately_without_any_dialog(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "never", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("doomed.txt");
                std::fs::write(&victim, b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                wait_until(vcx, |vcx| {
                    assert!(
                        workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_none()),
                        "confirm_delete = never must never construct a dialog"
                    );
                    !victim.exists()
                });
            },
        );
    }

    #[gpui::test]
    fn non_empty_dirs_policy_deletes_a_plain_file_without_confirming(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "non_empty_dirs", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("doomed.txt");
                std::fs::write(&victim, b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                wait_until(vcx, |vcx| {
                    assert!(
                        workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_none()),
                        "a plain file needs no confirmation under the non_empty_dirs policy"
                    );
                    !victim.exists()
                });
            },
        );
    }

    /// The edge case that makes the `non_empty_dirs` policy meaningful at
    /// all: a genuinely *empty* directory must read as empty (no false
    /// positive) and go straight through with no dialog.
    #[gpui::test]
    fn non_empty_dirs_policy_deletes_an_empty_directory_without_confirming(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "non_empty_dirs", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("hollow");
                std::fs::create_dir(&victim).unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                wait_until(vcx, |vcx| {
                    assert!(
                        workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_none()),
                        "an empty directory needs no confirmation under the non_empty_dirs policy"
                    );
                    !victim.exists()
                });
            },
        );
    }

    /// The other half of the same policy, and this task's own
    /// "non-empty directory warning" AC clause: a directory with something
    /// in it *does* confirm, and the warning names it.
    #[gpui::test]
    fn non_empty_dirs_policy_confirms_and_names_a_non_empty_directory(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("permanent", "non_empty_dirs", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("photos");
                std::fs::create_dir(&victim).unwrap();
                std::fs::write(victim.join("holiday.jpg"), b"jpeg").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let state = open_delete_dialog_state(&workspace, vcx);

                state.read_with(vcx, |state, _| {
                    assert_eq!(state.non_empty_dir_names(), &["photos".to_string()]);
                    let warning = state
                        .warning_text()
                        .expect("a non-empty directory target must render a warning");
                    assert!(warning.contains("photos"), "{warning}");
                });
                assert!(
                    victim.exists(),
                    "nothing may be deleted while the confirmation is still open"
                );
            },
        );
    }

    /// The `"always"` policy's own disclosed behaviour (see
    /// `Workspace::open_delete_dialog`'s doc comment): it doesn't need the
    /// non-empty answer to decide whether to confirm, but it still
    /// computes it so the warning isn't dead code under the shipped
    /// default configuration.
    #[gpui::test]
    fn the_always_policy_still_warns_about_a_non_empty_directory(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = dir.path().join("photos");
                std::fs::create_dir(&victim).unwrap();
                std::fs::write(victim.join("holiday.jpg"), b"jpeg").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.dispatch_action(DeleteDialog);
                let state = open_delete_dialog_state(&workspace, vcx);
                state.read_with(vcx, |state, _| {
                    assert_eq!(state.non_empty_dir_names(), &["photos".to_string()]);
                });
            },
        );
    }

    /// The one delete test driven by real keystrokes rather than
    /// `dispatch_action`: proves `bind_workspace_keys`' own `"f8"`/
    /// `"shift-f8"` bindings actually resolve from a focused panel (the
    /// `"Workspace"` key context really is in the dispatch path), which a
    /// direct action dispatch would bypass entirely. Safe to simulate here
    /// -- the panel, not an `InputState`, holds focus, so this doesn't hit
    /// the upstream `shape_line` debug-assert `f5_copy_end_to_end`'s own
    /// comment documents.
    #[gpui::test]
    fn the_f8_and_shift_f8_keystrokes_really_open_the_dialog(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
                focus_left_panel_at(&workspace, vcx, dir.path());

                vcx.simulate_keystrokes("f8");
                let state = open_delete_dialog_state(&workspace, vcx);
                state.read_with(vcx, |s, _| assert!(!s.permanent()));

                vcx.dispatch_action(CancelDelete);
                let _ = vcx.update(|window, cx| window.draw(cx));
                workspace.read_with(vcx, |ws, _| assert!(ws.delete_dialog.is_none()));

                vcx.simulate_keystrokes("shift-f8");
                let state = open_delete_dialog_state(&workspace, vcx);
                state.read_with(vcx, |s, _| {
                    assert!(
                        s.permanent(),
                        "Shift+F8 must reach the forced-permanent action, not plain F8's"
                    );
                });
            },
        );
    }

    // -- the two T-5.2.6 settings loaders ----------------------------------

    fn write_settings(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("settings.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn load_delete_default_permanent_reads_the_configured_target() {
        let dir = tempfile::tempdir().unwrap();
        let trash = write_settings(dir.path(), &delete_settings("trash", "always", true));
        assert!(!load_delete_default_permanent(&trash));

        let dir2 = tempfile::tempdir().unwrap();
        let permanent = write_settings(dir2.path(), &delete_settings("permanent", "always", true));
        assert!(load_delete_default_permanent(&permanent));
    }

    /// `docs/config-schema.md`: "when `trash.enabled == false`,
    /// `delete_default` cannot be `trash`" -- honored as a fallback to
    /// permanent, not as a hard error that would break deleting entirely.
    #[test]
    fn load_delete_default_permanent_falls_back_when_the_trash_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(dir.path(), &delete_settings("trash", "always", false));
        assert!(load_delete_default_permanent(&path));
    }

    #[test]
    fn load_delete_default_permanent_defaults_to_trash_with_no_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!load_delete_default_permanent(
            &dir.path().join("absent.toml")
        ));
    }

    #[test]
    fn load_confirm_delete_policy_reads_the_configured_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(
            dir.path(),
            &delete_settings("trash", "non_empty_dirs", true),
        );
        assert_eq!(load_confirm_delete_policy(&path), "non_empty_dirs");
    }

    #[test]
    fn load_confirm_delete_policy_defaults_to_always_with_no_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_confirm_delete_policy(&dir.path().join("absent.toml")),
            "always"
        );
    }

    // -- T-5.2.7 mkdir / rename / symlink / hardlink -------------------------

    fn open_mkdir_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<MkdirDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.mkdir_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.mkdir_dialog.clone())
            .expect("the mkdir dialog must be open by now")
    }

    fn open_rename_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<RenameDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.rename_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.rename_dialog.clone())
            .expect("the rename dialog must be open by now")
    }

    fn open_link_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<LinkDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.link_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.link_dialog.clone())
            .expect("the link dialog must be open by now")
    }

    /// Confirms whichever of the three T-5.2.7 dialogs currently holds
    /// focus. Dispatches the resolved `duet_widgets::input::Enter` action
    /// rather than `simulate_keystrokes("enter")`, for exactly the reason
    /// `f5_copy_end_to_end_copies_a_real_file_to_the_other_panels_directory`
    /// documents at length: the latter drives GPUI's synthetic IME pipeline
    /// into an unrelated upstream panic against a focused, non-empty
    /// `InputState`. This still exercises the real `InputState::enter` ->
    /// `cx.emit(PressEnter)` -> `cx.subscribe_in` -> `confirm` path, with no
    /// shortcut through any dialog's own private methods.
    fn press_enter(vcx: &mut VisualTestContext) {
        vcx.dispatch_action(duet_widgets::input::Enter { secondary: false });
    }

    /// Escape, likewise dispatched as the already-bound
    /// `duet_widgets::input::Escape` action `InputState` lets bubble out of
    /// its own `"Input"` key context -- see `copy_move_dialog.rs`'s module
    /// doc comment for why no dialog here binds Escape itself.
    fn press_escape(vcx: &mut VisualTestContext) {
        vcx.dispatch_action(duet_widgets::input::Escape);
    }

    // -- F7 mkdir -------------------------------------------------------------

    #[gpui::test]
    fn f7_opens_the_mkdir_dialog_prefilled_with_the_current_directory(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("anything.txt"), b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(MkdirDialog);
            let state = open_mkdir_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(
                    state.destination_value(cx),
                    format!("{}/", dir.path().to_string_lossy()),
                    "F7 pre-fills the focused panel's own directory plus a trailing slash, so \
                     the user types only the new segment(s)"
                );
                assert!(
                    state.destination_cursor_at_end(cx),
                    "UAT: the cursor must start at the end of the pre-filled path, ready to \
                     type the new folder name, not at the very beginning"
                );
            });
        });
    }

    /// This task's own AC clause: "F7 supports creating nested paths in one
    /// go, as TC does." Real tempdir, real `LocalFs`, the real off-thread
    /// `plan_mkdir` -> `QueueManager::enqueue` -> `execute()` path; all
    /// three levels must exist on disk afterwards.
    #[gpui::test]
    fn confirming_f7_creates_a_whole_nested_path_in_one_go(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("anything.txt"), b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(MkdirDialog);
            let state = open_mkdir_dialog_state(&workspace, vcx);

            let nested = dir.path().join("a/b/c");
            state.update_in(vcx, |state, window, cx| {
                state.set_destination_value(&nested.to_string_lossy(), window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                nested.is_dir() && workspace.read_with(vcx, |ws, _| ws.mkdir_dialog.is_none())
            });
            assert!(dir.path().join("a").is_dir());
            assert!(dir.path().join("a/b").is_dir());
        });
    }

    /// Confirming without typing anything past the pre-filled directory is
    /// a harmless no-op, not an error: `UnixPathBuf::new` strips the
    /// trailing slash, leaving the panel's already-existing directory, and
    /// `plan_mkdir`'s documented "already there is success" convention
    /// makes that a valid, zero-step plan. See `crate::mkdir_dialog`'s
    /// module doc comment.
    #[gpui::test]
    fn confirming_f7_with_nothing_typed_is_a_harmless_no_op(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("anything.txt"), b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(MkdirDialog);
            let _state = open_mkdir_dialog_state(&workspace, vcx);

            press_enter(vcx);
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.mkdir_dialog.is_none())
            });

            assert!(dir.path().is_dir(), "the directory itself must survive");
            assert!(
                dir.path().join("anything.txt").is_file(),
                "and so must everything in it"
            );
        });
    }

    #[gpui::test]
    fn escaping_the_mkdir_dialog_creates_nothing_and_restores_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("anything.txt"), b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(MkdirDialog);
            let state = open_mkdir_dialog_state(&workspace, vcx);
            let nested = dir.path().join("never-created");
            state.update_in(vcx, |state, window, cx| {
                state.set_destination_value(&nested.to_string_lossy(), window, cx);
            });

            press_escape(vcx);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.mkdir_dialog.is_none()));
            assert!(!nested.exists(), "Escape must enqueue nothing at all");

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "cancelling must restore focus to the panel F7 was pressed in"
                );
            });
        });
    }

    // -- Shift+F6 rename in place ---------------------------------------------

    /// This task's own AC clause, in the split-field form
    /// `crate::rename_dialog`'s module doc comment justifies at length: the
    /// editable field holds *only* the stem, and the extension is a fixed
    /// suffix the user cannot damage.
    #[gpui::test]
    fn shift_f6_prefills_the_stem_and_pins_the_extension(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("photo.jpg"), b"jpeg").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(RenameDialog);
            let state = open_rename_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(
                    state.stem_value(cx),
                    "photo",
                    "the editable field holds the stem alone, never the extension"
                );
                assert_eq!(state.extension(), Some("jpg"));
                assert_eq!(
                    state.source(),
                    &crate::file_table::local_vpath(&dir.path().join("photo.jpg")).unwrap(),
                    "Shift+F6 renames the cursor entry, resolved once at open time"
                );
                assert!(
                    state.stem_cursor_at_end(cx),
                    "UAT: the cursor must start at the end of the stem, ready to keep typing \
                     or backspace, not at the very beginning"
                );
            });
        });
    }

    /// The end-to-end half: a real rename on real disk, through the real
    /// off-thread `plan_rename_in_place` -> `QueueManager::enqueue` ->
    /// `execute()` path, with the extension carried across untouched even
    /// though the user only ever edited the stem.
    #[gpui::test]
    fn confirming_a_rename_renames_the_file_with_the_extension_intact(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let before = dir.path().join("photo.jpg");
            let after = dir.path().join("holiday.jpg");
            std::fs::write(&before, b"jpeg bytes").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(RenameDialog);
            let state = open_rename_dialog_state(&workspace, vcx);
            state.update_in(vcx, |state, window, cx| {
                state.set_stem_value("holiday", window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                after.is_file() && workspace.read_with(vcx, |ws, _| ws.rename_dialog.is_none())
            });
            assert!(!before.exists(), "the original name must be gone");
            assert_eq!(std::fs::read(&after).unwrap(), b"jpeg bytes");
        });
    }

    /// A dotfile has no extension by `Path::extension()`'s reckoning (the
    /// same convention `FileTableDelegate::select_same_extension` uses), so
    /// the whole name is editable and no fixed suffix is shown -- matching
    /// TC, and what this task's AC implicitly carves out by only ever
    /// mentioning "the extension".
    #[gpui::test]
    fn shift_f6_on_a_dotfile_makes_the_whole_name_editable(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(".bashrc"), b"export X=1").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(RenameDialog);
            let state = open_rename_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(state.stem_value(cx), ".bashrc");
                assert_eq!(
                    state.extension(),
                    None,
                    "a leading dot is not an extension -- there is no fixed suffix to show"
                );
            });
        });
    }

    #[gpui::test]
    fn escaping_the_rename_dialog_keeps_the_original_name_and_restores_focus(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let original = dir.path().join("keep.txt");
            std::fs::write(&original, b"unchanged").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(RenameDialog);
            let state = open_rename_dialog_state(&workspace, vcx);
            state.update_in(vcx, |state, window, cx| {
                state.set_stem_value("renamed", window, cx);
            });

            press_escape(vcx);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.rename_dialog.is_none()));
            assert_eq!(std::fs::read(&original).unwrap(), b"unchanged");
            assert!(!dir.path().join("renamed.txt").exists());

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "cancelling must restore focus to the panel Shift+F6 was pressed in"
                );
            });
        });
    }

    /// An empty listing has no cursor entry, so there is nothing to rename
    /// -- a no-op with an explanatory toast, not a dialog on nothing.
    #[gpui::test]
    fn shift_f6_in_an_empty_directory_opens_nothing(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(RenameDialog);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.rename_dialog.is_none()));
        });
    }

    // -- Ctrl+Shift+S symlink / Ctrl+Shift+H hardlink -------------------------

    /// Points the right panel at `dest_dir` and the left at `source_dir`,
    /// focusing the left -- the preamble both link commands need, since the
    /// new link's path defaults to the *other* panel's directory.
    fn focus_left_with_right_panel_at(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        source_dir: &Path,
        dest_dir: &Path,
    ) {
        let right_table =
            workspace.read_with(vcx, |ws, cx| ws.right_panel.read(cx).active_table().clone());
        navigate_panel_to(vcx, &right_table, dest_dir.to_path_buf());
        focus_left_panel_at(workspace, vcx, source_dir);
    }

    #[gpui::test]
    fn ctrl_shift_s_defaults_the_link_path_to_the_other_panels_directory(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("target.txt"), b"real file").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(SymlinkDialog);
            let state = open_link_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(state.kind(), LinkKind::Symlink);
                assert_eq!(
                    state.source(),
                    &crate::file_table::local_vpath(&source_dir.path().join("target.txt")).unwrap()
                );
                assert_eq!(
                    state.link_path_value(cx),
                    dest_dir
                        .path()
                        .join("target.txt")
                        .to_string_lossy()
                        .into_owned(),
                    "the new link defaults into the other panel's directory under the same name, \
                     matching F5/F6's own destination convention"
                );
                assert!(
                    state.link_path_cursor_at_end(cx),
                    "UAT: the cursor must start at the end of the pre-filled link path, not at \
                     the very beginning"
                );
            });
        });
    }

    /// The end-to-end symlink: a real link on real disk whose
    /// `std::fs::read_link` is exactly the source's own absolute path (a
    /// plain path, not `VPath`'s `file://` URI `Display` form -- see
    /// `crate::link_dialog`'s module doc comment).
    #[gpui::test]
    fn confirming_a_symlink_creates_a_real_link_pointing_at_the_source(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            let source = source_dir.path().join("target.txt");
            std::fs::write(&source, b"real file").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(SymlinkDialog);
            let _state = open_link_dialog_state(&workspace, vcx);
            press_enter(vcx);

            let link = dest_dir.path().join("target.txt");
            wait_until(vcx, |vcx| {
                std::fs::symlink_metadata(&link).is_ok()
                    && workspace.read_with(vcx, |ws, _| ws.link_dialog.is_none())
            });

            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "lstat must report a symlink -- metadata() would follow it"
            );
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                source,
                "the target stored in the link is the source's plain absolute path"
            );
            assert_eq!(
                std::fs::read(&link).unwrap(),
                b"real file",
                "and following it must reach the real content"
            );
        });
    }

    #[gpui::test]
    fn escaping_the_symlink_dialog_creates_nothing(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("target.txt"), b"real file").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(SymlinkDialog);
            let _state = open_link_dialog_state(&workspace, vcx);

            press_escape(vcx);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.link_dialog.is_none()));
            assert!(
                std::fs::symlink_metadata(dest_dir.path().join("target.txt")).is_err(),
                "Escape must enqueue nothing at all"
            );

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| assert!(left_handle.is_focused(window)));
        });
    }

    #[gpui::test]
    fn ctrl_shift_h_opens_the_hardlink_dialog_on_the_cursor_entry(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("target.txt"), b"real file").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(HardlinkDialog);
            let state = open_link_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(state.kind(), LinkKind::Hardlink);
                assert_eq!(
                    state.link_path_value(cx),
                    dest_dir
                        .path()
                        .join("target.txt")
                        .to_string_lossy()
                        .into_owned()
                );
            });
        });
    }

    /// The end-to-end hardlink: both names resolving to the same inode with
    /// `nlink == 2`, the same assertions `duet-ops`' own
    /// `hardlink_creates_a_second_name_for_the_same_inode` makes about the
    /// planner, now made about the whole UI path. The link is deliberately
    /// created *inside the source directory* rather than the other panel's:
    /// a hardlink cannot cross filesystems, and two independent `TempDir`s
    /// are not guaranteed to share one.
    #[gpui::test]
    fn confirming_a_hardlink_creates_a_second_name_for_the_same_inode(cx: &mut TestAppContext) {
        use std::os::unix::fs::MetadataExt as _;

        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            let source = source_dir.path().join("target.txt");
            std::fs::write(&source, b"shared content").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(HardlinkDialog);
            let state = open_link_dialog_state(&workspace, vcx);
            let link = source_dir.path().join("alias.txt");
            state.update_in(vcx, |state, window, cx| {
                state.set_link_path_value(&link.to_string_lossy(), window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                link.is_file() && workspace.read_with(vcx, |ws, _| ws.link_dialog.is_none())
            });

            let source_meta = std::fs::metadata(&source).unwrap();
            let link_meta = std::fs::metadata(&link).unwrap();
            assert_eq!(
                source_meta.ino(),
                link_meta.ino(),
                "both names must resolve to the same inode"
            );
            assert_eq!(source_meta.nlink(), 2);

            std::fs::write(&source, b"changed").unwrap();
            assert_eq!(
                std::fs::read(&link).unwrap(),
                b"changed",
                "a write through either name must be visible through the other"
            );
        });
    }

    #[gpui::test]
    fn escaping_the_hardlink_dialog_creates_nothing(cx: &mut TestAppContext) {
        use std::os::unix::fs::MetadataExt as _;

        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("target.txt"), b"real file").unwrap();
            focus_left_with_right_panel_at(&workspace, vcx, source_dir.path(), dest_dir.path());

            vcx.dispatch_action(HardlinkDialog);
            let state = open_link_dialog_state(&workspace, vcx);
            let link = source_dir.path().join("alias.txt");
            state.update_in(vcx, |state, window, cx| {
                state.set_link_path_value(&link.to_string_lossy(), window, cx);
            });

            press_escape(vcx);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.link_dialog.is_none()));
            assert!(!link.exists(), "Escape must enqueue nothing at all");
            assert_eq!(
                std::fs::metadata(source_dir.path().join("target.txt"))
                    .unwrap()
                    .nlink(),
                1,
                "the source must still have exactly one name"
            );
        });
    }

    // -- T-5.2.8 Ctrl+A attributes / permissions -------------------------------

    fn open_attributes_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<AttributesDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.attributes_dialog.clone())
            .expect("the attributes dialog must be open by now")
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn chmod(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `Ctrl+A` opens Change Attributes -- **not** select-all
    /// (`docs/keymap-tc.csv` row 16's own "single most-cited keybinding
    /// 'gotcha' in TC" note). Driven by a real keystroke rather than
    /// `dispatch_action`, so this actually proves `bind_workspace_keys`'
    /// `"ctrl-a"` binding resolves from a focused panel; safe to simulate
    /// here for the same reason `the_f8_and_shift_f8_keystrokes_really_open
    /// _the_dialog` is (a panel, not an `InputState`, holds focus at the
    /// moment the keystroke lands).
    #[gpui::test]
    fn ctrl_a_opens_the_attributes_dialog_prefilled_with_the_entrys_real_mode(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("subject.txt");
            std::fs::write(&file, b"x").unwrap();
            chmod(&file, 0o640);
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.simulate_keystrokes("ctrl-a");
            let state = open_attributes_dialog_state(&workspace, vcx);

            state.read_with(vcx, |state, cx| {
                assert_eq!(
                    state.targets(),
                    &[crate::file_table::local_vpath(&file).unwrap()],
                    "with nothing selected, Ctrl+A falls back to the cursor entry"
                );
                assert_eq!(
                    state.mode_octal_value(cx),
                    "640",
                    "the octal field opens showing the file's real current mode"
                );
                assert_eq!(
                    state.mode_symbolic_value(cx),
                    "rw-r-----",
                    "and the symbolic field shows exactly the same bits"
                );
                assert!(
                    !state.recursive(),
                    "recursive apply starts off -- it is the more destructive choice"
                );
            });
        });
    }

    /// This task's AC, verbatim: "octal and symbolic entry agree." Driven
    /// through the real `InputState::set_value` -> `InputEvent::Change` ->
    /// `cx.subscribe_in` path, not by calling the sync method directly.
    #[gpui::test]
    fn typing_in_either_mode_field_live_updates_the_other(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("subject.txt");
            std::fs::write(&file, b"x").unwrap();
            chmod(&file, 0o600);
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);

            // Octal -> symbolic.
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("644", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |state, cx| {
                assert_eq!(state.mode_symbolic_value(cx), "rw-r--r--");
                assert_eq!(state.mode_octal_value(cx), "644", "and stays put itself");
            });

            // Symbolic -> octal, including a special bit, which must come
            // back as the four-digit rendering.
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_symbolic_value("rwsr-xr-x", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |state, cx| {
                assert_eq!(state.mode_octal_value(cx), "4755");
                assert_eq!(state.mode_symbolic_value(cx), "rwsr-xr-x");
            });

            // Garbage in one field must leave the other alone rather than
            // propagating nonsense.
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("99z", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |state, cx| {
                assert_eq!(
                    state.mode_symbolic_value(cx),
                    "rwsr-xr-x",
                    "an unparseable octal value must not blank or corrupt the symbolic field"
                );
            });

            // Clearing one field *does* propagate -- both must agree that
            // the mode is being left unchanged.
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("", window, cx);
            });
            let _ = vcx.update(|window, cx| window.draw(cx));
            state.read_with(vcx, |state, cx| {
                assert_eq!(state.mode_symbolic_value(cx), "");
            });
        });
    }

    /// End-to-end, non-recursive: a real mode change on real disk through
    /// the real off-thread `plan_attributes` -> `QueueManager::enqueue` ->
    /// `execute()` path, asserted with a real `stat`.
    #[gpui::test]
    fn confirming_a_mode_change_changes_exactly_that_file_on_disk(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let subject = dir.path().join("a-subject.txt");
            let bystander = dir.path().join("z-bystander.txt");
            std::fs::write(&subject, b"x").unwrap();
            std::fs::write(&bystander, b"y").unwrap();
            chmod(&subject, 0o600);
            chmod(&bystander, 0o600);
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("754", window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                mode_of(&subject) == 0o754
                    && workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_none())
            });
            assert_eq!(
                mode_of(&bystander),
                0o600,
                "only the cursor entry was targeted -- the other file must be untouched"
            );
        });
    }

    /// The AC's other half: "recursive apply runs through the operation
    /// queue, not synchronously." The toggle is dispatched as its real
    /// `ToggleRecursiveApply` action (`Ctrl+R` inside the dialog's own key
    /// context); everything beneath the directory must end up changed, at
    /// every depth.
    #[gpui::test]
    fn confirming_a_recursive_mode_change_reaches_every_entry_beneath_the_directory(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("tree");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("top.txt"), b"x").unwrap();
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(root.join("sub/deep.txt"), b"x").unwrap();
            let everything = [
                root.clone(),
                root.join("top.txt"),
                root.join("sub"),
                root.join("sub/deep.txt"),
            ];
            for p in &everything {
                chmod(p, 0o700);
            }
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);
            vcx.dispatch_action(ToggleRecursiveApply);
            state.read_with(vcx, |state, _| {
                assert!(state.recursive(), "Ctrl+R must have flipped the toggle on");
            });
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("755", window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                everything.iter().all(|p| mode_of(p) == 0o755)
                    && workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_none())
            });
        });
    }

    /// Without the toggle, the very same directory target must leave its
    /// contents alone -- the other side of the recursive test above.
    #[gpui::test]
    fn confirming_without_the_recursive_toggle_leaves_the_contents_alone(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("tree");
            std::fs::create_dir(&root).unwrap();
            let inside = root.join("inside.txt");
            std::fs::write(&inside, b"x").unwrap();
            chmod(&root, 0o700);
            chmod(&inside, 0o600);
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("755", window, cx);
            });
            press_enter(vcx);

            wait_until(vcx, |vcx| {
                mode_of(&root) == 0o755
                    && workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_none())
            });
            assert_eq!(
                mode_of(&inside),
                0o600,
                "recursive apply is off by default -- the contents must be untouched"
            );
        });
    }

    /// The timestamp-editing half of the AC, read back through a real
    /// `stat` on real disk.
    #[gpui::test]
    fn confirming_a_timestamp_change_is_readable_back_from_disk(cx: &mut TestAppContext) {
        use std::os::unix::fs::MetadataExt as _;

        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("dated.txt");
            std::fs::write(&file, b"x").unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);

            // The dialog opens pre-filled with the file's real mtime, in
            // exactly the format the panel's own Date column uses.
            let current_mtime = std::fs::metadata(&file).unwrap().mtime();
            state.read_with(vcx, |state, cx| {
                let mut expected = String::new();
                crate::file_table::write_date(&mut expected, current_mtime);
                assert_eq!(state.modified_value(cx), expected);
            });

            state.update_in(vcx, |state, window, cx| {
                state.set_modified_value("2001-02-03 04:05", window, cx);
            });
            press_enter(vcx);

            // 2001-02-03 04:05:00 UTC.
            let expected_secs = 981_173_100i64;
            wait_until(vcx, |vcx| {
                std::fs::metadata(&file).unwrap().mtime() == expected_secs
                    && workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_none())
            });
        });
    }

    /// With more than one target there is no single "current value" to
    /// show, so both mode fields (and both timestamp fields) open blank --
    /// and a blank field means "leave unchanged", so confirming without
    /// typing anything must enqueue nothing at all.
    #[gpui::test]
    fn a_multi_selection_opens_blank_and_confirming_it_untouched_enqueues_nothing(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let a = dir.path().join("a.txt");
            let b = dir.path().join("b.txt");
            std::fs::write(&a, b"x").unwrap();
            std::fs::write(&b, b"y").unwrap();
            chmod(&a, 0o600);
            chmod(&b, 0o640);
            focus_left_panel_at(&workspace, vcx, dir.path());

            // Ctrl++ in this app, not Ctrl+A -- which is the whole point of
            // the TC binding this dialog is behind.
            vcx.dispatch_action(crate::file_table::SelectAll);
            let _ = vcx.update(|window, cx| window.draw(cx));

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);
            state.read_with(vcx, |state, cx| {
                assert_eq!(state.targets().len(), 2, "both selected entries");
                assert_eq!(
                    state.mode_octal_value(cx),
                    "",
                    "two entries with different modes have no single value to pre-fill"
                );
                assert_eq!(state.mode_symbolic_value(cx), "");
                assert_eq!(state.modified_value(cx), "");
            });

            press_enter(vcx);
            wait_until(vcx, |vcx| {
                workspace.read_with(vcx, |ws, _| ws.attributes_dialog.is_none())
            });

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.queue.snapshot().is_empty(),
                    "an entirely blank form is a no-op close, not an enqueued job"
                );
            });
            assert_eq!(mode_of(&a), 0o600, "and nothing on disk changed");
            assert_eq!(mode_of(&b), 0o640);
        });
    }

    #[gpui::test]
    fn escaping_the_attributes_dialog_changes_nothing_and_restores_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("subject.txt");
            std::fs::write(&file, b"x").unwrap();
            chmod(&file, 0o600);
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let state = open_attributes_dialog_state(&workspace, vcx);
            state.update_in(vcx, |state, window, cx| {
                state.set_mode_octal_value("777", window, cx);
            });

            press_escape(vcx);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| {
                assert!(ws.attributes_dialog.is_none());
                assert!(
                    ws.queue.snapshot().is_empty(),
                    "Escape must enqueue nothing at all"
                );
            });
            assert_eq!(mode_of(&file), 0o600, "the mode on disk must be unchanged");

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "cancelling must restore focus to the panel Ctrl+A was pressed in"
                );
            });
        });
    }

    /// An empty listing has no selection and no cursor entry, so there is
    /// nothing to change attributes on -- a no-op with a toast, not a
    /// dialog on nothing.
    #[gpui::test]
    fn ctrl_a_in_an_empty_directory_opens_nothing(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            focus_left_panel_at(&workspace, vcx, dir.path());

            vcx.dispatch_action(AttributesDialog);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.attributes_dialog.is_none()));
        });
    }

    // -- T-5.2.4 error/skip report + re-run failed ----------------------------

    /// Plans and enqueues a real permanent-delete job against the real
    /// `LocalFs`, exactly the way `Workspace::start_delete_job` does, and
    /// returns its `JobId`. Same shape as [`enqueue_slow_copy`] right
    /// above -- the plan runs on the ops runtime, and the caller gets the
    /// id back synchronously so it has something to wait on.
    ///
    /// Deliberately enqueued with no `ConflictResolver` (matching
    /// `delete_dialog::spawn_delete_job`'s own `None`), so any conflict
    /// falls through to `PlanOptions::default_conflict` rather than
    /// blocking an executor thread on a dialog no test is driving.
    fn enqueue_permanent_delete(
        tokio_handle: &tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        targets: Vec<VPath>,
        state_dir: PathBuf,
    ) -> duet_ops::JobId {
        let (tx, rx) = std::sync::mpsc::channel();
        tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let cancel = duet_ops::CancelToken::new();
            let plan = duet_ops::plan_delete(
                fs.as_ref(),
                &targets,
                duet_ops::DeleteMode::Permanent,
                duet_ops::PlanOptions::default(),
                &cancel,
            )
            .await
            .expect("plan_delete over real tempdirs must succeed");
            let id = queue.enqueue(
                JobKind::Delete { permanent: true },
                plan,
                0,
                fs,
                state_dir,
                1,
                None,
            );
            let _ = tx.send(id);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("plan_delete + enqueue must complete quickly")
    }

    /// The copy counterpart of [`enqueue_permanent_delete`]: a real
    /// `plan_copy` of `sources` into `dst` against the real, un-slowed
    /// `LocalFs`. With no resolver and `PlanOptions::default()`
    /// (`ConflictPolicy::Skip`), a destination that already exists makes
    /// the job end `CompletedWithSkips` with a real `SkipEntry` -- which
    /// is exactly the second half of what T-5.2.4's report has to show.
    fn enqueue_copy(
        tokio_handle: &tokio::runtime::Handle,
        queue: Arc<QueueManager>,
        sources: Vec<VPath>,
        dst: VPath,
        state_dir: PathBuf,
    ) -> duet_ops::JobId {
        let (tx, rx) = std::sync::mpsc::channel();
        tokio_handle.spawn(async move {
            let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
            let cancel = duet_ops::CancelToken::new();
            let plan = duet_ops::plan_copy(
                fs.as_ref(),
                &sources,
                &dst,
                duet_ops::PlanOptions::default(),
                &cancel,
            )
            .await
            .expect("plan_copy over real tempdirs must succeed");
            let id = queue.enqueue(JobKind::Copy, plan, 0, fs, state_dir, 1, None);
            let _ = tx.send(id);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("plan_copy + enqueue must complete quickly")
    }

    fn queue_state_dir_and_handle(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> (Arc<QueueManager>, PathBuf, tokio::runtime::Handle) {
        workspace.read_with(vcx, |ws, _| {
            (
                ws.queue.clone(),
                ws.state_dir
                    .clone()
                    .expect("test env always resolves a state dir"),
                ws.tokio_handle.clone(),
            )
        })
    }

    /// A directory whose write bit is off, holding `count` files: removing
    /// anything inside it fails with `EACCES` regardless of the files' own
    /// modes, which is `duet_ops::deleter`'s own
    /// `a_permission_denied_removal_surfaces_as_a_real_failure` setup and
    /// the most direct way to get a genuinely `Failed` job with real
    /// `StepFailure`s out of the real executor.
    ///
    /// Returns the locked directory and the paths inside it. **Every
    /// caller must `unlock_dir` before its `TempDir` drops**, or the
    /// cleanup itself fails for the same reason the job did.
    fn locked_dir_with_files(root: &Path, count: usize) -> (PathBuf, Vec<PathBuf>) {
        use std::os::unix::fs::PermissionsExt as _;
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let files: Vec<PathBuf> = (0..count)
            .map(|i| {
                let path = locked.join(format!("doomed{i}.txt"));
                std::fs::write(&path, b"bye").unwrap();
                path
            })
            .collect();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        (locked, files)
    }

    fn unlock_dir(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn wait_for_terminal(
        vcx: &mut VisualTestContext,
        queue: &QueueManager,
        job_id: duet_ops::JobId,
    ) -> JobOutcome {
        wait_until(vcx, |_vcx| {
            matches!(
                queue.job(job_id).map(|j| j.state),
                Some(JobState::Terminal { .. })
            )
        });
        match queue
            .job(job_id)
            .expect("the job must still be listed")
            .state
        {
            JobState::Terminal { outcome, .. } => outcome,
            other => panic!("expected a terminal job, got {other:?}"),
        }
    }

    /// Opens the operation manager on a queue holding exactly one job (so
    /// the cursor's default row 0 is unambiguously that job) and drills
    /// into its report with the real `OperationManagerOpenReport` action.
    fn open_report_via_the_manager(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        job_id: duet_ops::JobId,
    ) {
        focus_left_panel(workspace, vcx);
        vcx.dispatch_action(OpenOperationManager);
        let _ = vcx.update(|window, cx| window.draw(cx));
        let manager = workspace
            .read_with(vcx, |ws, _| ws.operation_manager.clone())
            .expect("Ctrl+O must open the manager");
        wait_until(vcx, |vcx| {
            manager.read_with(vcx, |m, _| {
                m.jobs_for_test().first().map(|j| j.id) == Some(job_id)
            })
        });
        vcx.dispatch_action(OperationManagerOpenReport);
        let _ = vcx.update(|window, cx| window.draw(cx));
    }

    fn job_report_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<JobReportDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.job_report_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.job_report_dialog.clone())
            .expect("the report dialog must be open by now")
    }

    /// This task's own AC, as literally as a test can state it: a job that
    /// fails on *every* one of its targets ends with **one** list holding
    /// every failure, not one dialog each. Real tempdirs, the real
    /// `LocalFs`, the real `plan_delete` -> `QueueManager::enqueue` ->
    /// `execute()` path, and the report read straight off the real job.
    #[gpui::test]
    fn a_job_that_fails_on_every_target_ends_with_one_actionable_list(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let (locked, files) = locked_dir_with_files(dir.path(), 5);
            let (queue, state_dir, tokio_handle) = queue_state_dir_and_handle(&workspace, vcx);

            let targets: Vec<VPath> = files
                .iter()
                .map(|p| crate::file_table::local_vpath(p).unwrap())
                .collect();
            let job_id = enqueue_permanent_delete(&tokio_handle, queue.clone(), targets, state_dir);

            let outcome = wait_for_terminal(vcx, &queue, job_id);
            assert_eq!(
                outcome,
                JobOutcome::Failed,
                "a delete denied on every target must end Failed"
            );

            open_report_via_the_manager(&workspace, vcx, job_id);
            let state = job_report_state(&workspace, vcx);

            state.read_with(vcx, |state, _| {
                assert_eq!(
                    state.title_text(),
                    "Delete (permanent) \u{2014} 5 errors",
                    "one headline for the whole job, not one per failure"
                );
                let lines = state.error_lines();
                assert_eq!(lines.len(), 5, "every failure gets its own row: {lines:?}");
                for (file, line) in files.iter().zip(&lines) {
                    assert!(
                        line.contains(&file.to_string_lossy().to_string()),
                        "each row must name its own path: {line}"
                    );
                    assert!(
                        line.contains("permission denied"),
                        "and classify it: {line}"
                    );
                }
                assert!(state.skip_lines().is_empty());
                assert_eq!(state.kind(), JobKind::Delete { permanent: true });
            });

            // Opening the report replaces the manager -- the disclosed
            // judgment call in `job_report_dialog`'s module doc comment.
            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.operation_manager.is_none(),
                    "drilling into the report closes the manager it came from"
                );
            });

            unlock_dir(&locked);
        });
    }

    /// The skip half of the same view: a copy whose destination already
    /// exists ends `CompletedWithSkips` under `PlanOptions::default()`'s
    /// `ConflictPolicy::Skip`, and the report lists the skipped path and
    /// the executor's own reason -- not an error row.
    #[gpui::test]
    fn a_job_that_skipped_work_reports_the_skips_not_errors(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let source_dir = tempfile::tempdir().unwrap();
            let dest_dir = tempfile::tempdir().unwrap();
            let source = source_dir.path().join("a.txt");
            std::fs::write(&source, b"new").unwrap();
            std::fs::write(dest_dir.path().join("a.txt"), b"already here").unwrap();
            let (queue, state_dir, tokio_handle) = queue_state_dir_and_handle(&workspace, vcx);

            let job_id = enqueue_copy(
                &tokio_handle,
                queue.clone(),
                vec![crate::file_table::local_vpath(&source).unwrap()],
                crate::file_table::local_vpath(dest_dir.path()).unwrap(),
                state_dir,
            );

            assert_eq!(
                wait_for_terminal(vcx, &queue, job_id),
                JobOutcome::CompletedWithSkips
            );

            open_report_via_the_manager(&workspace, vcx, job_id);
            let state = job_report_state(&workspace, vcx);

            state.read_with(vcx, |state, _| {
                assert_eq!(state.title_text(), "Copy \u{2014} 1 skipped");
                assert!(state.error_lines().is_empty());
                let skips = state.skip_lines();
                assert_eq!(skips.len(), 1, "{skips:?}");
                assert!(
                    skips[0].contains("a.txt"),
                    "the skipped row must name the path: {}",
                    skips[0]
                );
            });

            assert_eq!(
                std::fs::read(dest_dir.path().join("a.txt")).unwrap(),
                b"already here",
                "a skip really did leave the destination alone"
            );
        });
    }

    /// The gate: a job that finished cleanly has nothing to report, so
    /// Enter on its row is a silent no-op and the manager stays put --
    /// `docs/commands.md`'s own `job.has_errors` predicate, enforced.
    #[gpui::test]
    fn a_cleanly_completed_job_has_no_report_to_open(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let victim = dir.path().join("doomed.txt");
            std::fs::write(&victim, b"bye").unwrap();
            let (queue, state_dir, tokio_handle) = queue_state_dir_and_handle(&workspace, vcx);

            let job_id = enqueue_permanent_delete(
                &tokio_handle,
                queue.clone(),
                vec![crate::file_table::local_vpath(&victim).unwrap()],
                state_dir,
            );
            assert_eq!(
                wait_for_terminal(vcx, &queue, job_id),
                JobOutcome::Completed
            );

            open_report_via_the_manager(&workspace, vcx, job_id);

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.job_report_dialog.is_none(),
                    "there is nothing to show, so Enter must do nothing at all"
                );
                assert!(
                    ws.operation_manager.is_some(),
                    "and the manager the keystroke was aimed at must stay open"
                );
            });
        });
    }

    /// The other half of the AC's "actionable": re-running from the report
    /// genuinely re-attempts the failed work, and -- with the permission
    /// that caused the failure restored first -- genuinely succeeds this
    /// time. Asserted against real disk state, not merely against a second
    /// job having been enqueued.
    #[gpui::test]
    fn re_running_a_failed_delete_succeeds_once_the_permission_is_restored(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let (locked, files) = locked_dir_with_files(dir.path(), 2);
            let (queue, state_dir, tokio_handle) = queue_state_dir_and_handle(&workspace, vcx);

            let targets: Vec<VPath> = files
                .iter()
                .map(|p| crate::file_table::local_vpath(p).unwrap())
                .collect();
            let job_id = enqueue_permanent_delete(&tokio_handle, queue.clone(), targets, state_dir);
            assert_eq!(wait_for_terminal(vcx, &queue, job_id), JobOutcome::Failed);
            assert!(
                files.iter().all(|f| f.exists()),
                "nothing may have been deleted by the denied job"
            );

            open_report_via_the_manager(&workspace, vcx, job_id);
            let _state = job_report_state(&workspace, vcx);

            // The user's real fix, between reading the report and hitting
            // R: make the directory writable again.
            unlock_dir(&locked);

            vcx.dispatch_action(RerunFailedItems);
            wait_until(vcx, |vcx| {
                files.iter().all(|f| !f.exists())
                    && workspace.read_with(vcx, |ws, _| ws.job_report_dialog.is_none())
            });

            let ids: Vec<duet_ops::JobId> = queue.snapshot().into_iter().map(|j| j.id).collect();
            assert_eq!(
                ids.len(),
                2,
                "the retry is a real, separate job, not a mutation of the original"
            );
            let retry_id = *ids.iter().find(|id| **id != job_id).unwrap();
            assert_eq!(
                wait_for_terminal(vcx, &queue, retry_id),
                JobOutcome::Completed,
                "and it must genuinely succeed this time, not fail the same way"
            );
            let retry = queue.job(retry_id).unwrap();
            assert_eq!(
                retry.plan.steps.len(),
                2,
                "the retry plan holds exactly the two steps that failed"
            );
            assert_eq!(retry.kind, JobKind::Delete { permanent: true });
        });
    }

    #[gpui::test]
    fn escaping_the_job_report_re_runs_nothing_and_restores_focus(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            let dir = tempfile::tempdir().unwrap();
            let (locked, files) = locked_dir_with_files(dir.path(), 1);
            let (queue, state_dir, tokio_handle) = queue_state_dir_and_handle(&workspace, vcx);

            let job_id = enqueue_permanent_delete(
                &tokio_handle,
                queue.clone(),
                vec![crate::file_table::local_vpath(&files[0]).unwrap()],
                state_dir,
            );
            assert_eq!(wait_for_terminal(vcx, &queue, job_id), JobOutcome::Failed);

            open_report_via_the_manager(&workspace, vcx, job_id);
            let _state = job_report_state(&workspace, vcx);

            // Unlocked *before* Escape, so that "the file is still there"
            // afterwards can only mean "nothing was re-run" -- not "a
            // retry ran and was denied again".
            unlock_dir(&locked);

            vcx.dispatch_action(CloseJobReport);
            let _ = vcx.update(|window, cx| window.draw(cx));

            workspace.read_with(vcx, |ws, _| assert!(ws.job_report_dialog.is_none()));
            assert!(files[0].exists(), "Escape must enqueue nothing at all");
            assert_eq!(
                queue.snapshot().len(),
                1,
                "and no second job may have appeared"
            );

            let left_handle =
                workspace.read_with(vcx, |ws, cx| ws.left_panel.read(cx).active_focus_handle(cx));
            vcx.update(|window, _cx| {
                assert!(
                    left_handle.is_focused(window),
                    "closing the report must restore focus to the panel Ctrl+O was pressed in"
                );
            });
        });
    }

    // -- T-5.2.5 startup interrupted-operation recovery -----------------------

    /// Writes a real, still-dangling journal for `job_id` -- `JobStarted`
    /// plus one `Intent` for `plan.steps[step_index]`, deliberately no
    /// `Completion` and no `JobFinished` -- the exact "crashed mid-step"
    /// shape `duet_ops::journal`'s own tests already establish. Shared by
    /// every test below that needs a real, on-disk crashed job to recover
    /// from.
    fn write_dangling_journal(
        state_dir: &Path,
        job_id: JobId,
        kind: JobKind,
        plan: &duet_ops::Plan,
        step_index: u32,
        partial_name: Option<&str>,
    ) {
        let mut journal = Journal::open(job_id, state_dir).unwrap();
        journal
            .append(&JournalRecord::JobStarted {
                job_id,
                started_at: Timestamp::EPOCH,
                plan: plan.clone(),
                kind,
            })
            .unwrap();
        journal
            .append(&JournalRecord::Intent {
                step_index,
                step: plan.steps[step_index as usize].clone(),
                partial_name: partial_name.map(str::to_string),
            })
            .unwrap();
    }

    fn recovery_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<RecoveryDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.recovery_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.recovery_dialog.clone())
            .expect("the recovery dialog must be open by now")
    }

    /// A pre-seeded crashed journal (`JobStarted` + a dangling `Intent`, no
    /// `Completion`, no `JobFinished`) makes `Workspace::new` auto-open the
    /// recovery dialog with exactly that report.
    #[gpui::test]
    fn a_crashed_journal_makes_workspace_new_auto_open_the_recovery_dialog(
        cx: &mut TestAppContext,
    ) {
        let job_id = JobId(101);
        with_configured_workspace_and_recovery_seed(
            cx,
            move |state_dir| {
                let plan = duet_ops::Plan::new(
                    vec![duet_ops::Step::CreateDir {
                        dest: crate::file_table::local_vpath(Path::new(
                            "/tmp/duet-recovery-test-auto-open",
                        ))
                        .unwrap(),
                        mode: None,
                    }],
                    duet_ops::PlanOptions::default(),
                );
                write_dangling_journal(state_dir, job_id, JobKind::CreateDir, &plan, 0, None);
            },
            |workspace, vcx, _data_dir| {
                let state = recovery_dialog_state(&workspace, vcx);
                state.read_with(vcx, |state, _cx| {
                    let reports = state.reports();
                    assert_eq!(reports.len(), 1);
                    assert_eq!(reports[0].job_id, job_id);
                    assert_eq!(reports[0].kind, JobKind::CreateDir);
                    assert_eq!(reports[0].incomplete_steps, vec![0]);
                });
            },
        );
    }

    /// Up/Down move the cursor by one row, clamping at both ends rather
    /// than wrapping -- same convention `crate::file_table::
    /// move_cursor_by`/`crate::operation_manager::cursor_up`/`cursor_down`
    /// already establish for a plain row cursor.
    #[gpui::test]
    fn cursor_up_and_down_clamp_at_both_ends_of_the_report_list(cx: &mut TestAppContext) {
        with_configured_workspace_and_recovery_seed(
            cx,
            |state_dir| {
                for (job_id, target) in [
                    (JobId(11), "/tmp/duet-recovery-test-cursor-a"),
                    (JobId(12), "/tmp/duet-recovery-test-cursor-b"),
                ] {
                    let plan = duet_ops::Plan::new(
                        vec![duet_ops::Step::CreateDir {
                            dest: crate::file_table::local_vpath(Path::new(target)).unwrap(),
                            mode: None,
                        }],
                        duet_ops::PlanOptions::default(),
                    );
                    write_dangling_journal(state_dir, job_id, JobKind::CreateDir, &plan, 0, None);
                }
            },
            |workspace, vcx, _data_dir| {
                let state = recovery_dialog_state(&workspace, vcx);
                state.read_with(vcx, |state, _cx| {
                    assert_eq!(state.reports().len(), 2);
                    assert_eq!(state.cursor(), 0);
                });

                // Doesn't wrap past the top.
                vcx.dispatch_action(RecoveryDialogCursorUp);
                state.read_with(vcx, |state, _cx| assert_eq!(state.cursor(), 0));

                vcx.dispatch_action(RecoveryDialogCursorDown);
                state.read_with(vcx, |state, _cx| assert_eq!(state.cursor(), 1));

                // Doesn't run past the bottom either.
                vcx.dispatch_action(RecoveryDialogCursorDown);
                state.read_with(vcx, |state, _cx| assert_eq!(state.cursor(), 1));

                vcx.dispatch_action(RecoveryDialogCursorUp);
                state.read_with(vcx, |state, _cx| assert_eq!(state.cursor(), 0));
            },
        );
    }

    /// `Space` toggles the cursor row's inline "inspect" expansion --
    /// no third overlay layer, just a field flip. See `crate::
    /// recovery_dialog`'s module doc comment for why this is inline rather
    /// than a second dialog.
    #[gpui::test]
    fn space_toggles_the_cursor_rows_inline_inspect_detail(cx: &mut TestAppContext) {
        let job_id = JobId(13);
        with_configured_workspace_and_recovery_seed(
            cx,
            move |state_dir| {
                let plan = duet_ops::Plan::new(
                    vec![duet_ops::Step::CreateDir {
                        dest: crate::file_table::local_vpath(Path::new(
                            "/tmp/duet-recovery-test-inspect",
                        ))
                        .unwrap(),
                        mode: None,
                    }],
                    duet_ops::PlanOptions::default(),
                );
                write_dangling_journal(state_dir, job_id, JobKind::CreateDir, &plan, 0, None);
            },
            |workspace, vcx, _data_dir| {
                let state = recovery_dialog_state(&workspace, vcx);
                state.read_with(vcx, |state, _cx| assert!(!state.inspecting()));

                vcx.dispatch_action(RecoveryDialogToggleInspect);
                state.read_with(vcx, |state, _cx| assert!(state.inspecting()));

                vcx.dispatch_action(RecoveryDialogToggleInspect);
                state.read_with(vcx, |state, _cx| assert!(!state.inspecting()));
            },
        );
    }

    /// Enter (`RecoveryDialogResume`) on a crashed `CreateDir` actually
    /// creates the directory on disk, removes that report from the dialog
    /// (closing it, since it was the only one), and a *fresh*
    /// `JournalReader::scan` of the same `state_dir` shows the original
    /// job's report fully accounted for -- `incomplete_steps` empty,
    /// `last_outcome == Some(JobOutcome::Cancelled)` -- proving `Journal::
    /// resolve` actually ran, not just that the UI forgot about it.
    #[gpui::test]
    fn resuming_a_crashed_create_dir_creates_it_and_resolves_the_original_journal(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let new_dir_path = dir.path().join("resumed-dir");
        let job_id = JobId(202);

        with_configured_workspace_and_recovery_seed(
            cx,
            {
                let new_dir_path = new_dir_path.clone();
                move |state_dir| {
                    let plan = duet_ops::Plan::new(
                        vec![duet_ops::Step::CreateDir {
                            dest: crate::file_table::local_vpath(&new_dir_path).unwrap(),
                            mode: None,
                        }],
                        duet_ops::PlanOptions::default(),
                    );
                    write_dangling_journal(state_dir, job_id, JobKind::CreateDir, &plan, 0, None);
                }
            },
            {
                let new_dir_path = new_dir_path.clone();
                move |workspace, vcx, _data_dir| {
                    let _ = recovery_dialog_state(&workspace, vcx);
                    vcx.dispatch_action(RecoveryDialogResume);

                    let state_dir = duet_config::paths::duet_state_dir().unwrap();
                    wait_until(vcx, |vcx| {
                        new_dir_path.is_dir()
                            && workspace.read_with(vcx, |ws, _| ws.recovery_dialog.is_none())
                    });

                    let reports = JournalReader::scan(&state_dir).unwrap();
                    let original = reports
                        .iter()
                        .find(|r| r.job_id == job_id)
                        .expect("the original job's journal must still exist");
                    assert!(
                        original.incomplete_steps.is_empty(),
                        "resume must resolve the original job's dangling intent"
                    );
                    assert_eq!(original.last_outcome, Some(JobOutcome::Cancelled));
                }
            },
        );
    }

    /// `D` (`RecoveryDialogDiscard`) on a crashed copy with a real orphaned
    /// `.duet-partial-*` file actually deletes it from disk, removes that
    /// report from the dialog, and a fresh rescan shows the original job
    /// fully resolved the same way the resume test proves.
    #[gpui::test]
    fn discarding_a_crashed_copy_deletes_the_orphaned_partial_and_resolves_the_original_journal(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("dst");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest_file = dest_dir.join("a.txt");
        let partial_name = ".duet-partial-abc123-a.txt";
        let partial_path = dest_dir.join(partial_name);
        std::fs::write(&partial_path, b"partial content").unwrap();
        let job_id = JobId(303);

        with_configured_workspace_and_recovery_seed(
            cx,
            {
                let dest_file = dest_file.clone();
                move |state_dir| {
                    let plan = duet_ops::Plan::new(
                        vec![duet_ops::Step::CopyFile {
                            source: crate::file_table::local_vpath(Path::new(
                                "/tmp/duet-recovery-test-nonexistent-source",
                            ))
                            .unwrap(),
                            dest: crate::file_table::local_vpath(&dest_file).unwrap(),
                            size: 4,
                            conflict: None,
                        }],
                        duet_ops::PlanOptions::default(),
                    );
                    write_dangling_journal(
                        state_dir,
                        job_id,
                        JobKind::Copy,
                        &plan,
                        0,
                        Some(partial_name),
                    );
                }
            },
            {
                let partial_path = partial_path.clone();
                move |workspace, vcx, _data_dir| {
                    let _ = recovery_dialog_state(&workspace, vcx);
                    vcx.dispatch_action(RecoveryDialogDiscard);

                    let state_dir = duet_config::paths::duet_state_dir().unwrap();
                    wait_until(vcx, |vcx| {
                        !partial_path.exists()
                            && workspace.read_with(vcx, |ws, _| ws.recovery_dialog.is_none())
                    });

                    let reports = JournalReader::scan(&state_dir).unwrap();
                    let original = reports
                        .iter()
                        .find(|r| r.job_id == job_id)
                        .expect("the original job's journal must still exist");
                    assert!(
                        original.incomplete_steps.is_empty(),
                        "discard must resolve the original job's dangling intent"
                    );
                    assert_eq!(original.last_outcome, Some(JobOutcome::Cancelled));
                }
            },
        );
    }

    /// Escape closes the dialog without resolving anything -- a *fresh*
    /// `JournalReader::scan` (called directly here, not through the UI)
    /// must still report the exact same `incomplete_steps`/
    /// `orphaned_partials` as before Escape was pressed, proving nothing
    /// was silently written. See `crate::recovery_dialog`'s module doc
    /// comment for why this is the deliberate, disclosed behaviour.
    #[gpui::test]
    fn escaping_the_recovery_dialog_resolves_nothing_and_the_report_is_unmutated(
        cx: &mut TestAppContext,
    ) {
        let job_id = JobId(404);
        with_configured_workspace_and_recovery_seed(
            cx,
            move |state_dir| {
                let plan = duet_ops::Plan::new(
                    vec![duet_ops::Step::CreateDir {
                        dest: crate::file_table::local_vpath(Path::new(
                            "/tmp/duet-recovery-test-escape",
                        ))
                        .unwrap(),
                        mode: None,
                    }],
                    duet_ops::PlanOptions::default(),
                );
                write_dangling_journal(state_dir, job_id, JobKind::CreateDir, &plan, 0, None);
            },
            |workspace, vcx, _data_dir| {
                let _ = recovery_dialog_state(&workspace, vcx);
                let state_dir = duet_config::paths::duet_state_dir().unwrap();
                let before_report = JournalReader::scan(&state_dir)
                    .unwrap()
                    .into_iter()
                    .find(|r| r.job_id == job_id)
                    .expect("must exist before Escape");

                vcx.dispatch_action(CloseRecoveryDialog);
                let _ = vcx.update(|window, cx| window.draw(cx));

                workspace.read_with(vcx, |ws, _| assert!(ws.recovery_dialog.is_none()));

                let after_report = JournalReader::scan(&state_dir)
                    .unwrap()
                    .into_iter()
                    .find(|r| r.job_id == job_id)
                    .expect("must still exist after Escape -- nothing was resolved");
                assert_eq!(
                    after_report.incomplete_steps,
                    before_report.incomplete_steps
                );
                assert_eq!(
                    after_report.orphaned_partials,
                    before_report.orphaned_partials
                );
                assert_eq!(after_report.last_outcome, before_report.last_outcome);
            },
        );
    }

    /// A journal representing a cleanly finished job (`JobStarted` + a
    /// matching `Intent`+`Completion` for its one step + `JobFinished {
    /// outcome: Completed }`, no orphaned partials) does not cause the
    /// dialog to open at all.
    #[gpui::test]
    fn a_cleanly_finished_job_does_not_open_the_recovery_dialog(cx: &mut TestAppContext) {
        with_configured_workspace_and_recovery_seed(
            cx,
            |state_dir| {
                let plan = duet_ops::Plan::new(
                    vec![duet_ops::Step::CreateDir {
                        dest: crate::file_table::local_vpath(Path::new(
                            "/tmp/duet-recovery-test-finished",
                        ))
                        .unwrap(),
                        mode: None,
                    }],
                    duet_ops::PlanOptions::default(),
                );
                let job_id = JobId(505);
                let mut journal = Journal::open(job_id, state_dir).unwrap();
                journal
                    .append(&JournalRecord::JobStarted {
                        job_id,
                        started_at: Timestamp::EPOCH,
                        plan: plan.clone(),
                        kind: JobKind::CreateDir,
                    })
                    .unwrap();
                journal
                    .append(&JournalRecord::Intent {
                        step_index: 0,
                        step: plan.steps[0].clone(),
                        partial_name: None,
                    })
                    .unwrap();
                journal
                    .append(&JournalRecord::Completion {
                        step_index: 0,
                        outcome: StepOutcome::Succeeded,
                    })
                    .unwrap();
                journal
                    .append(&JournalRecord::JobFinished {
                        outcome: JobOutcome::Completed,
                        finished_at: Timestamp::EPOCH,
                    })
                    .unwrap();
            },
            |workspace, vcx, _data_dir| {
                let _ = vcx.update(|window, cx| window.draw(cx));
                workspace.read_with(vcx, |ws, _| {
                    assert!(
                        ws.recovery_dialog.is_none(),
                        "a cleanly finished job with no orphaned partials must not surface \
                         anything"
                    );
                });
            },
        );
    }

    /// A corrupt journal file (a real parse failure, not the expected
    /// torn-trailing-record artifact `JournalReader::scan` already
    /// tolerates -- mirrors `duet_ops::journal`'s own
    /// `scan_treats_a_truncated_trailing_record_as_absent_not_an_error`
    /// test for the exact shape this must differ from) produces a notice
    /// rather than crashing `Workspace::new`, and the dialog does not
    /// open.
    #[gpui::test]
    fn a_corrupt_journal_surfaces_a_notice_instead_of_crashing_workspace_new(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace_and_recovery_seed(
            cx,
            |state_dir| {
                let job_id = JobId(606);
                let path;
                {
                    let mut journal = Journal::open(job_id, state_dir).unwrap();
                    journal
                        .append(&JournalRecord::JobStarted {
                            job_id,
                            started_at: Timestamp::EPOCH,
                            plan: duet_ops::Plan::new(Vec::new(), duet_ops::PlanOptions::default()),
                            kind: JobKind::Copy,
                        })
                        .unwrap();
                    path = journal.path().to_path_buf();
                }
                // A non-JSON line *followed by a valid record*. Since the
                // journal moved to batched writes (2026-09-06), any trailing
                // run of unparseable lines is the tolerated torn-tail crash
                // artifact; what `JournalReader::scan` still rejects as
                // real corruption is garbage with durable records after it
                // (see `duet_ops::journal`'s module doc comment, "Wire
                // format" section), which an append-only file cannot
                // produce by crashing.
                use std::io::Write as _;
                {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap();
                    writeln!(file, "not valid json at all").unwrap();
                }
                Journal::open(job_id, state_dir)
                    .unwrap()
                    .append(&JournalRecord::Completion {
                        step_index: 0,
                        outcome: duet_ops::StepOutcome::Succeeded,
                    })
                    .unwrap();
            },
            |workspace, vcx, _data_dir| {
                let _ = vcx.update(|window, cx| window.draw(cx));
                workspace.read_with(vcx, |ws, _| {
                    assert!(
                        ws.recovery_dialog.is_none(),
                        "a scan that fails outright must not open the dialog"
                    );
                });
                let notification_count = vcx.update(|window, cx| window.notifications(cx).len());
                assert!(
                    notification_count > 0,
                    "a corrupt journal must surface a notice, not silently no-op"
                );
            },
        );
    }

    // -- T-5.3.2 phase 2: trash browser ------------------------------------

    fn trash_dialog_state(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
    ) -> Entity<TrashDialogState> {
        wait_until(vcx, |vcx| {
            workspace.read_with(vcx, |ws, _| ws.trash_dialog.is_some())
        });
        workspace
            .read_with(vcx, |ws, _| ws.trash_dialog.clone())
            .expect("the trash dialog must be open by now")
    }

    /// Writes `name` under `dir`, trashes it end-to-end through the real
    /// F8 delete dialog (trash mode -- the caller is expected to have
    /// configured `delete_settings("trash", "always", true)` or
    /// equivalent), and returns its real on-disk path once it's confirmed
    /// gone. The shared seeding step every trash-browser test below needs,
    /// per this task's own instruction to seed through a real
    /// `duet_ops::plan_delete`/`DeleteMode::Trash` job run through the real
    /// executor rather than hand-writing `.trashinfo` files.
    fn seed_trashed_file(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        dir: &Path,
        name: &str,
    ) -> PathBuf {
        let victim = dir.join(name);
        std::fs::write(&victim, name.as_bytes()).unwrap();
        focus_left_panel_at(workspace, vcx, dir);
        vcx.dispatch_action(DeleteDialog);
        let _ = open_delete_dialog_state(workspace, vcx);
        vcx.dispatch_action(ConfirmDelete);
        wait_until(vcx, |vcx| {
            !victim.exists() && workspace.read_with(vcx, |ws, _| ws.delete_dialog.is_none())
        });
        victim
    }

    /// Real trashed entries on disk (seeded via the real F8-trash path),
    /// `Alt+T` opens the browser, and it lists them sorted newest-first --
    /// the two seeds are separated by a real >1s sleep since `.trashinfo`'s
    /// own `DeletionDate` only has one-second resolution.
    #[gpui::test]
    fn opening_the_trash_browser_shows_real_entries_sorted_newest_first(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                seed_trashed_file(&workspace, vcx, dir.path(), "old.txt");
                std::thread::sleep(std::time::Duration::from_millis(1100));
                seed_trashed_file(&workspace, vcx, dir.path(), "new.txt");

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                state.read_with(vcx, |state, _| {
                    let entries = state.entries();
                    assert_eq!(entries.len(), 2, "{entries:?}");
                    assert_eq!(
                        entries[0].original_path.file_name().unwrap(),
                        "new.txt",
                        "the more recently deleted entry must sort first"
                    );
                    assert_eq!(entries[1].original_path.file_name().unwrap(), "old.txt");
                });
            },
        );
    }

    /// Restoring the cursor entry (nothing marked) moves the real content
    /// back to `original_path` on real disk and removes it from the
    /// dialog's own list.
    #[gpui::test]
    fn restoring_the_cursor_entry_moves_it_back_and_removes_it_from_the_list(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let victim = seed_trashed_file(&workspace, vcx, dir.path(), "a.txt");

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                vcx.dispatch_action(TrashDialogRestore);
                wait_until(vcx, |vcx| {
                    victim.exists() && state.read_with(vcx, |s, _| s.entries().is_empty())
                });
                assert_eq!(std::fs::read(&victim).unwrap(), b"a.txt");
            },
        );
    }

    /// A marked multi-entry restore restores every marked entry and leaves
    /// every unmarked one alone -- `docs/commands.md`'s own
    /// `selection.nonempty` precondition for `trash.restore`.
    #[gpui::test]
    fn restoring_a_marked_set_restores_every_marked_entry_and_leaves_the_rest(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let a = seed_trashed_file(&workspace, vcx, dir.path(), "a.txt");
                let b = seed_trashed_file(&workspace, vcx, dir.path(), "b.txt");
                let c = seed_trashed_file(&workspace, vcx, dir.path(), "c.txt");

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                // Mark the cursor row and the one right after it, leaving
                // the third entry unmarked -- which two that ends up being
                // (deletion order can tie within the same second) doesn't
                // matter: the assertions below check every path against
                // the dialog's own recorded marked set, not a hard-coded
                // name.
                vcx.dispatch_action(TrashDialogToggleMark);
                vcx.dispatch_action(TrashDialogCursorDown);
                state.read_with(vcx, |s, _| assert_eq!(s.cursor(), 1));
                vcx.dispatch_action(TrashDialogToggleMark);

                let marked_names: std::collections::HashSet<String> =
                    state.read_with(vcx, |s, _| {
                        (0..s.entries().len())
                            .filter(|&ix| s.is_marked(ix))
                            .map(|ix| {
                                s.entries()[ix]
                                    .original_path
                                    .file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .into_owned()
                            })
                            .collect()
                    });
                assert_eq!(marked_names.len(), 2, "{marked_names:?}");

                vcx.dispatch_action(TrashDialogRestore);
                wait_until(vcx, |vcx| {
                    state.read_with(vcx, |s, _| s.entries().len() == 1)
                });

                for path in [&a, &b, &c] {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    if marked_names.contains(&name) {
                        assert!(path.exists(), "{name} should have been restored");
                    } else {
                        assert!(!path.exists(), "{name} should still be in the trash");
                    }
                }
            },
        );
    }

    /// The AC's own "recreates it" clause, end to end through the dialog:
    /// restoring into a path whose parent directory no longer exists
    /// recreates that parent and still lands the file at the right place.
    #[gpui::test]
    fn restoring_into_a_deleted_parent_recreates_it(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                let sub = dir.path().join("sub");
                std::fs::create_dir(&sub).unwrap();
                let victim = seed_trashed_file(&workspace, vcx, &sub, "a.txt");
                std::fs::remove_dir(&sub).unwrap();
                assert!(!sub.exists());

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                vcx.dispatch_action(TrashDialogRestore);
                wait_until(vcx, |vcx| {
                    victim.exists() && state.read_with(vcx, |s, _| s.entries().is_empty())
                });
                assert!(sub.is_dir(), "the deleted parent must have been recreated");
            },
        );
    }

    /// The AC's other clause -- "...or reports clearly": when the parent
    /// genuinely can't be recreated (a permission-denied ancestor), the
    /// entry must survive in the dialog's own list rather than vanishing
    /// as if it had been restored. The job's own `JobEvent::Finished`
    /// failure toast is this app's general "reports clearly" mechanism
    /// (already exercised directly, without a GPUI dialog in the loop, by
    /// `duet_ops::trash_restore`'s own
    /// `restore_reports_clearly_when_parent_recreation_fails` test) -- this
    /// test's own job is only to prove the *dialog* doesn't optimistically
    /// drop the entry.
    #[gpui::test]
    fn restoring_when_parent_recreation_fails_keeps_the_entry_in_the_list(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                use std::os::unix::fs::PermissionsExt as _;

                let dir = tempfile::tempdir().unwrap();
                let locked = dir.path().join("locked");
                std::fs::create_dir(&locked).unwrap();
                let sub = locked.join("newdir");
                std::fs::create_dir(&sub).unwrap();
                let victim = seed_trashed_file(&workspace, vcx, &sub, "a.txt");
                std::fs::remove_dir(&sub).unwrap();
                std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                vcx.dispatch_action(TrashDialogRestore);
                wait_until(vcx, |vcx| !state.read_with(vcx, |s, _| s.busy()));

                std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

                state.read_with(vcx, |s, _| {
                    assert_eq!(
                        s.entries().len(),
                        1,
                        "a genuinely failed restore must not vanish from the list"
                    );
                });
                assert!(!victim.exists(), "content must never have moved");
            },
        );
    }

    /// `trash.empty` requires its own confirmation: a single `E` only arms
    /// it (nothing destroyed yet); a second `E` within the confirmation
    /// window actually empties the trash.
    #[gpui::test]
    fn empty_requires_pressing_e_twice_before_purging_anything(cx: &mut TestAppContext) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                seed_trashed_file(&workspace, vcx, dir.path(), "a.txt");

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                vcx.dispatch_action(TrashDialogEmpty);
                let _ = vcx.update(|window, cx| window.draw(cx));
                state.read_with(vcx, |s, _| {
                    assert!(s.confirm_armed(), "the first E must only arm, not fire");
                    assert_eq!(s.entries().len(), 1, "arming must not delete anything");
                });

                vcx.dispatch_action(TrashDialogEmpty);
                wait_until(vcx, |vcx| {
                    state.read_with(vcx, |s, _| s.entries().is_empty())
                });
            },
        );
    }

    /// Same confirmation gate, `trash.delete_selected`'s own `D` this time,
    /// scoped to the marked-or-cursor target set rather than the whole
    /// trash.
    #[gpui::test]
    fn delete_selected_requires_pressing_d_twice_before_purging_the_target(
        cx: &mut TestAppContext,
    ) {
        with_configured_workspace(
            cx,
            Some(&delete_settings("trash", "always", true)),
            |workspace, vcx, _data_dir| {
                let dir = tempfile::tempdir().unwrap();
                seed_trashed_file(&workspace, vcx, dir.path(), "a.txt");
                seed_trashed_file(&workspace, vcx, dir.path(), "b.txt");

                vcx.dispatch_action(OpenTrashDialog);
                let state = trash_dialog_state(&workspace, vcx);

                vcx.dispatch_action(TrashDialogDeleteSelected);
                let _ = vcx.update(|window, cx| window.draw(cx));
                state.read_with(vcx, |s, _| {
                    assert!(s.confirm_armed(), "the first D must only arm, not fire");
                    assert_eq!(s.entries().len(), 2, "arming must not delete anything");
                });

                vcx.dispatch_action(TrashDialogDeleteSelected);
                wait_until(vcx, |vcx| {
                    state.read_with(vcx, |s, _| s.entries().len() == 1)
                });
            },
        );
    }

    /// An empty trash still opens the browser (rather than the keypress
    /// silently doing nothing), showing an empty list -- see
    /// `crate::trash_dialog`'s module doc comment for why this dialog
    /// chooses that over declining to open at all.
    #[gpui::test]
    fn opening_the_browser_on_an_empty_trash_still_opens_it(cx: &mut TestAppContext) {
        with_workspace(cx, |workspace, vcx| {
            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenTrashDialog);
            let state = trash_dialog_state(&workspace, vcx);
            state.read_with(vcx, |s, _| assert!(s.entries().is_empty()));
        });
    }

    /// A genuine `duet_ops::TrashError` from the scan (the trash `info`
    /// directory exists but can't be read at all, per `duet_platform::
    /// trash::list_trash_entries`'s own documented error case -- distinct
    /// from a single malformed `.trashinfo`, which phase 1's own
    /// `list_trash_entries` already tolerates and skips, not something this
    /// test re-proves) surfaces a notice rather than crashing, and the
    /// dialog does not open with whatever partial/garbage data.
    #[gpui::test]
    fn a_trash_scan_failure_surfaces_a_notice_and_does_not_open_the_dialog(
        cx: &mut TestAppContext,
    ) {
        with_workspace(cx, |workspace, vcx| {
            use std::os::unix::fs::PermissionsExt as _;

            let data_dir = duet_config::paths::xdg_data_home().unwrap();
            let info_dir = data_dir.join("Trash").join("info");
            std::fs::create_dir_all(&info_dir).unwrap();
            std::fs::set_permissions(&info_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

            if std::fs::read_dir(&info_dir).is_ok() {
                // Running as a user (root, e.g.) that can read a mode-000
                // directory regardless -- this environment can't verify
                // the failure path, so this test degrades to a no-op
                // rather than asserting something false. Same "don't claim
                // untested results" convention `duet_platform::trash`'s own
                // permission-dependent tests already follow.
                eprintln!(
                    "a_trash_scan_failure_surfaces_a_notice_and_does_not_open_the_dialog: \
                     running as a user that can read a mode-000 directory -- not verified here"
                );
                std::fs::set_permissions(&info_dir, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                return;
            }

            focus_left_panel(&workspace, vcx);
            vcx.dispatch_action(OpenTrashDialog);
            wait_until(vcx, |vcx| {
                vcx.update(|window, cx| window.notifications(cx).len()) > 0
            });

            std::fs::set_permissions(&info_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

            workspace.read_with(vcx, |ws, _| {
                assert!(
                    ws.trash_dialog.is_none(),
                    "a scan that fails outright must not open the dialog"
                );
            });
        });
    }
}
