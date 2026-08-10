//! `EntryStore`: struct-of-arrays backing data for the 1,000,000-row spike
//! table, matching the SoA shape sketched in design.md §9.2.
//!
//! Design choices, and why:
//!
//! - Names, and the pre-formatted display text for size/date, live in ONE
//!   leaked `&'static str` arena built with a single pass (a handful of
//!   `String` reallocations at most, never one allocation per row). Spans
//!   into the arena are stored as `(u32, u32)` offset/len pairs, 8 bytes a
//!   row. Because the arena is `&'static`, `SharedString::new_static` can
//!   wrap a slice of it with **zero** allocation and zero refcounting at
//!   render time — it's just a borrow.
//! - Raw numeric fields (`sizes: Vec<u64>`, `dates: Vec<i64>`) are kept
//!   separately so sorting compares integers, never strings.
//! - `mode` and `ext` are drawn from small closed vocabularies (a handful of
//!   permission-string variants, ~20 extensions) so their *display* text is
//!   a `&'static str` literal compiled into the binary — no arena entry
//!   needed at all for those two columns.
//! - `order: Vec<u32>` is the permutation the table actually renders through.
//!   Sorting permutes this vector; the SoA arrays underneath are never
//!   touched, matching "sorting permutes order, never the data" from the
//!   design doc.
//! - `selection` is a flat bitset (`Vec<u64>`) indexed by *stable* row id,
//!   not a `Vec<bool>` per row and not attached to display position, so it
//!   survives re-sorting.

use std::fmt::Write as _;

use rayon::slice::ParallelSliceMut as _;

pub const ROW_COUNT: usize = 1_000_000;

const WORDS: &[&str] = &[
    "invoice", "report", "photo", "draft", "backup", "config", "session", "ledger", "archive",
    "snapshot", "notes", "build", "release", "patch", "index", "cache", "log", "manifest",
    "profile", "asset",
];

const EXTENSIONS: &[&str] = &[
    "rs", "txt", "md", "json", "toml", "png", "jpg", "zip", "tar", "gz", "log", "conf", "py",
    "js", "ts", "html", "css", "pdf", "csv", "bin",
];

const MODES: &[&str] = &[
    "-rw-r--r--",
    "-rwxr-xr-x",
    "drwxr-xr-x",
    "-rw-------",
    "lrwxrwxrwx",
    "-rw-rw-r--",
];

/// Tiny deterministic xorshift64* PRNG so runs are reproducible without an
/// external `rand` dependency (keeps the spike's build graph small).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E3779B97F4A7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Convert unix seconds to (year, month, day, hour, minute, second) using
/// Howard Hinnant's civil_from_days algorithm. No `chrono`/`time` dependency
/// needed for a spike that only wants a plausible, varied date string.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, hour, minute, second)
}

pub struct EntryStore {
    len: usize,

    arena: &'static str,
    name_span: Vec<(u32, u32)>,
    size_span: Vec<(u32, u32)>,
    date_span: Vec<(u32, u32)>,

    pub sizes: Vec<u64>,
    pub dates: Vec<i64>,
    mode_id: Vec<u8>,
    ext_id: Vec<u16>,

    /// The permutation the table renders through: `order[display_row] =
    /// stable_id`. Sorting only ever rewrites this vector.
    pub order: Vec<u32>,
}

impl EntryStore {
    pub fn generate(n: usize) -> Self {
        let mut rng = Rng::new(0xD00F_5EED);

        // ~55 bytes/row average (name ~20, size text ~9, date text ~19) plus
        // slack; a single generous up-front capacity means the arena never
        // reallocates mid-build.
        let mut arena = String::with_capacity(n * 72);

        let mut name_span = Vec::with_capacity(n);
        let mut size_span = Vec::with_capacity(n);
        let mut date_span = Vec::with_capacity(n);
        let mut sizes = Vec::with_capacity(n);
        let mut dates = Vec::with_capacity(n);
        let mut mode_id = Vec::with_capacity(n);
        let mut ext_id = Vec::with_capacity(n);

        let now = 1_754_486_400i64; // 2025-08-06T00:00:00Z, arbitrary "today" for the spike
        let five_years_secs: i64 = 5 * 365 * 86_400;

        for i in 0..n {
            let word = WORDS[(i + rng.range(WORDS.len() as u64) as usize) % WORDS.len()];
            let ext_idx = rng.range(EXTENSIONS.len() as u64) as u16;
            let ext = EXTENSIONS[ext_idx as usize];

            let start = arena.len() as u32;
            write!(arena, "{word}_{i:07}.{ext}").expect("arena write cannot fail");
            let len = arena.len() as u32 - start;
            name_span.push((start, len));

            // Log-ish distributed size from 0 bytes to ~4 GiB so sorting by
            // size is meaningful across many orders of magnitude.
            let magnitude = rng.range(32); // 0..=31 "bits"
            let base = if magnitude == 0 { 0 } else { 1u64 << magnitude.min(32) };
            let jitter = rng.range(base.max(1));
            let size = base.saturating_add(jitter).min(4 * 1024 * 1024 * 1024);
            sizes.push(size);

            let start = arena.len() as u32;
            write!(arena, "{}", human_size(size)).expect("arena write cannot fail");
            let len = arena.len() as u32 - start;
            size_span.push((start, len));

            let date = now - rng.range(five_years_secs as u64) as i64;
            dates.push(date);
            let (y, mo, d, hh, mm, _ss) = civil_from_unix(date);
            let start = arena.len() as u32;
            write!(arena, "{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}").expect("arena write cannot fail");
            let len = arena.len() as u32 - start;
            date_span.push((start, len));

            mode_id.push(rng.range(MODES.len() as u64) as u8);
            ext_id.push(ext_idx);
        }

        let arena: &'static str = Box::leak(arena.into_boxed_str());
        let order: Vec<u32> = (0..n as u32).collect();

        Self {
            len: n,
            arena,
            name_span,
            size_span,
            date_span,
            sizes,
            dates,
            mode_id,
            ext_id,
            order,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn name(&self, id: usize) -> &'static str {
        let (s, l) = self.name_span[id];
        &self.arena[s as usize..(s + l) as usize]
    }

    #[inline]
    pub fn size_text(&self, id: usize) -> &'static str {
        let (s, l) = self.size_span[id];
        &self.arena[s as usize..(s + l) as usize]
    }

