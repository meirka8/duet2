// SPDX-License-Identifier: MIT
//! `duet-types`: `VPath`, `MountId`, `EntryId`, `Metadata`, `Caps`,
//! `MetaPatch`, and the error taxonomy.
//!
//! This is the foundation crate every other Duet crate builds on
//! (design.md §8.1): "no deps beyond std/serde" in spirit — `bitflags` and
//! `thiserror` are the two narrow exceptions, both anticipated by
//! design.md's own §9.1/§9.3 sketches (`bitflags!` for [`Caps`],
//! ergonomic error variants for the taxonomy) and both zero-transitive-
//! weight, compile-time-only proc-macro crates with no runtime footprint.
//!
//! Nothing in this crate touches the filesystem, spawns a thread, or does
//! I/O — it is pure data and parsing, safe to use from any layer
//! (including `duet-ui`, per ADR-002's UI-framework-agnostic core).

mod caps;
mod entry;
mod error;
mod meta;
mod mount;
mod path;
mod pct;

pub use caps::Caps;
pub use entry::EntryId;
pub use error::{ErrorKind, Result, VfsError, errno};
pub use meta::{EntryKind, MetaPatch, Metadata, Timestamp};
pub use mount::{Authority, MountId, Scheme};
pub use path::{PathParseError, UnixPathBuf, VPath};
