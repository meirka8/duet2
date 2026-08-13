// SPDX-License-Identifier: MIT
//! `gpui-compat`: wrappers for the specific `gpui`/`gpui-component` APIs
//! that have already shown, in building T-4.1.1/T-4.1.2, that they are
//! awkward or likely to shift under a version bump (ADR-003,
//! `documentation/design.md` §7.3/§7.4 R-G1).
//!
//! ADR-003's policy in full: pin an exact version, isolate anything
//! churn-prone behind this shim, and only ever bump the pin as a
//! deliberate task with its own compile-and-smoke gate -- never
//! implicitly as a side effect of an unrelated change. The pin itself is
//! recorded twice: exactly, in `Cargo.lock` (checked into the repo, so
//! `cargo build` never silently floats to a newer patch), and as an
//! intentional range in `duet-widgets/Cargo.toml` (see that file's doc
//! comment) that a bump task edits deliberately.
//!
//! This module stays small on purpose. `gpui = "0.2.2"` is a young pin --
//! T-4.1.1/T-4.1.2 have only actually touched a couple of upstream APIs
//! so far, so there is not yet a long list of *known* churn-prone
//! surface. Add to this file only when a real, observed API turns out to
//! be awkward or has already changed shape -- not preemptively, and not
//! to pad the module.

use gpui::{App, Window};

/// Sync `gpui-component`'s theme (light/dark palette) with the desktop's
/// current appearance, given a real, open `Window`.
///
/// Why this is wrapped rather than calling
/// `gpui_component::Theme::sync_system_appearance` directly at each call
/// site: upstream's own doc comment on that function links a still-open
/// bug (`longbridge/gpui-component#104`) noting that `App::window_appearance()`
/// alone is unreliable on Linux before a real `Window` exists, and that
/// callers should prefer the window's own `Window::appearance()` once one
/// does. That is exactly the shape of pre-1.0 behavioural churn ADR-003 is
/// guarding against: a future fix for #104 could change which path is
/// authoritative, fold the two-call pattern away entirely, or change the
/// signature. `duet-ui`'s window-bootstrap code
/// (`crates/duet-ui/src/workspace.rs`) already has to call the
/// underlying function twice for this reason -- once pre-window (inside
/// `duet_widgets::init`, no `Window` exists yet) and once post-window,
/// with the real handle. Centralising the *post-window* call here means
/// that two-call dance is documented and owned in one place instead of
/// re-derived at every window-open call site as the codebase grows more
/// windows/dialogs.
pub fn sync_theme_with_window(window: &mut Window, cx: &mut App) {
    gpui_component::Theme::sync_system_appearance(Some(window), cx);
    apply_font_scale(cx);
}

/// `gpui_component::theme::Theme::apply_config` (run by every
/// `sync_system_appearance`/mode-change call above, light<->dark included)
/// unconditionally resets `font_size`/`mono_font_size` back to its own
/// built-in 16px/13px defaults whenever the active `ThemeConfig` doesn't
/// specify them -- which Duet's never does, so a one-time override in
/// `duet_widgets::init` would only survive until the next appearance sync
/// (observed happening several times within the first second of startup
/// alone, via `ThemeController`'s live follow-system). Re-asserting the
/// scaled-down size here, right after every sync, is what actually makes
/// it stick.
fn apply_font_scale(cx: &mut App) {
    const SCALE: f32 = 0.8; // 20% smaller than gpui-component's defaults.
    let theme = gpui_component::Theme::global_mut(cx);
    theme.font_size = gpui::px(16.0 * SCALE);
    theme.mono_font_size = gpui::px(13.0 * SCALE);
}

/// The concrete element type `gpui_component::table::TableDelegate`'s
/// `render_tr` method must return.
///
/// Why this is wrapped: that trait method is typed to return
/// `gpui::Stateful<gpui::Div>` rather than `impl IntoElement` -- a
/// specific GPUI element-wrapper type baked directly into a
/// `gpui-component` trait signature (confirmed by reading
/// `gpui-component-0.5.1`'s `src/table/delegate.rs` while building this
/// façade, and consistent with S-1's spike delegate,
/// `spikes/s1-virtualised-table/src/table_delegate.rs`, needing the same
/// import). Element-wrapper types are exactly the kind of thing pre-1.0
/// GPUI has reshuffled before (design.md's R-G1). If a future
/// `gpui`/`gpui-component` bump changes this concrete type, every
/// `TableDelegate` implementation across `duet-widgets`/`duet-ui` would
/// otherwise need a matching edit; naming it here means a version bump
/// only has to update this alias, and implementers write `TableRow`
/// rather than the upstream type.
pub type TableRow = gpui::Stateful<gpui::Div>;
