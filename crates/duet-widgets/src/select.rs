// SPDX-License-Identifier: MIT
//! `Select`/`SelectDelegate` façade over `gpui_component::select` (R-G7).
//! Dropdown/combobox pickers (view mode, sort column, theme picker, ...)
//! go through `duet_widgets::select::*`, not `gpui_component::select`
//! directly.

pub use gpui_component::select::*;
