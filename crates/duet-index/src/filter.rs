//! T-3.2.3: filtering `DirectoryModel::order`.
//!
//! Four kinds of filter, per T-3.2.3's brief: hidden-file exclusion (the
//! dotfile convention), a quick-filter substring match, a glob-style mask
//! (`*.rs`), and named/reusable "saved" filters. All four compose into one
//! [`FilterSpec`] evaluated with AND semantics -- an entry is visible only
//! if it passes every active criterion.
//!
//! **Composability with sort, without a full rebuild** (the AC's real
//! design point, not just a speed target): `DirectoryModel` keeps
//! `full_order` (every live entry, sorted, filter-independent) separate
//! from `order` (the filtered view of it). Changing the filter
//! (`DirectoryModel::set_filter`) re-derives `order` from the *already
//! sorted* `full_order` with one `O(n)` scan through [`FilterSpec::matches`]
//! -- it never touches `sort_keys`/calls `Sorter::build_keys` or
//! `sort::sort_order` again. Changing the sort (`resort`) still has to
//! re-filter (an entry's visibility doesn't depend on its position), but
//! that's the same cheap `O(n)` scan, not a second sort. So: filter change
//! = one scan; sort change = one sort + one scan; neither pays for the
//! other's expensive half twice.

use duet_types::EntryKind;

/// Case-insensitive (ASCII only -- see the module's `ascii` helpers) glob
/// match supporting `*` (any run of characters, including empty) and `?`
/// (exactly one character). This is the classic two-pointer wildcard-match
/// algorithm (no recursion, no exponential blowup on adversarial patterns
/// like `"*a*a*a*a*a*"`): `star_p`/`star_t` remember the most recent `*`
/// and how much of `text` had been consumed when it was seen, so a failed
/// match past that point can retry by consuming one more `text` character
/// under that `*` rather than restarting the whole match.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || ascii_ieq(pattern[p], text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, t));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn ascii_ieq(a: u8, b: u8) -> bool {
    a.eq_ignore_ascii_case(&b)
}

/// Case-insensitive (ASCII-only) substring search, without allocating a
/// lowercased copy of either string -- folds bytes during comparison
/// instead, since this runs once per entry per filter application and
/// T-3.2.3's AC budgets the whole 1M-entry pass at 80 ms.
///
/// ASCII-only is a deliberate, documented simplification: full Unicode
/// case folding needs real casemapping tables (e.g. `icu_casemap`), which
/// is more machinery than a quick-filter's "type and narrow the list"
/// interaction needs. Non-ASCII bytes are compared as-is (case-sensitively),
/// so `"café"` still matches the query `"café"` exactly; it just won't
/// match `"CAFÉ"`.
fn ascii_icontains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(&a, &b)| ascii_ieq(a, b)))
}

/// A glob-style mask, e.g. `*.rs`, compiled once (lowercased into owned
/// bytes) and reused across every entry a filter pass evaluates, rather
/// than re-parsing/re-allocating per comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pattern: Box<str>,
}

impl Mask {
    pub fn new(pattern: impl Into<Box<str>>) -> Self {
        Mask {
            pattern: pattern.into(),
        }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn matches(&self, name: &str) -> bool {
        glob_match(self.pattern.as_bytes(), name.as_bytes())
    }
}

/// One filter configuration: every field is a criterion an entry must pass
/// (AND semantics). `#[derive(Default)]` (`show_hidden: false`, everything
/// else `None`) matches [`FilterSpec::new`]'s "dotfiles hidden, nothing
/// else active" common-default behavior -- for the true identity filter
/// (nothing excluded, including dotfiles), set `show_hidden: true`
/// explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterSpec {
    /// When `false`, entries with `EntryFlags::hidden()` set (the dotfile
    /// convention: name starts with `.` -- see `EntryStore::push`) are
    /// excluded.
    pub show_hidden: bool,
    /// Case-insensitive substring match against the entry name, e.g. the
    /// live-narrowing text field FR-... quick-filter UI would drive.
    pub quick_filter: Option<Box<str>>,
    /// Glob-style mask, e.g. `*.rs`.
    pub mask: Option<Mask>,
}

impl FilterSpec {
    /// The identity filter: dotfiles hidden (the common default), nothing
    /// else active.
    pub fn new() -> Self {
        FilterSpec {
            show_hidden: false,
            quick_filter: None,
            mask: None,
        }
    }

    /// `true` if every active criterion passes for an entry with this
    /// `name`/`hidden`/`kind`. Directories are exempted from the quick
    /// filter and mask (but not from `show_hidden`) -- narrowing "*.rs" or
    /// typing "conf" into a quick filter is universally expected to still
    /// let you navigate *into* folders that don't themselves match, rather
    /// than hiding the whole tree out from under you. This mirrors every
    /// mainstream file manager's quick-filter behavior.
    pub fn matches(&self, name: &str, hidden: bool, kind: EntryKind) -> bool {
        if hidden && !self.show_hidden {
            return false;
        }
        if kind.is_dir() {
            return true;
        }
        if let Some(q) = &self.quick_filter
            && !ascii_icontains(name, q)
        {
            return false;
        }
        if let Some(mask) = &self.mask
            && !mask.matches(name)
        {
            return false;
        }
        true
    }
}

