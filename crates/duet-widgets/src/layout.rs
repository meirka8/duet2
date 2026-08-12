// SPDX-License-Identifier: MIT
//! Flex-layout helper façade over `gpui_component::styled` (R-G7).
//! `h_flex`/`v_flex` are `gpui-component` conveniences (a `Div` pre
//! -configured as a horizontal/vertical flex container), not part of
//! T-4.1.2's AC widget list, but `duet-ui`'s root view already builds its
//! layout with them, so -- same reasoning as `theme.rs` -- they must be
//! re-exported here for `duet-ui` to stop importing `gpui_component`
//! directly.

pub use gpui_component::{h_flex, v_flex};
