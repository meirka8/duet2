//! [`NameArena`]: the interned-string backing store for [`crate::EntryStore`]
//! names (design.md §9.2: "Names in one interned arena (`Box<str>` slabs,
//! no per-entry `String` allocation)").

/// A byte range inside a [`NameArena`], as stored per-entry in
/// [`crate::EntryStore`] (`name_offset: Vec<u32>`, `name_len: Vec<u16>` --
/// kept as two parallel arrays rather than one `Vec<NameSpan>` so there is
/// no alignment padding between the fields; see `EntryStore`'s byte-budget
/// doc comment).
///
/// `offset` is a *global* offset across the whole arena, not relative to
/// any one slab -- [`NameArena::resolve`] does the slab lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameSpan {
    pub offset: u32,
    pub len: u16,
}

/// Target size of one arena slab before a new one is started. Chosen to
/// amortize allocation count (a 1M-entry directory at ~24 B/name averages
/// ~375 slabs, not 1M allocations) while keeping any single slab small
/// enough that `resolve`'s binary search over slab boundaries stays cheap
/// (a few dozen comparisons at 1M entries' worth of arena).
const SLAB_TARGET_BYTES: usize = 64 * 1024;

/// Append-only interned string arena.
///
/// Grows by pushing new `Box<str>` slabs rather than reallocating and
/// copying one big buffer on every regrow. That distinction matters here:
/// `EntryStore` is populated incrementally as `read_dir` chunks stream in
/// (design.md §9.2 "Streaming population" -- one flush per frame, max), so
/// a [`NameSpan`] handed out for chunk 1 must stay valid while chunk 400
/// is still arriving. A single reallocating `String` would invalidate
/// every previously issued span each time it outgrows its capacity (via
/// realloc-and-copy, even though the *contents* at each offset wouldn't
/// change -- the referencing spans are fine, but a naive single-buffer
/// arena using `Vec<u8>`/`String::with_capacity` sized by a guess would
/// still risk exactly that on underestimate). Once a slab is frozen into
/// `slabs`, it never moves again, so it can even be handed out as
/// `&'static str` via `Box::leak` if a caller needs that (S-1's spike did
/// this for its single-shot benchmark); `DirectoryModel` does not, because
/// unlike the spike it must be able to free the whole arena on a full
/// rescan (a fresh `read_dir` bumps `generation` and replaces the model
/// wholesale -- see `EntryId`'s doc comment on cross-generation stability).
pub struct NameArena {
    /// Finalized, immutable slabs, in order.
    slabs: Vec<Box<str>>,
    /// `slab_starts[i]` is the global offset of the first byte of
    /// `slabs[i]`, for `i < slabs.len()`. Has `slabs.len() + 1` entries: a
    /// trailing entry equal to `building`'s start offset, so `resolve` can
    /// binary-search a single slice for both finalized slabs and the
    /// in-progress one uniformly.
    ///
    /// Every entry, including the trailing one, is a **fixed breakpoint**:
    /// once written (at construction, or by [`Self::flush_slab`]), it is
    /// never mutated again — only ever appended to. This is deliberate and
    /// past bugs here are exactly why: an earlier version of this arena
    /// kept the trailing entry "live", mutating it on every [`Self::intern`]
    /// call to track `building`'s current end. That conflated two different
    /// things (a fixed boundary for bucketing offsets during `resolve`, and
    /// a running total that's meaningless once more names are interned
    /// after the point a given [`NameSpan`] was issued) and broke both:
    /// `resolve` would look up a *later* value than the one that was true
    /// when the span was minted, misidentifying which slab (or `building`)
    /// actually owns a given offset. Keeping this fixed and deriving
    /// `building`'s current end as `start + self.building.len()` on demand
    /// (see [`Self::intern`]) avoids the whole bug class.
    slab_starts: Vec<u32>,
    /// The slab currently being appended to. Flushed into `slabs` once it
    /// reaches [`SLAB_TARGET_BYTES`].
    building: String,
}

impl NameArena {
    /// Creates an empty arena.
    pub fn new() -> Self {
        NameArena {
            slabs: Vec::new(),
            slab_starts: vec![0],
            building: String::new(),
        }
    }

