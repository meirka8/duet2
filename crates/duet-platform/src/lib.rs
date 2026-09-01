// SPDX-License-Identifier: MIT
//! Clipboard, DnD, trash, mounts, D-Bus, polkit helper client, portals.
//!
//! Phase 2 (T-2.1.1) skeleton: this crate compiles as an empty workspace
//! member. Trait and type definitions land in their own dedicated Phase 2
//! tasks (see documentation/task.md) -- this file is intentionally close to
//! empty until then.
//!
//! T-5.3.1 is the first tenant: [`trash`], the full freedesktop trash-spec
//! implementation (design.md §9.10, FR-CFG-07) `duet_ops::deleter` calls
//! into to decide where a trashed file's content and `.trashinfo` sidecar
//! actually go. T-5.3.2 phase 1 adds the read side to the same module:
//! `trash::list_trash_entries` enumerates what's already there, for
//! `duet_ops::trash_restore`'s restore/purge planners.

pub mod trash;
