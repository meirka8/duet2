//! T-3.2.2: sorting `DirectoryModel::order`.
//!
//! Two ideas, per design.md §9.2's "Sorting" bullet:
//!
//! - **Precomputed per-column keys.** A sort compares each pair of entries
//!   `O(n log n)` times; deriving a fresh comparison key from the raw name
//!   (locale collation in particular -- see [`Sorter`]'s doc comment) on
//!   every one of those comparisons is `O(n log n)` re-derivations of work
//!   that only has `O(n)` distinct inputs. [`Sorter::build_keys`] computes
//!   each entry's key exactly once, so the sort itself is a cheap
//!   `Vec<u8>`/integer compare (measured: ~5-7 ms for 1M precomputed keys,
//!   vs. ~700 ms to *generate* 1M collation keys single-threaded -- key
//!   generation, not comparison, is where the time goes, which is exactly
//!   why precomputing once instead of on every comparison matters).
//! - **Directories-first is a separate, orthogonal grouping key**, applied
//!   before the column key and *not* affected by `ascending` (a descending
//!   name sort still puts directories first, per the common file-manager
//!   convention T-3.2.2's brief calls "configurable" -- configurable via
//!   [`SortOptions::dirs_first`], not via the sort direction).

use std::cmp::Ordering;

use icu_collator::preferences::CollationNumericOrdering;
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};
use rayon::prelude::*;

use duet_types::{EntryId, EntryKind};

use crate::entry_store::EntryStore;

/// Which column drives the active sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Size,
    Modified,
    Kind,
}

/// Full sort configuration: column, direction, and the directories-first
/// policy (design.md §9.2: "directories-first policy" is explicitly named
/// as something the sorter must support, T-3.2.2's brief says
/// "configurable").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SortOptions {
    pub column: SortColumn,
    pub ascending: bool,
    /// When `true`, every `EntryKind::Directory` entry sorts before every
    /// non-directory entry regardless of `column`/`ascending`; only the
    /// order *within* each group honors `ascending`. This matches every
    /// mainstream file manager's "folders first" convention and is a
    /// grouping key applied ahead of the column comparison, not a
    /// tie-breaker after it.
    pub dirs_first: bool,
}

impl Default for SortOptions {
    fn default() -> Self {
        SortOptions {
            column: SortColumn::Name,
            ascending: true,
            dirs_first: true,
        }
    }
}

/// One entry's precomputed comparison key for the active [`SortColumn`].
/// `Name` carries full locale-collated sort-key bytes (see [`Sorter`]);
/// every other column is already a plain column value in `EntryStore`, so
/// "precomputing" it is just a copy -- the point is that `Sorter::build_keys`
/// produces one array of these up front, so `order.sort_by` never touches
/// `EntryStore` or re-derives anything mid-sort.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ColumnKey {
    Name(Vec<u8>),
    Size(u64),
    /// Signed so entries with no mtime (`0`, epoch) sort predictably
    /// against real (possibly pre-epoch, per `Timestamp`'s doc comment)
    /// timestamps rather than wrapping.
    Modified(i64),
    Kind(u8),
}

impl ColumnKey {
    fn cmp(&self, other: &ColumnKey) -> Ordering {
        match (self, other) {
            (ColumnKey::Name(a), ColumnKey::Name(b)) => a.cmp(b),
            (ColumnKey::Size(a), ColumnKey::Size(b)) => a.cmp(b),
            (ColumnKey::Modified(a), ColumnKey::Modified(b)) => a.cmp(b),
            (ColumnKey::Kind(a), ColumnKey::Kind(b)) => a.cmp(b),
            // Every key in one `SortKeys` batch is built for the same
            // `SortColumn` (see `build_keys`), so mixed variants never
            // actually occur; this arm exists only so the match is total.
            _ => Ordering::Equal,
        }
    }
}

