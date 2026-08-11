// SPDX-License-Identifier: MIT
//! Logging, the in-memory trace ring buffer, and the crash-file writer
//! (design.md §12 "Error handling, logging, diagnostics"; WBS T-3.3.3).
//!
//! This module is app-lifecycle wiring rather than a reusable service, so
//! it lives in the `duet` binary crate (design.md §8.1: `duet/` is
//! "binary: wiring, CLI, single-instance, lifecycle") rather than in a
//! separate crate or in `duet-config` (settings/themes/hotlist
//! persistence -- a different concern from process-lifetime diagnostics).
//! It has no dependency on `gpui` and does no filesystem access from the
//! UI thread outside of the panic hook, which only ever runs once, while
//! the process is already terminating.
//!
//! Call [`init_tracing`] once, early in `main`, then pass its result to
//! [`install_panic_hook`] together with [`crash_dir`]:
//!
//! ```ignore
//! let ring = diagnostics::init_tracing();
//! diagnostics::install_panic_hook(ring, diagnostics::crash_dir());
//! ```
//!
//! (`ignore`, not `no_run`: `duet` is a binary-only crate with no library
//! target, so cargo does not collect or run doctests for it regardless;
//! this is illustrative wiring, shown as it appears in `main.rs`.)

mod crash;
mod ring_buffer;

pub use crash::{crash_dir, install_panic_hook};
pub use ring_buffer::{RING_BUFFER_CAPACITY, RingBuffer, RingBufferLayer};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Installs the global `tracing` subscriber and returns the [`RingBuffer`]
/// it feeds, ready to be handed to [`install_panic_hook`].
///
/// Two layers are attached, both gated by the same filter:
/// - `tracing_subscriber::fmt`'s layer, for normal human-readable output
///   on stderr.
/// - [`RingBufferLayer`], which mirrors every event that passes the
///   filter into an in-memory ring buffer for the panic hook to dump on
///   crash.
///
/// The filter is read from the `DUET_LOG` environment variable (design.md
/// §12: "per-crate filters; `--log-level` and `DUET_LOG`"), falling back
/// to `info` if unset or invalid. `--log-level` CLI plumbing is left for
/// whichever task wires up `duet`'s argument parser (none exists yet --
/// `main.rs` is still a Phase 2 skeleton); a future flag can set
/// `DUET_LOG` in the process environment before calling this function, or
/// this function can grow a parameter, without changing the layering
/// here.
///
/// Must be called once, early in `main`, before any other `tracing::*`
/// call. Panics if a global subscriber has already been installed --
/// deliberately: a second call means something is wired up twice, and
/// that is a startup bug worth crashing loudly on rather than silently
/// keeping only the first subscriber.
pub fn init_tracing() -> RingBuffer {
    let ring = RingBuffer::new(RING_BUFFER_CAPACITY);
    let ring_layer = RingBufferLayer::new(ring.clone());

    let env_filter = EnvFilter::try_from_env("DUET_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(ring_layer)
        .init();

    ring
}
