//! T-3.1.6 — the UI-thread blocking guard.
//!
//! Directive: "Never execute disk I/O, `stat`, or syscalls on the main
//! thread." `LocalFs`'s methods do their blocking work *inline* (see the
//! module-level doc comment on `local::mod` for why: the actual off-thread
//! dispatch is the shell layer's job, built in a later task) — which means
//! the one thing this crate can do *today* to make "never on the UI thread"
//! a checked invariant rather than a hoped-for convention is to assert it,
//! defensively, at every blocking call site.
//!
//! The mechanism: a thread-local flag, false by default on every thread.
//! The shell layer is expected to call [`mark_ui_thread`] exactly once, at
//! startup, on whichever OS thread GPUI's main/UI thread actually is. Every
//! `LocalFs` syscall wrapper calls [`assert_not_ui_thread`] before issuing a
//! blocking syscall; if that thread happens to be flagged, the
//! `debug_assert!` panics in a debug build. This task builds and proves the
//! mechanism only — nothing in this crate calls `mark_ui_thread` today, that
//! wiring lands wherever the GPUI shell boots (outside `duet-vfs`'s
//! scope).
//!
//! # Zero overhead in release
//!
//! `debug_assert!` expands to `if cfg!(debug_assertions) { assert!(...) }`
//! (see `std`'s definition). `cfg!(debug_assertions)` is a compile-time
//! constant, so in a release build (`debug-assertions = false`, the
//! `[profile.release]` default this workspace does not override — see the
//! comment on `[profile.dev]` in the workspace `Cargo.toml`, which
//! deliberately force-enables assertions in *dev* but leaves release
//! untouched) the whole `if false { ... }` branch — condition,
//! thread-local access, and panic machinery alike — is dead code from
//! LLVM's point of view and is eliminated during optimization, not merely
//! skipped at runtime. The thread-local itself uses `std`'s fast
//! `#[thread_local]`-backed storage (a `Cell<bool>`, `Copy`, no lazy
//! `OnceCell`/allocation on the read path) specifically so that even the
//! debug-build cost of a hit is a single TLS-relative load, not a hashmap
//! lookup or a lock.
//!
//! See `local/tests/guard.rs`-equivalent unit tests below for the two
//! properties this file must prove: (a) an unflagged thread never panics,
//! (b) a flagged thread panics in a debug build.

use std::cell::Cell;

thread_local! {
    static IS_UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as the UI thread. Intended to be called exactly
/// once, at startup, by the shell layer — not by anything in `duet-vfs`
/// itself. Idempotent and cheap enough to call more than once if a caller
/// needs to (e.g. test setup/teardown).
pub fn mark_ui_thread() {
    IS_UI_THREAD.with(|f| f.set(true));
}

/// Clears the calling thread's UI-thread flag. Mainly for tests that need
/// to simulate leaving the flag set behind them from contaminating other
/// tests sharing the same thread-pool worker thread.
pub fn unmark_ui_thread() {
    IS_UI_THREAD.with(|f| f.set(false));
}

/// `true` if the calling thread is currently flagged as the UI thread.
pub fn is_ui_thread() -> bool {
    IS_UI_THREAD.with(Cell::get)
}

/// Call before every blocking syscall wrapper in `local`. Panics (debug
/// builds only) if the calling thread is flagged as the UI thread —
/// T-3.1.6's core invariant: `LocalFs` must never block the UI thread.
#[inline(always)]
pub fn assert_not_ui_thread() {
    debug_assert!(
        !is_ui_thread(),
        "T-3.1.6 violation: a blocking LocalFs syscall was about to run on \
         a thread flagged as the UI thread via guard::mark_ui_thread(). \
         Dispatch filesystem work to a background thread/executor instead."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against thread-local leakage between tests running on a
    /// reused test-harness thread.
    struct ResetOnDrop;
    impl Drop for ResetOnDrop {
        fn drop(&mut self) {
            unmark_ui_thread();
        }
    }

    #[test]
    fn unflagged_thread_does_not_panic() {
        let _reset = ResetOnDrop;
        unmark_ui_thread();
        assert!(!is_ui_thread());
        // Must not panic.
        assert_not_ui_thread();
    }

    #[test]
    #[should_panic(expected = "T-3.1.6 violation")]
    fn flagged_thread_panics_in_debug_build() {
        let _reset = ResetOnDrop;
        mark_ui_thread();
        assert!(is_ui_thread());
        assert_not_ui_thread();
    }

    #[test]
    fn flag_is_thread_local_not_global() {
        let _reset = ResetOnDrop;
        unmark_ui_thread();
        let handle = std::thread::spawn(|| {
            // A fresh thread never inherits the flag from its spawner.
            assert!(!is_ui_thread());
            mark_ui_thread();
            assert!(is_ui_thread());
        });
        handle.join().unwrap();
        // The spawning thread's flag is unaffected by the child thread.
        assert!(!is_ui_thread());
    }
}
