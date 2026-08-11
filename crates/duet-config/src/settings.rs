// SPDX-License-Identifier: MIT
//! `settings.toml`: the primary config file (`docs/config-schema.md` §1).
//!
//! [`Settings`] is a typed *read view* of the document -- every field has a
//! `#[serde(default)]` matching the documented default, so a document
//! missing sections (a fresh install, or a hand-edited file with only a few
//! keys) still deserializes cleanly. It is never used to re-serialize the
//! file: [`SettingsFile`] (a [`ConfigFile<Settings>`]) keeps the live
//! [`toml_edit::DocumentMut`] as the only write path, which is what
//! preserves comments, formatting, and unrecognized keys across a save.

use serde::{Deserialize, Serialize};

use crate::document::ConfigFile;
use crate::error::Result;
use crate::migrate::MigrationRegistry;

/// Current schema version for `settings.toml`, per
/// `docs/config-schema.md`'s version table.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// A loaded `settings.toml`, with round-trip preservation, versioning, and
/// typed access.
pub type SettingsFile = ConfigFile<Settings>;

/// Loads `settings.toml` from `path`, migrating it to
/// [`SETTINGS_SCHEMA_VERSION`] if needed (backup written first).
pub fn load(path: &std::path::Path) -> Result<SettingsFile> {
    SettingsFile::load(
        path,
        &MigrationRegistry::settings(),
        SETTINGS_SCHEMA_VERSION,
    )
}

/// Typed read view of `settings.toml` (`docs/config-schema.md` §1).
///
/// Field/section names, defaults, and value ranges mirror that document's
/// "Key reference" table exactly; see there for the meaning of each field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Migration marker; see [`SETTINGS_SCHEMA_VERSION`].
    pub schema_version: u32,
    pub general: General,
    pub panels: Panels,
    pub selection: Selection,
    pub navigation: Navigation,
    pub operations: Operations,
    pub trash: Trash,
    pub appearance: Appearance,
    pub terminal: Terminal,
    pub clipboard: Clipboard,
    pub logging: Logging,
    pub plugins: Plugins,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            general: General::default(),
            panels: Panels::default(),
            selection: Selection::default(),
            navigation: Navigation::default(),
            operations: Operations::default(),
            trash: Trash::default(),
            appearance: Appearance::default(),
            terminal: Terminal::default(),
            clipboard: Clipboard::default(),
            logging: Logging::default(),
            plugins: Plugins::default(),
        }
    }
}

/// `[general]` -- locale, startup behavior, single-instance policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub locale: String,
    pub fallback_locale: String,
    pub startup_behavior: String,
    pub confirm_quit_with_running_jobs: bool,
    pub single_instance: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            locale: "system".into(),
            fallback_locale: "en-US".into(),
            startup_behavior: "restore_session".into(),
            confirm_quit_with_running_jobs: true,
            single_instance: true,
        }
    }
}

/// `[panels]` -- sort/view defaults for newly opened tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panels {
    pub sort_directories_first: bool,
    pub natural_sort: bool,
    pub case_sensitive_sort: bool,
    pub show_hidden: bool,
    pub default_view: String,
    pub default_sort_column: String,
    pub default_sort_order: String,
    pub remember_view_per_tab: bool,
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            sort_directories_first: true,
            natural_sort: true,
            case_sensitive_sort: false,
            show_hidden: false,
            default_view: "full".into(),
            default_sort_column: "name".into(),
            default_sort_order: "ascending".into(),
            remember_view_per_tab: true,
        }
    }
}

/// `[selection]` -- mouse selection convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Selection {
    pub mouse_mode: String,
    pub restore_selection_after_operation: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            mouse_mode: "windows".into(),
            restore_selection_after_operation: true,
        }
    }
}

/// `[navigation]` -- quick search, history depth, branch view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Navigation {
    pub quick_search_mode: String,
    pub quick_search_idle_timeout_ms: u32,
    pub history_size: u32,
    pub branch_view_show_hidden: bool,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            quick_search_mode: "jump".into(),
            quick_search_idle_timeout_ms: 1200,
            history_size: 100,
            branch_view_show_hidden: false,
        }
    }
}

