//! `ListOpts`/`ListFields` — which [`Metadata`](duet_types::Metadata) fields
//! a [`FileSystem::read_dir`](crate::FileSystem::read_dir) caller actually
//! wants (design.md §9.1).
//!
//! This is deliberately a bitflag/struct of field *requests*, not a
//! brief/full boolean: "A brief-mode panel asks for names only; a full-mode
//! panel asks for size/mtime/mode... the difference between one round trip
//! and ten thousand" on remote backends (design.md §9.1). A boolean would
//! force every backend to choose between "cheap" and "everything"; the real
//! cost curve is per-field (SFTP can `LSTAT` cheaply but `getfacl` is a
//! separate round trip; S3 has no ACL/xattr concept and would just ignore
//! those bits).

use bitflags::bitflags;

bitflags! {
    /// Which optional [`Metadata`](duet_types::Metadata) fields a caller
    /// wants populated. `Metadata::kind` and `Metadata::size` are always
    /// considered "cheap enough to populate" (design.md §9.1's comment on
    /// `Metadata`) and are not gated by a flag here; every other field is
    /// opt-in.
    ///
    /// A backend is never *required* to populate a requested field (a
    /// backend that cannot cheaply produce `created` just leaves it `None`
    /// regardless of the request) and is free to populate an unrequested
    /// field if it happened to be free (e.g. `getdents64`'s `d_type` gives
    /// `kind` for free even when nothing was requested). `ListFields` is a
    /// hint that lets a backend *skip* expensive round trips, not a
    /// contract that a field will or won't be present.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ListFields: u32 {
        /// `Metadata::size`, when it costs more than `d_type`/`readdir`
        /// already gave for free (rare locally, common on remote backends).
        const SIZE            = 1 << 0;
        const MODIFIED        = 1 << 1;
        const ACCESSED        = 1 << 2;
        const CREATED         = 1 << 3;
        /// `Metadata::mode` (permission + type bits).
        const MODE            = 1 << 4;
        /// `Metadata::uid`/`Metadata::gid`.
        const OWNER           = 1 << 5;
        const LINK_COUNT      = 1 << 6;
        /// `Metadata::dev`/`Metadata::ino` (hardlink-graph detection,
        /// design.md §9.3).
        const INODE           = 1 << 7;
        const SYMLINK_TARGET  = 1 << 8;
        /// `Metadata::xattrs`. Expensive almost everywhere (a separate
        /// `listxattr`/`getxattr` syscall or round trip per entry) — only
        /// requested by panels that specifically show xattrs (FR-OPS-05).
        const XATTRS          = 1 << 9;
        const ACL             = 1 << 10;
        const SELINUX         = 1 << 11;
    }
}

/// Options controlling [`FileSystem::read_dir`](crate::FileSystem::read_dir).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListOpts {
    /// Which extra `Metadata` fields to populate per entry. See
    /// [`ListFields`] for the "hint, not contract" semantics.
    pub fields: ListFields,
    /// If `true`, a symlink entry's metadata describes the link's target
    /// (`stat`-through-symlink semantics) rather than the link itself
    /// (`lstat` semantics). Listings default to `false` (`lstat`): a panel
    /// showing a directory's contents wants to know an entry *is* a
    /// symlink, not silently resolve it.
    pub follow_symlinks: bool,
}

impl ListOpts {
    /// Names and kinds only — the cheapest possible listing. What a
    /// brief-mode panel asks for (design.md §9.1); on `getdents64`-backed
    /// local directories this needs no `stat` calls at all.
    pub fn names_only() -> Self {
        ListOpts {
            fields: ListFields::empty(),
            follow_symlinks: false,
        }
    }

    /// Every optional field. What a full-mode/details panel asks for.
    pub fn full() -> Self {
        ListOpts {
            fields: ListFields::all(),
            follow_symlinks: false,
        }
    }

    /// Builder: request `follow_symlinks: true`.
    pub fn following_symlinks(mut self) -> Self {
        self.follow_symlinks = true;
        self
    }
}

impl Default for ListOpts {
    /// Defaults to [`ListOpts::names_only`] — the cheapest option, so a
    /// caller that forgets to specify fields never accidentally forces an
    /// expensive listing.
    fn default() -> Self {
        ListOpts::names_only()
    }
}
