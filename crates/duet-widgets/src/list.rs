// SPDX-License-Identifier: MIT
//! `List`/`ListDelegate` façade over `gpui_component::list` (R-G7). Use
//! `duet_widgets::list::*` for any virtualised, non-tabular list view
//! (command palette results, quick search, etc.) instead of importing
//! `gpui_component::list` directly.
//!
//! [`IndexPath`] and [`Selectable`] live at `gpui_component`'s crate root
//! (`index_path.rs`/`styled.rs`), not inside its own `list` module, but
//! every real consumer of `ListDelegate` needs both -- `render_item`'s
//! index parameter is an `IndexPath`, and its `Item` associated type must
//! implement `Selectable` (T-4.3.6's command palette is the first thing in
//! this codebase to implement `ListDelegate` itself, rather than just
//! using a widget that already does).

pub use gpui_component::list::*;
pub use gpui_component::{IndexPath, Selectable};
