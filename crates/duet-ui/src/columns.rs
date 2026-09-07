// SPDX-License-Identifier: MIT
//! T-4.2.4: column configuration for the file table (FR-NAV-05/06).
//!
//! The *model* half of the feature, kept free of any GPUI element code so
//! every rule is unit-testable with plain values:
//!
//! - [`ColumnKind`] is the catalogue of columns the table knows how to
//!   render and sort by (Name, Ext, Size, Modified, Attr -- TC's own Full
//!   view set; plugin-provided columns are T-8.1.4's extension point).
//! - [`ColumnLayout`] is an ordered list of columns with widths: what a
//!   view mode shows, in what order, how wide. It knows how to add/remove
//!   a column, move one (the drag-reorder the table widget reports), take
//!   drag-resized widths, and compute its responsive widths for a given
//!   panel width. It converts to and from `duet_config::ColumnLayout`,
//!   the `[panels.layouts.<view>]` section of `settings.toml` that makes
//!   the layout survive a restart.
//! - [`ColumnLayoutStore`] is the one shared, observable copy of the
//!   layout every `FileTable` in both panels renders from: a change made
//!   in any tab's header (toggle, drag-move, drag-resize) is written
//!   here, every table observes it and re-derives its columns, and the
//!   workspace observes it to persist -- one source of truth, no per-tab
//!   drift, exactly like the splitter ratio.
//!
//! Two deliberate rules worth knowing:
//!
//! - **Name is mandatory and elastic.** It can't be removed (a listing
//!   without names is not a file manager) and its width is never stored:
//!   it takes whatever the fixed-width columns leave over, down to
//!   [`NAME_MIN`], so the table always fills the panel exactly and the
//!   user only ever resizes the *other* columns. This is the same
//!   responsive rule the table had before T-4.2.4, generalised from a
//!   fixed three-column array to any layout.
//! - **The default layout is unchanged from before this task**
//!   (Name / Size / Modified), not TC's five-column Full view: Ext and
//!   Attr are one right-click away in the header, and nobody's panel
//!   changes shape on upgrade. The Ext column shows the extension
//!   alongside a full Name (Double Commander's convention) rather than
//!   TC's split stem/extension pair -- the Name cell is what quick-search
//!   highlights index into, and it stays the full name for that reason.

use duet_index::SortColumn;
use duet_types::EntryKind;
use gpui::Context;

/// The narrowest the elastic Name column is ever squeezed to before the
/// table falls back to horizontal scrolling.
pub(crate) const NAME_MIN: f32 = 60.0;

/// The Name column's width when there is nothing to be elastic against
/// (a first frame before the panel has been measured).
pub(crate) const NAME_IDEAL: f32 = 360.0;

/// The narrowest any fixed-width column can be drag-resized to. Below
/// this a column is unreadable, and the widget's own 10 px floor would
/// let a user lose a column by accident.
pub(crate) const FIXED_MIN: f32 = 40.0;

/// The view mode whose layout T-4.2.4 configures. `settings.toml` keys
/// layouts by view name so T-4.2.5's Brief/Thumbnails/Tree modes can add
/// their own without a schema change.
pub(crate) const FULL_VIEW: &str = "full";

/// Every column the file table can render and sort by, in catalogue
/// order (the order the header menu lists them, and the order a newly
/// added column slots into an existing layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ColumnKind {
    Name,
    Extension,
    Size,
    Modified,
    Attributes,
}

impl ColumnKind {
    pub(crate) const ALL: [ColumnKind; 5] = [
        ColumnKind::Name,
        ColumnKind::Extension,
        ColumnKind::Size,
        ColumnKind::Modified,
        ColumnKind::Attributes,
    ];

    /// The stable `settings.toml` key (`docs/config-schema.md`,
    /// `panels.layouts.<view>.columns[].key`).
    pub(crate) fn key(self) -> &'static str {
        match self {
            ColumnKind::Name => "name",
            ColumnKind::Extension => "ext",
            ColumnKind::Size => "size",
            ColumnKind::Modified => "modified",
            ColumnKind::Attributes => "attrs",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// Header text.
    pub(crate) fn title(self) -> &'static str {
        match self {
            ColumnKind::Name => "Name",
            ColumnKind::Extension => "Ext",
            ColumnKind::Size => "Size",
            ColumnKind::Modified => "Modified",
            ColumnKind::Attributes => "Attr",
        }
    }

    /// Width (logical px) a column gets when the layout doesn't say.
    pub(crate) fn default_width(self) -> f32 {
        match self {
            ColumnKind::Name => NAME_IDEAL,
            ColumnKind::Extension => 70.0,
            ColumnKind::Size => 110.0,
            ColumnKind::Modified => 170.0,
            ColumnKind::Attributes => 110.0,
        }
    }

