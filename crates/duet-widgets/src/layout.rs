// SPDX-License-Identifier: MIT
//! Flex-layout helper façade over `gpui_component::styled` (R-G7).
//! `h_flex`/`v_flex` are `gpui-component` conveniences (a `Div` pre
//! -configured as a horizontal/vertical flex container), not part of
//! T-4.1.2's AC widget list, but `duet-ui`'s root view already builds its
//! layout with them, so -- same reasoning as `theme.rs` -- they must be
//! re-exported here for `duet-ui` to stop importing `gpui_component`
//! directly.
//!
//! [`Root`] is `gpui-component`'s required window-root wrapper: several
//! widgets (`Input` among them, per T-4.1.2/T-4.1.4's `S-6` spike finding)
//! call `gpui_component::Root::read`/`Root::update` internally and panic
//! (`unwrap()` on `window.root::<Root>()`) if the window's actual root
//! view isn't one. Every `duet-ui` window-open callback must wrap its real
//! root view in `Root::new(view, window, cx)` rather than returning that
//! view directly -- see `duet-ui::workspace::run`.
//!
//! [`WindowExt`] is `Root`'s own extension trait on `gpui::Window`
//! (`window.push_notification(..)`, `open_dialog`/`open_sheet`, ...) --
//! re-exported alongside `Root` since every one of its methods requires
//! exactly the `Root`-wrapped window this module's own doc comment
//! describes, and calling any of them before that wrapping has happened
//! panics the same way (T-4.3.7: `Workspace::render` uses
//! `push_notification` for the "a corrupt session file degrades to
//! defaults with a notice" AC).

pub use gpui_component::{Root, WindowExt, h_flex, v_flex};
