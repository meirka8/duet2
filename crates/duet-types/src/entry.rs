//! `EntryId` — the stable per-listing identifier used by the panel model.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable identifier for an entry within a single `DirectoryModel`/
/// `EntryStore` (design.md §9.2's struct-of-arrays panel model).
///
/// Stable across sorts, filters, and re-renders *within one listing
/// generation* — this is what lets `selection: RoaringBitmap` and `cursor:
/// EntryId` survive a re-sort without recomputing. It is **not** stable
/// across a fresh `read_dir`/rescan (a refreshed listing may assign
/// different ids to the same on-disk file) and is **not** unique across
/// different directories/panels — it is only meaningful paired with the
/// `DirectoryModel` that issued it.
///
/// Backed by `u32` (not `u64`) to match `RoaringBitmap`'s key type and
/// keep the SoA entry footprint small, per NFR-06's ≤96-byte/entry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u32);

impl EntryId {
    /// Constructs an `EntryId` from a raw index.
    pub const fn new(index: u32) -> Self {
        EntryId(index)
    }

    /// The raw index this id wraps.
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u32> for EntryId {
    fn from(v: u32) -> Self {
        EntryId(v)
    }
}

impl From<EntryId> for u32 {
    fn from(v: EntryId) -> Self {
        v.0
    }
}
