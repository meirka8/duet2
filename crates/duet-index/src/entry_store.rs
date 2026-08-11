//! [`EntryStore`]: struct-of-arrays storage for one directory listing
//! (design.md §9.2), sized against T-3.2.1's "1M entries ≤ 120 MB" and
//! T-2.5.1's "≤ 96 B + name" per-entry budget.

use duet_types::{EntryId, EntryKind, Metadata};

use crate::name_arena::{NameArena, NameSpan};

/// Packed per-entry state: the `EntryKind` (3 bits, 8 variants -- exactly
/// covers [`EntryKind`]'s variant count) plus boolean flags, in one `u32`
/// column (design.md §9.2: "flags in a `Vec<u32>`").
///
/// Bit layout (bit 0 = LSB):
///
/// | bits   | meaning                                                        |
/// |--------|----------------------------------------------------------------|
/// | 0..3   | `EntryKind` discriminant (see [`kind_to_bits`]/[`bits_to_kind`])|
/// | 3      | `HIDDEN` -- dotfile or backend-reported hidden attribute        |
/// | 4      | `SYMLINK_BROKEN` -- target does not resolve (only meaningful when kind == Symlink) |
/// | 5      | `HAS_EXTENDED` -- `EntryStore::extended[i]` is `Some`           |
/// | 6      | `SIZE_PENDING` -- recursive directory size requested but not yet computed (FR-SEL-02) |
/// | 7      | `TOMBSTONE` -- slot removed from the live set but retained so its `EntryId` cannot be reissued this generation (see `EntryId`'s doc comment) |
/// | 8..32  | reserved for future per-entry state (git status, quick-search match cache, etc.) without widening the column |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryFlags(pub u32);

const KIND_MASK: u32 = 0b111;
const BIT_HIDDEN: u32 = 1 << 3;
const BIT_SYMLINK_BROKEN: u32 = 1 << 4;
const BIT_HAS_EXTENDED: u32 = 1 << 5;
const BIT_SIZE_PENDING: u32 = 1 << 6;
const BIT_TOMBSTONE: u32 = 1 << 7;

fn kind_to_bits(kind: EntryKind) -> u32 {
    match kind {
        EntryKind::File => 0,
        EntryKind::Directory => 1,
        EntryKind::Symlink => 2,
        EntryKind::Fifo => 3,
        EntryKind::Socket => 4,
        EntryKind::BlockDevice => 5,
        EntryKind::CharDevice => 6,
        EntryKind::Unknown => 7,
    }
}

fn bits_to_kind(bits: u32) -> EntryKind {
    match bits & KIND_MASK {
        0 => EntryKind::File,
        1 => EntryKind::Directory,
        2 => EntryKind::Symlink,
        3 => EntryKind::Fifo,
        4 => EntryKind::Socket,
        5 => EntryKind::BlockDevice,
        6 => EntryKind::CharDevice,
        _ => EntryKind::Unknown,
    }
}

impl EntryFlags {
    pub fn new(kind: EntryKind) -> Self {
        EntryFlags(kind_to_bits(kind))
    }

    pub fn kind(self) -> EntryKind {
        bits_to_kind(self.0)
    }

    pub fn set_kind(&mut self, kind: EntryKind) {
        self.0 = (self.0 & !KIND_MASK) | kind_to_bits(kind);
    }

    pub fn hidden(self) -> bool {
        self.0 & BIT_HIDDEN != 0
    }

    pub fn set_hidden(&mut self, v: bool) {
        set_bit(&mut self.0, BIT_HIDDEN, v);
    }

    pub fn symlink_broken(self) -> bool {
        self.0 & BIT_SYMLINK_BROKEN != 0
    }

    pub fn set_symlink_broken(&mut self, v: bool) {
        set_bit(&mut self.0, BIT_SYMLINK_BROKEN, v);
    }

    pub fn has_extended(self) -> bool {
        self.0 & BIT_HAS_EXTENDED != 0
    }

    pub fn set_has_extended(&mut self, v: bool) {
        set_bit(&mut self.0, BIT_HAS_EXTENDED, v);
    }

    pub fn size_pending(self) -> bool {
        self.0 & BIT_SIZE_PENDING != 0
    }

