// SPDX-License-Identifier: MIT
//! GPUI facade over gpui-component (isolation layer for R-G7). ONLY this crate and duet-ui may depend on gpui/gpui-component -- ADR-002.
//!
//! T-4.1.1 lands the first real slice: bootstrapping `gpui-component`
//! itself. The full widget façade (`Table`, `List`, `Input`, `Select`,
//! `Menu`, `Dialog`, `Toast`, resizable panels, ...) is T-4.1.2's job --
//! this module intentionally stays small until then so that task can wrap
//! each widget deliberately rather than inheriting an ad-hoc surface.

/// Initialise `gpui-component`'s global state for this process.
///
/// This must run exactly once, before any `gpui-component` widget is
/// constructed or any window is opened -- it registers the theme registry
/// (and syncs it to the system light/dark appearance, see
/// [`gpui_component::theme::Theme::sync_system_appearance`]), the icon
/// font, and the per-widget global state (dialogs, popovers, tables, ...)
/// that `gpui-component`'s widgets read from `App` globals.
///
/// Callers should follow up with another appearance sync once a window
/// exists (`Theme::sync_system_appearance(Some(window), cx)`): on Linux,
/// `App::window_appearance()` alone can be unreliable before a window is
/// open (see the upstream note on
/// `gpui_component::theme::Theme::sync_system_appearance`), so `duet-ui`'s
/// window-open callback re-syncs with the real `Window` handle.
pub fn init(cx: &mut gpui::App) {
    gpui_component::init(cx);
}