    pub(crate) fn sort_column(self) -> SortColumn {
        match self {
            ColumnKind::Name => SortColumn::Name,
            ColumnKind::Extension => SortColumn::Extension,
            ColumnKind::Size => SortColumn::Size,
            ColumnKind::Modified => SortColumn::Modified,
            ColumnKind::Attributes => SortColumn::Attributes,
        }
    }

    /// Numeric-ish columns read better right-aligned (a size string, a
    /// date); text columns stay left.
    pub(crate) fn right_aligned(self) -> bool {
        matches!(self, ColumnKind::Size | ColumnKind::Modified)
    }

    /// Everything but Name can be hidden -- see the module doc comment.
    pub(crate) fn removable(self) -> bool {
        self != ColumnKind::Name
    }

    fn catalogue_index(self) -> usize {
        Self::ALL
            .iter()
            .position(|k| *k == self)
            .expect("every ColumnKind is in ALL")
    }
}

/// One column of a [`ColumnLayout`]. `width` is the stored, user-chosen
/// width for a fixed-width column; for `Name` it is only the fallback
/// used before the panel has been measured (see the module doc comment).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ColumnEntry {
    pub(crate) kind: ColumnKind,
    pub(crate) width: f32,
}

/// An ordered column set with widths -- see the module doc comment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnLayout {
    columns: Vec<ColumnEntry>,
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self::from_kinds([ColumnKind::Name, ColumnKind::Size, ColumnKind::Modified])
    }
}

impl ColumnLayout {
    /// A layout of `kinds` in that order, every column at its default
    /// width. Name is inserted at the front if missing; duplicates after
    /// the first occurrence are dropped.
    pub(crate) fn from_kinds(kinds: impl IntoIterator<Item = ColumnKind>) -> Self {
        let mut columns: Vec<ColumnEntry> = Vec::new();
        for kind in kinds {
            if columns.iter().all(|c| c.kind != kind) {
                columns.push(ColumnEntry {
                    kind,
                    width: kind.default_width(),
                });
            }
        }
        let mut layout = Self { columns };
        layout.ensure_name();
        layout
    }

    /// Reads a `[panels.layouts.<view>]` section. Unknown keys are
    /// skipped (a plugin column whose plugin is gone, a typo), duplicates
    /// collapse to the first, a stored width below [`FIXED_MIN`] is
    /// raised to it, and a missing Name is put back at the front. Returns
    /// `None` when nothing usable is left (an empty `columns = []`, or
    /// only unknown keys), so the caller falls back to the default rather
    /// than showing a bare Name column nobody asked for.
    pub(crate) fn from_config(config: &duet_config::ColumnLayout) -> Option<Self> {
        let mut columns: Vec<ColumnEntry> = Vec::new();
        for spec in &config.columns {
            let Some(kind) = ColumnKind::from_key(&spec.key) else {
                continue;
            };
            if columns.iter().any(|c| c.kind == kind) {
                continue;
            }
            let width = match spec.width {
                Some(w) if kind != ColumnKind::Name && w.is_finite() => w.max(FIXED_MIN),
                _ => kind.default_width(),
            };
            columns.push(ColumnEntry { kind, width });
        }
        if columns.is_empty() {
            return None;
        }
        let mut layout = Self { columns };
        layout.ensure_name();
        Some(layout)
    }

    /// The inverse of [`Self::from_config`]. Name's width is omitted: it
    /// is elastic and would only be a stale number in the file.
    pub(crate) fn to_config(&self) -> duet_config::ColumnLayout {
        duet_config::ColumnLayout {
            columns: self
                .columns
                .iter()
                .map(|c| duet_config::ColumnSpec {
                    key: c.kind.key().to_string(),
                    width: (c.kind != ColumnKind::Name).then_some(c.width),
                })
                .collect(),
        }
    }

    pub(crate) fn columns(&self) -> &[ColumnEntry] {
        &self.columns
    }

    pub(crate) fn kind_at(&self, col_ix: usize) -> Option<ColumnKind> {
        self.columns.get(col_ix).map(|c| c.kind)
    }

    pub(crate) fn index_of(&self, kind: ColumnKind) -> Option<usize> {
        self.columns.iter().position(|c| c.kind == kind)
    }

    pub(crate) fn contains(&self, kind: ColumnKind) -> bool {
        self.index_of(kind).is_some()
    }

