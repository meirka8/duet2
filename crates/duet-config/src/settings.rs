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
//!
//! Field docs below are deliberately terse pointers back to
//! `docs/config-schema.md` §1's "Key reference" table, which remains the
//! single source of truth for meaning, defaults, and valid ranges.

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
    /// `[general]`: locale, startup behavior, single-instance policy.
    pub general: General,
    /// `[panels]`: sort/view defaults for newly opened tabs.
    pub panels: Panels,
    /// `[selection]`: mouse selection convention.
    pub selection: Selection,
    /// `[navigation]`: quick search, history depth, branch view.
    pub navigation: Navigation,
    /// `[operations]`: copy/move/delete defaults.
    pub operations: Operations,
    /// `[trash]`: freedesktop trash behavior.
    pub trash: Trash,
    /// `[appearance]`: theme selection, font, density.
    pub appearance: Appearance,
    /// `[terminal]`: embedded terminal / shell.
    pub terminal: Terminal,
    /// `[clipboard]`: cut-marker interop convention.
    pub clipboard: Clipboard,
    /// `[logging]`: default trace filter and file persistence.
    pub logging: Logging,
    /// `[plugins]`: plugin host master switch and bundle directory.
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
    /// `"system"` (read `$LANG`/portal locale) or a BCP-47 tag. FR-CFG-10.
    pub locale: String,
    /// BCP-47 tag used when `locale`'s resolved translation is incomplete.
    pub fallback_locale: String,
    /// `restore_session` \| `open_home` \| `open_last_cwd` \| `open_specified`.
    pub startup_behavior: String,
    /// Prompt before quitting while the operation queue is non-empty.
    pub confirm_quit_with_running_jobs: bool,
    /// Forward a second launch's CLI to the running instance (FR-CFG-08).
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
    /// FR-NAV-06 directories-first policy.
    pub sort_directories_first: bool,
    /// Natural (version) sort for numeric runs in names.
    pub natural_sort: bool,
    /// Case sensitivity of the name comparator.
    pub case_sensitive_sort: bool,
    /// Show dotfiles / hidden-attribute entries by default.
    pub show_hidden: bool,
    /// `full` \| `brief` \| `thumbnails` \| `tree` (FR-NAV-04).
    pub default_view: String,
    /// `name` \| `ext` \| `size` \| `date` \| `attrs`.
    pub default_sort_column: String,
    /// `ascending` \| `descending`.
    pub default_sort_order: String,
    /// Whether view/sort changes persist per tab or reset each launch.
    pub remember_view_per_tab: bool,
    /// The dual-pane splitter's left-panel fraction of the workspace width,
    /// `0.1..=0.9` (FR-NAV-01: "ratio persists per session"). T-4.1.4 wires
    /// this as the simple, single global default; per-tab/per-session
    /// splitter state (distinct ratios across restored tabs) is T-4.3.7's
    /// job ("Session persistence: panes, tabs, cwds, ..., splitter").
    pub splitter_ratio: f32,
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
            splitter_ratio: 0.5,
        }
    }
}

/// `[selection]` -- mouse selection convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Selection {
    /// `windows` \| `norton` \| `none` (FR-SEL-06).
    pub mouse_mode: String,
    /// FR-SEL-04.
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
    /// `jump` \| `filter` (FR-NAV-07).
    pub quick_search_mode: String,
    /// 200-5000ms idle time before the quick-search buffer resets.
    pub quick_search_idle_timeout_ms: u32,
    /// Per-tab back/forward history depth, 10-1000 (FR-NAV-08).
    pub history_size: u32,
    /// Hidden-file visibility specifically inside branch view (FR-NAV-10).
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
    /// Global default for FR-OPS-08 post-copy checksum verification.
    pub verify_after_copy: bool,
    /// FR-OPS-05.
    pub preserve_xattrs: bool,
    /// FR-OPS-05.
    pub preserve_acls: bool,
    /// mtime/atime preserved via `utimensat` after content+metadata.
    pub preserve_timestamps: bool,
    /// Whether ownership is preserved when running privileged.
    pub preserve_ownership_if_privileged: bool,
    /// `"auto"` or a worker count (`1`-`32`); kept as a string since TOML
    /// has no native "int or string" union and the doc allows both.
    pub concurrency: String,
    /// `trash` \| `permanent`.
    pub delete_default: String,
    /// `always` \| `non_empty_dirs` \| `never`.
    pub confirm_delete: String,
    /// Job-level conflict-resolution default before any interactive
    /// "apply to all" answer (FR-OPS-04).
    pub default_conflict_policy: String,
    /// Age (days, 0 = forever) after which completed job journals are
    /// pruned.
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
    /// Master switch for freedesktop trash (FR-CFG-07).
    pub enabled: bool,
    /// Use `$topdir/.Trash-$uid` for deletes on non-home filesystems.
    pub use_top_level_on_other_mounts: bool,
    /// Confirm before emptying the trash.
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
    /// Follow the desktop dark/light preference (FR-CFG-04).
    pub theme_follow_system: bool,
    /// Theme used for light mode when following the system.
    pub theme_light: String,
    /// Theme used for dark mode when following the system.
    pub theme_dark: String,
    /// Theme used when `theme_follow_system = false`.
    pub theme: String,
    /// `"system-ui"` or a fontconfig family name.
    pub font: String,
    /// Base UI font size in points, 8-32.
    pub font_size: u32,
    /// `compact` \| `comfortable` \| `spacious`.
    pub row_height: String,
    /// `"system"` or an installed XDG icon theme name.
    pub icon_theme: String,
    /// Disable to render text-only rows.
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
    /// Shell used by the embedded command line and terminal panel.
    pub shell: String,
    /// FR-TOOL-07 toggle for the embedded terminal panel.
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
    /// `auto` \| `gnome` \| `kde` cut-marker MIME convention (FR-CFG-05).
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
    /// `error` \| `warn` \| `info` \| `debug` \| `trace`.
    pub log_level: String,
    /// Persist the ring buffer / session log under
    /// `~/.local/state/duet/`.
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
    /// Master switch for the plugin host (FR-PLUG-*).
    pub enabled: bool,
    /// Override for where installed plugin bundles are read from.
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
