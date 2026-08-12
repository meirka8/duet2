// SPDX-License-Identifier: MIT
//! T-4.1.5's live theme system: builds Duet's [`duet_widgets::theme::TokenPalette`]
//! from the built-in default plus an optional `themes/*.toml` override, and
//! keeps both live for the lifetime of the window:
//!
//! - **Follow-system**: [`gpui::Window::observe_window_appearance`] fires
//!   synchronously on the GPUI thread whenever the desktop's light/dark
//!   preference changes; the observer here rebuilds and reinstalls the
//!   palette for the new mode and re-points the theme-file watch at that
//!   mode's file. This supersedes T-4.1.1's one-shot
//!   `sync_theme_with_window` (still called once, for the same
//!   Linux-`Window`-appearance-reliability reason its own doc comment
//!   explains) with something that keeps firing.
//! - **Theme file hot reload**: `duet_config::watch` runs `notify` on a
//!   dedicated background thread (never the UI thread, per T-3.1.6) and
//!   calls back with an already-deserialized [`duet_config::ThemeTokensDocument`];
//!   the callback hands it to GPUI's foreground executor over a
//!   `tokio::sync::mpsc` channel, the same executor-bridging shape
//!   `workspace`'s T-4.1.1 demo already established (a channel `Receiver`
//!   polls fine under any executor, GPUI's included -- it only needs a
//!   `Waker`, not a live Tokio runtime underfoot).
//!
//! The one-time initial resolution (mode from the real `Window`, plus a
//! synchronous small-file read of the active theme file if one exists) is
//! the same "initial synchronous startup path before the UI is running"
//! carve-out `duet-config`'s own docs describe; the *live* paths above are
//! what actually differ from a one-shot sync.

use std::path::PathBuf;

use duet_widgets::theme::{Theme, ThemeMode, TokenPalette};
use gpui::{App, Entity, Subscription, Window};

use crate::workspace::Workspace;

/// Keeps the live theme system alive for as long as the window is open.
/// Lives inside [`Workspace`]; dropping it (window close) cancels the
/// appearance observer and stops the file watcher.
pub struct ThemeController {
    mode: ThemeMode,
    active_file: Option<PathBuf>,
    _appearance_sub: Option<Subscription>,
    _file_watch: Option<duet_config::ConfigWatcher>,
}

impl ThemeController {
    /// Builds and installs the initial palette (T-4.1.1's existing
    /// pre-window + post-window appearance sync has already run by the
    /// time this is called, so `window.appearance()` is reliable), then
    /// registers the live follow-system observer and the active theme
    /// file's hot-reload watch. `workspace` is used only by the two live
    /// callbacks (to `.update()` and `cx.notify()` after a live change);
    /// this function itself does not touch it.
    pub fn install(window: &mut Window, cx: &mut App, workspace: Entity<Workspace>) -> Self {
        let mode = Theme::global(cx).mode;
        let active_file = build_and_install(mode, cx);

        let appearance_sub = {
            let workspace = workspace.clone();
            window.observe_window_appearance(move |window, cx| {
                let new_mode = window.appearance().into();
                tracing::info!(
                    target: "duet_ui::theme_controller",
                    "desktop appearance changed -> {} mode",
                    if new_mode == ThemeMode::Dark { "dark" } else { "light" }
                );
                duet_widgets::compat::sync_theme_with_window(window, cx);
                let new_active_file = build_and_install(new_mode, cx);
                let new_watch =
                    spawn_file_watch(new_active_file.clone(), new_mode, workspace.clone(), cx);
                workspace.update(cx, |ws, cx| {
                    ws.theme_mut().mode = new_mode;
                    ws.theme_mut().active_file = new_active_file;
                    ws.theme_mut()._file_watch = new_watch;
                    cx.notify();
                });
                window.refresh();
            })
        };

        let file_watch = spawn_file_watch(active_file.clone(), mode, workspace, cx);

        Self {
            mode,
            active_file,
            _appearance_sub: Some(appearance_sub),
            _file_watch: file_watch,
        }
    }

    pub fn mode(&self) -> ThemeMode {
        self.mode
    }

    pub fn active_file(&self) -> Option<&PathBuf> {
        self.active_file.as_ref()
    }
}