    /// Total bytes interned so far (finalized slabs + the in-progress
    /// one), i.e. the arena's contribution to `EntryStore::approx_bytes`.
    pub fn len(&self) -> usize {
        self.slabs.iter().map(|s| s.len()).sum::<usize>() + self.building.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Interns `name`, returning the [`NameSpan`] that addresses it.
    ///
    /// Panics if `name.len() > u16::MAX`: `EntryStore::push` is expected to
    /// have already rejected or truncated names longer than that (Linux
    /// `NAME_MAX` is 255 bytes; the `u16` budget is deliberately generous
    /// headroom for non-POSIX backends, not an invitation to store
    /// multi-segment paths here).
    pub fn intern(&mut self, name: &str) -> NameSpan {
        assert!(
            name.len() <= u16::MAX as usize,
            "name of {} bytes exceeds NameSpan's u16 length budget",
            name.len()
        );
        // `slab_starts.last()` is `building`'s fixed start (see the field
        // doc comment) — `building`'s *current* end is just that plus
        // however much has accumulated since, no separate bookkeeping
        // needed.
        let building_start = *self.slab_starts.last().expect("sentinel always present");
        let offset = building_start + self.building.len() as u32;
        self.building.push_str(name);
        if self.building.len() >= SLAB_TARGET_BYTES {
            self.flush_slab();
        }
        NameSpan {
            offset,
            len: name.len() as u16,
        }
    }

    /// Freezes the in-progress slab into `slabs` and starts a fresh one.
    fn flush_slab(&mut self) {
        if self.building.is_empty() {
            return;
        }
        let finished = std::mem::take(&mut self.building);
        // `slab_starts.last()` is already `finished`'s start (it's a fixed
        // breakpoint, set the last time this ran, or at construction —
        // never mutated by `intern`; see the field doc comment). The new
        // entry pushed here becomes the *next* fixed breakpoint: the start
        // of whatever `building` accumulates from here.
        let start = *self.slab_starts.last().expect("sentinel always present");
        let end = start + finished.len() as u32;
        self.slabs.push(finished.into_boxed_str());
        self.slab_starts.push(end);
    }

    /// Resolves a [`NameSpan`] back to its string slice.
    ///
    /// `span` must have been returned by this same arena instance (spans
    /// are not portable across arenas/generations -- see [`NameSpan`]'s
    /// doc comment and `EntryId`'s generation-scoping note).
    pub fn resolve(&self, span: NameSpan) -> &str {
        let offset = span.offset as usize;
        let end = offset + span.len as usize;
        // Binary search for the slab whose [start, next_start) range
        // contains `offset`. `slab_starts` is sorted and has one more
        // entry than `slabs` (the trailing sentinel covers `building`).
        //
        // `slab_starts[i]` (for `i < slabs.len()`) is exactly `slabs[i]`'s
        // start, and `slab_starts[slabs.len()]` is exactly `building`'s
        // start — so an *exact* match at index `i` is already the correct
        // `slab_ix` unconditionally (whether that's a real slab or, when
        // `i == slabs.len()`, `building`; the `if slab_ix < self.slabs.len()`
        // check below dispatches between the two). A *non*-exact match at
        // insertion point `i` means `offset` falls strictly inside the
        // range owned by index `i - 1`.
        let slab_ix = match self.slab_starts.binary_search(&span.offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        if slab_ix < self.slabs.len() {
            let start = self.slab_starts[slab_ix] as usize;
            &self.slabs[slab_ix][offset - start..end - start]
        } else {
            // Falls in the in-progress slab. `slab_starts.last()` is
            // `building`'s fixed start directly — no arithmetic needed.
            let start = *self.slab_starts.last().expect("sentinel always present") as usize;
            &self.building[offset - start..end - start]
        }
    }
}

impl Default for NameArena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_and_resolve_round_trips() {
        let mut arena = NameArena::new();
        let a = arena.intern("alpha.txt");
        let b = arena.intern("beta.rs");
        assert_eq!(arena.resolve(a), "alpha.txt");
        assert_eq!(arena.resolve(b), "beta.rs");
    }

    #[test]
    fn resolve_across_a_slab_boundary() {
        let mut arena = NameArena::new();
        let mut spans = Vec::new();
        // Force multiple slab flushes.
        for i in 0..10_000 {
            spans.push((
                format!("entry_{i:06}.bin"),
                arena.intern(&format!("entry_{i:06}.bin")),
            ));
        }
        for (expected, span) in &spans {
            assert_eq!(arena.resolve(*span), expected.as_str());
        }
    }
}
