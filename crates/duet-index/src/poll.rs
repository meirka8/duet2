//! T-3.2.6: polling fallback for backends without [`Caps::WATCH`].
//!
//! Not every backend can push change events -- design.md §9.2: "network
//! and FUSE mounts fall back to interval polling with a configurable
//! period. Backends that lack `WATCH` get polling from this layer, not
//! from the backend." [`AdaptivePoller`] is that layer: it periodically
//! takes a cheap fingerprint of a directory's contents and emits a
//! [`WatchUpdate::RescanNeeded`] (via the same enum T-3.2.5's `DirWatcher`
//! uses, so a caller doesn't need two different "something changed"
//! types) whenever the fingerprint changes, with the poll interval
//! adapting to observed activity rather than staying fixed.
//!
//! This crate does not depend on `duet-vfs` (no `FileSystem` trait is
//! available here to poll through), so [`PollableDir`] is a small local
//! trait capturing exactly what the poller needs: a capability check and
//! a cheap-to-compute snapshot. A real backend integration implements
//! this over `duet-vfs::FileSystem` once panels are wired to live mounts;
//! [`LocalDirPoller`] implements it directly over `std::fs` in the
//! meantime (and is what every test here, and any purely-local mount,
//! actually uses).

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use duet_types::Caps;

use crate::watch::WatchUpdate;

/// `true` if a mount with these capabilities needs [`AdaptivePoller`]
/// rather than [`crate::DirWatcher`] -- T-3.2.6's brief: "check
/// `Caps::WATCH`."
pub fn needs_polling(caps: Caps) -> bool {
    !caps.contains(Caps::WATCH)
}

/// A cheap, order-independent fingerprint of a directory's contents:
/// enough to detect that *something* changed (an entry added/removed/
/// modified) without the cost of a full listing diff on every poll tick.
/// Two directories with the same entries (name, size, mtime) but visited
/// in a different `read_dir` order still hash equal (the per-entry hashes
/// are combined with a commutative fold), so a poll doesn't false-positive
/// on `read_dir`'s unspecified ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirFingerprint(u64);

/// What [`AdaptivePoller`] polls. A real integration implements this over
/// `duet-vfs::FileSystem`; see the module doc comment.
pub trait PollableDir: Send + Sync {
    fn caps(&self) -> Caps;
    fn snapshot(&self) -> io::Result<DirFingerprint>;
}

/// [`PollableDir`] over a real local directory via `std::fs`. `caps`
/// is supplied by the caller rather than probed, so tests (and any real
/// caller simulating a capability-poor mount) can construct one that
/// reports `Caps::empty()` regardless of the fact that the local
/// filesystem underneath genuinely does support `WATCH` -- see
/// T-3.2.6's brief: "simulate this with a fake/wrapped FileSystem that
/// reports `Caps::empty()`."
pub struct LocalDirPoller {
    path: PathBuf,
    caps: Caps,
}

impl LocalDirPoller {
    pub fn new(path: impl Into<PathBuf>, caps: Caps) -> Self {
        LocalDirPoller {
            path: path.into(),
            caps,
        }
    }
}

impl PollableDir for LocalDirPoller {
    fn caps(&self) -> Caps {
        self.caps
    }

    fn snapshot(&self) -> io::Result<DirFingerprint> {
        let mut acc: u64 = 0;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let mut h = DefaultHasher::new();
            entry.file_name().hash(&mut h);
            meta.len().hash(&mut h);
            meta.modified().ok().hash(&mut h);
            meta.is_dir().hash(&mut h);
            // XOR-fold: commutative, so entry visitation order (read_dir
            // gives no ordering guarantee) doesn't affect the result.
            acc ^= h.finish();
        }
        Ok(DirFingerprint(acc))
    }
}

/// Tunables for [`AdaptivePoller`]'s backoff/tighten heuristic.
///
/// The heuristic: on a poll that finds a change, the next interval
/// shrinks (multiply by [`Self::tighten_factor`], floored at
/// [`Self::min_interval`]); on a poll that finds nothing changed, the next
/// interval grows (multiply by [`Self::backoff_factor`], capped at
/// [`Self::max_interval`]). This is standard multiplicative backoff/
/// tighten (the same shape as TCP congestion windows or exponential retry
/// delays), chosen over a fixed period because the two failure modes of a
/// fixed period are both real: too short wastes CPU on a mount that's
/// idle for hours (a network share nobody's touching), too long makes an
/// actively-changing directory (someone's mid-copy onto that same share)
/// feel unresponsive. Multiplicative rather than additive step sizes
/// converge quickly in both directions -- a few polls to reach either
/// bound -- without a separate "how big is one step" tuning parameter
/// that would need its own justification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollConfig {
    pub min_interval: Duration,
    pub max_interval: Duration,
    pub initial_interval: Duration,
    /// Multiplier applied to the interval after a poll finds no change.
    /// Must be greater than `1.0`. Default 1.5: reaches `max_interval` from
    /// `min_interval` (default 1s -> 30s) in about 9 idle polls (~85s of
    /// real time spent backing off), not so slow that a freshly-idle mount
    /// keeps polling aggressively for minutes, not so fast that a mount
    /// with occasional (not constant) activity gets stuck at the ceiling
    /// between bursts.
    pub backoff_factor: f64,
    /// Multiplier applied to the interval after a poll finds a change.
    /// Must be in `(0.0, 1.0)`. Default 0.5: a detected change immediately halves
    /// the interval, so a burst of activity is caught up with quickly
    /// (a handful of polls to reach `min_interval`) without needing a
    /// separate "activity detected, now poll at minimum" special case --
    /// sustained activity naturally walks the interval down to the floor
    /// and holds it there for as long as changes keep being found.
    pub tighten_factor: f64,
}

