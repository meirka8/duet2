// SPDX-License-Identifier: MIT
//! Resizable-panel-group façade over `gpui_component::resizable` (R-G7).
//! The dual-pane splitter (T-4.1.4) and any other draggable/keyboard
//! -resizable layout go through `duet_widgets::resizable::*`, not
//! `gpui_component::resizable` directly.

pub use gpui_component::resizable::*;
