// SPDX-License-Identifier: MIT
//! Schema-versioned migrations for config documents.
//!
//! Per `docs/config-schema.md` ("Schema versioning and migration"):
//! migrations are forward-only, keyed on `(file_kind, schema_version)`, and
//! run synchronously on load before the document is handed to its consumer.
//! A timestamped backup of the pre-migration file is written before any
//! upgraded content is saved back.
//!
//! This module defines the mechanism -- a [`Migration`] trait and a
//! [`MigrationRegistry`] that chains steps from a document's current
//! version up to a target version -- as a small, ordered list rather than a
//! single hardcoded v0-to-v1 function, so a future v1-to-v2 migration is
//! "add another `Migration` impl and register it", not a rewrite.

use toml_edit::{DocumentMut, value};

use crate::error::{ConfigError, Result};

/// One version-to-version upgrade step for a config document.
///
/// Implementations mutate the document in place -- typically adding,
/// renaming, or reshaping keys -- and must leave `schema_version` at
/// [`Migration::dest_version`] when they return `Ok`. [`MigrationRegistry`]
/// sets `schema_version` itself after a successful call, so implementations
/// don't need to (and doing it themselves is harmless, just redundant).
pub trait Migration: Send + Sync {
    /// The `schema_version` this migration applies to.
    fn source_version(&self) -> u32;

    /// The `schema_version` this migration produces.
    fn dest_version(&self) -> u32;

    /// Applies the upgrade in place. Must not fail for a well-formed
    /// `source_version`-shaped document; a legitimately malformed document
    /// should already have failed to parse before migration runs.
    fn migrate(&self, doc: &mut DocumentMut) -> Result<()>;
}

/// An ordered set of [`Migration`] steps, chained by `source_version` /
/// `dest_version` to walk a document from whatever version it's currently at
/// up to a target version.
#[derive(Default)]
pub struct MigrationRegistry {
    steps: Vec<Box<dyn Migration>>,
}

impl MigrationRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Registers a migration step. Order of registration doesn't matter --
    /// [`Self::run`] looks up the next step by `source_version` each time.
    pub fn register(mut self, migration: impl Migration + 'static) -> Self {
        self.steps.push(Box::new(migration));
        self
    }

    /// The registry `settings.toml` uses: currently just v0 -> v1
    /// (`docs/config-schema.md` names v1 as the current schema version for
    /// all four file kinds, and v0 as reserved for pre-`schema_version`
    /// files). Extend this when a v1 -> v2 migration is ever needed.
    pub fn settings() -> Self {
        Self::new().register(SetSchemaVersion { from: 0, to: 1 })
    }

    /// The registry used by `keymap.toml`, `connections.toml`, and
    /// `themes/*.toml`: same v0 -> v1 step, since none of the three has any
    /// content difference between v0 and v1 per `docs/config-schema.md` --
    /// v0 only ever meant "predates the `schema_version` field".
    pub fn generic_v0_to_v1() -> Self {
        Self::new().register(SetSchemaVersion { from: 0, to: 1 })
    }

    /// Walks `doc` from its current `schema_version` (missing = implicitly
    /// `0`, per spec) up to `target_version`, applying registered steps in
    /// sequence. Returns the version the document ended at (equal to
    /// `target_version` on success). A no-op (`Ok` immediately) if the
    /// document is already at or newer than `target_version` -- newer
    /// documents are left alone, not downgraded.
    pub fn run(
        &self,
        doc: &mut DocumentMut,
        path: &std::path::Path,
        target_version: u32,
    ) -> Result<u32> {
        let mut current = read_schema_version(doc);
        while current < target_version {
            let Some(step) = self.steps.iter().find(|s| s.source_version() == current) else {
                return Err(ConfigError::NoMigrationPath {
                    path: path.to_path_buf(),
                    from: current,
                    target: target_version,
                });
            };
            step.migrate(doc)?;
            current = step.dest_version();
            doc["schema_version"] = value(i64::from(current));
        }
        Ok(current)
    }
}