    pub fn set_size_pending(&mut self, v: bool) {
        set_bit(&mut self.0, BIT_SIZE_PENDING, v);
    }

    pub fn tombstone(self) -> bool {
        self.0 & BIT_TOMBSTONE != 0
    }

    pub fn set_tombstone(&mut self, v: bool) {
        set_bit(&mut self.0, BIT_TOMBSTONE, v);
    }
}

fn set_bit(word: &mut u32, mask: u32, v: bool) {
    if v {
        *word |= mask;
    } else {
        *word &= !mask;
    }
}

/// Struct-of-arrays storage for one directory listing's entries.
///
/// # Per-entry byte budget
///
/// T-2.5.1's AC and T-3.2.1's DoD both target "≤ 96 B + name" per entry.
/// Every fixed-width column below is a separate `Vec`, so there is no
/// per-entry struct padding to account for (true SoA, per design.md §9.2)
/// -- the arithmetic is just the sum of each column's element width:
///
/// | column         | type                    | bytes/entry |
/// |----------------|-------------------------|-------------|
/// | `name_offset`  | `Vec<u32>`               | 4           |
/// | `name_len`     | `Vec<u16>`               | 2           |
/// | `sizes`        | `Vec<u64>`               | 8           |
/// | `mtimes`       | `Vec<i64>`               | 8           |
/// | `flags`        | `Vec<EntryFlags>` (u32)  | 4           |
/// | `extended`     | `Vec<Option<Box<Metadata>>>` (niche-optimized to a pointer) | 8 |
/// | **fixed total**|                          | **34**      |
///
/// 34 B is well inside the 96 B budget, leaving ~62 B/entry of headroom
/// before the name's own bytes even enter the count (the name lives in
/// [`NameArena`], addressed by `name_offset`/`name_len`, and is charged
/// separately per the "+ name" clause of the AC). At 1M entries:
///
/// - fixed columns: `34 B * 1_000_000 ≈ 32.4 MB`
/// - name arena at a generous 32 B/name average: `32 B * 1_000_000 ≈ 30.5 MB`
/// - `DirectoryModel::order: Vec<u32>` (one entry per *visible* row, so
///   `<= 34 B` bound doesn't apply, but bounded by the same 1M): `≈ 3.8 MB`
///
/// Total ≈ 67 MB, comfortably under T-3.2.1's 120 MB ceiling with room for
/// allocator overhead and a non-trivial `selection: RoaringBitmap` (which
/// is itself sub-linear for contiguous or sparse selections, per its
/// compressed-bitmap design).
///
/// `extended` is `None` for every entry until something asks for fields
/// beyond the cheap core (design.md §9.1's `ListOpts`: "a brief-mode panel
/// asks for names only"), so in the common case it costs one null pointer
/// per entry and zero heap allocations -- `Option<Box<T>>` has the same
/// size as `Box<T>` via the null-pointer niche, so this column does not
/// widen the struct beyond the 8 B already budgeted for it.
pub struct EntryStore {
    len: usize,
    names: NameArena,
    name_offset: Vec<u32>,
    name_len: Vec<u16>,
    sizes: Vec<u64>,
    /// Modified time, seconds since the Unix epoch. Deliberately just the
    /// `i64` seconds component (design.md §9.2: "times in a `Vec<i64>`"),
    /// not the full `duet_types::Timestamp` (which also carries nanos) --
    /// sub-second precision isn't needed for sort/display in the hot
    /// column and would cost another 4 B/entry for something the full
    /// `Metadata` in `extended` already carries when anyone needs it.
    mtimes: Vec<i64>,
    flags: Vec<EntryFlags>,
    /// Populated on demand when a caller fetches metadata beyond the cheap
    /// core (§9.1 `ListOpts`). See the byte-budget note above for why this
    /// doesn't blow the per-entry budget in the common case.
    extended: Vec<Option<Box<Metadata>>>,
}

