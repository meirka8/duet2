// SPDX-License-Identifier: MIT
//! Directory model: listing, watching, sorting, filtering, caching.
//!
//! T-2.5.1 (design.md §9.2) built the panel model's *shapes* --
//! [`EntryStore`]'s struct-of-arrays layout, [`DirectoryModel`], and the
//! [`DirEntryDiff`]/[`DirDiffBatch`] event protocol to the UI -- as
//! `todo!()`-bodied skeletons. Phase 3 (WBS 3.2, T-3.2.1 through T-3.2.8)
//! fills those in with real algorithms: verified zero-allocation SoA
//! storage, locale-aware natural-numeric sorting, composable
//! filtering, `EntryId`-stable selection, `notify`-based watching with a
//! polling fallback, minimal-diff computation, and a cancellable
//! directory-size service. See each submodule's doc comment for its
//! task's specifics.
//!
//! Unlike T-2.5.1's Phase 2 state, this crate *does* now touch the
//! filesystem directly (watching and directory-size computation are
//! inherently I/O -- design.md §9.2 assigns both to `duet-index`, not to
//! `duet-vfs`, since they're panel-model concerns, not VFS-backend ones).
//! What it still does not touch, per ADR-002, is any UI framework: no
//! crate here may depend on `gpui`/`gpui-component`
//! (`scripts/check-gpui-isolation.sh` enforces this in CI), which is what
//! keeps the model testable headless and reusable from `duet-ui` without a
//! window.

#[cfg(test)]
mod alloc_track;
mod diff;
mod entry_store;
mod model;
mod name_arena;
mod sort;

pub use diff::{DirDiffBatch, DirEntryDiff};
pub use entry_store::{EntryFlags, EntryStore};
pub use model::DirectoryModel;
pub use name_arena::{NameArena, NameSpan};
pub use sort::{SortColumn, SortOptions, Sorter};

/// Installed only for the crate's own unit-test binary (`cargo test -p
/// duet-index`), never for normal builds -- see `alloc_track`'s doc
/// comment. T-3.2.1's AC ("verified with a counting allocator") needs this
/// wired at the binary root so `entry_store::tests` can gate tracking
/// around just the measured phase.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: alloc_track::CountingAllocator = alloc_track::CountingAllocator;
