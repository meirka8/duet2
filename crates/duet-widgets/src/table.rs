// SPDX-License-Identifier: MIT
//! `Table`/`TableDelegate` façade over `gpui_component::table` (R-G7):
//! callers outside `duet-widgets` reach the virtualised-table API through
//! `duet_widgets::table::*`, never `gpui_component::table` directly, so a
//! fork or replacement of the underlying implementation stays local to
//! this crate.
//!
//! Not yet consumed by a concrete view -- T-4.1.1's placeholder panels are
//! static boxes, and the real virtualised file table is T-4.2.1's job
//! (backed by S-1's spike, `spikes/s1-virtualised-table/`). This module
//! exists now so T-4.2.1 has a façade to build against rather than
//! reaching past it on day one.

pub use gpui_component::table::*;

/// See [`crate::compat::TableRow`]: the concrete row-container type
/// `TableDelegate::render_tr` must return. Re-exported here so callers can
/// write `duet_widgets::table::TableRow` alongside the rest of this
/// module's types, even though the wrapper itself lives in `compat`
/// (T-4.1.3) because it exists specifically to insulate against upstream
/// churn, not just to rename something.
pub use crate::compat::TableRow;
