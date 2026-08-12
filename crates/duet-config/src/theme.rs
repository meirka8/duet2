// SPDX-License-Identifier: MIT
//! `themes/*.toml` loading (`docs/config-schema.md` §4): the color/spacing
//! token set. Deliberately thin per T-3.3.1's brief; a full typed `Theme`
//! struct mirroring every documented token is left for T-9.1.x (the
//! settings/theme UI) or whichever task first renders against these
//! values, so it can be modeled alongside the actual `gpui` color types it
//! feeds. This module supplies the same round-trip / versioning /
//! hot-reload file layer as [`crate::settings`], generic over the caller's
//! typed shape.

use crate::document::ConfigFile;
use crate::error::Result;
use crate::migrate::MigrationRegistry;

/// Current schema version for `themes/*.toml`, per
/// `docs/config-schema.md`'s version table.
pub const THEME_SCHEMA_VERSION: u32 = 1;

/// A loaded theme file, generic over the caller's typed token-set shape
/// `T`.
pub type ThemeDocument<T> = ConfigFile<T>;

/// Loads a theme file from `path`, migrating it to [`THEME_SCHEMA_VERSION`]
/// if needed (backup written first).
pub fn load<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<ThemeDocument<T>> {
    ThemeDocument::load(
        path,
        &MigrationRegistry::generic_v0_to_v1(),
        THEME_SCHEMA_VERSION,
    )
}
