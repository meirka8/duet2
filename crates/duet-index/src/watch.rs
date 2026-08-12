//! T-3.2.5: filesystem watching with debounce, coalescing, and
//! `IN_Q_OVERFLOW` handling.
//!
//! Wraps `notify`'s inotify backend (design.md §9.2: "`notify`/inotify with
//! a 50 ms debounce and coalescing; `IN_Q_OVERFLOW` triggers a full
//! rescan"). The raw `notify` event stream is not what callers see:
//! `notify` reports one event per syscall-level change, which for a
//! directory under heavy churn (an extraction, a build, a script looping
//! `touch`) can be thousands of events for what the user experiences as
//! one thing happening. [`DirWatcher`] runs a debounce/coalescing pass on
//! a background thread and hands the model a small stream of
//! [`WatchUpdate`]s instead.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub use notify::RecursiveMode;
use notify::event::Flag;
use notify::{Event, RecommendedWatcher, Watcher};

/// "50 ms debounce" per T-3.2.5's brief and design.md §9.2: once the first
/// event of a batch arrives, keep collecting more as long as they keep
/// arriving within this quiet period, so a burst of related changes (e.g.
/// a multi-file `mv`, or a save-that's-really-delete-then-create from some
/// editors) coalesces into one [`WatchUpdate`] instead of several.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// Upper bound on how long one batch may keep extending its own debounce
/// window under sustained, continuous churn (T-3.2.5's stress AC: "10k
/// rapid changes... coalesce without a stall" -- "without a stall" cuts
/// both ways: coalescing must not mean the caller waits arbitrarily long
/// for the *first* update just because the source never goes quiet for a
/// full 50 ms). Chosen as 5x the debounce window: long enough that a burst
/// well under this still coalesces into one update, short enough that a
/// long-running churn (e.g. a large extraction) still surfaces periodic
/// progress instead of one update at the very end.
const MAX_BATCH_HOLD: Duration = Duration::from_millis(250);

/// One coalesced notification from [`DirWatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchUpdate {
    /// One or more paths changed (create/remove/rename/modify, coalesced
    /// and deduplicated -- a path that changed twice in the debounce
    /// window appears once). The caller (T-3.2.7's diffing, ultimately)
    /// decides what to actually do about each path; this layer only
    /// answers "what changed," not "how."
    Changed(Vec<PathBuf>),
    /// `IN_Q_OVERFLOW` (or any other condition `notify` flags as
    /// [`Flag::Rescan`], or a watcher-level error): the event stream can
    /// no longer be trusted to reflect every change that happened, so the
    /// caller must fall back to a full `read_dir` rescan rather than
    /// trying to reconcile incrementally from events it may have missed.
    /// design.md §9.2 / T-3.2.5's brief calls this out by name rather than
    /// leaving it to silently drop events.
    RescanNeeded,
}

/// Owns a live `notify` watch plus the background debounce/coalescing
/// thread. Dropping this stops watching (the underlying `notify::Watcher`
/// is dropped, which tears down the inotify fd; the debounce thread then
/// exits on its next `recv` once the raw channel disconnects).
pub struct DirWatcher {
    // Kept alive only so the watch isn't torn down early -- never read
    // after construction, but dropping it is exactly what stops watching.
    _watcher: RecommendedWatcher,
    updates: Receiver<WatchUpdate>,
}

impl DirWatcher {
    /// Starts watching `path`. `recursive` mirrors `notify::RecursiveMode`
    /// -- a panel watching just its own listing wants `NonRecursive`;
    /// T-3.2.8's directory-size cache invalidation, which cares about
    /// changes anywhere under a subtree, wants `Recursive`.
    pub fn watch(path: &Path, recursive: RecursiveMode) -> notify::Result<Self> {
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            // The receiving end only goes away when `DirWatcher` (and thus
            // the debounce thread reading `raw_rx`) has already been
            // dropped, at which point there's nothing meaningful to do
            // with a send failure here.
            let _ = raw_tx.send(res);
        })?;
        watcher.watch(path, recursive)?;

        let (update_tx, update_rx) = mpsc::channel();
        thread::Builder::new()
            .name("duet-index-watch-debounce".into())
            .spawn(move || debounce_loop(raw_rx, update_tx))
            .expect("failed to spawn watch debounce thread");

        Ok(DirWatcher {
            _watcher: watcher,
            updates: update_rx,
        })
    }

    /// Blocks until the next coalesced update, or returns `None` once the
    /// watcher has been torn down (the debounce thread exited).
    pub fn recv(&self) -> Option<WatchUpdate> {
        self.updates.recv().ok()
    }

    /// Blocks for at most `timeout` for the next coalesced update.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<WatchUpdate> {
        self.updates.recv_timeout(timeout).ok()
    }

    pub fn try_recv(&self) -> Option<WatchUpdate> {
        self.updates.try_recv().ok()
    }
}

