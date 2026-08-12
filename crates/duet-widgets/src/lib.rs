// SPDX-License-Identifier: MIT
//! GPUI facade over gpui-component (isolation layer for R-G7). ONLY this crate and duet-ui may depend on gpui/gpui-component -- ADR-002.
//!
//! T-4.1.1 landed the first slice: bootstrapping `gpui-component` itself.
//! T-4.1.2 adds the full widget façade -- every `gpui-component` widget
//! type used anywhere in Duet is re-exported from one of the modules
//! below, and no crate other than this one may write `use gpui_component::
//! ...` directly (enforced by `scripts/check-gpui-component-facade.sh`, a
//! narrower sibling of the existing `check-gpui-isolation.sh` from
//! T-2.1.2 -- that one allows `duet-ui` to see `gpui-component`, this one
//! doesn't). `crates/duet-ui` -- the only other GPUI-aware crate
//! (ADR-002) -- reaches `gpui-component` exclusively through
//! `duet_widgets::{table, list, input, select, menu, dialog, toast,
//! resizable, theme, layout}`.
//!
//! T-4.1.3 adds [`compat`], a shim for the specific upstream APIs that
//! have already shown signs of being awkward or churn-prone (ADR-003).

pub mod compat;
pub mod dialog;
pub mod input;
pub mod layout;
pub mod list;
pub mod menu;
pub mod resizable;
pub mod select;
pub mod table;
pub mod theme;
pub mod toast;

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
/// exists ([`compat::sync_theme_with_window`]): on Linux,
/// `App::window_appearance()` alone can be unreliable before a window is
/// open (see the upstream note on
/// `gpui_component::theme::Theme::sync_system_appearance`, and
/// `compat`'s doc comment for why that call is wrapped), so `duet-ui`'s
/// window-open callback re-syncs with the real `Window` handle.
pub fn init(cx: &mut gpui::App) {
    gpui_component::init(cx);
}
