// SPDX-License-Identifier: MIT
//! Hot reload: watch a config file for external changes and reload it off
//! the UI thread, per `docs/config-schema.md`'s "Hot reload" section and
//! T-3.1.6's UI-thread blocking guard.
//!
//! The watch registration, the `notify` event loop, the debounce timer, and
//! every reload's parse/migrate/deserialize all happen on one dedicated
//! background thread spawned by [`watch`]. Nothing here ever runs on the
//! thread that owns the UI. Total edit-to-callback latency budget is
//! 200 ms (T-3.3.1 AC); the ~50 ms debounce plus a parse of a config file
//! (kilobytes, not gigabytes) comfortably fits inside that.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::de::DeserializeOwned;

use crate::document::ConfigFile;
use crate::error::{ConfigError, Result};
use crate::migrate::MigrationRegistry;

/// Debounce window absorbing editor save patterns (write-then-rename,
/// multiple writes within one logical save) -- `docs/config-schema.md`
/// specifies ~50 ms.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// How long the watch loop blocks on the event channel when no reload is
/// pending. Just needs to be short enough that [`ConfigWatcher::drop`]
/// doesn't stall noticeably; it does not affect reload latency.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// A background handle watching one config file for external changes.
///
/// Dropping it stops the watcher thread (best-effort join). The watch stays
/// active for as long as this value is alive.
pub struct ConfigWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    // Kept alive only so the OS-level watch (inotify) stays registered;
    // never read directly after construction.
    _watcher: RecommendedWatcher,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Watches `path` for external changes. Whenever a burst of filesystem
/// activity touching `path` settles (after the debounce window), the file
/// is reloaded off-thread and `on_change` is called with the freshly
/// deserialized value.
///
/// A parse or migration failure calls `on_error` instead and does **not**
/// call `on_change` -- per `docs/config-schema.md`, "a malformed file on
/// reload does not tear down the running config": the caller's own
/// last-known-good value (whatever it captured from the previous
/// `on_change`, or the initial load) simply stays in effect since it's
/// never told to replace it.
///
/// `path`'s *parent directory* is what's actually watched (not the file
/// inode itself), because editors commonly save via write-to-temp +
/// rename-over-target, which would silently orphan a watch on the original
/// inode.
pub fn watch<T>(
    path: impl Into<PathBuf>,
    migrations: MigrationRegistry,
    target_version: u32,
    on_change: impl Fn(T) + Send + 'static,
    on_error: impl Fn(ConfigError) + Send + 'static,
) -> Result<ConfigWatcher>
where
    T: DeserializeOwned + Send + 'static,
{
    let path = path.into();
    let watch_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = path.file_name().map(OsStr::to_owned);

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx).map_err(|source| ConfigError::Watch {
        path: path.clone(),
        source,
    })?;
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|source| ConfigError::Watch {
            path: path.clone(),
            source,
        })?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread_path = path.clone();

    let handle = std::thread::Builder::new()
        .name("duet-config-watch".into())
        .spawn(move || {
            run_loop(
                &thread_path,
                &file_name,
                &migrations,
                target_version,
                &rx,
                &stop_thread,
                &on_change,
                &on_error,
            )
        })
        .map_err(ConfigError::Io)?;

    Ok(ConfigWatcher {
        stop,
        handle: Some(handle),
        _watcher: watcher,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_loop<T: DeserializeOwned>(
    path: &Path,
    file_name: &Option<std::ffi::OsString>,
    migrations: &MigrationRegistry,
    target_version: u32,
    rx: &mpsc::Receiver<notify::Result<Event>>,
    stop: &AtomicBool,
    on_change: &(impl Fn(T) + Send + 'static),
    on_error: &(impl Fn(ConfigError) + Send + 'static),
) {
    let mut pending_deadline: Option<Instant> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let wait = match pending_deadline {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
            None => IDLE_POLL,
        };
        match rx.recv_timeout(wait) {
            Ok(Ok(event)) => {
                if event_touches(&event, file_name.as_deref()) {
                    pending_deadline = Some(Instant::now() + DEBOUNCE);
                }
            }
            Ok(Err(_)) => {
                // A notify-internal error (e.g. a transient inotify
                // hiccup). Nothing actionable; keep watching.
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(deadline) = pending_deadline
                    && Instant::now() >= deadline
                {
                    pending_deadline = None;
                    reload(path, migrations, target_version, on_change, on_error);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn event_touches(event: &Event, file_name: Option<&OsStr>) -> bool {
    if !matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    match file_name {
        Some(name) => event.paths.iter().any(|p| p.file_name() == Some(name)),
        None => true,
    }
}

fn reload<T: DeserializeOwned>(
    path: &Path,
    migrations: &MigrationRegistry,
    target_version: u32,
    on_change: &(impl Fn(T) + Send + 'static),
    on_error: &(impl Fn(ConfigError) + Send + 'static),
) {
    let text = match crate::io::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            on_error(e);
            return;
        }
    };
    match ConfigFile::<T>::from_str(path, &text, migrations, target_version) {
        Ok(file) => match file.typed() {
            Ok(value) => on_change(value),
            Err(e) => on_error(e),
        },
        Err(e) => on_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SETTINGS_SCHEMA_VERSION, Settings};
    use std::sync::mpsc::channel;
    use std::time::Instant;

    #[test]
    fn external_edit_applies_within_200ms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n\n[panels]\nshow_hidden = false\n",
        )
        .unwrap();

        let (tx, rx) = channel::<Settings>();
        let _watcher = watch::<Settings>(
            &path,
            MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
            move |settings| {
                let _ = tx.send(settings);
            },
            |err| panic!("unexpected reload error: {err}"),
        )
        .unwrap();

        // Give the watcher a moment to finish registering with inotify
        // before we perform the edit we're timing.
        std::thread::sleep(Duration::from_millis(50));

        let start = Instant::now();
        // Simulate an external editor save: write-to-temp + rename-over,
        // the exact pattern docs/config-schema.md calls out as what the
        // debounce must absorb.
        let tmp = dir.path().join("settings.toml.tmp");
        std::fs::write(&tmp, "schema_version = 1\n\n[panels]\nshow_hidden = true\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();

        let settings = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reload callback did not fire");
        let elapsed = start.elapsed();
        eprintln!("hot reload latency: {elapsed:?}");

        assert!(
            settings.panels.show_hidden,
            "reloaded value should reflect the external edit"
        );
        assert!(
            elapsed <= Duration::from_millis(200),
            "hot reload took {elapsed:?}, budget is 200ms"
        );
    }

    #[test]
    fn malformed_external_edit_reports_error_not_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "schema_version = 1\n").unwrap();

        let (change_tx, change_rx) = channel::<Settings>();
        let (err_tx, err_rx) = channel::<()>();
        let _watcher = watch::<Settings>(
            &path,
            MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
            move |settings| {
                let _ = change_tx.send(settings);
            },
            move |_err| {
                let _ = err_tx.send(());
            },
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&path, "this is not [ valid toml").unwrap();

        err_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected an error callback for malformed TOML");
        assert!(
            change_rx.try_recv().is_err(),
            "on_change must not fire for a malformed reload"
        );
    }
}
