// SPDX-License-Identifier: MIT
//! Duet: wiring, CLI, single-instance activation, application lifecycle.
//!
//! Phase 2 (T-2.1.1) skeleton -- real bootstrap arrives with T-4.1.1.
//! T-3.3.3 adds the one piece of lifecycle wiring that has to exist before
//! anything else can usefully run: logging and the crash-file writer
//! (design.md §12).

mod diagnostics;

fn main() {
    let ring = diagnostics::init_tracing();
    diagnostics::install_panic_hook(ring, diagnostics::crash_dir());

    tracing::info!("duet: phase 2 skeleton, no UI yet");
    println!("duet: phase 2 skeleton, no UI yet");
}
