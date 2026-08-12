// SPDX-License-Identifier: MIT
//! Panels, tabs, dialogs, viewer, editor, palette -- the only other GPUI-aware layer besides duet-widgets -- ADR-002.
//!
//! T-4.1.1 lands the application bootstrap: [`run`] opens the GPUI
//! `Application`, initialises `gpui-component` (via `duet-widgets`),
//! syncs the theme to the desktop's light/dark preference, opens the main
//! window, and wires a demonstration of the core's Tokio runtime driving
//! a UI update through GPUI's foreground executor. The `duet` binary
//! crate calls [`run`] and otherwise never imports `gpui` itself --
//! ADR-002 restricts that dependency to this crate and `duet-widgets`.
//!
//! The root view here (`workspace::Workspace`) is intentionally minimal:
//! a title and a static two-box stand-in for the eventual dual-pane
//! layout. The real workspace shell (splitter, tabs, function-key bar,
//! status bar, command line) is built out starting T-4.1.4.

mod workspace;

pub use workspace::run;