    /// Shows `kind` if hidden, hides it if shown. A newly shown column
    /// slots in at its catalogue position relative to the columns already
    /// present (so re-adding Ext lands between Name and Size, not at the
    /// far right), at its default width. Name is never toggled. Returns
    /// whether anything changed.
    pub(crate) fn toggle(&mut self, kind: ColumnKind) -> bool {
        if !kind.removable() {
            return false;
        }
        if let Some(ix) = self.index_of(kind) {
            self.columns.remove(ix);
            return true;
        }
        let insert_at = self
            .columns
            .iter()
            .position(|c| c.kind.catalogue_index() > kind.catalogue_index())
            .unwrap_or(self.columns.len());
        self.columns.insert(
            insert_at,
            ColumnEntry {
                kind,
                width: kind.default_width(),
            },
        );
        true
    }

    /// Moves the column at `from` so it sits at index `to` -- the
    /// `TableDelegate::move_column` contract (remove, then insert at the
    /// target index). Returns whether anything changed.
    pub(crate) fn move_column(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.columns.len() || to >= self.columns.len() {
            return false;
        }
        let entry = self.columns.remove(from);
        self.columns.insert(to, entry);
        true
    }

    /// Takes the widths the table widget reports after a drag-resize
    /// (`TableEvent::ColumnWidthsChanged`, one per column in display
    /// order). Name's is ignored (elastic), the rest are clamped to
    /// [`FIXED_MIN`]. Returns whether any stored width changed by a
    /// pixel or more -- sub-pixel jitter from the drag must not trigger a
    /// settings write.
    pub(crate) fn set_widths(&mut self, widths: &[f32]) -> bool {
        let mut changed = false;
        for (entry, &width) in self.columns.iter_mut().zip(widths) {
            if entry.kind == ColumnKind::Name || !width.is_finite() {
                continue;
            }
            let width = width.max(FIXED_MIN);
            if (entry.width - width).abs() >= 1.0 {
                entry.width = width;
                changed = true;
            }
        }
        changed
    }

    /// The width of every fixed (non-Name) column added up.
    fn fixed_total(&self) -> f32 {
        self.columns
            .iter()
            .filter(|c| c.kind != ColumnKind::Name)
            .map(|c| c.width)
            .sum()
    }

    /// Column widths (display order) for `available` px of table width:
    /// every fixed column at its stored width, Name taking the rest down
    /// to [`NAME_MIN`]. Narrower than that, Name holds at `NAME_MIN` and
    /// the table's own horizontal scrollbar takes over ("impossibly
    /// narrow", not worth squeezing further).
    pub(crate) fn responsive_widths(&self, available: f32) -> Vec<f32> {
        let name = (available - self.fixed_total()).max(NAME_MIN);
        self.columns
            .iter()
            .map(|c| match c.kind {
                ColumnKind::Name => name,
                _ => c.width,
            })
            .collect()
    }

    fn ensure_name(&mut self) {
        if !self.contains(ColumnKind::Name) {
            self.columns.insert(
                0,
                ColumnEntry {
                    kind: ColumnKind::Name,
                    width: NAME_IDEAL,
                },
            );
        }
    }
}

/// The shared, observable layout every `FileTable` renders from -- see
/// the module doc comment. One per `Workspace`, handed to both `Panel`s
/// and from there to every tab. Keyed by view mode in `settings.toml`;
/// only [`FULL_VIEW`] exists until T-4.2.5.
pub(crate) struct ColumnLayoutStore {
    full: ColumnLayout,
}

impl ColumnLayoutStore {
    pub(crate) fn new(full: ColumnLayout) -> Self {
        Self { full }
    }

    pub(crate) fn full(&self) -> &ColumnLayout {
        &self.full
    }

    /// Replaces the Full-view layout and notifies observers (every table,
    /// and the workspace's persistence hook) -- only if it actually
    /// differs, so a no-op toggle or a sub-pixel resize never fans out a
    /// re-render or a settings write.
    pub(crate) fn set_full(&mut self, layout: ColumnLayout, cx: &mut Context<Self>) -> bool {
        if self.full == layout {
            return false;
        }
        self.full = layout;
        cx.notify();
        true
    }
}

