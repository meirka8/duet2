// SPDX-License-Identifier: MIT
//! Local FileSystem backend(s). Populated starting Phase 3 (local) / Phase 6 (archive) / Phase 7 (remote).
//!
//! # Architecture note: blocking-in-async, guarded rather than offloaded
//!
//! Every `FileSystem` method is `async` (via `#[async_trait]`), but the
//! actual syscalls this module issues (`getdents64`, `statx`, `openat`,
//! ...) are ordinary blocking Linux syscalls, called *inline* inside the
//! `async fn`/`Stream::poll` bodies rather than dispatched to a background
//! thread pool from within this crate. That dispatch — "which executor
//! thread is a `LocalFs` call allowed to run on" — is a decision that
//! belongs to whatever runs the ops engine/index layer above `duet-vfs`
//! (a later task), not to the backend itself, which has no opinion on the
//! host application's executor.
//!
//! What this module *does* own is making "never on the UI thread"
//! (NFR-02, T-3.1.6) a checked invariant rather than an unenforced
//! convention: every blocking call site in this module calls
//! [`guard::assert_not_ui_thread`] first, which `debug_assert!`s that the
//! calling thread hasn't been flagged via [`guard::mark_ui_thread`] (a call
//! the shell layer is expected to make, once, on GPUI's actual UI thread —
//! nothing in `duet-vfs` calls it itself). See `guard`'s module doc comment
//! for the full mechanism and the zero-release-overhead argument.

mod fs;
mod guard;
mod meta;
mod pathutil;
mod probe;
mod readdir;
mod rw;
mod statx;
mod traverse;

pub use fs::LocalFs;
pub use guard::{assert_not_ui_thread, is_ui_thread, mark_ui_thread, unmark_ui_thread};
pub use probe::{FsKind, FsProps};
