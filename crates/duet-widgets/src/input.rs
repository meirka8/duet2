// SPDX-License-Identifier: MIT
//! `Input`/`InputState` façade over `gpui_component::input` (R-G7). Text
//! fields (path bar, rename, filter box, command line, ...) go through
//! `duet_widgets::input::*`, not `gpui_component::input` directly.

pub use gpui_component::input::*;
