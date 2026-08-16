// SPDX-License-Identifier: MIT
//! FileSystem trait, registry, mount table, path resolution.
//!
//! T-2.2.2 (design.md §9.1): the `FileSystem` trait plus its supporting
//! types (`ListOpts`/`ListFields`, `WriteOpts`/`Mode`, `DirEntry`,
//! `RemoveKind`, `RenameFlags`, `ChangeEvent`, `CopyOutcome`,
//! `AsyncReadSeek`, `AsyncWriteCommit`) and a trivial `NullFs`
//! implementation proving the trait shape is usable. The mount table and
//! path-resolution logic that consumes this trait land in later Phase 2/6
//! tasks (see documentation/task.md).

mod copy;
mod dir_entry;
mod fs;
mod io;
mod list_opts;
mod null;
mod ops;
mod volume_stats;
mod watch;
mod write_opts;

pub mod archive;
pub mod local;
pub mod remote;

pub use copy::CopyOutcome;
pub use dir_entry::DirEntry;
pub use fs::FileSystem;
pub use io::{AsyncReadSeek, AsyncWriteCommit, CommitOutcome};
pub use list_opts::{ListFields, ListOpts};
pub use local::LocalFs;
pub use null::NullFs;
pub use ops::{RemoveKind, RenameFlags};
pub use volume_stats::VolumeStats;
pub use watch::ChangeEvent;
pub use write_opts::{Mode, WriteOpts};