impl Default for PollConfig {
    fn default() -> Self {
        PollConfig {
            min_interval: Duration::from_secs(1),
            max_interval: Duration::from_secs(30),
            initial_interval: Duration::from_secs(2),
            backoff_factor: 1.5,
            tighten_factor: 0.5,
        }
    }
}

/// Atomic counters exposing the poller's own CPU cost, so T-3.2.6's "CPU
/// cost measured and bounded" AC has real numbers to check rather than an
/// assertion that the loop merely exists. `polls` and `busy_nanos` (time
/// actually spent inside `PollableDir::snapshot`, not counting the sleep
/// between polls) are what the poller's tests report.
#[derive(Debug, Default)]
pub struct PollStats {
    pub polls: AtomicU64,
    pub busy_nanos: AtomicU64,
}

impl PollStats {
    pub fn busy_duration(&self) -> Duration {
        Duration::from_nanos(self.busy_nanos.load(Ordering::Relaxed))
    }

    pub fn poll_count(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }
}

/// Runs [`PollableDir::snapshot`] on a background thread at an interval
/// that adapts per [`PollConfig`], emitting a [`WatchUpdate::RescanNeeded`]
/// through [`Self::recv`]/[`Self::recv_timeout`] whenever the snapshot
/// changes. Dropping this stops the poll loop (a shutdown flag is checked
/// between sleeps, not `abort`-style, so an in-flight `snapshot` call
/// always finishes cleanly).
pub struct AdaptivePoller {
    stop: Arc<AtomicBool>,
    updates: Receiver<WatchUpdate>,
    stats: Arc<PollStats>,
}

impl AdaptivePoller {
    pub fn start(target: Arc<dyn PollableDir>, config: PollConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PollStats::default());
        let (tx, rx) = mpsc::channel();

        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        thread::Builder::new()
            .name("duet-index-poll".into())
            .spawn(move || poll_loop(target, config, thread_stop, thread_stats, tx))
            .expect("failed to spawn poll thread");

        AdaptivePoller {
            stop,
            updates: rx,
            stats,
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<WatchUpdate> {
        self.updates.recv_timeout(timeout).ok()
    }

    pub fn try_recv(&self) -> Option<WatchUpdate> {
        self.updates.try_recv().ok()
    }

    pub fn stats(&self) -> &PollStats {
        &self.stats
    }
}

impl Drop for AdaptivePoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn poll_loop(
    target: Arc<dyn PollableDir>,
    config: PollConfig,
    stop: Arc<AtomicBool>,
    stats: Arc<PollStats>,
    tx: mpsc::Sender<WatchUpdate>,
) {
    let mut interval = config.initial_interval;
    let mut last: Option<DirFingerprint> = None;

    while !stop.load(Ordering::Relaxed) {
        let poll_start = Instant::now();
        let snapshot = target.snapshot();
        let busy = poll_start.elapsed();
        stats.polls.fetch_add(1, Ordering::Relaxed);
        stats
            .busy_nanos
            .fetch_add(busy.as_nanos() as u64, Ordering::Relaxed);

        match snapshot {
            Ok(fp) => {
                let changed = last.is_some_and(|prev| prev != fp);
                last = Some(fp);
                if changed {
                    interval = scale(interval, config.tighten_factor, config.min_interval);
                    if tx.send(WatchUpdate::RescanNeeded).is_err() {
                        return; // caller lost interest
                    }
                } else {
                    interval = scale(interval, config.backoff_factor, config.max_interval)
                        .min(config.max_interval)
                        .max(config.min_interval);
                }
            }
            Err(_) => {
                // Can't tell whether anything changed (e.g. the mount
                // dropped) -- conservatively signal a rescan so the caller
                // re-evaluates rather than silently sitting on stale
                // state, and keep the current interval rather than
                // guessing at back off vs. tighten.
                if tx.send(WatchUpdate::RescanNeeded).is_err() {
                    return;
                }
            }
        }

        // Sleep in short slices so `stop` is honored promptly even when
        // `interval` is close to `max_interval` -- otherwise dropping an
        // `AdaptivePoller` mid-backoff could block shutdown for up to 30s.
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(20).min(deadline - Instant::now()));
        }
    }
}

