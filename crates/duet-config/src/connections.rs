// SPDX-License-Identifier: MIT
//! `connections.toml` loading (`docs/config-schema.md` §3): remote backend
//! profiles. **No secrets live in this file** -- see the module doc there;
//! `keyring_ref` is the only credential-adjacent field, and it is just a
//! lookup key into the Secret Service keyring, never a secret itself.
//!
//! Deliberately thin per T-3.3.1's brief; a full typed `Connection` struct
//! (with its per-backend `options` variants) is left for whichever task
//! first needs to construct VFS remote-backend sessions from this file
//! (T-7.1.x), so it can be modeled against the real backend trait shapes
//! rather than guessed here. This module supplies the same round-trip /
//! versioning / hot-reload file layer as [`crate::settings`] and
//! [`crate::keymap`], generic over the caller's typed shape.

use crate::document::ConfigFile;
use crate::error::Result;
use crate::migrate::MigrationRegistry;

/// Current schema version for `connections.toml`, per
/// `docs/config-schema.md`'s version table.
pub const CONNECTIONS_SCHEMA_VERSION: u32 = 1;

/// A loaded `connections.toml`, generic over the caller's typed connection
/// list shape `T`.
pub type ConnectionsDocument<T> = ConfigFile<T>;

/// Loads `connections.toml` from `path`, migrating it to
/// [`CONNECTIONS_SCHEMA_VERSION`] if needed (backup written first).
pub fn load<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> Result<ConnectionsDocument<T>> {
    ConnectionsDocument::load(
        path,
        &MigrationRegistry::generic_v0_to_v1(),
        CONNECTIONS_SCHEMA_VERSION,
    )
}
