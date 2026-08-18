// SPDX-License-Identifier: MIT
//! `hotlist.toml`: the directory hotlist (bookmarks), FR-NAV-08.
//!
//! Its own dedicated file -- not a `settings.toml` section, not
//! `session.json` -- per design.md §10's directory layout
//! (`~/.config/duet/hotlist.toml`) and `docs/config-schema.md`'s directory
//! listing (both name it explicitly, alongside `settings.toml`/
//! `keymap.toml`/`connections.toml`/`themes/*.toml`). It gets the same
//! [`ConfigFile`] round-trip/versioning treatment those get: a user-curated,
//! hand-editable-in-principle list that survives a restart is exactly the
//! shape that machinery exists for, unlike `session.json`'s disposable,
//! plain-`serde_json` app state (see `session.rs`'s own doc comment for that
//! distinction).
//!
//! [`Hotlist`]/[`HotlistEntry`] are a typed *read view*, same convention as
//! [`crate::settings::Settings`] -- never used to re-serialize directly.
//! Every write (add/remove/reorder) goes through [`HotlistFile::set`]
//! ([`ConfigFile::set`]) on the whole `entries` key at once, rebuilding the
//! array from an updated `Vec<HotlistEntry>` -- simpler and just as
//! round-trip-safe as an incremental splice would be, since `entries` has
//! no sibling keys of its own to accidentally disturb.

use serde::{Deserialize, Serialize};
use toml_edit::{Array, InlineTable, Value};

use crate::document::ConfigFile;
use crate::error::Result;
use crate::migrate::MigrationRegistry;

/// Current schema version for `hotlist.toml`. No hotlist-specific
/// migration exists yet (it's a brand-new file kind, T-4.3.5) -- loads go
/// through [`MigrationRegistry::generic_v0_to_v1`], the same "v0 only ever
/// meant 'predates `schema_version`'" step `keymap.toml`/`connections.toml`/
/// `themes/*.toml` already share.
pub const HOTLIST_SCHEMA_VERSION: u32 = 1;

/// A loaded `hotlist.toml`, with round-trip preservation, versioning, and
/// typed access.
pub type HotlistFile = ConfigFile<Hotlist>;

/// Loads `hotlist.toml` from `path`, migrating it to
/// [`HOTLIST_SCHEMA_VERSION`] if needed (backup written first).
pub fn load(path: &std::path::Path) -> Result<HotlistFile> {
    HotlistFile::load(
        path,
        &MigrationRegistry::generic_v0_to_v1(),
        HOTLIST_SCHEMA_VERSION,
    )
}

/// Typed read view of `hotlist.toml`. `#[serde(default)]` on every field
/// (an empty `entries` list, same as a brand-new install with no bookmarks
/// yet) so a document missing keys still deserializes cleanly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotlist {
    /// Migration marker; see [`HOTLIST_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Bookmarked directories, in display/navigation order -- what T-4.3.5's
    /// keyboard overlay lists top to bottom, and what its reorder command
    /// permutes.
    pub entries: Vec<HotlistEntry>,
}

impl Default for Hotlist {
    fn default() -> Self {
        Self {
            schema_version: HOTLIST_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

/// One bookmarked directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotlistEntry {
    /// The bookmarked directory's absolute path.
    pub path: String,
    /// A user-chosen display label, distinct from `path` --
    /// `docs/commands.md`'s `hotlist.rename_entry` command implies this
    /// field's existence even though no UI sets it yet (T-4.3.5 doesn't
    /// build rename; see that task's own PR description). `None` means
    /// "show the path itself" -- the only state reachable today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Hotlist {
    /// `entries`, converted to the `toml_edit::Array` shape
    /// [`ConfigFile::set`] needs to write the whole list back in one call
    /// -- `&["entries"]` -- after any add/remove/reorder.
    pub fn entries_to_toml_array(entries: &[HotlistEntry]) -> Array {
        let mut array = Array::new();
        for entry in entries {
            let mut table = InlineTable::new();
            table.insert("path", entry.path.clone().into());
            if let Some(label) = &entry.label {
                table.insert("label", label.clone().into());
            }
            array.push(Value::InlineTable(table));
        }
        array
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> HotlistEntry {
        HotlistEntry {
            path: path.to_string(),
            label: None,
        }
    }

    #[test]
    fn fresh_document_with_no_entries_key_deserializes_to_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotlist.toml");
        std::fs::write(&path, "schema_version = 1\n").unwrap();

        let file = load(&path).unwrap();
        let typed = file.typed().unwrap();
        assert_eq!(typed.entries, Vec::new());
    }

    #[test]
    fn round_trips_entries_with_and_without_a_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotlist.toml");
        std::fs::write(&path, "schema_version = 1\nentries = []\n").unwrap();

        let mut file = load(&path).unwrap();
        let entries = vec![
            entry("/home/user/projects"),
            HotlistEntry {
                path: "/home/user/Downloads".to_string(),
                label: Some("Downloads".to_string()),
            },
        ];
        file.set(&["entries"], Hotlist::entries_to_toml_array(&entries));
        file.save().unwrap();

        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.typed().unwrap().entries, entries);
    }

    #[test]
    fn setting_entries_preserves_unrelated_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotlist.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nentries = []\n\n[future_feature]\nsome_new_key = 42\n",
        )
        .unwrap();

        let mut file = load(&path).unwrap();
        file.set(
            &["entries"],
            Hotlist::entries_to_toml_array(&[entry("/tmp")]),
        );
        file.save().unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[future_feature]"));
        assert!(after.contains("some_new_key = 42"));
    }

    #[test]
    fn v0_document_migrates_to_v1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotlist.toml");
        std::fs::write(&path, "entries = []\n").unwrap();

        let file = load(&path).unwrap();
        assert_eq!(file.schema_version(), 1);
        assert_eq!(file.typed().unwrap().schema_version, 1);
    }

    #[test]
    fn reorder_is_just_a_vec_swap_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotlist.toml");
        std::fs::write(&path, "schema_version = 1\nentries = []\n").unwrap();

        let mut file = load(&path).unwrap();
        let mut entries = vec![entry("/a"), entry("/b"), entry("/c")];
        entries.swap(0, 1); // move "/a" down one
        file.set(&["entries"], Hotlist::entries_to_toml_array(&entries));
        file.save().unwrap();

        let reloaded = load(&path).unwrap().typed().unwrap();
        assert_eq!(
            reloaded
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/b", "/a", "/c"]
        );
    }
}
