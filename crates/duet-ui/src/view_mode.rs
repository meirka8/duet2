// SPDX-License-Identifier: MIT
//! T-4.2.5: the file table's view modes (FR-NAV-04) -- the pure half.
//!
//! [`ViewMode`] names the four modes and their `settings.toml` /
//! `session.json` keys. The two geometry types do the arithmetic the
//! Brief and Thumbnails views need every frame and on every cursor move,
//! kept free of GPUI so the column/row mapping, paging and
//! scroll-to-reveal rules are unit-tested with plain numbers:
//!
//! - **Brief** ([`BriefGeometry`]) lays names out top-to-bottom, then
//!   column by column, the way Total Commander does: with `rows` rows
//!   fitting the viewport, display index `d` sits in column `d / rows`,
//!   row `d % rows`, and the whole listing scrolls horizontally.
//! - **Thumbnails** ([`GridGeometry`]) lays cells out left-to-right,
//!   then row by row: index `d` sits in row `d / columns`, and the grid
//!   scrolls vertically like the table does.
//!
//! Full is the table itself (`file_table.rs`); Tree is `tree_view.rs`.

use std::ops::Range;

/// A tab's view mode. `Full`/`Brief`/`Thumbnails` are *list* modes: they
/// render the same `FileTableDelegate` (same cursor, selection,
/// quick-search, icons) and only differ in layout; `Tree` swaps the
/// listing for a directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ViewMode {
    #[default]
    Full,
    Brief,
    Thumbnails,
    Tree,
}

impl ViewMode {
    pub(crate) const ALL: [ViewMode; 4] = [
        ViewMode::Full,
        ViewMode::Brief,
        ViewMode::Thumbnails,
        ViewMode::Tree,
    ];

    /// The `settings.toml` (`panels.default_view`) / `session.json` key.
    pub(crate) fn key(self) -> &'static str {
        match self {
            ViewMode::Full => "full",
            ViewMode::Brief => "brief",
            ViewMode::Thumbnails => "thumbnails",
            ViewMode::Tree => "tree",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == key)
    }

    /// Lenient parse for a settings/session value: unknown -> `Full`.
    pub(crate) fn from_settings_str(key: &str) -> Self {
        Self::from_key(key.trim()).unwrap_or_default()
    }

    /// Whether the mode renders the listing (as opposed to the tree).
    pub(crate) fn is_list(self) -> bool {
        self != ViewMode::Tree
    }
}

/// Brief view: one row of names is this tall.
pub(crate) const BRIEF_ROW_HEIGHT: f32 = 24.0;
/// Brief view: every column is this wide (names truncate inside it).
pub(crate) const BRIEF_COLUMN_WIDTH: f32 = 240.0;

/// Thumbnails view: cell size and the icon drawn in it.
pub(crate) const THUMB_CELL_WIDTH: f32 = 120.0;
pub(crate) const THUMB_CELL_HEIGHT: f32 = 112.0;
pub(crate) const THUMB_ICON_PX: f32 = 64.0;

/// Brief layout for `item_count` display rows in a viewport -- see the
/// module doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BriefGeometry {
    /// Rows per column: as many as fit the viewport height, at least 1.
    pub(crate) rows: usize,
    /// Total columns needed for every item.
    pub(crate) columns: usize,
}

impl BriefGeometry {
    pub(crate) fn new(item_count: usize, viewport_height: f32) -> Self {
        let rows = ((viewport_height / BRIEF_ROW_HEIGHT).floor() as usize).max(1);
        Self {
            rows,
            columns: item_count.div_ceil(rows),
        }
    }

    pub(crate) fn column_of(&self, display_ix: usize) -> usize {
        display_ix / self.rows
    }

    /// Display index at (`column`, `row`), unchecked against the count.
    pub(crate) fn index_at(&self, column: usize, row: usize) -> usize {
        column * self.rows + row
    }

    /// Total content width in px.
    pub(crate) fn content_width(&self) -> f32 {
        self.columns as f32 * BRIEF_COLUMN_WIDTH
    }

    /// The columns at least partly inside a viewport `viewport_width`
    /// wide whose content is scrolled by `offset_x` (`<= 0`, GPUI's
    /// convention: more negative = scrolled further right). Clamped to
    /// the real column range; one extra column on the right so a
    /// partially visible column is drawn.
    pub(crate) fn visible_columns(&self, offset_x: f32, viewport_width: f32) -> Range<usize> {
        let scrolled = (-offset_x).max(0.0);
        let first = (scrolled / BRIEF_COLUMN_WIDTH).floor() as usize;
        let last = ((scrolled + viewport_width) / BRIEF_COLUMN_WIDTH).ceil() as usize + 1;
        first.min(self.columns)..last.min(self.columns)
    }

