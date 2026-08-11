//! Deterministic synthetic-corpus generator (T-3.3.4).
//!
//! # Determinism contract
//!
//! [`plan`] is a pure function of `(scale, seed)`: no wall-clock reads, no
//! `HashMap`/`HashSet` iteration (every collection here is a `Vec` built in
//! index order), and no OS randomness. Per-entry variety (which entries get
//! a unicode name, become part of a hardlink farm, etc.) is decided by a
//! [`rand::rngs::StdRng`] seeded from `(seed, index)` alone via
//! [`mix64`] + [`rand::SeedableRng::seed_from_u64`], so entry `i`'s
//! attributes never depend on entries generated before or after it. Two
//! consequences fall out of that:
//!
//! 1. `plan(scale, seed)` called twice produces byte-identical output
//!    (`corpus::tests::plan_is_deterministic_across_calls` below), and the
//!    same holds across processes/machines given the same `rand`/toolchain
//!    versions (pinned by the committed `Cargo.lock`).
//! 2. Planning (and, per group, materializing) is embarrassingly
//!    parallel — [`materialize`] uses `rayon` to fan the 100k/1M-entry
//!    scales out across cores, which is what keeps corpus setup from
//!    dominating a benchmark run.
//!
//! # Shape
//!
//! Each scale plans exactly `scale.entry_count()` entries as the direct
//! children of one root directory (this is deliberate, not incidental: the
//! NFR-03/04 scenario this feeds — "list this huge directory" — and
//! `EntryStore`'s own contract ("storage for one directory listing") both
//! want one wide directory, not entries scattered thin across a tree).
//! Variety is layered on top of that flat set by role, chosen
//! deterministically per index:
//!
//! | role | frequency | notes |
//! |---|---|---|
//! | plain file | remainder (~88%) | size drawn from a small/zero/medium split, mirroring S-4's spike corpus |
//! | unicode name | 1 in 20 (5%) | RTL (Hebrew/Arabic), CJK, Cyrillic, combining accents, emoji |
//! | hardlink farm member | ~4% | groups of 8 (1 target + 7 links) sharing one inode |
//! | subdirectory | 1 in 40 (2.5%) | empty; gives `EntryKind::Directory` variety in the listing |
//! | broken symlink | 1 in 100 (1%) | target deliberately never created |
//! | sparse file | 1 in 300 (~0.3%) | large logical size, ~0 real bytes (disk-conscious per T-3.3.4's process note) |
//!
//! Deep nesting is *not* part of the flat root count — [`materialize`]
//! additionally always creates a handful of fixed-depth directory chains
//! under `<root>/deep/`, independent of `scale`, so every corpus (even the
//! 10-entry one) exercises deep-path resolution without perturbing the
//! root listing's entry count.

use std::io;
use std::path::{Path, PathBuf};

use duet_types::{EntryKind, Metadata, Timestamp};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rayon::prelude::*;

/// The four corpus sizes T-3.3.4 asks for. `entry_count()` is the exact
/// number of entries [`plan`] places directly under the corpus root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorpusScale {
    Ten,
    OneK,
    HundredK,
    OneM,
}

impl CorpusScale {
    pub const ALL: [CorpusScale; 4] = [
        CorpusScale::Ten,
        CorpusScale::OneK,
        CorpusScale::HundredK,
        CorpusScale::OneM,
    ];

    pub fn entry_count(self) -> u64 {
        match self {
            CorpusScale::Ten => 10,
            CorpusScale::OneK => 1_000,
            CorpusScale::HundredK => 100_000,
            CorpusScale::OneM => 1_000_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CorpusScale::Ten => "10",
            CorpusScale::OneK => "1k",
            CorpusScale::HundredK => "100k",
            CorpusScale::OneM => "1M",
        }
    }
}