impl EntryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        EntryStore {
            len: 0,
            names: NameArena::new(),
            name_offset: Vec::new(),
            name_len: Vec::new(),
            sizes: Vec::new(),
            mtimes: Vec::new(),
            flags: Vec::new(),
            extended: Vec::new(),
        }
    }

    /// Creates a store pre-sized for `capacity` entries, to avoid
    /// reallocation during a known-size streaming population.
    pub fn with_capacity(capacity: usize) -> Self {
        EntryStore {
            len: 0,
            names: NameArena::new(),
            name_offset: Vec::with_capacity(capacity),
            name_len: Vec::with_capacity(capacity),
            sizes: Vec::with_capacity(capacity),
            mtimes: Vec::with_capacity(capacity),
            flags: Vec::with_capacity(capacity),
            extended: Vec::with_capacity(capacity),
        }
    }

    /// Number of entries, live and tombstoned. This is the array length,
    /// not the visible count -- `DirectoryModel::order` is what filters
    /// tombstones and hidden entries out for rendering.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends a new entry, interning `name` and recording `size`/`mtime`/
    /// `kind` from `metadata`. Returns the freshly assigned, generation-
    /// scoped [`EntryId`] (== this store's array index at the time of the
    /// call -- see `EntryId`'s doc comment on why that equivalence holds
    /// only within one generation).
    ///
    /// `metadata` beyond `size`/`modified`/`kind` is stashed in `extended`
    /// only if any such field is actually present, to honor the
    /// zero-allocation-for-the-common-case budget above.
    pub fn push(&mut self, name: &str, metadata: &Metadata) -> EntryId {
        let id = EntryId::new(self.len as u32);
        let span: NameSpan = self.names.intern(name);
        self.name_offset.push(span.offset);
        self.name_len.push(span.len);
        self.sizes.push(metadata.size);
        self.mtimes
            .push(metadata.modified.map(|t| t.secs).unwrap_or(0));

        let mut flags = EntryFlags::new(metadata.kind);
        let has_extended = has_extended_fields(metadata);
        flags.set_has_extended(has_extended);
        self.flags.push(flags);
        self.extended
            .push(has_extended.then(|| Box::new(metadata.clone())));

        self.len += 1;
        id
    }

    /// The entry's display name.
    pub fn name(&self, id: EntryId) -> &str {
        let ix = id.index() as usize;
        self.names.resolve(NameSpan {
            offset: self.name_offset[ix],
            len: self.name_len[ix],
        })
    }

    pub fn size(&self, id: EntryId) -> u64 {
        self.sizes[id.index() as usize]
    }

    /// Modified time, seconds since epoch. See the field doc comment on
    /// `mtimes` for the precision trade-off.
    pub fn mtime_secs(&self, id: EntryId) -> i64 {
        self.mtimes[id.index() as usize]
    }

    pub fn kind(&self, id: EntryId) -> EntryKind {
        self.flags[id.index() as usize].kind()
    }

    pub fn flags(&self, id: EntryId) -> EntryFlags {
        self.flags[id.index() as usize]
    }

    /// Full stat metadata, when previously fetched -- `None` if this entry
    /// was populated brief-mode (names/kind only) and nothing has since
    /// requested more (§9.1 `ListOpts`).
    pub fn extended(&self, id: EntryId) -> Option<&Metadata> {
        self.extended[id.index() as usize].as_deref()
    }

    /// Records a fresh `Metadata` fetch for an existing entry (e.g. after
    /// an on-demand `stat` for a column the brief-mode listing didn't
    /// populate). Updates the hot `sizes`/`mtimes`/`flags` columns too, so
    /// they stay authoritative without requiring every reader to fall
    /// back to `extended`.
    pub fn set_extended(&mut self, id: EntryId, metadata: Metadata) {
        let ix = id.index() as usize;
        self.sizes[ix] = metadata.size;
        self.mtimes[ix] = metadata.modified.map(|t| t.secs).unwrap_or(0);
        self.flags[ix].set_kind(metadata.kind);
        self.flags[ix].set_has_extended(true);
        self.extended[ix] = Some(Box::new(metadata));
    }

    /// Marks an entry removed. The slot, its `EntryId`, and its arena
    /// bytes are retained (not reclaimed) for the rest of this
    /// generation -- see [`EntryFlags`]'s `TOMBSTONE` bit and `EntryId`'s
    /// doc comment on why ids must not be reissued mid-generation
    /// (`selection`/`cursor` reference them and must not silently start
    /// pointing at a different, later-inserted file).
    pub fn remove(&mut self, id: EntryId) {
        self.flags[id.index() as usize].set_tombstone(true);
    }

    pub fn is_tombstoned(&self, id: EntryId) -> bool {
        self.flags[id.index() as usize].tombstone()
    }

    /// Accountable byte size of this store's columns (not RSS -- see S-1's
    /// spike `approx_bytes`, which this mirrors): the number the byte-
    /// budget doc comment above is checked against.
    pub fn approx_bytes(&self) -> usize {
        self.names.len()
            + self.name_offset.len() * size_of::<u32>()
            + self.name_len.len() * size_of::<u16>()
            + self.sizes.len() * size_of::<u64>()
            + self.mtimes.len() * size_of::<i64>()
            + self.flags.len() * size_of::<EntryFlags>()
            + self.extended.len() * size_of::<Option<Box<Metadata>>>()
            + self
                .extended
                .iter()
                .filter_map(|e| e.as_deref())
                .map(size_of_val)
                .sum::<usize>()
    }
}