/// Reads `schema_version` from a document, defaulting to `0` when absent --
/// "reserved for files predating this field" per `docs/config-schema.md`.
pub fn read_schema_version(doc: &DocumentMut) -> u32 {
    doc.get("schema_version")
        .and_then(|i| i.as_integer())
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

/// The only migration that exists today: stamp a pre-`schema_version` (v0)
/// document with `schema_version = 1`.
///
/// Per `docs/config-schema.md`, v1 introduced the `schema_version` field
/// itself; none of the four file kinds changed shape between "no field" and
/// "field present, value 1". So the only real content change here is adding
/// the key -- everything else in the document (including keys this Duet
/// version doesn't recognize) passes through untouched because migrations
/// operate on the live `DocumentMut`, never a re-serialized typed struct.
struct SetSchemaVersion {
    from: u32,
    to: u32,
}

impl Migration for SetSchemaVersion {
    fn source_version(&self) -> u32 {
        self.from
    }

    fn dest_version(&self) -> u32 {
        self.to
    }

    fn migrate(&self, doc: &mut DocumentMut) -> Result<()> {
        // Insert as the first key when it's wholly absent, so a freshly
        // migrated file reads the same way as a hand-written v1 example
        // from docs/config-schema.md (`schema_version` first, then
        // sections). If some other value is already present (e.g. an
        // explicit `schema_version = 0`), just overwrite it in place.
        if doc.get("schema_version").is_none() {
            doc.insert(
                "schema_version",
                toml_edit::Item::Value(i64::from(self.to).into()),
            );
            reorder_schema_version_first(doc);
        } else {
            doc["schema_version"] = value(i64::from(self.to));
        }
        Ok(())
    }
}

/// Moves `schema_version` to the front of the top-level table, purely
/// cosmetic (round-trip correctness does not depend on key order).
fn reorder_schema_version_first(doc: &mut DocumentMut) {
    let table = doc.as_table_mut();
    table.sort_values_by(|k1, _, k2, _| {
        let rank = |k: &str| if k == "schema_version" { 0 } else { 1 };
        rank(k1).cmp(&rank(k2))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn missing_schema_version_is_treated_as_v0() {
        let doc: DocumentMut = "[general]\nlocale = \"system\"\n".parse().unwrap();
        assert_eq!(read_schema_version(&doc), 0);
    }

    #[test]
    fn run_migrates_v0_document_to_v1_and_preserves_other_keys() {
        let mut doc: DocumentMut = "[general]\nlocale = \"system\"\ncustom_legacy_key = \"kept\"\n"
            .parse()
            .unwrap();
        let registry = MigrationRegistry::settings();
        let ending = registry
            .run(&mut doc, Path::new("settings.toml"), 1)
            .unwrap();
        assert_eq!(ending, 1);
        assert_eq!(read_schema_version(&doc), 1);
        assert_eq!(doc["general"]["locale"].as_str(), Some("system"));
        assert_eq!(doc["general"]["custom_legacy_key"].as_str(), Some("kept"));
    }

    #[test]
    fn run_is_noop_for_already_current_document() {
        let mut doc: DocumentMut = "schema_version = 1\n".parse().unwrap();
        let registry = MigrationRegistry::settings();
        let ending = registry
            .run(&mut doc, Path::new("settings.toml"), 1)
            .unwrap();
        assert_eq!(ending, 1);
    }

    #[test]
    fn run_leaves_newer_documents_alone() {
        let mut doc: DocumentMut = "schema_version = 5\n".parse().unwrap();
        let registry = MigrationRegistry::settings();
        let ending = registry
            .run(&mut doc, Path::new("settings.toml"), 1)
            .unwrap();
        assert_eq!(
            ending, 5,
            "a document newer than target_version must not be downgraded"
        );
    }

    #[test]
    fn run_errors_when_no_migration_covers_the_gap() {
        let mut doc: DocumentMut = "schema_version = 0\n".parse().unwrap();
        let registry = MigrationRegistry::new(); // empty: no v0->v1 step registered
        let err = registry
            .run(&mut doc, Path::new("settings.toml"), 1)
            .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NoMigrationPath {
                from: 0,
                target: 1,
                ..
            }
        ));
    }
}
