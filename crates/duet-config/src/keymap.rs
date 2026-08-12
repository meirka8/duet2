// SPDX-License-Identifier: MIT
//! `keymap.toml` / `keymaps/*.toml` loading (`docs/config-schema.md` §2).
//!
//! The typed binding schema (`Binding`, `Unbind`, `KeymapFile`, base-layer
//! resolution and conflict detection) already lives in `duet-commands`
//! (T-2.4.1), built directly against a `text: &str` via plain `toml`. This
//! module intentionally does not duplicate that: it only supplies the
//! round-trip/versioning/hot-reload *file* layer this task owns, generic
//! over whatever typed shape a caller wants to project the document into
//! (in practice, `duet_commands::keymap::KeymapFile`).
//!
//! Deliberately thin per T-3.3.1's brief ("keymap.toml/connections.toml/
//! theme loading can follow the same pattern but don't need to be as fully
//! fleshed out ... prioritize settings.toml").

use crate::document::ConfigFile;
use crate::error::Result;
use crate::migrate::MigrationRegistry;

/// Current schema version for `keymap.toml` and the shipped base files,
/// per `docs/config-schema.md`'s version table.
pub const KEYMAP_SCHEMA_VERSION: u32 = 1;

/// A loaded `keymap.toml` (or base keymap file), generic over the caller's
/// typed binding-set shape `T`.
pub type KeymapDocument<T> = ConfigFile<T>;

/// Loads a keymap file from `path`, migrating it to
/// [`KEYMAP_SCHEMA_VERSION`] if needed (backup written first). `T` is
/// whatever the caller deserializes bindings into (e.g. `duet_commands::
/// keymap::KeymapFile`); this crate does not depend on `duet-commands`, so
/// it stays a type parameter rather than a concrete type here.
pub fn load<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<KeymapDocument<T>> {
    KeymapDocument::load(
        path,
        &MigrationRegistry::generic_v0_to_v1(),
        KEYMAP_SCHEMA_VERSION,
    )
}