    /// The scroll offset that brings `display_ix`'s column fully into a
    /// `viewport_width`-wide viewport with the least movement from
    /// `offset_x`, or `None` when it already is.
    pub(crate) fn offset_to_reveal(
        &self,
        offset_x: f32,
        viewport_width: f32,
        display_ix: usize,
    ) -> Option<f32> {
        let column = self.column_of(display_ix) as f32;
        let left = column * BRIEF_COLUMN_WIDTH;
        let right = left + BRIEF_COLUMN_WIDTH;
        let scrolled = (-offset_x).max(0.0);
        if left < scrolled {
            Some(-left)
        } else if right > scrolled + viewport_width && viewport_width >= BRIEF_COLUMN_WIDTH {
            Some(-(right - viewport_width))
        } else {
            None
        }
    }

    /// How many items one PageUp/PageDown moves: a viewport's worth of
    /// whole columns.
    pub(crate) fn page(&self, viewport_width: f32) -> usize {
        let columns = ((viewport_width / BRIEF_COLUMN_WIDTH).floor() as usize).max(1);
        self.rows * columns
    }
}

/// Thumbnails layout for `item_count` display rows in a viewport -- see
/// the module doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GridGeometry {
    /// Cells per row: as many as fit the viewport width, at least 1.
    pub(crate) columns: usize,
    /// Total rows needed for every item.
    pub(crate) rows: usize,
}

impl GridGeometry {
    pub(crate) fn new(item_count: usize, viewport_width: f32) -> Self {
        let columns = ((viewport_width / THUMB_CELL_WIDTH).floor() as usize).max(1);
        Self {
            columns,
            rows: item_count.div_ceil(columns),
        }
    }

    pub(crate) fn row_of(&self, display_ix: usize) -> usize {
        display_ix / self.columns
    }

    /// How many items one PageUp/PageDown moves: a viewport's worth of
    /// whole rows.
    pub(crate) fn page(&self, viewport_height: f32) -> usize {
        let rows = ((viewport_height / THUMB_CELL_HEIGHT).floor() as usize).max(1);
        self.columns * rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_unknown_falls_back_to_full() {
        for mode in ViewMode::ALL {
            assert_eq!(ViewMode::from_key(mode.key()), Some(mode));
            assert_eq!(ViewMode::from_settings_str(mode.key()), mode);
        }
        assert_eq!(ViewMode::from_key("wide"), None);
        assert_eq!(ViewMode::from_settings_str(" bogus "), ViewMode::Full);
        assert!(ViewMode::Brief.is_list());
        assert!(!ViewMode::Tree.is_list());
    }

    #[test]
    fn brief_geometry_maps_indices_top_to_bottom_then_across() {
        // 100 px tall -> 4 rows of 24 px; 10 items -> 3 columns.
        let g = BriefGeometry::new(10, 100.0);
        assert_eq!((g.rows, g.columns), (4, 3));
        assert_eq!(g.column_of(0), 0);
        assert_eq!(g.column_of(3), 0);
        assert_eq!(g.column_of(4), 1);
        assert_eq!(g.column_of(9), 2);
        assert_eq!(g.index_at(2, 1), 9);
        assert_eq!(g.content_width(), 3.0 * BRIEF_COLUMN_WIDTH);
        // Too short for even one row still gives one.
        assert_eq!(BriefGeometry::new(5, 10.0).rows, 1);
        assert_eq!(BriefGeometry::new(0, 100.0).columns, 0);
    }

    #[test]
    fn brief_visible_columns_and_reveal_offsets_follow_the_scroll() {
        let g = BriefGeometry::new(40, 100.0); // 4 rows, 10 columns
        // Viewport 500 px wide at the left edge: columns 0..3 (+1 spare).
        assert_eq!(g.visible_columns(0.0, 500.0), 0..4);
        // Scrolled one column: 1..5.
        assert_eq!(g.visible_columns(-BRIEF_COLUMN_WIDTH, 500.0), 1..5);
        // Far right: clamped to the real range.
        assert_eq!(g.visible_columns(-10_000.0, 500.0), 10..10);

        // Item 0 is visible at offset 0: nothing to do.
        assert_eq!(g.offset_to_reveal(0.0, 500.0, 0), None);
        // Item in column 3 (x 720..960) is off the right edge of a
        // 500 px viewport: scroll so its right edge lands on the edge.
        assert_eq!(g.offset_to_reveal(0.0, 500.0, 13), Some(-(960.0 - 500.0)));
        // Now column 0 is off the left: scroll back to its left edge.
        assert_eq!(g.offset_to_reveal(-460.0, 500.0, 2), Some(0.0));
        // Already inside: none.
        assert_eq!(g.offset_to_reveal(-460.0, 500.0, 9), None);
        assert_eq!(g.page(500.0), 8, "2 whole columns x 4 rows");
        assert_eq!(g.page(10.0), 4, "at least one column");
    }

    #[test]
    fn grid_geometry_maps_indices_across_then_down() {
        let g = GridGeometry::new(10, 500.0); // 4 columns of 120 px
        assert_eq!((g.columns, g.rows), (4, 3));
        assert_eq!(g.row_of(3), 0);
        assert_eq!(g.row_of(4), 1);
        assert_eq!(g.row_of(9), 2);
        assert_eq!(GridGeometry::new(10, 50.0).columns, 1);
        assert_eq!(GridGeometry::new(0, 500.0).rows, 0);
        assert_eq!(g.page(300.0), 8, "2 whole rows x 4 columns");
    }
}
