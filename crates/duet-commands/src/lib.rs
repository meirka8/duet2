// SPDX-License-Identifier: MIT
//! Command registry, keymap parsing/resolution, and the command palette
//! index (design.md §8.1, §9.4).
//!
//! # T-2.4.1 scope
//!
//! This is a Phase 2 task: "design the command registry, keymap resolution,
//! and context predicate evaluator" as **interfaces and types that
//! compile**, not full runtime behaviour. Concretely:
//!
//! - [`Command`] / [`CommandRegistry`]: the `{ id, title, category,
//!   args_schema, precondition, handler }` shape from §9.4, and the table
//!   every command (built-in or plugin-provided) registers into.
//! - [`predicate`]: the context-predicate grammar and evaluator --
//!   `panel && selection.nonempty`, `vfs.scheme == 'file'`, etc. -- with a
//!   hand-rolled recursive-descent parser and a full test corpus (this
//!   module's `#[cfg(test)]` blocks; see [`predicate::parse`]'s doc comment
//!   for the grammar itself).
//! - [`keymap`]: `Binding`/`Unbind`/`KeymapFile` types matching
//!   `docs/config-schema.md` §2's `keymap.toml` schema exactly (they
//!   `#[derive(serde::Deserialize)]` straight from TOML), chord support
//!   (`"ctrl-k ctrl-b"`), base-keymap layering, and load-time conflict
//!   detection ([`keymap::resolve`]).
//! - [`palette`]: the [`palette::PaletteIndex`] data structure FR-TOOL-11's
//!   command palette queries, with fuzzy search via `nucleo-matcher` (the
//!   same crate family design.md §9.2 already settles on for FR-NAV-13's
//!   quick-search, kept consistent rather than picking a second fuzzy
//!   matcher).
//!
//! Command *bodies* (real `CommandHandler` closures wired to
//! `duet-ops`/`duet-index`/etc.) and the full 307-command catalogue
//! registration are T-3.3.2's job ("Command registry + keymap parser +
//! context predicate evaluator", Phase 3, AC: "200 commands register; the
//! TC keymap loads; binding conflicts produce diagnostics with file/line").
//! This crate is what T-3.3.2 builds on top of.
//!
//! # ADR-002: no GPUI dependency
//!
//! §9.4 says "GPUI's own action-dispatch context system is used as the
//! substrate" for contexts. This crate does **not** depend on `gpui`
//! (verified by `../../scripts/check-gpui-isolation.sh`, which only
//! `crates/duet-ui` and `crates/duet-widgets` may fail). See
//! [`predicate::ContextState`] for the seam: `duet-ui` is expected to
//! implement it over GPUI's real focus/context stack, so this crate's
//! predicate evaluator can be driven by *any* UI framework, including none
//! (a headless test harness, or a future CLI).
//!
//! # `duet-types` note (resolved at G2 consolidation)
//!
//! This crate was built in parallel with T-2.2.1 (`duet-types`), before
//! `duet_types::{ErrorKind, VfsError}` existed, so [`CommandIdError`],
//! [`CommandError`], [`DuplicateCommandError`], [`keymap::KeyParseError`],
//! and [`PredicateParseError`] were left as local types with a "replace
//! once T-2.2.1 lands" TODO. Checked at G2 consolidation, now that both
//! exist side by side: `duet_types::ErrorKind`/`VfsError` classify
//! *filesystem/VFS* failures (`Retryable`/`Permission`/`Space`/`NotFound`/
//! `Conflict`/`Fatal`, each carrying a `VPath`) — a different problem
//! domain from parse errors and duplicate-registration errors, which have
//! no filesystem path or retry semantics to speak of. These types
//! **correctly stay local**; the TODO was itself the byproduct of `duet-
//! types` being empty at the time, not a real shared-type gap. No
//! reconciliation needed.

mod command;
mod id;
pub mod keymap;
pub mod palette;
pub mod predicate;
mod registry;

pub use command::{
    ArgField, ArgKind, ArgValue, ArgsSchema, Command, CommandArgs, CommandCategory, CommandContext,
    CommandError, CommandHandler, CommandResult,
};
pub use id::{CommandId, CommandIdError};
pub use predicate::{
    CompareOp, ContextState, ContextTerm, ContextValue, Literal, MapContextState, Predicate,
    PredicateParseError,
};
pub use registry::{CommandRegistry, DuplicateCommandError};
