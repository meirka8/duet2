//! `VolumeStats` — free/total space for the volume backing a path
//! (T-4.2.7's per-panel free-space indicator).

/// Free/total space for the volume containing a given path.
///
/// `available_bytes`, not `total_bytes - free_bytes`-style raw free space,
/// is what callers should show: it's `statvfs`'s `f_bavail` (space
/// available to an unprivileged process), which already excludes whatever
/// the filesystem reserves for root -- the number that actually answers
/// "how much can I write here," not just "how much is unused."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VolumeStats {
    pub total_bytes: u64,
    pub available_bytes: u64,
}
