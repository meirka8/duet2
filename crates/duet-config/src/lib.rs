// SPDX-License-Identifier: MIT
//! Settings, themes, hotlist, persistence, hot reload.
//!
//! # T-3.3.1 scope
//!
//! TOML config loading against the schema `docs/config-schema.md` already
//! fully specifies (Phase 1, T-1.6.1) for `settings.toml`, `keymap.toml`,
//! `connections.toml`, and `themes/*.toml`:
//!
//! - **Round-trip preservation** ([`document`]): every file is loaded as a
//!   [`toml_edit::DocumentMut`], never a plain `serde`-deserialized struct,
//!   so comments, formatting, and keys this Duet version doesn't recognize
//!   (a newer config from a future version, or a user's own addition)
//!   survive being loaded and re-saved unchanged. Typed structs (like
//!   [`settings::Settings`]) are read-only projections of the live
//!   document, never the thing that gets re-serialized.
//! - **Schema versioning + migration** ([`migrate`]): every file carries a
//!   `schema_version` integer; [`migrate::MigrationRegistry`] chains
//!   [`migrate::Migration`] steps to walk a document up to the current
//!   version, writing a timestamped backup of the pre-migration bytes
//!   first. Only v0 -> v1 exists today (matching
//!   `docs/config-schema.md`'s version table, all four kinds at v1), but
//!   the registry is a list, not a single hardcoded function, so a future
//!   v1 -> v2 step is additive.
//! - **Hot reload** ([`watch`]): `notify`-based watching of a config
//!   file's parent directory, ~50 ms debounced, entirely on a dedicated
//!   background thread -- never the UI thread (T-3.1.6's blocking guard).
//!   A malformed external edit reports an error without discarding the
//!   caller's last-known-good value.
//!
//! `settings.toml` ([`settings`]) is the fully fleshed-out file kind, per
//! the task brief's priority (it's what the AC tests exercise).
//! `keymap.toml` ([`keymap`]), `connections.toml` ([`connections`]), and
//! `themes/*.toml` ([`theme`]) get the same generic file-loading layer
//! ([`document::ConfigFile`]) but no dedicated typed schema struct here --
//! `keymap.toml`'s typed shape already lives in `duet-commands` (T-2.4.1),
//! and `connections.toml`/theme tokens are left for the tasks that first
//! consume them (T-7.1.x, T-9.1.x) to model precisely.
//!
//! # Not UI-thread-safe by design
//!
//! [`document::ConfigFile::load`] and [`document::ConfigFile::save`]
//! perform blocking filesystem I/O and must only be called from the I/O
//! runtime or a synchronous startup path before the UI is running --
//! exactly the same rule `duet-vfs`/`duet-index` apply (§8.2, T-3.1.6).
//! [`watch::watch`] is the UI-safe way to react to changes after startup:
//! it does its own I/O entirely off-thread and calls back with an already
//! -deserialized value.

/// `connections.toml` loading (remote backend profiles).
pub mod connections;
mod document;
mod error;
mod io;
/// `keymap.toml` / `keymaps/*.toml` loading.
pub mod keymap;
/// Schema-versioned migration registry.
pub mod migrate;
/// XDG config path resolution.
pub mod paths;
/// `settings.toml` loading: the fully typed, primary config file.
pub mod settings;
/// `themes/*.toml` loading (color/spacing token files).
pub mod theme;
mod watch;

pub use document::ConfigFile;
pub use error::{ConfigError, Result};
pub use migrate::{Migration, MigrationRegistry};
pub use settings::{Settings, SettingsFile};
pub use theme::{ThemeDocument, ThemeTokensDocument};
pub use watch::{ConfigWatcher, watch};
