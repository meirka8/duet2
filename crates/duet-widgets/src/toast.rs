// SPDX-License-Identifier: MIT
//! Toast/notification façade over `gpui_component::notification` (R-G7).
//!
//! Named `toast` here rather than `notification` (the upstream module
//! name) deliberately: the AC for this façade (`documentation/task.md`
//! T-4.1.2) enumerates "toast" as the widget category, and keeping our
//! own naming independent of upstream's means a future fork or
//! replacement doesn't have to match `gpui-component`'s vocabulary, only
//! this module's.

pub use gpui_component::notification::*;
