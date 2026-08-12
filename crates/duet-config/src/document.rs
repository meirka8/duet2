// SPDX-License-Identifier: MIT
//! [`ConfigFile`]: the generic, round-trip-preserving, schema-versioned
//! document wrapper every config-file kind (`settings.toml`, `keymap.toml`,
//! `connections.toml`, `themes/*.toml`) is built from.
//!
//! The core trick for round-trip preservation (T-3.3.1 AC: "an unknown key
//! survives a rewrite") is that the [`toml_edit::DocumentMut`] is the
//! *only* thing ever written back to disk. Typed structs (like
//! [`crate::settings::Settings`]) are read-only projections obtained by
//! deserializing a snapshot of the document; they are never the source
//! truth used to re-serialize. Edits go through [`ConfigFile::set`], which
//! mutates one key path in the live document and leaves every other key --
//! recognized or not -- untouched.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{ConfigError, Result};
use crate::io;
use crate::migrate::{MigrationRegistry, read_schema_version};

/// A loaded, round-trip-preserving config file, generic over its typed read
/// view `T`.
///
/// `T` is only used by [`ConfigFile::typed`] to project the current
/// document into a concrete struct; `ConfigFile` itself doesn't otherwise
/// care what `T` is, which is what lets `duet-config` support four
/// differently-shaped file kinds (and let other crates, like `duet-commands`
/// for `keymap.toml`, supply their own type) from one implementation.
pub struct ConfigFile<T> {
    path: PathBuf,
    doc: DocumentMut,
    schema_version: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> ConfigFile<T> {
    /// Loads `path`, parses it as TOML, and migrates it up to
    /// `target_version` if it's older (writing a backup first -- see
    /// [`crate::migrate`]). The migrated content is saved back to `path`
    /// immediately, matching `docs/config-schema.md`'s "migrations ... run
    /// synchronously on load before the file is handed to its consumer".
    ///
    /// Not for use on the UI thread (T-3.1.6): this performs blocking
    /// filesystem I/O. Callers on the I/O runtime, or the initial
    /// synchronous startup path before the UI is up, are the intended
    /// callers; [`crate::watch`] is the UI-safe path for reacting to
    /// changes after startup.
    pub fn load(path: &Path, migrations: &MigrationRegistry, target_version: u32) -> Result<Self> {
        let text = io::read_to_string(path)?;
        Self::from_str(path, &text, migrations, target_version)
    }

    /// As [`Self::load`], but from an in-memory string instead of a file --
    /// used by [`crate::watch`] on every reload, and directly by tests.
    pub fn from_str(
        path: &Path,
        text: &str,
        migrations: &MigrationRegistry,
        target_version: u32,
    ) -> Result<Self> {
        let mut doc: DocumentMut = text.parse().map_err(|e| parse_error(path, text, &e))?;

        let before = read_schema_version(&doc);
        let schema_version = migrations.run(&mut doc, path, target_version)?;
        if schema_version != before {
            // A migration ran: persist the upgraded document immediately,
            // after a backup of the pre-migration bytes. `before` is the
            // version the file was at before `run`, which is also the
            // version the backup file name should carry
            // (`<file>.v<N>.bak-<ts>`, N = the version being migrated
            // *away from*).
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            io::write_backup(&io::backup_path(path, before, ts), text)?;
            io::atomic_write(path, &doc.to_string())?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            doc,
            schema_version,
            _marker: PhantomData,
        })
    }

    /// The path this document was loaded from (and will be saved to).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The document's `schema_version` after any load-time migration.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Deserializes the current document into `T`. Unknown keys are simply
    /// absent from the returned value (they still exist in the underlying
    /// document and survive a subsequent [`Self::save`]).
    pub fn typed(&self) -> Result<T> {
        toml_edit::de::from_document(self.doc.clone()).map_err(|source| ConfigError::Deserialize {
            path: self.path.clone(),
            source,
        })
    }

    /// Read-only access to the underlying round-trip document, for callers
    /// that need to inspect keys `T` doesn't model (e.g. a settings UI
    /// listing unrecognized keys for the user).
    pub fn raw(&self) -> &DocumentMut {
        &self.doc
    }

    /// Sets the value at a dotted key path (e.g. `&["panels",
    /// "show_hidden"]`), creating intermediate tables as needed. Every
    /// other key in the document -- sibling keys, unknown keys, comments,
    /// formatting -- is left exactly as it was.
    ///
    /// This is the primitive [`crate::settings::SettingsFile`]'s typed
    /// setters (and any future per-file-kind convenience API) are built on;
    /// it's exposed directly too since `keymap.toml`/`connections.toml`
    /// callers may not want a full typed setter surface yet.
    pub fn set(&mut self, key_path: &[&str], value: impl Into<Value>) {
        let mut table: &mut Table = self.doc.as_table_mut();
        let (leaf, ancestors) = key_path.split_last().expect("key_path must be non-empty");
        for segment in ancestors {
            table = table
                .entry(segment)
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .unwrap_or_else(|| {
                    panic!("config key path segment {segment:?} exists but is not a table")
                });
        }
        table[*leaf] = toml_edit::value(value);
    }

    /// Serializes the current document and atomically overwrites `path`
    /// with it (see [`crate::io::atomic_write`]).
    pub fn save(&self) -> Result<()> {
        io::atomic_write(&self.path, &self.doc.to_string())
    }
}