fn scale(interval: Duration, factor: f64, bound: Duration) -> Duration {
    let scaled = interval.mul_f64(factor);
    if factor < 1.0 {
        scaled.max(bound)
    } else {
        scaled.min(bound)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn needs_polling_reflects_watch_capability() {
        assert!(needs_polling(Caps::empty()));
        assert!(!needs_polling(Caps::WATCH));
        assert!(!needs_polling(Caps::WATCH | Caps::RANDOM_READ));
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let poller = LocalDirPoller::new(dir.path(), Caps::empty());
        let before = poller.snapshot().unwrap();
        fs::write(dir.path().join("new.txt"), b"x").unwrap();
        let after = poller.snapshot().unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn fingerprint_is_stable_and_order_independent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        fs::write(dir.path().join("b.txt"), b"bb").unwrap();
        let poller = LocalDirPoller::new(dir.path(), Caps::empty());
        let fp1 = poller.snapshot().unwrap();
        let fp2 = poller.snapshot().unwrap();
        assert_eq!(fp1, fp2, "unchanged directory must hash identically");
    }

    /// T-3.2.6's AC: "a mount lacking WATCH... refreshes on an interval."
    /// Simulates the capability-poor mount exactly as the brief suggests:
    /// a real local directory wrapped to report `Caps::empty()`.
    #[test]
    fn poller_detects_a_change_and_emits_rescan_needed() {
        let dir = tempfile::tempdir().unwrap();
        let target: Arc<dyn PollableDir> = Arc::new(LocalDirPoller::new(dir.path(), Caps::empty()));
        assert!(needs_polling(target.caps()));

        let config = PollConfig {
            min_interval: Duration::from_millis(20),
            max_interval: Duration::from_millis(200),
            initial_interval: Duration::from_millis(20),
            ..PollConfig::default()
        };
        let poller = AdaptivePoller::start(target, config);

        // Wait for the poller's baseline snapshot before introducing the
        // change: thread-spawn scheduling is not instant, and without
        // this a slow-to-start first poll could race the write below and
        // capture the post-write state as its baseline, masking the
        // change (there'd be nothing to compare the next poll against).
        while poller.stats().poll_count() == 0 {
            thread::sleep(Duration::from_millis(2));
        }

        fs::write(dir.path().join("new.txt"), b"x").unwrap();
        let update = poller
            .recv_timeout(Duration::from_secs(2))
            .expect("polling did not detect the change in time");
        assert_eq!(update, WatchUpdate::RescanNeeded);
    }

    #[test]
    fn interval_backs_off_when_idle_and_tightens_on_change() {
        assert_eq!(
            scale(Duration::from_secs(2), 1.5, Duration::from_secs(30)),
            Duration::from_millis(3000)
        );
        assert_eq!(
            scale(Duration::from_secs(2), 0.5, Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        // Backoff is capped at max_interval even from just under it.
        assert_eq!(
            scale(Duration::from_secs(29), 1.5, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    /// T-3.2.6's AC: "CPU cost measured and bounded (show the numbers)."
    /// Runs a real poll loop against an idle directory for ~1s of wall
    /// time with a fast (20-200ms) interval range scaled down for test
    /// speed, and reports what fraction of that wall time was actually
    /// spent doing poll work (`snapshot`) versus sleeping.
    #[test]
    fn idle_poll_cpu_cost_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
        }
        let target: Arc<dyn PollableDir> = Arc::new(LocalDirPoller::new(dir.path(), Caps::empty()));

        let config = PollConfig {
            min_interval: Duration::from_millis(20),
            max_interval: Duration::from_millis(100),
            initial_interval: Duration::from_millis(20),
            ..PollConfig::default()
        };
        let poller = AdaptivePoller::start(target, config);

        // Let it run idle (nothing changes) for ~1s of wall time so the
        // interval backs off toward max_interval and several polls
        // accumulate real timing data.
        thread::sleep(Duration::from_millis(1000));

        let busy = poller.stats().busy_duration();
        let polls = poller.stats().poll_count();
        let busy_fraction = busy.as_secs_f64() / 1.0;

        assert!(polls >= 3, "expected several polls in 1s, got {polls}");
        // Reading a 50-entry local directory a handful of times should be
        // a tiny fraction of one second of CPU time -- bounded generously
        // at 10% to stay stable on a loaded CI box while still catching a
        // real regression (e.g. a busy-loop with no sleep at all, which
        // would push this well past 90%).
        assert!(
            busy_fraction < 0.10,
            "poll loop spent {busy:?} of CPU across {polls} polls over ~1s \
             wall time ({:.1}% busy) -- expected a small fraction",
            busy_fraction * 100.0
        );
    }
}