impl Default for EntryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn has_extended_fields(metadata: &Metadata) -> bool {
    metadata.accessed.is_some()
        || metadata.created.is_some()
        || metadata.mode.is_some()
        || metadata.uid.is_some()
        || metadata.gid.is_some()
        || metadata.nlink.is_some()
        || metadata.dev.is_some()
        || metadata.ino.is_some()
        || metadata.symlink_target.is_some()
        || metadata.xattrs.is_some()
        || metadata.acl.is_some()
        || metadata.selinux_label.is_some()
}

#[cfg(test)]
mod tests {
    use duet_types::Timestamp;

    use super::*;

    fn brief(kind: EntryKind, size: u64) -> Metadata {
        let mut m = Metadata::minimal(kind);
        m.size = size;
        m.modified = Some(Timestamp::new(1_700_000_000, 0));
        m
    }

    #[test]
    fn push_and_read_back_columns() {
        let mut store = EntryStore::new();
        let a = store.push("alpha.txt", &brief(EntryKind::File, 42));
        let b = store.push("beta", &brief(EntryKind::Directory, 0));

        assert_eq!(store.len(), 2);
        assert_eq!(store.name(a), "alpha.txt");
        assert_eq!(store.name(b), "beta");
        assert_eq!(store.size(a), 42);
        assert_eq!(store.kind(b), EntryKind::Directory);
        assert!(
            store.extended(a).is_none(),
            "brief metadata shouldn't allocate extended"
        );
    }

    #[test]
    fn entry_id_equals_push_order_index() {
        let mut store = EntryStore::new();
        let ids: Vec<EntryId> = (0..10)
            .map(|i| store.push(&format!("f{i}"), &brief(EntryKind::File, 0)))
            .collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(id.index(), i as u32);
        }
    }

    #[test]
    fn remove_tombstones_without_reindexing() {
        let mut store = EntryStore::new();
        let a = store.push("a", &brief(EntryKind::File, 0));
        let b = store.push("b", &brief(EntryKind::File, 0));
        store.remove(a);
        assert!(store.is_tombstoned(a));
        assert!(!store.is_tombstoned(b));
        // b's id/index is unaffected by a's removal.
        assert_eq!(store.name(b), "b");
    }

    #[test]
    fn extended_metadata_round_trips_when_present() {
        let mut m = Metadata::minimal(EntryKind::File);
        m.uid = Some(1000);
        let mut store = EntryStore::new();
        let id = store.push("x", &m);
        assert!(store.flags(id).has_extended());
        assert_eq!(store.extended(id).unwrap().uid, Some(1000));
    }

    #[test]
    fn fixed_column_width_matches_documented_budget() {
        // Sanity-checks the "34 B fixed total" arithmetic in the doc
        // comment against the actual Rust type sizes, so the two can't
        // silently drift apart.
        assert_eq!(size_of::<u32>(), 4); // name_offset
        assert_eq!(size_of::<u16>(), 2); // name_len
        assert_eq!(size_of::<u64>(), 8); // sizes
        assert_eq!(size_of::<i64>(), 8); // mtimes
        assert_eq!(size_of::<EntryFlags>(), 4); // flags
        assert_eq!(size_of::<Option<Box<Metadata>>>(), 8); // extended (niche-optimized)
        assert_eq!(4 + 2 + 8 + 8 + 4 + 8, 34);
    }
}