fn parse_error(path: &Path, text: &str, err: &toml_edit::TomlError) -> ConfigError {
    let (line, column) = err
        .span()
        .map(|span| offset_to_line_col(text, span.start))
        .unwrap_or((0, 0));
    ConfigError::Parse {
        path: path.to_path_buf(),
        line,
        column,
        message: err.message().to_string(),
    }
}

/// Converts a byte offset into a 1-based (line, column) pair.
fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for ch in text[..offset.min(text.len())].chars() {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SETTINGS_SCHEMA_VERSION, Settings};

    /// T-3.3.1 AC: "editing `settings.toml` ... an unknown key survives a
    /// rewrite". This is the literal test the acceptance criterion asks
    /// for: load a document with a key this schema version doesn't
    /// recognize (both a stray key in a known section and a whole unknown
    /// top-level table, covering both shapes a future-version or
    /// third-party addition could take), edit one *known* key through the
    /// typed API, save, and confirm on re-read that (a) the edit took
    /// effect and (b) both unrecognized additions are still there
    /// untouched, alongside a comment and custom formatting.
    #[test]
    fn editing_one_key_preserves_unknown_keys_comments_and_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let original = r#"schema_version = 1

# a user's hand-written comment that should survive
[general]
locale = "system"
custom_field_this_version_does_not_know = "please keep me"

[panels]
show_hidden = false

# an entire section from a newer Duet version this build has never heard of
[future_feature]
some_new_key = 42
"#;
        std::fs::write(&path, original).unwrap();

        let mut file = ConfigFile::<Settings>::load(
            &path,
            &MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
        )
        .unwrap();

        // Edit exactly one known key.
        file.set(&["panels", "show_hidden"], true);
        file.save().unwrap();

        let after = std::fs::read_to_string(&path).unwrap();

        // The edit took effect...
        assert!(
            after.contains("show_hidden = true"),
            "edited key did not change:\n{after}"
        );
        // ...and everything this schema version doesn't recognize is still
        // there: an unknown key inside a known table, an entirely unknown
        // top-level table, and the user's comment.
        assert!(
            after.contains("custom_field_this_version_does_not_know = \"please keep me\""),
            "unknown key inside [general] was lost:\n{after}"
        );
        assert!(
            after.contains("[future_feature]"),
            "unknown top-level table was lost:\n{after}"
        );
        assert!(
            after.contains("some_new_key = 42"),
            "unknown table's contents were lost:\n{after}"
        );
        assert!(
            after.contains("# a user's hand-written comment that should survive"),
            "comment was lost:\n{after}"
        );
        assert!(
            after.contains(
                "# an entire section from a newer Duet version this build has never heard of"
            ),
            "comment above the unknown table was lost:\n{after}"
        );

        // And the unknown top-level table round-trips through a second
        // load/typed-view cycle too -- it's not just surviving as inert
        // text, the reloaded document is still fully valid TOML with it
        // present.
        let reloaded = ConfigFile::<Settings>::load(
            &path,
            &MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
        )
        .unwrap();
        assert!(reloaded.raw().get("future_feature").is_some());
        assert!(reloaded.typed().unwrap().panels.show_hidden);
    }

    /// T-3.3.1 AC: "a v0->v1 migration test passes with a backup written".
    /// A real v0-shaped `settings.toml` -- no `schema_version` field at
    /// all, which per `docs/config-schema.md` is exactly what "v0" means --
    /// loaded through the normal `SettingsFile::load` path. Confirms the
    /// migration actually happened (schema_version is now 1, content
    /// otherwise intact) *and* that a backup of the original v0 bytes
    /// exists on disk before the migrated file replaced it, named per the
    /// documented `<file>.v<N>.bak-<unix_ts>` convention.
    #[test]
    fn v0_to_v1_migration_writes_a_backup_and_upgrades_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let v0_content = "[general]\nlocale = \"de-DE\"\n\n[panels]\nshow_hidden = true\n";
        std::fs::write(&path, v0_content).unwrap();

        let file = ConfigFile::<Settings>::load(
            &path,
            &MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
        )
        .unwrap();

        // The in-memory result is correct: migrated to v1, original content
        // (a non-default locale, a non-default show_hidden) intact.
        assert_eq!(file.schema_version(), 1);
        let typed = file.typed().unwrap();
        assert_eq!(typed.schema_version, 1);
        assert_eq!(typed.general.locale, "de-DE");
        assert!(typed.panels.show_hidden);

        // The file on disk was rewritten with the migrated content...
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("schema_version = 1"),
            "on-disk file was not upgraded:\n{on_disk}"
        );

        // ...and exactly one backup of the pre-migration bytes exists,
        // named `settings.toml.v0.bak-<unix_ts>`, containing the original
        // v0 content verbatim.
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".v0.bak-"))
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "expected exactly one v0 backup, found: {backups:?}"
        );
        let backup_name = backups[0].file_name();
        let backup_name = backup_name.to_string_lossy();
        assert!(
            backup_name.starts_with("settings.toml.v0.bak-"),
            "backup name {backup_name:?} doesn't match the documented <file>.v<N>.bak-<unix_ts> convention"
        );
        let backup_content = std::fs::read_to_string(backups[0].path()).unwrap();
        assert_eq!(
            backup_content, v0_content,
            "backup must contain the original pre-migration bytes verbatim"
        );
    }
}
