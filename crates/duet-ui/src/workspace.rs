// SPDX-License-Identifier: MIT
//! The application-bootstrap root view: window, theme, and the
//! Tokio-to-GPUI executor bridge demo (T-4.1.1), built out into the real
//! workspace shell by T-4.1.4/T-4.1.5: a draggable/keyboard-resizable
//! dual-pane splitter, a function-key bar, a status bar, and a
//! command-line row, all themed by [`crate::theme_controller`].

use std::path::{Path, PathBuf};

use duet_config::SessionTab;
use duet_types::{UnixPathBuf, VPath};
use duet_vfs::{FileSystem, ListOpts, LocalFs};
use duet_widgets::{
    input::{Input, InputState},
    layout::{Root, h_flex, v_flex},
    resizable::{ResizableState, h_resizable, resizable_panel},
    theme::{ActiveTheme as _, TokenPalette},
};
use futures_util::StreamExt;
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, px,
    size,
};

use crate::file_table::{FileTable, write_byte_count};
use crate::function_bar::{self, FKeySlot};
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
actions!(
    duet_workspace,
    [ResizeSplitterLeft, ResizeSplitterRight, FocusOtherPanel]
);

/// Registers the workspace's own keybindings. Called once from [`run`],
/// before any window opens. `Some("Workspace")` scopes the splitter
/// bindings to elements tagged with that key context -- see the root
/// view's `.key_context("Workspace")` in [`Workspace::render`].
fn bind_workspace_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-left", ResizeSplitterLeft, Some("Workspace")),
        KeyBinding::new("ctrl-right", ResizeSplitterRight, Some("Workspace")),
        KeyBinding::new("tab", FocusOtherPanel, Some("FileTable")),
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
    /// comment), not just at graceful shutdown -- T-4.3.7's later AC
    /// ("kill -9 then restart restores the full workspace") only holds if
    /// saves are already this eager by the time that task adds to the
    /// same file.
    session_path: Option<PathBuf>,

    /// Set once, right after construction, by [`run`] (needs a `Window`
    /// and this view's own `Entity` to exist first -- see
    /// `ThemeController::install`'s doc comment). `Option` only to bridge
    /// that one-frame gap; every render after startup sees `Some`.
    theme: Option<ThemeController>,
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
        let session_path = duet_config::paths::session_path().ok();
        let session = session_path.as_deref().and_then(|path| {
            duet_config::session::load(path)
                .inspect_err(|err| {
                    tracing::info!(
                        target: "duet_ui::workspace",
                        "using default session ({path:?} not loaded: {err})"
                    );
                })
                .ok()
        });
        let (left_tabs, left_active) =
            resolve_panel_session(session.as_ref().map(|s| &s.left), &initial_dir);
        let (right_tabs, right_active) =
            resolve_panel_session(session.as_ref().map(|s| &s.right), &initial_dir);

        let left_panel =
            cx.new(|cx| Panel::new(left_tabs, left_active, tokio_handle.clone(), window, cx));
        let right_panel =
            cx.new(|cx| Panel::new(right_tabs, right_active, tokio_handle.clone(), window, cx));
        // Every structural tab change (`Panel::new_tab`/`close_active`/...)
        // and every real per-tab directory change (via each `FileTable`'s
        // `DirectoryChanged` event, which `Panel` already re-notifies on --
        // see `Panel::add_tab_entry`'s doc comment) calls `cx.notify()` on
        // the panel entity, which is exactly what these observers fire on.
        cx.observe(&left_panel, |this, _panel, cx| this.persist_session(cx))
            .detach();
        cx.observe(&right_panel, |this, _panel, cx| this.persist_session(cx))
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

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .justify_between()
            .bg(tokens.color.statusbar_bg)
            .text_color(tokens.color.statusbar_fg)
            .text_size(px(12.))
            .child(gpui::div().child(status_text))
            .child(gpui::div().child(theme_text))
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

        v_flex()
            .id("workspace-root")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
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
            .child(gpui::div().flex_1().p_2().child(self.dual_pane(window, cx)))
            .child(self.command_line_row(cx))
            .child(self.status_bar_row(cx))
            .child(self.function_key_bar(cx))
    }
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
    use super::*;

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

    /// T-4.3.2: `resolve_panel_session` is the pure (GPUI-free) half of
    /// startup session-loading -- everything about it that doesn't need a
    /// real `Panel`/`FileTable`/window to exercise, unlike `Panel`'s own
    /// tab-command tests (`panel.rs`, which need `gpui::TestAppContext`).
    #[test]
    fn resolve_panel_session_falls_back_to_a_single_tab_at_fallback_dir_when_session_is_none() {
        let fallback = PathBuf::from("/tmp");
        let (tabs, active) = resolve_panel_session(None, &fallback);
        assert_eq!(
            tabs,
            vec![SessionTab {
                dir: fallback,
                locked: false,
                lock_dir_change: false,
            }]
        );
        assert_eq!(active, 0);
    }

    #[test]
    fn resolve_panel_session_filters_out_tabs_whose_directory_no_longer_exists() {
        let real = tempfile::tempdir().unwrap();
        let gone = real.path().join("this-directory-was-deleted");
        let session = duet_config::SessionPanel {
            tabs: vec![
                SessionTab {
                    dir: gone,
                    locked: false,
                    lock_dir_change: false,
                },
                SessionTab {
                    dir: real.path().to_path_buf(),
                    locked: true,
                    lock_dir_change: false,
                },
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
            tabs: vec![SessionTab {
                dir: gone,
                locked: false,
                lock_dir_change: false,
            }],
            active_tab: 0,
        };
        let fallback = PathBuf::from("/tmp");
        let (tabs, active) = resolve_panel_session(Some(&session), &fallback);
        assert_eq!(
            tabs,
            vec![SessionTab {
                dir: fallback,
                locked: false,
                lock_dir_change: false,
            }]
        );
        assert_eq!(active, 0);
    }

    #[test]
    fn resolve_panel_session_clamps_active_tab_into_range() {
        let real = tempfile::tempdir().unwrap();
        let session = duet_config::SessionPanel {
            tabs: vec![SessionTab {
                dir: real.path().to_path_buf(),
                locked: false,
                lock_dir_change: false,
            }],
            active_tab: 99,
        };
        let (tabs, active) = resolve_panel_session(Some(&session), Path::new("/tmp"));
        assert_eq!(tabs.len(), 1);
        assert_eq!(active, 0);
    }
}
