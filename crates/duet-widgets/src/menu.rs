// SPDX-License-Identifier: MIT
//! `PopupMenu`/`PopupMenuItem` façade over `gpui_component::menu` (R-G7).
//! Use `duet_widgets::menu::*` for any right-click context menu instead of
//! importing `gpui_component::menu` directly.
//!
//! T-4.3.8 is the first thing in this codebase to build a real context
//! menu (`FileTableDelegate`'s `TableDelegate::context_menu` override).
//! Its items are wired with [`PopupMenuItem::on_click`] (a plain
//! `Fn(&ClickEvent, &mut Window, &mut App)` closure), not `.menu(label,
//! action)`'s `Box<dyn Action>` dispatch -- `window`/`App` are already in
//! scope wherever a menu gets built, and a captured `WeakEntity::update`
//! is a directly verifiable call, unlike relying on action-dispatch's
//! focus-routing behaviour (unverifiable in this sandbox, see the T-4.3.8
//! PR description for the full reasoning).
//!
//! [`ContextMenuExt`] is re-exported too, though `FileTableDelegate`'s own
//! `context_menu` override -- which `Table` already invokes automatically
//! per right-clicked row -- is the real integration point for T-4.3.8, not
//! this trait's `.context_menu(f)` builder method.

pub use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