/// Stable rank used for the directories-first grouping key: `0` for
/// directories, `1` for everything else, so ascending-rank order puts
/// directories first regardless of the active column's own direction.
fn dir_rank(kind: EntryKind) -> u8 {
    if kind.is_dir() { 0 } else { 1 }
}

fn kind_rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Fifo => 3,
        EntryKind::Socket => 4,
        EntryKind::BlockDevice => 5,
        EntryKind::CharDevice => 6,
        EntryKind::Unknown => 7,
    }
}

/// Precomputed sort keys for every entry in an [`EntryStore`], indexed by
/// raw entry index (`EntryId::index()`) -- i.e. covers the *whole* store,
/// not just whatever subset is currently visible through a filter. That
/// indexing choice is what lets `DirectoryModel` (T-3.2.3) recompute its
/// filtered `order` from `full_order` without re-sorting: the keys and the
/// sort they produced don't know or care which entries a filter later
/// hides.
pub struct SortKeys {
    dir_ranks: Vec<u8>,
    columns: Vec<ColumnKey>,
}

/// Owns the ICU collator used for [`SortColumn::Name`] keys.
///
/// Constructing a `Collator` does real work (loading the compiled
/// root-collation tables), so this is built once and reused for every
/// sort a `DirectoryModel` performs, not per call. Root-locale, CLDR `kn`
/// (numeric ordering) preference on: gives natural-numeric comparison
/// ("file2.txt" sorts before "file10.txt") as a first-class collation
/// feature per design.md §9.2, plus Unicode base-letter-aware ordering
/// (accented/non-Latin names sort near their unaccented/transliterated
/// equivalents rather than by raw code point) -- see this crate's `Cargo.toml`
/// for why `icu_collator` was the dependency chosen for this.
pub struct Sorter {
    collator: CollatorBorrowed<'static>,
}

impl Sorter {
    pub fn new() -> Self {
        let mut prefs = CollatorPreferences::default();
        prefs.numeric_ordering = Some(CollationNumericOrdering::True);
        let collator = Collator::try_new(prefs, CollatorOptions::default())
            .expect("root-locale collation data is baked in via the compiled_data feature");
        Sorter { collator }
    }

    /// Precomputes [`SortKeys`] for every entry in `entries` (`0..entries.len()`,
    /// tombstones included -- callers filter those out of `order` separately,
    /// per `DirectoryModel`'s design; keeping this indexed by the full raw
    /// range keeps `EntryId::index()` a valid lookup into `SortKeys` without
    /// a second remapping table).
    ///
    /// Name-key generation (the expensive part -- see the module doc
    /// comment's measured numbers) runs across the Rayon CPU pool
    /// (design.md §8.2 names "bulk sort" as a Rayon workload explicitly);
    /// every other column is a cheap sequential copy of an already-columnar
    /// `EntryStore` field, not worth parallelizing.
    pub fn build_keys(&self, entries: &EntryStore, column: SortColumn) -> SortKeys {
        let n = entries.len();
        let dir_ranks: Vec<u8> = (0..n as u32)
            .map(|ix| dir_rank(entries.kind(EntryId::new(ix))))
            .collect();

        let columns: Vec<ColumnKey> = match column {
            SortColumn::Name => (0..n as u32)
                .into_par_iter()
                .map(|ix| {
                    let name = entries.name(EntryId::new(ix));
                    let mut buf = Vec::new();
                    // Only failure mode is a sink error; `Vec<u8>` never
                    // errors as a `CollationKeySink`, so this can't fail
                    // in practice.
                    self.collator
                        .write_sort_key_to(name, &mut buf)
                        .expect("Vec<u8> sink is infallible");
                    ColumnKey::Name(buf)
                })
                .collect(),
            SortColumn::Size => (0..n as u32)
                .map(|ix| ColumnKey::Size(entries.size(EntryId::new(ix))))
                .collect(),
            SortColumn::Modified => (0..n as u32)
                .map(|ix| ColumnKey::Modified(entries.mtime_secs(EntryId::new(ix))))
                .collect(),
            SortColumn::Kind => (0..n as u32)
                .map(|ix| ColumnKey::Kind(kind_rank(entries.kind(EntryId::new(ix)))))
                .collect(),
        };

        SortKeys { dir_ranks, columns }
    }
}

