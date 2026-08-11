//! `ChangeEvent` — items yielded by
//! [`FileSystem::watch`](crate::FileSystem::watch).

use duet_types::VPath;

/// A single filesystem change notification.
///
/// Deliberately coarse-grained (no separate "which xattr changed" detail,
/// no raw inotify mask passthrough): `duet-index` (design.md §9.2) treats
/// every variant except [`ChangeEvent::Overflow`] as "re-stat this path and
/// diff it into the model," so the exact cause matters less than the
/// affected path(s). Backends without a push mechanism don't implement
/// `watch` in a way that reaches this type at all — `duet-index`'s polling
/// fallback (design.md §9.2: "Backends that lack `WATCH` get polling from
/// this layer, not from the backend") synthesizes the same diff without
/// ever constructing a `ChangeEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChangeEvent {
    /// A new entry appeared at this path.
    Created(VPath),
    /// The entry at this path was removed.
    Removed(VPath),
    /// The entry's content changed (size/mtime moved).
    Modified(VPath),
    /// The entry's metadata changed without its content changing
    /// (permissions, xattrs, ownership).
    MetadataChanged(VPath),
    /// An entry moved from one path to another. Backends that can't
    /// correlate the two halves of a rename (most `inotify` setups, across
    /// directories) instead emit a `Removed` + `Created` pair; `Renamed` is
    /// an optimisation, not a guarantee.
    Renamed { from: VPath, to: VPath },
    /// The watch's event queue overflowed (e.g. `IN_Q_OVERFLOW`) and
    /// events were dropped. The receiver's only correct response is a full
    /// rescan of the watched path (design.md §9.2) — do not attempt to
    /// reconcile individual entries after this.
    Overflow,
}
