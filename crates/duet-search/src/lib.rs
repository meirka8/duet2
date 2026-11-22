// SPDX-License-Identifier: MIT
//! Name + content search, mask/regex compiler, result streaming
//! (T-5.3.7, FR-TOOL-04, design.md §9.7).
//!
//! - [`mask`]: Total Commander's file-name mask syntax (`*.c;*.h`,
//!   `report`, `*.* | *.bak .git/`), compiled to byte regexes.
//! - [`content`]: "contains text" / "does not contain", literal or regex,
//!   case folding, whole words, UTF-8/UTF-16/legacy code pages, an
//!   optional binary-file skip.
//! - [`spec`]: the request as plain data ([`SearchSpec`]), the parsers
//!   for the dialog's size and date fields, and compilation.
//! - [`walk`]: the parallel breadth-first traversal that applies all of
//!   it and streams [`SearchEvent`]s to a sink.
//!
//! Like `duet-index`, this crate reads the local filesystem directly
//! (design.md §9.7 gives it the traversal), on threads of its own -- it
//! never runs on the UI thread, and it depends on no UI crate (ADR-002).
//! Searching through other VFS backends (archives, T-6.1.11; remote
//! filesystems) is later work.

pub mod content;
pub mod mask;
pub mod spec;
pub mod walk;

pub use content::{ContentEncodings, ContentError, ContentMatcher, ContentSpec, FileVerdict};
pub use mask::{MaskError, NameMask};
pub use spec::{
    CompiledSearch, KindFilter, NameMatcher, SearchSpec, SpecError, parse_roots, parse_size,
    parse_time,
};
pub use walk::{
    PROGRESS_INTERVAL, SearchEvent, SearchHandle, SearchHit, SearchProgress, SearchSink,
    metadata_from_std, relative_name, run, start,
};
