// SPDX-License-Identifier: MIT
//! Crash points for the data-safety suite (T-10.2.1, design.md §14.4).
//!
//! `docs/crash-safety.md` enumerates, per step kind, every point at which a
//! `SIGKILL` can land. Proving the invariant at each of those points needs
//! the kill to land *exactly* there, which a parent process timing a
//! `kill(2)` from outside cannot do. A crash point is the deterministic
//! alternative: a named call site that, when the running process was asked
//! to die there, sends itself a real `SIGKILL` -- no unwinding, no `Drop`,
//! no buffered-writer flush, exactly what the kernel leaves behind for any
//! other `kill -9`.
//!
//! # Zero cost, zero surface in a normal build
//!
//! Everything here is behind the `failpoints` Cargo feature. Without it
//! [`hit`] is an empty `#[inline(always)]` function, so the call sites
//! compile to nothing and a release binary contains no code that could
//! make it kill itself. Only `duet-ops`'s own test builds turn the feature
//! on (through its self dev-dependency).
//!
//! # Selecting a crash point
//!
//! With the feature on, the `DUET_FAILPOINT` environment variable, read
//! once per process, selects at most one point:
//!
//! ```text
//! DUET_FAILPOINT = <name> [ "@" <key> ] [ "#" <nth> ]
//! ```
//!
//! - `name` is the call site's name, e.g. `step.after_effect`.
//! - `key`, when given, must equal the call site's `key` argument -- by
//!   convention the step index for executor and journal points, a phase or
//!   `0` for backend points. Omitted, any key matches.
//! - `nth` (1-based, default 1) is which matching hit kills; earlier ones
//!   pass through. This is what lands a kill after the *n*-th unlink of a
//!   recursive delete.

/// Marks a crash point. A no-op unless the crate was built with the
/// `failpoints` feature *and* `DUET_FAILPOINT` selects this `name`/`key`;
/// in that case the process `SIGKILL`s itself and this never returns.
#[cfg(not(feature = "failpoints"))]
#[inline(always)]
pub fn hit(_name: &'static str, _key: u64) {}

/// Marks a crash point. A no-op unless the crate was built with the
/// `failpoints` feature *and* `DUET_FAILPOINT` selects this `name`/`key`;
/// in that case the process `SIGKILL`s itself and this never returns.
#[cfg(feature = "failpoints")]
pub fn hit(name: &'static str, key: u64) {
    imp::hit(name, key);
}

#[cfg(feature = "failpoints")]
mod imp {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Spec {
        name: String,
        key: Option<u64>,
        nth: u64,
        seen: AtomicU64,
    }

    /// Parses `name[@key][#nth]`. A malformed spec panics rather than
    /// being ignored: a crash-injection run that silently injects nothing
    /// would report a pass it has not earned.
    fn parse(raw: &str) -> Spec {
        let (rest, nth) = match raw.rsplit_once('#') {
            Some((rest, nth)) => (
                rest,
                nth.parse::<u64>()
                    .unwrap_or_else(|_| panic!("DUET_FAILPOINT: bad hit count in {raw:?}")),
            ),
            None => (raw, 1),
        };
        assert!(nth >= 1, "DUET_FAILPOINT: hit count is 1-based in {raw:?}");
        let (name, key) = match rest.split_once('@') {
            Some((name, key)) => (
                name,
                Some(
                    key.parse::<u64>()
                        .unwrap_or_else(|_| panic!("DUET_FAILPOINT: bad key in {raw:?}")),
                ),
            ),
            None => (rest, None),
        };
        assert!(!name.is_empty(), "DUET_FAILPOINT: empty name in {raw:?}");
        Spec {
            name: name.to_string(),
            key,
            nth,
            seen: AtomicU64::new(0),
        }
    }

    fn spec() -> Option<&'static Spec> {
        static SPEC: OnceLock<Option<Spec>> = OnceLock::new();
        SPEC.get_or_init(|| {
            std::env::var("DUET_FAILPOINT")
                .ok()
                .filter(|raw| !raw.is_empty())
                .map(|raw| parse(&raw))
        })
        .as_ref()
    }

    pub(super) fn hit(name: &'static str, key: u64) {
        let Some(spec) = spec() else {
            return;
        };
        if spec.name != name || spec.key.is_some_and(|wanted| wanted != key) {
            return;
        }
        if spec.seen.fetch_add(1, Ordering::SeqCst) + 1 != spec.nth {
            return;
        }
        // A real SIGKILL to ourselves: the kernel tears the process down
        // with whatever is in the page cache and nothing else -- no
        // destructor, no flush, no journal record this thread had not yet
        // handed to `write(2)`.
        let _ =
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
        // Signal delivery is asynchronous with respect to this thread's
        // next instruction; never let it take one more step of real work.
        loop {
            std::thread::park();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::parse;

        #[test]
        fn parses_a_bare_name() {
            let spec = parse("step.after_effect");
            assert_eq!(spec.name, "step.after_effect");
            assert_eq!(spec.key, None);
            assert_eq!(spec.nth, 1);
        }

        #[test]
        fn parses_key_and_hit_count() {
            let spec = parse("vfs.remove_recursive.unlinked@0#3");
            assert_eq!(spec.name, "vfs.remove_recursive.unlinked");
            assert_eq!(spec.key, Some(0));
            assert_eq!(spec.nth, 3);
        }

        #[test]
        #[should_panic(expected = "bad key")]
        fn rejects_a_non_numeric_key() {
            parse("step.after_effect@two");
        }
    }
}