/// What [`materialize`] should do on disk to realize a [`PlannedEntry`].
/// Kept separate from `metadata` (which is what a real `stat()`/`read_dir()`
/// of the materialized entry would report) so in-memory-only consumers
/// (the `EntryStore` benches) can use `plan()`'s output without ever
/// touching a filesystem.
#[derive(Debug, Clone)]
pub enum OnDisk {
    /// A regular file with `size` bytes of real (non-sparse) content.
    PlainFile { size: u64 },
    /// A regular file with a large logical size but ~0 bytes actually
    /// written (a hole via `File::set_len`) — exercises `stx_blocks` vs
    /// `stx_size` divergence without costing real disk space.
    SparseFile { logical_size: u64 },
    /// An empty subdirectory.
    Directory,
    /// A symlink whose target is guaranteed to not exist.
    BrokenSymlink { target: String },
    /// The first file of a hardlink farm — a normal file that farm members
    /// (`HardlinkMember`) later link to.
    HardlinkTarget { size: u64 },
    /// A `hard_link()` to a farm's `HardlinkTarget`, named `target_name`.
    HardlinkMember { target_name: String },
}

/// One entry planned for the corpus root: its name, the `Metadata` an
/// `EntryStore`/`FileSystem::stat` caller would see, and the on-disk recipe
/// to realize it.
#[derive(Debug, Clone)]
pub struct PlannedEntry {
    pub name: String,
    pub metadata: Metadata,
    pub on_disk: OnDisk,
}

/// A full plan: the flat root listing (`entries.len() == scale.entry_count()`).
#[derive(Debug, Clone)]
pub struct CorpusPlan {
    pub scale: CorpusScale,
    pub seed: u64,
    pub entries: Vec<PlannedEntry>,
}

/// Fixed-depth chains always created under `<root>/deep/` by [`materialize`],
/// independent of `scale` — see the module doc's "Shape" section.
pub const DEEP_CHAIN_DEPTHS: [u32; 3] = [16, 32, 48];