/// `[operations]` -- copy/move/delete defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Operations {
    pub verify_after_copy: bool,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_timestamps: bool,
    pub preserve_ownership_if_privileged: bool,
    /// `"auto"` or a worker count (`1`-`32`); kept as a string since TOML
    /// has no native "int or string" union and the doc allows both.
    pub concurrency: String,
    pub delete_default: String,
    pub confirm_delete: String,
    pub default_conflict_policy: String,
    pub journal_retention_days: u32,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            verify_after_copy: false,
            preserve_xattrs: true,
            preserve_acls: true,
            preserve_timestamps: true,
            preserve_ownership_if_privileged: true,
            concurrency: "auto".into(),
            delete_default: "trash".into(),
            confirm_delete: "always".into(),
            default_conflict_policy: "ask".into(),
            journal_retention_days: 7,
        }
    }
}

/// `[trash]` -- freedesktop trash behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trash {
    pub enabled: bool,
    pub use_top_level_on_other_mounts: bool,
    pub confirm_empty: bool,
}

impl Default for Trash {
    fn default() -> Self {
        Self {
            enabled: true,
            use_top_level_on_other_mounts: true,
            confirm_empty: true,
        }
    }
}

/// `[appearance]` -- theme selection, font, density.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme_follow_system: bool,
    pub theme_light: String,
    pub theme_dark: String,
    pub theme: String,
    pub font: String,
    pub font_size: u32,
    pub row_height: String,
    pub icon_theme: String,
    pub show_icons: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme_follow_system: true,
            theme_light: "duet-light".into(),
            theme_dark: "duet-dark".into(),
            theme: "duet-dark".into(),
            font: "system-ui".into(),
            font_size: 13,
            row_height: "compact".into(),
            icon_theme: "system".into(),
            show_icons: true,
        }
    }
}

/// `[terminal]` -- embedded terminal / shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub shell: String,
    pub embedded_terminal_enabled: bool,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            shell: "$SHELL".into(),
            embedded_terminal_enabled: false,
        }
    }
}

/// `[clipboard]` -- cut-marker interop convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Clipboard {
    pub cut_marker_convention: String,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            cut_marker_convention: "auto".into(),
        }
    }
}

/// `[logging]` -- default trace filter and file persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Logging {
    pub log_level: String,
    pub log_to_file: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            log_to_file: true,
        }
    }
}

/// `[plugins]` -- plugin host master switch and bundle directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugins {
    pub enabled: bool,
    pub directory: String,
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "~/.config/duet/plugins".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::MigrationRegistry;
    use std::path::Path;

    const EXAMPLE: &str = r#"
schema_version = 1

[general]
locale                          = "system"
fallback_locale                 = "en-US"
startup_behavior                = "restore_session"
confirm_quit_with_running_jobs  = true
single_instance                 = true

[panels]
sort_directories_first  = true
natural_sort             = true
case_sensitive_sort      = false
show_hidden              = false
default_view             = "full"
default_sort_column      = "name"
default_sort_order       = "ascending"
remember_view_per_tab    = true
"#;

    #[test]
    fn typed_view_matches_documented_defaults() {
        let file = SettingsFile::from_str(
            Path::new("settings.toml"),
            EXAMPLE,
            &MigrationRegistry::settings(),
            SETTINGS_SCHEMA_VERSION,
        )
        .unwrap();
        let settings = file.typed().unwrap();
        assert_eq!(settings.general.locale, "system");
        assert_eq!(settings.panels.default_sort_column, "name");
        // Sections absent from EXAMPLE (selection, navigation, ...) still
        // deserialize via #[serde(default)].
        assert_eq!(settings.selection, Selection::default());
        assert_eq!(settings.trash, Trash::default());
    }

    #[test]
    fn settings_default_matches_documented_values() {
        let s = Settings::default();
        assert_eq!(s.schema_version, 1);
        assert_eq!(s.selection.mouse_mode, "windows");
        assert_eq!(s.operations.concurrency, "auto");
        assert_eq!(s.appearance.font_size, 13);
    }
}
