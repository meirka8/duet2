//! `Caps` — per-backend capability flags (design.md §9.1).
//!
//! `FileSystem::caps()` (T-2.2.2) returns one of these so the engine can
//! pick a strategy rather than refuse an operation outright: "capability
//! -driven strategy, not capability-driven refusal" (design.md §9.1). E.g.
//! missing `REFLINK` degrades the copy engine to `copy_file_range`, then to
//! a buffered copy, rather than failing the job.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Capabilities a mounted filesystem backend advertises.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Caps: u32 {
        /// Seekable reads (as opposed to a forward-only stream).
        const RANDOM_READ    = 1 << 0;
        /// Seekable/in-place writes.
        const RANDOM_WRITE   = 1 << 1;
        /// Backend supports renaming entries in place.
        const RENAME         = 1 << 2;
        /// Rename-over-destination (atomic replace) semantics.
        const ATOMIC_REPLACE = 1 << 3;
        /// Hard links.
        const HARDLINK       = 1 << 4;
        /// Symbolic links.
        const SYMLINK        = 1 << 5;
        /// Extended attributes.
        const XATTR          = 1 << 6;
        /// POSIX permission bits are meaningful and settable.
        const PERMISSIONS    = 1 << 7;
        /// mtime/atime are meaningful and settable.
        const TIMESTAMPS     = 1 << 8;
        /// Sparse files (`SEEK_HOLE`/`SEEK_DATA`-aware).
        const SPARSE         = 1 << 9;
        /// Copy-on-write reflink (`FICLONE`).
        const REFLINK        = 1 << 10;
        /// Backend can push change events (inotify-like) rather than
        /// requiring the index layer to poll.
        const WATCH          = 1 << 11;
        /// `stat` is not a network/IPC round-trip.
        const CHEAP_STAT     = 1 << 12;
        /// An interrupted write can be resumed rather than restarted.
        const APPEND_RESUME  = 1 << 13;
    }
}

impl Default for Caps {
    fn default() -> Self {
        Caps::empty()
    }
}
