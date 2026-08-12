// SPDX-License-Identifier: MIT
//! The application-bootstrap root view: window, theme, and the
//! Tokio-to-GPUI executor bridge demo (T-4.1.1).

use std::path::PathBuf;

use duet_types::{UnixPathBuf, VPath};
use duet_vfs::{FileSystem, ListOpts, LocalFs};
use duet_widgets::{
    layout::{h_flex, v_flex},
    theme::ActiveTheme as _,
};
use futures_util::StreamExt;
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Entity, Hsla, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, TitlebarOptions, Window, WindowBounds,
    WindowOptions, px, size,
};

/// Opens the Duet application window.
///
/// This is the T-4.1.1 walking-skeleton bootstrap: it does not yet build
/// the real dual-pane workspace (T-4.1.4+/T-4.2.x own that) -- it proves
/// out the four pieces everything else depends on:
///
/// 1. A `gpui::Application` opens a window on the running display server
///    (Wayland or X11).
/// 2. `gpui-component` is initialised and its theme follows the desktop's
///    light/dark preference at startup.
/// 3. A dedicated multi-threaded Tokio runtime -- the same runtime shape
///    the core (`duet-vfs`, `duet-ops`, `duet-index`) uses (design.md
///    §8.2: "I/O runtime ... sized to min(8, cpus)") -- runs alongside
///    GPUI's own executor.
/// 4. A background task on that Tokio runtime performs real core I/O
///    (`LocalFs::read_dir` on the process's current directory) and its
///    result is delivered back onto GPUI's foreground executor, updating
///    the root view and triggering a repaint -- the concrete mechanism
///    behind the AC "the core's async tasks drive UI updates through the
///    foreground executor."
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
                // Re-sync appearance now that a real `Window` exists. See
                // `duet_widgets::compat::sync_theme_with_window`'s doc
                // comment for why this second, post-window sync is needed
                // on top of the pre-window one already done inside
                // `duet_widgets::init` (which delegates to the underlying
                // gpui-component crate's own init).
                duet_widgets::compat::sync_theme_with_window(window, cx);

                let workspace = cx.new(|cx| Workspace::new(window, cx));
                spawn_entry_count_demo(tokio_handle.clone(), workspace.clone(), cx);
                workspace
            },
        )
        .expect("failed to open the Duet window");
    });
}

/// The walking-skeleton root view: a title, a static two-box stand-in for
/// the eventual dual-pane layout, and a status line showing the
/// executor-wiring demo's progress/result.
struct Workspace {
    demo: DemoState,
}

/// Progress of the background Tokio task that lists the current
/// directory -- exists purely to demonstrate the executor bridge
/// (T-4.1.1's AC), not as a real feature.
enum DemoState {
    Loading,
    Ready { dir: String, entry_count: usize },
    Failed(String),
}

impl Workspace {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            demo: DemoState::Loading,
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.background;
        let fg = theme.foreground;
        let border = theme.border;
        let muted = theme.muted;
        let muted_fg = theme.muted_foreground;

        let status_text: SharedString = match &self.demo {
            DemoState::Loading => "Reading current directory via the core Tokio runtime...".into(),
            DemoState::Ready { dir, entry_count } => {
                format!("core -> UI bridge OK: {entry_count} entries in {dir}").into()
            }
            DemoState::Failed(err) => format!("core -> UI bridge error: {err}").into(),
        };

        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .p_4()
            .gap_3()
            .child(
                gpui::div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("Duet"),
            )
            .child(
                h_flex()
                    .flex_1()
                    .gap_3()
                    .child(placeholder_panel("Left panel", muted, border, muted_fg))
                    .child(placeholder_panel("Right panel", muted, border, muted_fg)),
            )
            .child(
                gpui::div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(status_text),
            )
    }
}

/// A static box standing in for a future `PanelView` (T-4.1.4/T-4.2.x).
/// Proves the bootstrap window can lay out the eventual dual-pane shape;
/// it renders no data of its own.
fn placeholder_panel(label: &'static str, bg: Hsla, border: Hsla, fg: Hsla) -> impl IntoElement {
    v_flex()
        .flex_1()
        .h_full()
        .items_center()
        .justify_center()
        .bg(bg)
        .border_1()
        .border_color(border)
        .rounded_md()
        .child(gpui::div().text_color(fg).child(label))
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