/// Cheap deterministic 64-bit mix (splitmix64) used only to derive an
/// independent sub-seed per `(seed, index)` pair — not a cryptographic
/// concern here, just decorrelation so adjacent indices don't draw
/// correlated `StdRng` streams.
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Per-entry RNG: deterministic from `(seed, index)` alone (module doc's
/// determinism contract), built via the exact primitive T-3.3.4 asks for
/// (`StdRng::seed_from_u64`).
fn entry_rng(seed: u64, index: u64) -> StdRng {
    StdRng::seed_from_u64(mix64(seed ^ mix64(index)))
}

/// A curated set of non-ASCII name fragments: RTL scripts (Hebrew, Arabic),
/// CJK, Cyrillic, a combining-accent form (not pre-composed), and emoji —
/// deliberately not just "an accented Latin letter," per S-6's finding that
/// RTL and combining forms are the cases that actually break naive text
/// handling.
const UNICODE_WORDS: &[&str] = &[
    "café",
    "Zürich_Bahnhofstraße",
    "日本語ファイル名",
    "北京市朝阳区",
    "москва_отчёт",
    "Санкт-Петербург",
    "עברית_קובץ",
    "שלום_עולם",
    "ملف_تجريبي",
    "شكرا_جزيلا",
    "Ελληνικά_αρχείο",
    "한국어_파일",
    "e\u{0301}tude_combining", // "e" + COMBINING ACUTE ACCENT, not U+00E9
    "n\u{0303}ame_combining",  // "n" + COMBINING TILDE
    "𝔘𝔫𝔦𝔠𝔬𝔡𝔢_math_alphanumeric",
    "emoji_📁🎉✨_report",
    "Việt_Nam_tệp",
    "देवनागरी_फ़ाइल",
];

fn plain_name(seed: u64, index: u64) -> String {
    // Fixed-width hash prefix (stable average name length across scales) +
    // the index, so names are always unique even if the mix collides.
    format!("f{:016x}_{index}", mix64(seed ^ index))
}

fn unicode_name(seed: u64, index: u64) -> String {
    let word = UNICODE_WORDS[(mix64(seed ^ index ^ 0xC0FF_EE00) as usize) % UNICODE_WORDS.len()];
    format!("{word}_{index}")
}

/// Size for an ordinary (non-sparse) plain file: mirrors S-4's spike
/// corpus distribution (roughly a third empty, a third small, a third
/// medium), scaled down from S-4's byte range so a 1M-entry root stays
/// disk-conscious (T-3.3.4's process note).
fn plain_size(rng: &mut StdRng) -> u64 {
    match rng.random_range(0..3) {
        0 => 0,
        1 => rng.random_range(1..128),
        _ => rng.random_range(128..1024),
    }
}

const HARDLINK_GROUP_STRIDE: u64 = 200;
const HARDLINK_GROUP_SIZE: u64 = 8;

/// Plans `scale.entry_count()` entries for one flat root directory, per the
/// role table in the module doc. Pure and parallel: entry `i` depends only
/// on `(seed, i)`, so this is computed with `rayon` and the result is
/// always in index order (a `Vec` collected from an indexed parallel
/// iterator, not a `HashMap`), preserving byte-identical output regardless
/// of how many threads ran it.
pub fn plan(scale: CorpusScale, seed: u64) -> CorpusPlan {
    let count = scale.entry_count();
    let entries: Vec<PlannedEntry> = (0..count)
        .into_par_iter()
        .map(|i| plan_one(seed, i, count))
        .collect();
    CorpusPlan {
        scale,
        seed,
        entries,
    }
}

fn plan_one(seed: u64, i: u64, count: u64) -> PlannedEntry {
    let mut rng = entry_rng(seed, i);
    let mtime_secs = 1_700_000_000 + (i as i64 % 31_536_000); // spread across ~1 year, deterministic

    // Hardlink farms: a fixed stride of 200, first 8 slots of each stride
    // (when the corpus is large enough to have a full stride).
    let stride_pos = i % HARDLINK_GROUP_STRIDE;
    if count >= HARDLINK_GROUP_STRIDE && stride_pos < HARDLINK_GROUP_SIZE {
        let group = i / HARDLINK_GROUP_STRIDE;
        return if stride_pos == 0 {
            let size = plain_size(&mut rng);
            let name = format!("hlink_{group:06}_target");
            let mut metadata = Metadata::minimal(EntryKind::File);
            metadata.size = size;
            metadata.modified = Some(Timestamp::new(mtime_secs, 0));
            metadata.nlink = Some(HARDLINK_GROUP_SIZE);
            PlannedEntry {
                name,
                metadata,
                on_disk: OnDisk::HardlinkTarget { size },
            }
        } else {
            let target_name = format!("hlink_{group:06}_target");
            let name = format!("hlink_{group:06}_m{stride_pos}");
            let size = plain_size(&mut StdRng::seed_from_u64(mix64(seed ^ (group * 1000))));
            let mut metadata = Metadata::minimal(EntryKind::File);
            metadata.size = size;
            metadata.modified = Some(Timestamp::new(mtime_secs, 0));
            metadata.nlink = Some(HARDLINK_GROUP_SIZE);
            PlannedEntry {
                name,
                metadata,
                on_disk: OnDisk::HardlinkMember { target_name },
            }
        };
    }

    let bucket = i % 100;
    if bucket < 5 {
        // 5%: unicode name, plain file otherwise.
        let size = plain_size(&mut rng);
        let mut metadata = Metadata::minimal(EntryKind::File);
        metadata.size = size;
        metadata.modified = Some(Timestamp::new(mtime_secs, 0));
        PlannedEntry {
            name: unicode_name(seed, i),
            metadata,
            on_disk: OnDisk::PlainFile { size },
        }
    } else if bucket < 5 + 2 {
        // next 2.5% (rounded to 2%): empty subdirectory.
        let mut metadata = Metadata::minimal(EntryKind::Directory);
        metadata.modified = Some(Timestamp::new(mtime_secs, 0));
        PlannedEntry {
            name: format!("dir_{i}"),
            metadata,
            on_disk: OnDisk::Directory,
        }
    } else if bucket == 7 {
        // 1%: broken symlink.
        let target = format!("ENOENT-{seed:x}-{i}");
        let mut metadata = Metadata::minimal(EntryKind::Symlink);
        metadata.modified = Some(Timestamp::new(mtime_secs, 0));
        // `UnixPathBuf` only models absolute paths; the on-disk symlink
        // itself is a plain relative target string (set separately in
        // `on_disk`, matching real broken-symlink `readlink()` output).
        metadata.symlink_target = duet_types::UnixPathBuf::new(&format!("/{target}")).ok();
        PlannedEntry {
            name: format!("broken_symlink_{i}"),
            metadata,
            on_disk: OnDisk::BrokenSymlink { target },
        }
    } else if bucket == 8 && i.is_multiple_of(3) {
        // ~0.3% (bucket 8, further thinned): sparse file.
        let logical_size = 8 * 1024 * 1024 + (i % 4096);
        let mut metadata = Metadata::minimal(EntryKind::File);
        metadata.size = logical_size;
        metadata.modified = Some(Timestamp::new(mtime_secs, 0));
        PlannedEntry {
            name: format!("sparse_{i}"),
            metadata,
            on_disk: OnDisk::SparseFile { logical_size },
        }
    } else {
        let size = plain_size(&mut rng);
        let mut metadata = Metadata::minimal(EntryKind::File);
        metadata.size = size;
        metadata.modified = Some(Timestamp::new(mtime_secs, 0));
        PlannedEntry {
            name: plain_name(seed, i),
            metadata,
            on_disk: OnDisk::PlainFile { size },
        }
    }
}

/// Stats returned by [`materialize`], mostly for `eprintln!` progress and
/// sanity assertions in tests/benches.
#[derive(Debug, Clone, Copy, Default)]
pub struct MaterializeStats {
    pub entries_written: u64,
    pub bytes_written: u64,
}

/// Realizes a [`CorpusPlan`] on disk under `root` (created if absent; must
/// be empty or absent, mirroring S-4 spike's corpus generator — refuses to
/// silently mix corpora). Also always creates the fixed `deep/` chains
/// (module doc's "Shape" section), independent of the plan's scale.
///
/// Two passes, required for correctness (not just an optimization):
/// hardlink targets must exist on disk before `hard_link()` can point at
/// them, and plan order doesn't guarantee a target's index precedes its
/// members' indices once entries are processed in parallel. Pass 1 creates
/// everything except hardlink members; pass 2 links the members.
pub fn materialize(root: &Path, plan: &CorpusPlan) -> io::Result<MaterializeStats> {
    std::fs::create_dir_all(root)?;
    let existing = std::fs::read_dir(root)?.count();
    if existing != 0 {
        return Err(io::Error::other(format!(
            "refusing to materialize into non-empty dir {} ({existing} entries present)",
            root.display()
        )));
    }

    let bytes_written: u64 = plan
        .entries
        .par_iter()
        .filter(|e| !matches!(e.on_disk, OnDisk::HardlinkMember { .. }))
        .map(|e| materialize_one(root, e))
        .collect::<io::Result<Vec<u64>>>()?
        .into_iter()
        .sum();

    plan.entries
        .par_iter()
        .filter(|e| matches!(e.on_disk, OnDisk::HardlinkMember { .. }))
        .map(|e| materialize_one(root, e))
        .collect::<io::Result<Vec<u64>>>()?;

    materialize_deep_chains(root, plan.seed)?;

    Ok(MaterializeStats {
        entries_written: plan.entries.len() as u64,
        bytes_written,
    })
}

fn materialize_one(root: &Path, entry: &PlannedEntry) -> io::Result<u64> {
    let path = root.join(&entry.name);
    match &entry.on_disk {
        OnDisk::PlainFile { size } | OnDisk::HardlinkTarget { size } => {
            write_filled(&path, *size)?;
            Ok(*size)
        }
        OnDisk::SparseFile { logical_size } => {
            let f = std::fs::File::create(&path)?;
            f.set_len(*logical_size)?;
            Ok(0)
        }
        OnDisk::Directory => {
            std::fs::create_dir(&path)?;
            Ok(0)
        }
        OnDisk::BrokenSymlink { target } => {
            std::os::unix::fs::symlink(target, &path)?;
            Ok(0)
        }
        OnDisk::HardlinkMember { target_name } => {
            std::fs::hard_link(root.join(target_name), &path)?;
            Ok(0)
        }
    }
}

fn write_filled(path: &Path, size: u64) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    if size > 0 {
        // Deterministic, cheap fill (not zeros-only, so a naive
        // hole-detecting "is this secretly sparse?" check can't false
        // positive): repeat the path's own byte length as fill.
        let chunk = vec![(size % 251) as u8; size.min(4096) as usize];
        let mut remaining = size;
        while remaining > 0 {
            let n = remaining.min(chunk.len() as u64) as usize;
            f.write_all(&chunk[..n])?;
            remaining -= n as u64;
        }
    }
    Ok(())
}