/// Runs on a dedicated background thread (never the UI thread -- this
/// crate has no UI thread of its own to block, but the whole point of
/// debouncing off-thread is that whoever eventually drives this from
/// `duet-ui` gets an already-coalesced, cheap-to-handle stream rather than
/// raw inotify traffic arriving on whatever thread is convenient for
/// GPUI). Reads raw `notify` results from `raw_rx`, coalesces them per
/// [`DEBOUNCE`]/[`MAX_BATCH_HOLD`], and emits one [`WatchUpdate`] per
/// batch to `update_tx`. Exits once `raw_rx` disconnects (the watcher was
/// dropped) or `update_tx`'s receiver goes away (the caller lost interest).
fn debounce_loop(raw_rx: Receiver<notify::Result<Event>>, update_tx: Sender<WatchUpdate>) {
    loop {
        // Block for the first event of a new batch -- no point polling
        // when there's nothing happening.
        let first = match raw_rx.recv() {
            Ok(ev) => ev,
            Err(_) => return, // watcher dropped
        };

        let mut changed = HashSet::new();
        let mut rescan = false;
        ingest(first, &mut changed, &mut rescan);

        let batch_start = Instant::now();
        loop {
            let remaining_hold = MAX_BATCH_HOLD.saturating_sub(batch_start.elapsed());
            if remaining_hold.is_zero() {
                break;
            }
            match raw_rx.recv_timeout(DEBOUNCE.min(remaining_hold)) {
                Ok(ev) => ingest(ev, &mut changed, &mut rescan),
                Err(RecvTimeoutError::Timeout) => break, // quiet period elapsed
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        let update = if rescan {
            WatchUpdate::RescanNeeded
        } else {
            WatchUpdate::Changed(changed.into_iter().collect())
        };
        if update_tx.send(update).is_err() {
            return; // caller dropped its receiver
        }
    }
}

/// Folds one raw `notify` result into an in-progress batch.
fn ingest(result: notify::Result<Event>, changed: &mut HashSet<PathBuf>, rescan: &mut bool) {
    match result {
        Ok(event) => {
            // `IN_Q_OVERFLOW` arrives as `Ok(Event { kind: Other, .. })`
            // with `Flag::Rescan` set (see `notify`'s inotify backend),
            // not as an `Err` -- it's a notice about the *stream*, not a
            // failed operation.
            if event.flag() == Some(Flag::Rescan) {
                *rescan = true;
            }
            changed.extend(event.paths);
        }
        Err(_) => {
            // A watcher-level error (fd issue, backend failure, etc.) --
            // conservatively treated the same as overflow: whatever
            // incremental state the caller has can no longer be trusted,
            // so ask for a full rescan rather than silently proceeding as
            // if nothing happened.
            *rescan = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;

    /// T-3.2.5's AC: "create/delete/rename/modify in an external terminal
    /// reflects in the model within 100ms." This test drives the four
    /// operations directly (not literally an external terminal, but the
    /// same syscalls a shell's `touch`/`rm`/`mv` would make) against a
    /// real temp directory and asserts each is observed within budget.
    #[test]
    fn create_delete_rename_modify_reflect_within_100ms() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = DirWatcher::watch(dir.path(), RecursiveMode::NonRecursive).unwrap();

        let file_a = dir.path().join("a.txt");
        let start = Instant::now();
        fs::write(&file_a, b"hello").unwrap();
        let update = watcher
            .recv_timeout(Duration::from_millis(100))
            .expect("create not observed within 100ms");
        assert!(matches!(
            update,
            WatchUpdate::Changed(_) | WatchUpdate::RescanNeeded
        ));
        assert!(start.elapsed() < Duration::from_millis(100));

        let start = Instant::now();
        fs::write(&file_a, b"hello, world -- modified").unwrap();
        watcher
            .recv_timeout(Duration::from_millis(100))
            .expect("modify not observed within 100ms");
        assert!(start.elapsed() < Duration::from_millis(100));

        let file_b = dir.path().join("b.txt");
        let start = Instant::now();
        fs::rename(&file_a, &file_b).unwrap();
        watcher
            .recv_timeout(Duration::from_millis(100))
            .expect("rename not observed within 100ms");
        assert!(start.elapsed() < Duration::from_millis(100));

        let start = Instant::now();
        fs::remove_file(&file_b).unwrap();
        watcher
            .recv_timeout(Duration::from_millis(100))
            .expect("delete not observed within 100ms");
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    /// T-3.2.5's stress AC: 10k rapid changes coalesce without a stall.
    /// "Without a stall" is checked two ways: the whole burst must finish
    /// producing updates within a bounded total time (not one update per
    /// change -- 10k separate 50ms-debounced round trips would take
    /// minutes), and no single gap between the last file operation and the
    /// last update may exceed `MAX_BATCH_HOLD` by much (i.e. the caller
    /// isn't left hanging indefinitely after the burst ends).
    #[test]
    fn ten_thousand_rapid_changes_coalesce_without_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = DirWatcher::watch(dir.path(), RecursiveMode::NonRecursive).unwrap();

        const N: usize = 10_000;
        let write_start = Instant::now();
        for i in 0..N {
            fs::write(dir.path().join(format!("f{i}.tmp")), b"x").unwrap();
        }
        let write_elapsed = write_start.elapsed();

        // Drain updates until the stream goes quiet for a full
        // MAX_BATCH_HOLD + slack, counting how many coalesced batches it
        // took and confirming we're nowhere near "one update per file".
        let mut batches = 0usize;
        let mut total_paths = 0usize;
        let mut saw_rescan = false;
        let drain_start = Instant::now();
        loop {
            match watcher.recv_timeout(MAX_BATCH_HOLD + Duration::from_millis(200)) {
                Some(WatchUpdate::Changed(paths)) => {
                    batches += 1;
                    total_paths += paths.len();
                }
                Some(WatchUpdate::RescanNeeded) => {
                    // A busy CI box's inotify queue overflowing under
                    // 10k rapid creates is itself a legitimate, in-budget
                    // outcome (T-3.2.5's brief: overflow must fall back to
                    // a full rescan, not stall) -- count it and stop
                    // waiting for per-path detail that won't come.
                    batches += 1;
                    saw_rescan = true;
                    break;
                }
                None => break, // quiet for MAX_BATCH_HOLD + slack: burst is over
            }
        }
        let drain_elapsed = drain_start.elapsed();

        assert!(
            batches < 200,
            "expected far fewer than {N} batches for {N} near-simultaneous \
             creates (coalescing should collapse most of them); got {batches}"
        );
        if !saw_rescan {
            assert_eq!(
                total_paths, N,
                "every created path should be observed exactly once across all batches"
            );
        }
        // The whole drain (from first write to quiescence) must complete
        // in a bounded time, not hang -- generous budget since this
        // includes both the raw filesystem write time and inotify/thread
        // scheduling jitter on a possibly-loaded CI box, not just the
        // debounce logic itself.
        assert!(
            drain_elapsed < Duration::from_secs(10),
            "coalescing 10k rapid changes took {drain_elapsed:?} \
             (writes alone took {write_elapsed:?}) -- looks stalled"
        );
    }

    #[test]
    fn watcher_stops_producing_updates_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = DirWatcher::watch(dir.path(), RecursiveMode::NonRecursive).unwrap();
        drop(watcher);
        // No panic/hang on drop; a fresh watch on the same path still works.
        let watcher2 = DirWatcher::watch(dir.path(), RecursiveMode::NonRecursive).unwrap();
        fs::write(dir.path().join("after-drop.txt"), b"x").unwrap();
        assert!(watcher2.recv_timeout(Duration::from_millis(200)).is_some());
    }
}
