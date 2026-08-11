//! `DirEntry` — one row of a
//! [`FileSystem::read_dir`](crate::FileSystem::read_dir) chunk.

use duet_types::Metadata;

/// A single directory entry as produced by a listing.
///
/// `name` is the entry's final path component only (not a full `VPath`) —
/// callers join it onto the directory's own `VPath` (`dir.join(&entry.name)`)
/// to address it. This mirrors what `getdents64`/backends naturally hand
/// back and avoids reconstructing a full path per entry.
///
/// `metadata` reflects whatever the backend could produce given the
/// `ListOpts` the caller passed to `read_dir` — see [`Metadata`]'s own doc
/// comment and [`crate::ListFields`] for exactly which fields are
/// best-effort. `metadata.kind` is always meaningful (even
/// `names_only()`-style listings need to know file-vs-directory to render
/// an icon and decide whether the row expands); `metadata.size` follows the
/// same "always cheap enough" convention `Metadata` documents, so a `0`
/// there means either "empty" or "not requested/not cheap on this
/// backend" — indistinguishable by design, exactly as `Metadata::size`'s
/// own doc comment already notes for directories.
#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub metadata: Metadata,
}

impl DirEntry {
    pub fn new(name: impl Into<String>, metadata: Metadata) -> Self {
        DirEntry {
            name: name.into(),
            metadata,
        }
    }
}