fn materialize_deep_chains(root: &Path, seed: u64) -> io::Result<()> {
    let deep_root = root.join("deep");
    std::fs::create_dir_all(&deep_root)?;
    for (chain_ix, &depth) in DEEP_CHAIN_DEPTHS.iter().enumerate() {
        let mut dir: PathBuf = deep_root.join(format!("chain_{chain_ix}"));
        std::fs::create_dir_all(&dir)?;
        for level in 0..depth {
            dir = dir.join(format!("lvl_{level:03}"));
            std::fs::create_dir(&dir)?;
        }
        let leaf_size = plain_size(&mut StdRng::seed_from_u64(mix64(
            seed ^ (chain_ix as u64) ^ u64::from(depth),
        )));
        write_filled(&dir.join("bottom.txt"), leaf_size)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_deterministic_across_calls() {
        for scale in [CorpusScale::Ten, CorpusScale::OneK] {
            let a = plan(scale, 42);
            let b = plan(scale, 42);
            assert_eq!(a.entries.len(), b.entries.len());
            for (ea, eb) in a.entries.iter().zip(b.entries.iter()) {
                assert_eq!(ea.name, eb.name, "name mismatch for scale {scale:?}");
                assert_eq!(ea.metadata.size, eb.metadata.size);
                assert_eq!(ea.metadata.kind, eb.metadata.kind);
            }
        }
    }

    #[test]
    fn plan_differs_across_seeds() {
        let a = plan(CorpusScale::OneK, 1);
        let b = plan(CorpusScale::OneK, 2);
        let same_names = a
            .entries
            .iter()
            .zip(b.entries.iter())
            .filter(|(ea, eb)| ea.name == eb.name)
            .count();
        assert!(
            same_names < a.entries.len() / 2,
            "different seeds should mostly diverge in names"
        );
    }

    #[test]
    fn plan_len_matches_scale() {
        for scale in CorpusScale::ALL {
            // Skip 1M/100k in the default test run's plan-only check to
            // keep `cargo test` fast; entry_count() itself is checked for
            // every scale.
            assert!(scale.entry_count() > 0);
        }
        assert_eq!(plan(CorpusScale::Ten, 7).entries.len(), 10);
        assert_eq!(plan(CorpusScale::OneK, 7).entries.len(), 1_000);
    }

    #[test]
    fn plan_contains_every_role() {
        let p = plan(CorpusScale::OneK, 99);
        let has_unicode = p.entries.iter().any(|e| !e.name.is_ascii());
        let has_dir = p
            .entries
            .iter()
            .any(|e| e.metadata.kind == EntryKind::Directory);
        let has_symlink = p
            .entries
            .iter()
            .any(|e| e.metadata.kind == EntryKind::Symlink);
        let has_sparse = p
            .entries
            .iter()
            .any(|e| matches!(e.on_disk, OnDisk::SparseFile { .. }));
        let has_hardlink = p
            .entries
            .iter()
            .any(|e| matches!(e.on_disk, OnDisk::HardlinkTarget { .. }));
        assert!(
            has_unicode,
            "expected at least one unicode name at 1k scale"
        );
        assert!(has_dir, "expected at least one directory entry at 1k scale");
        assert!(
            has_symlink,
            "expected at least one broken symlink at 1k scale"
        );
        assert!(has_sparse, "expected at least one sparse file at 1k scale");
        assert!(
            has_hardlink,
            "expected at least one hardlink farm at 1k scale"
        );
    }

    #[test]
    fn materialize_roundtrip_small_scale() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(CorpusScale::Ten, 5);
        let stats = materialize(dir.path(), &p).unwrap();
        assert_eq!(stats.entries_written, 10);
        let on_disk: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        // 10 planned entries + the always-present `deep/` directory.
        assert_eq!(on_disk.len(), 11);
        // The deepest chain actually resolves to the configured depth.
        let mut deepest = dir.path().join("deep").join("chain_2");
        for level in 0..DEEP_CHAIN_DEPTHS[2] {
            deepest = deepest.join(format!("lvl_{level:03}"));
        }
        assert!(deepest.join("bottom.txt").is_file());
    }

    #[test]
    fn materialize_refuses_non_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("preexisting"), b"x").unwrap();
        let p = plan(CorpusScale::Ten, 1);
        assert!(materialize(dir.path(), &p).is_err());
    }
}
