//! `RemoveKind` and `RenameFlags` — options for
//! [`FileSystem::remove`](crate::FileSystem::remove) and
//! [`FileSystem::rename`](crate::FileSystem::rename).

use bitflags::bitflags;

/// What kind of removal is being requested, so `FileSystem::remove` never
/// has to guess (and never silently recurses when the caller only meant to
/// remove a single, possibly-non-empty-if-that's-wrong node).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoveKind {
    /// Remove a single file, symlink, or other non-directory node
    /// (`unlinkat` without `AT_REMOVEDIR`). Returns `Err(ErrorKind::
    /// Conflict)` (`EISDIR`) if the target is actually a directory.
    File,
    /// Remove a directory that must already be empty (`unlinkat` with
    /// `AT_REMOVEDIR`, or `rmdir`). Returns `Err(ErrorKind::Conflict)`
    /// (`ENOTEMPTY`) if it isn't.
    EmptyDir,
    /// Recursively remove a directory and everything under it. The only
    /// variant that can partially succeed then fail partway through (a
    /// permission error on one descendant) — implementations should still
    /// remove everything they can before returning the first error, since
    /// the operation engine's journal (design.md §9.3) is what makes
    /// partial progress safe to resume, not an all-or-nothing `remove`
    /// call.
    Recursive,
}

bitflags! {
    /// Flags controlling [`FileSystem::rename`](crate::FileSystem::rename),
    /// mirroring Linux's `renameat2(2)` flags (directive: exploit
    /// `renameat2` directly on local backends).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct RenameFlags: u32 {
        /// `RENAME_NOREPLACE`: fail with `Err(ErrorKind::Conflict)` instead
        /// of silently replacing an existing destination. Without this
        /// flag, `rename` follows plain POSIX `rename(2)` semantics
        /// (destination silently replaced if it exists and is
        /// replaceable).
        const NO_REPLACE = 1 << 0;
        /// `RENAME_EXCHANGE`: atomically swap `from` and `to` in place.
        /// Both must already exist; mutually exclusive with `NO_REPLACE`
        /// (`renameat2` itself rejects that combination with `EINVAL`,
        /// surfaced here as `Err(ErrorKind::Fatal)`).
        const EXCHANGE = 1 << 1;
    }
}
