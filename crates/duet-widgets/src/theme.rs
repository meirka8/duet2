// SPDX-License-Identifier: MIT
//! Theme façade over `gpui_component::theme` (R-G7). `ActiveTheme`/`Theme`
//! are not in T-4.1.2's AC widget list (table/list/input/select/menu/
//! dialog/toast/resizable), but the isolation rule this task enforces is
//! unconditional -- "no crate other than `duet-widgets` may write
//! `use gpui_component::...`" -- and `duet-ui`'s root view already reads
//! theme colours every frame (`cx.theme().background`, `.foreground`,
//! ...), so this has to be re-exported too for that call site to move out
//! of `gpui_component` entirely.

pub use gpui_component::{ActiveTheme, Theme, ThemeMode};
