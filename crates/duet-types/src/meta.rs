//! `Metadata`, `EntryKind`, `Timestamp`, and `MetaPatch`.
//!
//! `Metadata` is what `FileSystem::stat` (T-2.2.2) returns. Not every field
//! is populated on every call — `ListOpts` (T-2.2.2) controls which fields a
//! caller actually needs (a brief-mode panel asks for names only; SFTP/S3
//! backends make richer fields expensive), so anything beyond the cheap
//! core is `Option`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::path::UnixPathBuf;

/// A POSIX-style file type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Fifo,
    Socket,
    BlockDevice,
    CharDevice,
    /// Backend reported a type this enum doesn't model (or none at all,
    /// e.g. some remote backends only distinguish file/dir).
    Unknown,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        matches!(self, EntryKind::Directory)
    }

    pub fn is_file(self) -> bool {
        matches!(self, EntryKind::File)
    }

    pub fn is_symlink(self) -> bool {
        matches!(self, EntryKind::Symlink)
    }
}

/// A timestamp as seconds + nanoseconds since the Unix epoch.
///
/// Deliberately not `std::time::SystemTime`: `SystemTime` has no portable
/// serde support in std, and its internal representation isn't guaranteed
/// stable across platforms. `secs` is signed to allow (rare but real)
/// pre-1970 timestamps some filesystems and archive formats can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp {
    pub secs: i64,
    /// Always in `0..1_000_000_000`.
    pub nanos: u32,
}

impl Timestamp {
    pub const EPOCH: Timestamp = Timestamp { secs: 0, nanos: 0 };

    pub fn new(secs: i64, nanos: u32) -> Self {
        debug_assert!(nanos < 1_000_000_000, "nanos out of range: {nanos}");
        Timestamp {
            secs,
            nanos: nanos % 1_000_000_000,
        }
    }
}

impl From<std::time::SystemTime> for Timestamp {
    fn from(t: std::time::SystemTime) -> Self {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Timestamp {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(e) => {
                // Before the epoch: negate the "distance" duration.
                let d = e.duration();
                let secs = -(d.as_secs() as i64) - i64::from(d.subsec_nanos() > 0);
                let nanos = if d.subsec_nanos() > 0 {
                    1_000_000_000 - d.subsec_nanos()
                } else {
                    0
                };
                Timestamp { secs, nanos }
            }
        }
    }
}

/// Metadata for a single `VPath` entry, as returned by `FileSystem::stat`.
///
/// `size`/`kind` are considered "always cheap enough to populate"; every
/// other field is optional both because backends vary in what they can
/// report and because `ListOpts` may not have asked for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub kind: EntryKind,
    /// Logical size in bytes. `0` for directories on backends that don't
    /// report a meaningful directory size.
    pub size: u64,
    pub modified: Option<Timestamp>,
    pub accessed: Option<Timestamp>,
    /// Creation/birth time, when the backend can report one (`statx`
    /// `STATX_BTIME` on Linux; not always available).
    pub created: Option<Timestamp>,
    /// POSIX mode bits (permissions + file-type bits), when meaningful.
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub nlink: Option<u64>,
    /// Device id, for hardlink-graph detection (`(dev, ino)` pairs) during
    /// operations (design.md §9.3).
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    /// Symlink target, when `kind == Symlink` and the caller asked for it.
    pub symlink_target: Option<UnixPathBuf>,
    /// Extended attributes, lazily populated per `ListOpts`/FR-OPS-05.
    pub xattrs: Option<BTreeMap<String, Vec<u8>>>,
    /// Raw POSIX ACL text, when requested and available. Structured ACL
    /// modelling is deferred to whichever later task actually consumes it;
    /// this crate just carries the bytes.
    pub acl: Option<Vec<u8>>,
    pub selinux_label: Option<String>,
}

impl Metadata {
    /// The smallest legal `Metadata` for a given kind: everything optional
    /// is `None`, size `0`. Useful as a builder base or in tests.
    pub fn minimal(kind: EntryKind) -> Self {
        Metadata {
            kind,
            size: 0,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            nlink: None,
            dev: None,
            ino: None,
            symlink_target: None,
            xattrs: None,
            acl: None,
            selinux_label: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn is_symlink(&self) -> bool {
        self.kind.is_symlink()
    }
}

/// The write-side counterpart of `Metadata`, used by `FileSystem::set_meta`
/// (design.md §9.1). Every field is `Option`/absent-by-default: a field
/// left unset means "don't change it," not "clear it." Clearing an xattr
/// is an explicit removal via `remove_xattrs`, not `Some(None)` nesting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetaPatch {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub modified: Option<Timestamp>,
    pub accessed: Option<Timestamp>,
    /// Extended attributes to set/overwrite, name -> value.
    pub set_xattrs: BTreeMap<String, Vec<u8>>,
    /// Extended attribute names to remove.
    pub remove_xattrs: Vec<String>,
}

impl MetaPatch {
    /// `true` if applying this patch would be a no-op.
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
            && self.modified.is_none()
            && self.accessed.is_none()
            && self.set_xattrs.is_empty()
            && self.remove_xattrs.is_empty()
    }

    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn with_modified(mut self, t: Timestamp) -> Self {
        self.modified = Some(t);
        self
    }
}