/// A named, reusable [`FilterSpec`] (T-3.2.3's "saved filters"). Storage
/// here is deliberately minimal -- an in-memory named collection, not
/// disk persistence (T-3.2.3's brief: "the storage/persistence can be
/// minimal for now, focus on the filter *evaluation* logic being real").
/// Wiring this to `duet-config`'s actual persistence layer is a later
/// task's job once that crate exists to receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFilter {
    pub name: Box<str>,
    pub spec: FilterSpec,
}

/// An ordered, named collection of [`SavedFilter`]s. Ordered (`Vec`, not a
/// map) so a UI palette can show them in a stable, user-controlled order;
/// lookup by name is a linear scan, which is fine at the scale a person
/// hand-curates saved filters to (tens, not thousands).
#[derive(Debug, Clone, Default)]
pub struct SavedFilters {
    filters: Vec<SavedFilter>,
}

impl SavedFilters {
    pub fn new() -> Self {
        SavedFilters {
            filters: Vec::new(),
        }
    }

    /// Adds or replaces (by name) a saved filter.
    pub fn save(&mut self, name: impl Into<Box<str>>, spec: FilterSpec) {
        let name = name.into();
        if let Some(existing) = self.filters.iter_mut().find(|f| f.name == name) {
            existing.spec = spec;
        } else {
            self.filters.push(SavedFilter { name, spec });
        }
    }

    pub fn get(&self, name: &str) -> Option<&FilterSpec> {
        self.filters
            .iter()
            .find(|f| &*f.name == name)
            .map(|f| &f.spec)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.filters.len();
        self.filters.retain(|f| &*f.name != name);
        self.filters.len() != before
    }

    pub fn iter(&self) -> impl Iterator<Item = &SavedFilter> {
        self.filters.iter()
    }

    pub fn len(&self) -> usize {
        self.filters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_question_mark() {
        assert!(glob_match(b"*.rs", b"main.rs"));
        assert!(glob_match(b"*.rs", b".rs"));
        assert!(!glob_match(b"*.rs", b"main.rs.bak"));
        assert!(glob_match(b"file?.txt", b"file1.txt"));
        assert!(!glob_match(b"file?.txt", b"file10.txt"));
        assert!(glob_match(b"*a*a*a*", b"banana"));
        assert!(!glob_match(b"*a*a*a*a*a*", b"banana"));
    }

    #[test]
    fn glob_is_case_insensitive() {
        assert!(glob_match(b"*.RS", b"main.rs"));
        assert!(glob_match(b"README*", b"readme.md"));
    }

    #[test]
    fn quick_filter_is_case_insensitive_substring() {
        let spec = FilterSpec {
            show_hidden: true,
            quick_filter: Some("CONF".into()),
            mask: None,
        };
        assert!(spec.matches("app.config.toml", false, EntryKind::File));
        assert!(!spec.matches("readme.md", false, EntryKind::File));
    }

    #[test]
    fn hidden_dotfiles_excluded_unless_show_hidden() {
        let hide = FilterSpec::new();
        assert!(!hide.matches(".bashrc", true, EntryKind::File));
        assert!(hide.matches("visible.txt", false, EntryKind::File));

        let show = FilterSpec {
            show_hidden: true,
            ..FilterSpec::new()
        };
        assert!(show.matches(".bashrc", true, EntryKind::File));
    }

    #[test]
    fn directories_pass_quick_filter_and_mask_regardless() {
        let spec = FilterSpec {
            show_hidden: true,
            quick_filter: Some("zzz".into()),
            mask: Some(Mask::new("*.rs")),
        };
        assert!(spec.matches("src", false, EntryKind::Directory));
        assert!(!spec.matches("main.py", false, EntryKind::File));
    }

    #[test]
    fn saved_filters_roundtrip_by_name() {
        let mut saved = SavedFilters::new();
        saved.save(
            "rust-sources",
            FilterSpec {
                show_hidden: false,
                quick_filter: None,
                mask: Some(Mask::new("*.rs")),
            },
        );
        assert_eq!(saved.len(), 1);
        assert!(saved.get("rust-sources").unwrap().mask.is_some());
        assert!(saved.get("missing").is_none());

        // Saving again under the same name replaces, not duplicates.
        saved.save("rust-sources", FilterSpec::new());
        assert_eq!(saved.len(), 1);
        assert!(saved.get("rust-sources").unwrap().mask.is_none());

        assert!(saved.remove("rust-sources"));
        assert!(saved.is_empty());
        assert!(!saved.remove("rust-sources"));
    }
}