    #[inline]
    pub fn date_text(&self, id: usize) -> &'static str {
        let (s, l) = self.date_span[id];
        &self.arena[s as usize..(s + l) as usize]
    }

    #[inline]
    pub fn mode_text(&self, id: usize) -> &'static str {
        MODES[self.mode_id[id] as usize]
    }

    #[inline]
    pub fn ext_text(&self, id: usize) -> &'static str {
        EXTENSIONS[self.ext_id[id] as usize]
    }

    /// Byte size of the arena + parallel arrays, for the RSS-vs-model note in
    /// the report (this is NOT the RSS number, just the accountable data size).
    pub fn approx_bytes(&self) -> usize {
        self.arena.len()
            + self.name_span.len() * 8
            + self.size_span.len() * 8
            + self.date_span.len() * 8
            + self.sizes.len() * 8
            + self.dates.len() * 8
            + self.mode_id.len()
            + self.ext_id.len() * 2
            + self.order.len() * 4
    }

    /// Sort `order` by the given column, ascending or descending. Column
    /// indices match the table: 0 name, 1 size, 2 date, 3 mode, 4 ext.
    ///
    /// This is the operation the AC's "sort of 1M rows under 400ms" is
    /// measured against. It permutes `order` only — `sizes`, `dates`, the
    /// arena, etc. never move.
    pub fn sort_by_column(&mut self, col_ix: usize, ascending: bool) {
        match col_ix {
            0 => {
                // Split borrow: `name_span`/`arena` are pre-bound so the
                // closure only touches those fields, leaving `self.order`
                // free to be borrowed mutably by `sort_unstable_by`.
                let arena = self.arena;
                let name_span = &self.name_span;
                // Name comparisons are the expensive case (variable-length
                // byte-slice compares vs. a single integer compare for the
                // other columns), so this is the one column sorted with
                // rayon's parallel pattern-defeating quicksort rather than
                // std's single-threaded sort_unstable_by.
                self.order.par_sort_unstable_by(|&a, &b| {
                    let (sa, la) = name_span[a as usize];
                    let (sb, lb) = name_span[b as usize];
                    let na = &arena[sa as usize..(sa + la) as usize];
                    let nb = &arena[sb as usize..(sb + lb) as usize];
                    na.cmp(nb)
                });
            }
            1 => {
                self.order
                    .sort_unstable_by_key(|&id| self.sizes[id as usize]);
            }
            2 => {
                self.order
                    .sort_unstable_by_key(|&id| self.dates[id as usize]);
            }
            3 => {
                self.order
                    .sort_unstable_by_key(|&id| self.mode_id[id as usize]);
            }
            4 => {
                self.order
                    .sort_unstable_by_key(|&id| self.ext_id[id as usize]);
            }
            _ => return,
        }
        if !ascending {
            self.order.reverse();
        }
    }

    pub fn reset_order(&mut self) {
        for (i, slot) in self.order.iter_mut().enumerate() {
            *slot = i as u32;
        }
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// A flat bitset over stable row ids — the "bitmask or index set, not
/// per-row bools" multi-selection store.
#[derive(Default)]
pub struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            words: vec![0u64; n.div_ceil(64)],
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        match self.words.get(i >> 6) {
            Some(w) => (w >> (i & 63)) & 1 != 0,
            None => false,
        }
    }

    #[inline]
    pub fn set(&mut self, i: usize, v: bool) {
        let word = &mut self.words[i >> 6];
        if v {
            *word |= 1 << (i & 63);
        } else {
            *word &= !(1 << (i & 63));
        }
    }

    #[inline]
    pub fn toggle(&mut self, i: usize) {
        self.words[i >> 6] ^= 1 << (i & 63);
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn clear(&mut self) {
        for w in &mut self.words {
            *w = 0;
        }
    }

    /// Select every `stride`-th stable id, for stress-testing render cost
    /// under a large, sparse multi-selection.
    pub fn select_stride(&mut self, stride: usize) {
        self.clear();
        let mut i = 0;
        while i < self.words.len() * 64 {
            self.set(i, true);
            i += stride;
        }
    }
}