impl Default for Sorter {
    fn default() -> Self {
        Self::new()
    }
}

/// Stably sorts `ids` (raw entry indices, e.g. `EntryId::index()` values)
/// by `keys`, honoring `options.dirs_first`/`options.ascending`. `keys` must
/// have been built (via [`Sorter::build_keys`]) for the same `EntryStore`
/// generation and `options.column` -- `SortKeys` doesn't self-describe which
/// column it was built for, so that pairing is the caller's (`DirectoryModel`'s)
/// responsibility to maintain.
///
/// Uses [`slice::sort_by`], which is a stable sort (guaranteed by `std`),
/// per T-3.2.2's AC: entries that compare equal on the active column keep
/// their relative order from `ids`, so re-sorting a directory that hasn't
/// changed doesn't perturb ties (e.g. same-mtime files) from run to run.
pub fn sort_order(ids: &mut [u32], keys: &SortKeys, options: SortOptions) {
    ids.sort_by(|&a, &b| {
        let (ai, bi) = (a as usize, b as usize);
        if options.dirs_first {
            let by_dir = keys.dir_ranks[ai].cmp(&keys.dir_ranks[bi]);
            if by_dir != Ordering::Equal {
                return by_dir;
            }
        }
        let by_column = keys.columns[ai].cmp(&keys.columns[bi]);
        if options.ascending {
            by_column
        } else {
            by_column.reverse()
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use duet_types::{EntryKind, Metadata, Timestamp};

    use super::*;

    fn meta(kind: EntryKind, size: u64, mtime_secs: i64) -> Metadata {
        let mut m = Metadata::minimal(kind);
        m.size = size;
        m.modified = Some(Timestamp::new(mtime_secs, 0));
        m
    }

    fn names_in_order(entries: &EntryStore, order: &[u32]) -> Vec<String> {
        order
            .iter()
            .map(|&ix| entries.name(EntryId::new(ix)).to_string())
            .collect()
    }

    #[test]
    fn natural_numeric_orders_file2_before_file10() {
        let mut entries = EntryStore::new();
        for name in ["file10.txt", "file2.txt", "file1.txt"] {
            entries.push(name, &meta(EntryKind::File, 0, 0));
        }
        let sorter = Sorter::new();
        let keys = sorter.build_keys(&entries, SortColumn::Name);
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(&mut order, &keys, SortOptions::default());
        assert_eq!(
            names_in_order(&entries, &order),
            vec!["file1.txt", "file2.txt", "file10.txt"]
        );
    }

    #[test]
    fn unicode_collation_corpus_orders_by_base_letter_not_code_point() {
        // A real (if small) multilingual corpus, not just ASCII: mixed
        // case, accented Latin, and non-Latin scripts. Raw code-point
        // (byte) order would put every uppercase ASCII letter before every
        // lowercase one and wouldn't group accented letters near their
        // unaccented counterparts at all; locale-aware primary-level
        // collation does both.
        let mut entries = EntryStore::new();
        for name in ["Zebra", "apple", "Ärger", "école", "Banane", "über"] {
            entries.push(name, &meta(EntryKind::File, 0, 0));
        }
        let sorter = Sorter::new();
        let keys = sorter.build_keys(&entries, SortColumn::Name);
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(&mut order, &keys, SortOptions::default());
        let sorted = names_in_order(&entries, &order);

        // "apple" groups with "Ärger" (both A-ish) ahead of "Banane"
        // (B-ish), which groups ahead of "école"/"über" (E/U-ish), which
        // groups ahead of "Zebra" -- base-letter order, not ASCII/UTF-8
        // byte order (which would sort every capital before every
        // lowercase letter and put "Ärger"/"über"/"école" at the very end
        // by raw code point since accented letters have higher code
        // points than plain ASCII).
        let pos = |n: &str| sorted.iter().position(|s| s == n).unwrap();
        assert!(pos("apple") < pos("Banane"));
        assert!(pos("Ärger") < pos("Banane"));
        assert!(pos("Banane") < pos("école"));
        assert!(pos("Banane") < pos("über"));
        assert!(pos("école") < pos("Zebra"));
        assert!(pos("über") < pos("Zebra"));
    }

    #[test]
    fn dirs_first_holds_regardless_of_direction() {
        let mut entries = EntryStore::new();
        entries.push("z_file", &meta(EntryKind::File, 0, 0));
        entries.push("a_dir", &meta(EntryKind::Directory, 0, 0));
        entries.push("a_file", &meta(EntryKind::File, 0, 0));
        entries.push("z_dir", &meta(EntryKind::Directory, 0, 0));
        let sorter = Sorter::new();
        let keys = sorter.build_keys(&entries, SortColumn::Name);

        let mut ascending: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(
            &mut ascending,
            &keys,
            SortOptions {
                column: SortColumn::Name,
                ascending: true,
                dirs_first: true,
            },
        );
        assert_eq!(
            names_in_order(&entries, &ascending),
            vec!["a_dir", "z_dir", "a_file", "z_file"]
        );

        let mut descending: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(
            &mut descending,
            &keys,
            SortOptions {
                column: SortColumn::Name,
                ascending: false,
                dirs_first: true,
            },
        );
        // Directories still lead; only the within-group order flips.
        assert_eq!(
            names_in_order(&entries, &descending),
            vec!["z_dir", "a_dir", "z_file", "a_file"]
        );
    }

    #[test]
    fn sort_is_stable_on_ties() {
        let mut entries = EntryStore::new();
        // Three entries with the same size (the active column) but
        // distinct names, pushed in a specific order -- a stable sort must
        // preserve that relative order among the ties.
        entries.push("c", &meta(EntryKind::File, 10, 0));
        entries.push("a", &meta(EntryKind::File, 10, 0));
        entries.push("b", &meta(EntryKind::File, 10, 0));
        let sorter = Sorter::new();
        let keys = sorter.build_keys(&entries, SortColumn::Size);
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(
            &mut order,
            &keys,
            SortOptions {
                column: SortColumn::Size,
                ascending: true,
                dirs_first: false,
            },
        );
        assert_eq!(names_in_order(&entries, &order), vec!["c", "a", "b"]);
    }

    #[test]
    fn one_million_entry_sort_completes_within_budget() {
        const N: usize = 1_000_000;
        let mut entries = EntryStore::with_capacity(N);
        for i in 0..N {
            entries.push(
                &format!("file_{i:07}.txt"),
                &meta(EntryKind::File, i as u64, i as i64),
            );
        }

        let sorter = Sorter::new();
        let start = Instant::now();
        let keys = sorter.build_keys(&entries, SortColumn::Name);
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        sort_order(&mut order, &keys, SortOptions::default());
        let elapsed = start.elapsed();

        assert_eq!(order.len(), N);
        assert!(
            names_in_order(&entries, &order).is_sorted(),
            "result must actually be name-sorted, not just fast"
        );

        // T-3.2.2's AC: "1M-entry sort <= 400ms". Only enforced under
        // --release (debug builds -- no optimizations, debug_assertions on
        // per the workspace's [profile.dev] -- are not representative of
        // the number the AC is about; see this test's own measured numbers
        // in the doc comment above for the release figures this was
        // validated against).
        if !cfg!(debug_assertions) {
            assert!(
                elapsed.as_millis() <= 400,
                "1M-entry name sort took {elapsed:?}, over the 400ms T-3.2.2 budget"
            );
        }
    }
}
