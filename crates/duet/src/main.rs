// SPDX-License-Identifier: MIT
//! Duet: wiring, CLI, single-instance activation, application lifecycle.
//!
//! T-3.3.3 wired logging and the crash-file writer (design.md §12).
//! T-4.1.1 adds the GPUI application bootstrap itself -- but per ADR-002
//! (`gpui`/`gpui-component` restricted to `duet-ui` and `duet-widgets`),
//! this crate never imports `gpui` directly: it just calls
//! [`duet_ui::run`], which owns the `Application`, the window, theme
//! init, and the core-to-UI executor wiring.

mod diagnostics;

fn main() {
    let ring = diagnostics::init_tracing();
    diagnostics::install_panic_hook(ring, diagnostics::crash_dir());

    tracing::info!("duet: starting GPUI shell");
    duet_ui::run();
}
