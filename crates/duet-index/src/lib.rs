// SPDX-License-Identifier: MIT
//! Directory model: listing, watching, sorting, filtering, caching.
//!
//! T-2.5.1 (design.md §9.2): the panel model's *shapes* --
//! [`EntryStore`]'s struct-of-arrays layout, [`DirectoryModel`], and the
//! [`DirEntryDiff`]/[`DirDiffBatch`] event protocol to the UI. Genuine
//! runtime algorithms that later Phase 3 tasks own outright -- sorting
//! (T-3.2.2), filtering (T-3.2.3), watching (T-3.2.5/T-3.2.6), and the
//! actual diff computation (T-3.2.7) -- are `todo!()` here; the struct
//! layouts, byte budgets, and protocol enum variants are real and
//! compiling, not sketched in prose.
//!
//! Nothing in this crate touches the filesystem or a UI framework
//! (ADR-002): it operates purely on data handed to it (names + `Metadata`
//! from `duet-types`), which is what makes it testable headless and
//! reusable from `duet-ui` without a window.

mod diff;
mod entry_store;
mod model;
mod name_arena;

pub use diff::{DirDiffBatch, DirEntryDiff};
pub use entry_store::{EntryFlags, EntryStore};
pub use model::{DirectoryModel, SortColumn};
pub use name_arena::{NameArena, NameSpan};
