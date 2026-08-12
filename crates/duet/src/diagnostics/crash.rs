// SPDX-License-Identifier: MIT
//! The panic hook and crash-file writer (design.md §12, T-3.3.3).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::ring_buffer::{RING_BUFFER_CAPACITY, RingBuffer};

/// Resolves the directory crash reports are written to.
///
/// `$XDG_STATE_HOME/duet/crashes`, falling back to
/// `~/.local/state/duet/crashes` per the XDG Base Directory spec when
/// `XDG_STATE_HOME` is unset or empty. This sits under design.md §10's
/// `~/.local/state/duet/` state directory (which already holds
/// `session.json`, `history/`, and `jobs/*.journal`); `crashes/` follows
/// the same "one subdirectory per kind of persisted state" pattern as
/// `history/` and `jobs/`.
pub fn crash_dir() -> PathBuf {
    state_dir().join("crashes")
}

/// `$XDG_STATE_HOME/duet`, falling back to `~/.local/state/duet`.
fn state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("duet");
    }
    // Linux-only per design.md's scope, so `$HOME` is always the right
    // fallback root (no Windows/macOS profile-directory handling needed).
    // If even `$HOME` is unset (unusual, but possible under some minimal
    // containers/services), fall back to a per-process temp directory
    // rather than panicking -- a crash-diagnostics path must not itself be
    // a new source of panics.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/state/duet")
}

/// Installs a `std::panic::set_hook` that, on any panic, writes a crash
/// file into `dir` containing the panicking thread's name, the panic
/// message and source location, a Unix timestamp, and a snapshot of the
/// ring buffer's last (up to [`RING_BUFFER_CAPACITY`]) trace events
/// (design.md §12, T-3.3.3's acceptance criterion).
///
/// Chains to the previously installed hook first (normally the Rust
/// default, which prints the panic to stderr), so panic visibility on the
/// terminal is unchanged -- the crash file is additional, not a
/// replacement. This also matches design.md §12's "Panics in non-UI tasks
/// are caught at the task boundary... A panic on the UI thread writes a
/// crash file and attempts a session-state save before dying": the app's
/// normal panic behavior (abort, or letting the task boundary catch it)
/// is untouched; this hook only adds the crash artifact as a side effect.
///
/// Best-effort on the write itself: if the crash directory can't be
/// created or the file can't be written (read-only filesystem, out of
/// space -- the process is already dying), the failure is reported to
/// stderr and swallowed rather than causing a second panic inside the
/// panic hook, which would abort the process without unwinding.
pub fn install_panic_hook(ring: RingBuffer, dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let report = format_crash_report(info, &ring);
        if let Err(err) = write_crash_report(&dir, &report) {
            eprintln!(
                "duet: failed to write crash report to {}: {err}",
                dir.display()
            );
        }
    }));
}

/// Builds the crash report body: session state (timestamp, pid, thread,
/// panic location and message) followed by the ring buffer's trace-event
/// history, oldest first.
fn format_crash_report(info: &std::panic::PanicHookInfo<'_>, ring: &RingBuffer) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let message = panic_message(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown location>".to_string());

    let mut report = String::new();
    let _ = writeln!(report, "=== Duet crash report ===");
    let _ = writeln!(
        report,
        "timestamp_unix = {}.{:03}",
        now.as_secs(),
        now.subsec_millis()
    );
    let _ = writeln!(report, "pid = {}", std::process::id());
    let _ = writeln!(report, "thread = {thread_name}");
    let _ = writeln!(report, "location = {location}");
    let _ = writeln!(report, "message = {message}");
    let _ = writeln!(report);

    let events = ring.snapshot();
    let _ = writeln!(
        report,
        "=== last {} trace event(s) (ring buffer capacity {}) ===",
        events.len(),
        RING_BUFFER_CAPACITY
    );
    for line in events {
        let _ = writeln!(report, "{line}");
    }
    report
}

