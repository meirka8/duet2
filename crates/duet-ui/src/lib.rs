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
//! T-4.1.4 builds out the real workspace shell: a draggable/keyboard
//! -resizable splitter between the two panels, a function-key bar, a
//! status bar, and a command-line row (`workspace::Workspace`).
//! T-4.1.5 builds out a real theme system on top of T-4.1.1's one-shot
//! appearance sync: Duet's own documented token palette
//! (`docs/config-schema.md` §4), live desktop light/dark follow, and
//! `themes/*.toml` loading with hot reload (`theme_controller`).

mod command_palette;
mod copy_move_dialog;
pub mod file_table;
mod function_bar;
mod hotlist;
pub mod panel;
mod theme_controller;
mod workspace;

pub use workspace::run;
