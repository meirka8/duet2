// SPDX-License-Identifier: MIT
//! The in-memory trace-event ring buffer (design.md §12, T-3.3.3).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Number of formatted trace-event lines retained in the ring buffer.
///
/// design.md §12 specifies "a ring buffer of the last N events dumped
/// alongside any crash"; the task table (T-3.3.3) pins `N` at 200.
pub const RING_BUFFER_CAPACITY: usize = 200;

/// A fixed-capacity, thread-safe ring buffer of formatted trace-event
/// lines.
///
/// Backed by a `Mutex<VecDeque<String>>` capped at [`RING_BUFFER_CAPACITY`]
/// entries: once full, appending a new line evicts the oldest one. This is
/// a deliberately simple Phase 3 implementation -- a lock-free ring buffer
/// would be premature here. Event volume through this path is low (it
/// exists for crash diagnostics, not a hot per-frame path), so a mutex
/// never becomes a bottleneck, and correctness/readability matter more for
/// a crash-diagnostics artifact than raw throughput.
///
/// Cheap to clone: internally an `Arc`, so [`RingBufferLayer`] (which
/// writes into it from the tracing dispatch path) and the panic hook
/// installed by [`super::install_panic_hook`] (which reads a snapshot from
/// it) share one buffer without any unsafe code or global statics.
#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
}

impl RingBuffer {
    /// Creates an empty ring buffer holding at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        RingBuffer {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Appends a line, evicting the oldest entry first if already at
    /// capacity.
    pub fn push(&self, line: String) {
        let mut buf = self.lock();
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    /// Returns a snapshot of the currently buffered lines, oldest first.
    ///
    /// A snapshot (an owned `Vec`, not a live view) is deliberate: the
    /// panic hook needs a value it can format into a crash report and
    /// write to disk without holding the buffer's lock across I/O.
    pub fn snapshot(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    /// Number of lines currently buffered (`0..=capacity`).
    ///
    /// `duet` is a binary crate, so rustc's `dead_code` lint can't see
    /// that this and [`Self::is_empty`] are real API surface (exercised
    /// today by this module's tests, and by any future caller that wants
    /// to report ring-buffer fill level, e.g. in a diagnostics/about
    /// panel) rather than genuinely unused -- hence the explicit `allow`.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True if no lines have been buffered yet.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Locks the inner deque, recovering from a poisoned lock rather than
    /// panicking. A panicking thread that poisoned this mutex while
    /// holding it is exactly the scenario the crash-file writer runs
    /// under (it reads the ring buffer from inside a panic hook), so this
    /// path must stay available even after a panic elsewhere left the
    /// lock poisoned.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// A `tracing_subscriber::Layer` that formats every event it sees into a
/// single line and appends it to a shared [`RingBuffer`].
///
/// This exists so a crash handler can dump recent trace history even
/// though the full log (stderr, or a future file appender) may not be
/// flushed or persisted yet at the moment of a panic (design.md §12).
pub struct RingBufferLayer {
    buffer: RingBuffer,
}

impl RingBufferLayer {
    /// Wraps `buffer` as a tracing layer; events this layer observes are
    /// appended to `buffer`, which the panic hook reads from later.
    pub fn new(buffer: RingBuffer) -> Self {
        RingBufferLayer { buffer }
    }
}

impl<S> Layer<S> for RingBufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.buffer.push(format_event(event));
    }
}

/// Formats a single trace event as one line: a Unix timestamp, level,
/// target, message, and any additional structured fields.
///
/// Deliberately hand-rolled rather than pulling in a date/time formatting
/// dependency: design.md §7.5's dependency list does not anticipate one,
/// and a raw Unix timestamp is sufficient for a crash artifact meant to be
/// read once, shortly after the fact, by a developer or bug reporter.
fn format_event(event: &Event<'_>) -> String {
    let metadata = event.metadata();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut visitor = LineVisitor::default();
    event.record(&mut visitor);

    let mut line = format!(
        "[{}.{:03}] {:>5} {}",
        now.as_secs(),
        now.subsec_millis(),
        metadata.level(),
        metadata.target(),
    );
    if !visitor.message.is_empty() {
        let _ = write!(line, ": {}", visitor.message);
    }
    for (name, value) in &visitor.fields {
        let _ = write!(line, " {name}={value}");
    }
    line
}

/// Collects a tracing event's `message` field and any other structured
/// fields into plain strings for [`format_event`].
#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: Vec<(&'static str, String)>,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push((field.name(), format!("{value:?}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `tracing_subscriber::registry()` returns a bare `Registry`; `.with()`
    // (used below) is an extension method from `SubscriberExt`.
    use tracing_subscriber::layer::SubscriberExt as _;

    #[test]
    fn evicts_oldest_once_at_capacity() {
        let ring = RingBuffer::new(3);
        ring.push("a".to_string());
        ring.push("b".to_string());
        ring.push("c".to_string());
        ring.push("d".to_string());
        assert_eq!(ring.snapshot(), vec!["b", "c", "d"]);
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn starts_empty() {
        let ring = RingBuffer::new(RING_BUFFER_CAPACITY);
        assert!(ring.is_empty());
        assert_eq!(ring.snapshot(), Vec::<String>::new());
    }

    #[test]
    fn layer_captures_message_and_fields() {
        let ring = RingBuffer::new(RING_BUFFER_CAPACITY);
        let layer = RingBufferLayer::new(ring.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "duet::test", code = 7, "hello ring buffer");
        });

        let lines = ring.snapshot();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello ring buffer"));
        assert!(lines[0].contains("code=7"));
        assert!(lines[0].contains("duet::test"));
    }
}
