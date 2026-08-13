// SPDX-License-Identifier: MIT
//! The application-bootstrap root view: window, theme, and the
//! Tokio-to-GPUI executor bridge demo (T-4.1.1), built out into the real
//! workspace shell by T-4.1.4/T-4.1.5: a draggable/keyboard-resizable
//! dual-pane splitter, a function-key bar, a status bar, and a
//! command-line row, all themed by [`crate::theme_controller`].

use std::path::PathBuf;

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

use crate::file_table::FileTable;
use crate::function_bar::{self, FKeySlot};
use crate::theme_controller::ThemeController;

// FR-NAV-01's "keyboard resize": while the workspace has focus, adjust the
// splitter ratio without touching the mouse. Bound below to `ctrl-left`/
// `ctrl-right`. `gpui-component`'s `ResizablePanelGroup` (T-4.1.2's
// `duet_widgets::resizable` façade) has no keyboard-resize API of its own
// to call into (verified by reading `gpui-component-0.5.1`'s
// `resizable/mod.rs`: every size-mutating method is `pub(crate)`, so even
// this façade crate cannot reach it) -- this is the "or add a reasonable
// one" half of the task brief.
actions!(duet_workspace, [ResizeSplitterLeft, ResizeSplitterRight]);

/// Registers the workspace's own keybindings. Called once from [`run`],
/// before any window opens. `Some("Workspace")` scopes both bindings to
/// elements tagged with that key context -- see the root view's
/// `.key_context("Workspace")` in [`Workspace::render`].
fn bind_workspace_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-left", ResizeSplitterLeft, Some("Workspace")),
        KeyBinding::new("ctrl-right", ResizeSplitterRight, Some("Workspace")),
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
                window.focus(&workspace.read(cx).focus_handle);

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

    /// T-4.2.1: the left panel's real, virtualised directory table --
    /// `duet_index::DirectoryModel`/`EntryStore` backed, not a placeholder.
    /// The right panel stays a placeholder for now (T-4.2.2 onward builds
    /// out per-panel selection/cursor/navigation before both are real).
    left_panel: Entity<FileTable>,

    function_keys: Vec<FKeySlot>,
    command_line: Entity<InputState>,

    /// `~/.config/duet/settings.toml` (or `None` if `$HOME`/
    /// `$XDG_CONFIG_HOME` can't be resolved -- splitter-ratio persistence
    /// is then skipped, not fatal).
    settings_path: Option<PathBuf>,

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

        // T-4.2.1: list the process's current directory in the left panel --
        // same directory the T-4.1.1 executor-wiring demo below counts, so
        // the status bar's "N entries in <dir>" line and the left panel's
        // actual row count are checkable against each other.
        let initial_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let left_panel = cx.new(|cx| FileTable::new(initial_dir, tokio_handle.clone(), window, cx));

        Self {
            demo: DemoState::Loading,
            focus_handle: cx.focus_handle(),
            splitter_ratio,
            resizable_state: cx.new(|_| ResizableState::default()),
            left_panel,
            function_keys: function_bar::build_function_bar(),
            command_line,
            settings_path,
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

    fn dual_pane(&self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let theme = cx.theme();
        let total = px(900.); // A reasonable initial estimate; the widget's own
        // canvas-driven `adjust_to_container_size` immediately corrects this to
        // the real measured width on first layout and on every subsequent
        // window resize (see `gpui-component-0.5.1/src/resizable/mod.rs`), so
        // this only affects the very first frame before layout has happened.
        let left_w = total * self.splitter_ratio;
        let right_w = total * (1.0 - self.splitter_ratio);

        h_resizable("workspace-splitter")
            .with_state(&self.resizable_state)
            .child(
                resizable_panel()
                    .size(left_w)
                    .size_range(px(160.)..Pixels::MAX)
                    .child(left_panel_view(&self.left_panel, tokens, theme.border)),
            )
            .child(
                resizable_panel()
                    .size(right_w)
                    .size_range(px(160.)..Pixels::MAX)
                    .child(placeholder_panel(
                        "Right panel",
                        false,
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
            // Scaled down 20% along with the theme's default font size --
            // see `duet_widgets::compat::apply_font_scale`.
            .text_size(px(9.6))
            .child(gpui::div().child(status_text))
            .child(gpui::div().child(theme_text))
            // Placeholder selection-stats slot -- real content (selected
            // count/size, free space) is T-4.2.7's job; this establishes
            // the layout slot the AC asks for.
            .child(gpui::div().child("0 items, 0 bytes selected"))
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
                            .text_size(px(8.8))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(tokens.color.accent)
                            .child(slot.key),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(8.8))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(gpui::div().flex_1().p_2().child(self.dual_pane(cx)))
            .child(self.command_line_row(cx))
            .child(self.status_bar_row(cx))
            .child(self.function_key_bar(cx))
    }
}

/// Wraps the real, virtualised [`FileTable`] (T-4.2.1) in the same
/// themed frame [`placeholder_panel`] below uses, so the left panel's chrome
/// matches the still-placeholder right panel until T-4.2.2 onward makes
/// both real.
fn left_panel_view(
    table: &Entity<FileTable>,
    tokens: &TokenPalette,
    border: gpui::Hsla,
) -> impl IntoElement {
    v_flex()
        .size_full()
        .bg(tokens.color.panel_bg_active)
        .border_1()
        .border_color(border)
        .rounded_md()
        .child(table.clone())
}

/// A placeholder panel standing in for a future `PanelView` (T-4.2.x).
/// Proves the resizable dual-pane shape and is themed by
/// [`TokenPalette`]; it renders no directory data of its own yet.
fn placeholder_panel(
    label: &'static str,
    active: bool,
    tokens: &TokenPalette,
    border: gpui::Hsla,
) -> impl IntoElement {
    let (bg, fg) = if active {
        (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
    } else {
        (
            tokens.color.panel_bg_inactive,
            tokens.color.panel_fg_inactive,
        )
    };
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .bg(bg)
        .border_1()
        .border_color(border)
        .rounded_md()
        .child(gpui::div().text_color(fg).child(label))
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
}