/// Extracts a human-readable message from a panic payload.
///
/// `panic!("literal")` payloads are `&str`; `panic!("{}", x)` and
/// `panic!("formatted {x}")` payloads are `String`. Anything else (a
/// caller of `panic_any` with a non-string payload) is rare and reported
/// as such rather than guessed at.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Writes `report` to a new timestamped file under `dir`, creating `dir`
/// (and any missing parents) first, and `fsync`s the file before
/// returning so the crash report survives even if the process is killed
/// immediately after the panic hook returns.
fn write_crash_report(dir: &Path, report: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let filename = format!("crash-{}-{}.txt", now.as_secs(), std::process::id());
    let path = dir.join(filename);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(report.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::ring_buffer::RingBufferLayer;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// Unique-per-test scratch directory under the OS temp dir, so
    /// parallel test runs (and the wider test suite) never collide.
    fn scratch_dir(label: &str) -> PathBuf {
        let unique = format!(
            "duet-crash-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        std::env::temp_dir().join(unique)
    }

    /// AC (T-3.3.3): a forced panic writes a crash file containing the
    /// last 200 trace events and the session state.
    ///
    /// Emits tracing events into a ring buffer, installs the real panic
    /// hook pointed at a scratch directory, triggers a deliberate panic
    /// through `catch_unwind`, and asserts the hook ran and produced a
    /// crash file with the expected content.
    #[test]
    fn panic_writes_crash_file_with_ring_buffer_and_session_state() {
        let dir = scratch_dir("basic");
        let _ = std::fs::remove_dir_all(&dir);

        let ring = RingBuffer::new(RING_BUFFER_CAPACITY);
        let layer = RingBufferLayer::new(ring.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "duet::test", "starting a risky operation");
            tracing::warn!(target: "duet::test", entries = 42, "getting suspicious");
        });
        assert_eq!(
            ring.len(),
            2,
            "events should have landed in the ring buffer before the panic"
        );

        install_panic_hook(ring.clone(), dir.clone());

        let panic_message = "deliberate test panic for T-3.3.3";
        let result = std::panic::catch_unwind(|| {
            panic!("{panic_message}");
        });
        assert!(result.is_err(), "catch_unwind should observe the panic");

        // Reset to the default hook so this test doesn't leak a closure
        // capturing `dir`/`ring` into later tests in this binary.
        let _ = std::panic::take_hook();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("crash dir {} was not created: {err}", dir.display()))
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one crash file in {}",
            dir.display()
        );

        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("=== Duet crash report ==="));
        assert!(
            content.contains(panic_message),
            "crash file missing panic message:\n{content}"
        );
        assert!(
            content.contains("timestamp_unix ="),
            "crash file missing timestamp:\n{content}"
        );
        assert!(
            content.contains("pid ="),
            "crash file missing pid:\n{content}"
        );
        assert!(
            content.contains("thread ="),
            "crash file missing thread name:\n{content}"
        );
        assert!(
            content.contains("location ="),
            "crash file missing panic location:\n{content}"
        );
        assert!(
            content.contains("starting a risky operation"),
            "crash file missing first ring-buffer event:\n{content}"
        );
        assert!(
            content.contains("getting suspicious") && content.contains("entries=42"),
            "crash file missing second ring-buffer event:\n{content}"
        );
        assert!(
            content.contains("=== last 2 trace event(s) (ring buffer capacity 200) ==="),
            "crash file missing event-count header:\n{content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dir_respects_xdg_state_home() {
        // SAFETY: `set_var`/`remove_var` are unsafe since Rust 2024
        // because env vars are process-global and technically racy with
        // other threads reading them; this test does not run any code
        // concurrently that reads `XDG_STATE_HOME`, and Rust test
        // harness threads each run a distinct `#[test]` fn without
        // interleaving env mutation from this one.
        let previous = std::env::var("XDG_STATE_HOME").ok();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", "/tmp/duet-xdg-test-state");
        }
        let dir = crash_dir();
        unsafe {
            match &previous {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        assert_eq!(dir, PathBuf::from("/tmp/duet-xdg-test-state/duet/crashes"));
    }
}