/// `~/.config/duet/themes/duet-{dark,light}.toml` -- matches
/// `duet_config::Settings::appearance`'s documented default
/// `theme_light`/`theme_dark` names (`docs/config-schema.md` §1).
fn theme_file_name(mode: ThemeMode) -> &'static str {
    if mode.is_dark() {
        "duet-dark"
    } else {
        "duet-light"
    }
}

/// Builds the built-in palette for `mode`, merges a `themes/<name>.toml`
/// override on top if one exists and parses cleanly, installs the result
/// as the process-wide [`TokenPalette`] (and onto `gpui-component`'s own
/// `Theme`), and returns the file path that was actually applied (`None`
/// if no override file exists or it failed to load/parse -- the built-in
/// default is always a safe fallback, matching `duet-config::watch`'s "a
/// malformed reload does not tear down the running config" posture).
fn build_and_install(mode: ThemeMode, cx: &mut App) -> Option<PathBuf> {
    let mut palette = TokenPalette::built_in(mode);
    let mut applied_path = None;

    if let Ok(path) = duet_config::paths::theme_path(theme_file_name(mode)) {
        match duet_config::theme::load_tokens(&path) {
            Ok(file) => match file.typed() {
                Ok(doc) => {
                    palette.apply_overrides(&doc.color, &doc.syntax, &doc.spacing);
                    tracing::info!(
                        target: "duet_ui::theme_controller",
                        path = %path.display(),
                        name = %doc.name,
                        "loaded custom theme file override"
                    );
                    applied_path = Some(path);
                }
                Err(err) => {
                    tracing::warn!(
                        target: "duet_ui::theme_controller",
                        path = %path.display(),
                        "theme file did not match the expected schema, using built-in default: {err}"
                    );
                }
            },
            Err(_) => {
                // No file at this path (the common case -- Duet ships no
                // built-in themes/*.toml) or it failed to read/parse; the
                // built-in default already installed below covers both.
            }
        }
    }

    palette.install(cx);
    applied_path
}

/// Starts (or skips, if `path` is `None`) a hot-reload watch on the active
/// theme file, bridging `duet-config`'s background-thread callback into
/// GPUI's foreground executor over a `tokio::sync::mpsc` channel -- see
/// this module's doc comment for why that bridge is safe to poll from
/// GPUI's own executor without a live Tokio runtime.
fn spawn_file_watch(
    path: Option<PathBuf>,
    mode: ThemeMode,
    workspace: Entity<Workspace>,
    cx: &mut App,
) -> Option<duet_config::ConfigWatcher> {
    let path = path?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<duet_config::ThemeTokensDocument>();

    let watch_result = duet_config::watch::<duet_config::ThemeTokensDocument>(
        path.clone(),
        duet_config::MigrationRegistry::generic_v0_to_v1(),
        duet_config::theme::THEME_SCHEMA_VERSION,
        move |doc| {
            let _ = tx.send(doc);
        },
        {
            let path = path.clone();
            move |err| {
                tracing::warn!(
                    target: "duet_ui::theme_controller",
                    path = %path.display(),
                    "theme file hot-reload error (kept previous palette): {err}"
                );
            }
        },
    );

    let watcher = match watch_result {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(
                target: "duet_ui::theme_controller",
                path = %path.display(),
                "failed to start theme file watch: {err}"
            );
            return None;
        }
    };

    cx.spawn(async move |cx| {
        while let Some(doc) = rx.recv().await {
            // `mode` is fixed to whichever mode this watch was started for
            // (this file's own path is `theme_file_name(mode)`); the
            // document's own `variant` field is metadata, not re-consulted
            // here, so a hand-edited file with a mismatched `variant`
            // string still applies to the mode it's actually *watched*
            // under rather than silently going to the wrong palette.
            let updated = cx.update(|cx| {
                let mut palette = TokenPalette::built_in(mode);
                palette.apply_overrides(&doc.color, &doc.syntax, &doc.spacing);
                palette.install(cx);
            });
            if updated.is_err() {
                break; // App shut down mid-reload.
            }
            let notified = workspace.update(cx, |_ws, cx| cx.notify());
            tracing::info!(target: "duet_ui::theme_controller", "theme file hot-reload applied");
            if notified.is_err() {
                break;
            }
        }
    })
    .detach();

    Some(watcher)
}
