// SPDX-License-Identifier: MIT
//! Polkit-activated privilege helper (FR-OPS-13).
//!
//! Phase 2 skeleton. Real implementation is T-9.1.13 (Phase 9). Exposes only
//! a fixed verb set (copy/move/delete/chmod/chown) on validated paths --
//! never a general command execution surface. See design.md section 9.10 / section 13.

fn main() {
    println!("duet-privileged: phase 2 skeleton, not yet wired to polkit");
}