/// Writes an `ls -l`-style attribute string (`drwxr-xr-x`, `-rw-r--r--`,
/// `lrwxrwxrwx`, setuid/setgid/sticky as `s`/`S`/`t`/`T`) for the Attr
/// column. The type character comes from the mode's own type bits when
/// present, else from `kind` (a backend that reports kinds but no mode
/// still gets a `d`/`-`/`l` prefix); with no mode at all the permission
/// part is left blank rather than shown as all-dashes, which would read
/// as a real "no permissions" file.
pub(crate) fn write_mode(out: &mut String, mode: Option<u32>, kind: EntryKind) {
    const S_IFMT: u32 = 0o170000;
    let type_char = match mode.map(|m| m & S_IFMT) {
        Some(0o040000) => 'd',
        Some(0o100000) => '-',
        Some(0o120000) => 'l',
        Some(0o010000) => 'p',
        Some(0o140000) => 's',
        Some(0o060000) => 'b',
        Some(0o020000) => 'c',
        _ => match kind {
            EntryKind::Directory => 'd',
            EntryKind::File => '-',
            EntryKind::Symlink => 'l',
            EntryKind::Fifo => 'p',
            EntryKind::Socket => 's',
            EntryKind::BlockDevice => 'b',
            EntryKind::CharDevice => 'c',
            EntryKind::Unknown => '?',
        },
    };
    out.push(type_char);
    let Some(mode) = mode else {
        return;
    };
    let bit = |mask: u32, ch: char| if mode & mask != 0 { ch } else { '-' };
    out.push(bit(0o400, 'r'));
    out.push(bit(0o200, 'w'));
    out.push(match (mode & 0o100 != 0, mode & 0o4000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });
    out.push(bit(0o040, 'r'));
    out.push(bit(0o020, 'w'));
    out.push(match (mode & 0o010 != 0, mode & 0o2000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });
    out.push(bit(0o004, 'r'));
    out.push(bit(0o002, 'w'));
    out.push(match (mode & 0o001 != 0, mode & 0o1000 != 0) {
        (true, true) => 't',
        (false, true) => 'T',
        (true, false) => 'x',
        (false, false) => '-',
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(layout: &ColumnLayout) -> Vec<ColumnKind> {
        layout.columns().iter().map(|c| c.kind).collect()
    }

    #[test]
    fn default_layout_is_the_pre_t424_three_columns() {
        assert_eq!(
            kinds(&ColumnLayout::default()),
            [ColumnKind::Name, ColumnKind::Size, ColumnKind::Modified]
        );
    }

    #[test]
    fn keys_round_trip_and_unknown_keys_are_rejected() {
        for kind in ColumnKind::ALL {
            assert_eq!(ColumnKind::from_key(kind.key()), Some(kind));
        }
        assert_eq!(ColumnKind::from_key("camera"), None);
        for (i, a) in ColumnKind::ALL.iter().enumerate() {
            for b in &ColumnKind::ALL[i + 1..] {
                assert_ne!(a.sort_column(), b.sort_column(), "one sort key per column");
            }
        }
    }

    #[test]
    fn from_config_skips_unknown_and_duplicate_keys_and_restores_a_missing_name() {
        let config = duet_config::ColumnLayout {
            columns: vec![
                duet_config::ColumnSpec {
                    key: "size".into(),
                    width: Some(90.0),
                },
                duet_config::ColumnSpec {
                    key: "camera".into(),
                    width: Some(200.0),
                },
                duet_config::ColumnSpec {
                    key: "size".into(),
                    width: Some(300.0),
                },
                duet_config::ColumnSpec {
                    key: "attrs".into(),
                    width: Some(5.0), // below FIXED_MIN
                },
            ],
        };
        let layout = ColumnLayout::from_config(&config).expect("usable");
        assert_eq!(
            kinds(&layout),
            [ColumnKind::Name, ColumnKind::Size, ColumnKind::Attributes]
        );
        assert_eq!(layout.columns()[1].width, 90.0, "first occurrence wins");
        assert_eq!(layout.columns()[2].width, FIXED_MIN, "clamped up");
    }

    #[test]
    fn from_config_with_nothing_usable_is_none_so_the_default_applies() {
        let empty = duet_config::ColumnLayout::default();
        assert!(ColumnLayout::from_config(&empty).is_none());
        let junk = duet_config::ColumnLayout {
            columns: vec![duet_config::ColumnSpec {
                key: "bogus".into(),
                width: None,
            }],
        };
        assert!(ColumnLayout::from_config(&junk).is_none());
    }

    #[test]
    fn to_config_omits_the_elastic_name_width_and_round_trips() {
        let mut layout = ColumnLayout::default();
        layout.toggle(ColumnKind::Extension);
        layout.set_widths(&[999.0, 55.0, 123.0, 171.0]);
        let config = layout.to_config();
        assert_eq!(config.columns[0].key, "name");
        assert_eq!(config.columns[0].width, None);
        assert_eq!(config.columns[1].key, "ext");
        assert_eq!(config.columns[1].width, Some(55.0));
        assert_eq!(ColumnLayout::from_config(&config), Some(layout));
    }

    #[test]
    fn toggle_hides_shows_in_catalogue_order_and_never_touches_name() {
        let mut layout = ColumnLayout::default();
        assert!(!layout.toggle(ColumnKind::Name));
        assert!(layout.contains(ColumnKind::Name));

        assert!(layout.toggle(ColumnKind::Size));
        assert_eq!(kinds(&layout), [ColumnKind::Name, ColumnKind::Modified]);

        // Re-adding lands at the catalogue position, not the end.
        assert!(layout.toggle(ColumnKind::Size));
        assert_eq!(
            kinds(&layout),
            [ColumnKind::Name, ColumnKind::Size, ColumnKind::Modified]
        );
        assert!(layout.toggle(ColumnKind::Attributes));
        assert!(layout.toggle(ColumnKind::Extension));
        assert_eq!(kinds(&layout), ColumnKind::ALL);
    }

    #[test]
    fn toggle_respects_a_user_reordering_when_slotting_a_column_back_in() {
        // User put Modified before Size; a re-added Ext goes right after
        // Name (the last column that precedes it in the catalogue).
        let mut layout =
            ColumnLayout::from_kinds([ColumnKind::Name, ColumnKind::Modified, ColumnKind::Size]);
        assert!(layout.toggle(ColumnKind::Extension));
        assert_eq!(
            kinds(&layout),
            [
                ColumnKind::Name,
                ColumnKind::Extension,
                ColumnKind::Modified,
                ColumnKind::Size
            ]
        );
    }

    #[test]
    fn move_column_follows_the_remove_then_insert_contract() {
        let mut layout = ColumnLayout::default();
        assert!(layout.move_column(2, 0));
        assert_eq!(
            kinds(&layout),
            [ColumnKind::Modified, ColumnKind::Name, ColumnKind::Size]
        );
        assert!(!layout.move_column(1, 1));
        assert!(!layout.move_column(0, 7));
        assert!(!layout.move_column(9, 0));
    }

    #[test]
    fn set_widths_ignores_name_clamps_and_reports_only_real_changes() {
        let mut layout = ColumnLayout::default();
        assert!(
            !layout.set_widths(&[500.0, 110.4, 170.0]),
            "sub-pixel jitter"
        );
        assert!(layout.set_widths(&[500.0, 90.0, 12.0]));
        assert_eq!(layout.columns()[0].width, NAME_IDEAL, "Name untouched");
        assert_eq!(layout.columns()[1].width, 90.0);
        assert_eq!(layout.columns()[2].width, FIXED_MIN);
        assert!(!layout.set_widths(&[1.0, f32::NAN, 40.0]));
    }

    #[test]
    fn responsive_widths_give_name_the_leftover_down_to_its_floor() {
        let layout = ColumnLayout::default();
        assert_eq!(layout.responsive_widths(1000.0), [720.0, 110.0, 170.0]);
        assert_eq!(layout.responsive_widths(340.0), [NAME_MIN, 110.0, 170.0]);
        assert_eq!(layout.responsive_widths(100.0), [NAME_MIN, 110.0, 170.0]);

        let mut wide = layout.clone();
        wide.toggle(ColumnKind::Attributes);
        assert_eq!(wide.responsive_widths(1000.0), [610.0, 110.0, 170.0, 110.0]);
    }

    #[test]
    fn write_mode_renders_ls_style_strings() {
        let render = |mode: Option<u32>, kind: EntryKind| {
            let mut s = String::new();
            write_mode(&mut s, mode, kind);
            s
        };
        assert_eq!(render(Some(0o100644), EntryKind::File), "-rw-r--r--");
        assert_eq!(render(Some(0o040755), EntryKind::Directory), "drwxr-xr-x");
        assert_eq!(render(Some(0o120777), EntryKind::Symlink), "lrwxrwxrwx");
        assert_eq!(render(Some(0o104755), EntryKind::File), "-rwsr-xr-x");
        assert_eq!(render(Some(0o102644), EntryKind::File), "-rw-r-Sr--");
        assert_eq!(render(Some(0o041777), EntryKind::Directory), "drwxrwxrwt");
        assert_eq!(render(Some(0o041776), EntryKind::Directory), "drwxrwxrwT");
        // Mode unknown: the kind still gives a type char, permissions
        // are blank rather than a misleading "---------".
        assert_eq!(render(None, EntryKind::Directory), "d");
        assert_eq!(render(None, EntryKind::Unknown), "?");
        // Type bits win over `kind` when both are present.
        assert_eq!(render(Some(0o040700), EntryKind::File), "drwx------");
    }
}
