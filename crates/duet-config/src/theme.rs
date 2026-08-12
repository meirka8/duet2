// SPDX-License-Identifier: MIT
//! `themes/*.toml` loading (`docs/config-schema.md` §4): the color/spacing
//! token set. T-3.3.1 left this deliberately thin ("a full typed `Theme`
//! struct mirroring every documented token is left for ... whichever task
//! first renders against these values"); [`ThemeTokensDocument`] is that
//! struct, added by T-4.1.5 ("Theme system: token set, light/dark,
//! follow-system detection, theme file loading"). This module supplies the
//! same round-trip / versioning / hot-reload file layer as
//! [`crate::settings`], generic over the caller's typed shape -- callers
//! that want a different projection of a theme file can still supply their
//! own `T` to [`load`]/[`crate::watch::watch`].

use std::collections::HashMap;

use serde::Deserialize;

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

/// Typed read view of a `themes/*.toml` file, matching
/// `docs/config-schema.md` §4's documented shape exactly:
/// `schema_version`/`name`/`variant` plus three flat `[color]`/`[syntax]`/
/// `[spacing]` tables.
///
/// Deliberately **not** a struct with one named field per token (that would
/// duplicate the ~48-token list in two places -- this doc's §4 table and a
/// Rust struct -- with no compiler-enforced link between them). Instead the
/// three tables deserialize as plain maps, keyed by the token's documented
/// name (e.g. `"panel_bg_active"`, `"keyword"`, `"radius_sm"`); the
/// GPUI-aware caller (`duet-widgets`, per ADR-002 the only place allowed to
/// know what a `gpui::Hsla` is) is the one that knows the canonical token
/// *names* and looks each one up by string, falling back to its own
/// built-in default for any token a given theme file omits -- see
/// `duet_widgets::theme::TokenPalette::apply_overrides`. This crate only
/// needs to parse the file and hand back "whatever key/value pairs were
/// there", not validate the token vocabulary itself.
///
/// A theme file supplies one variant (light or dark) and is meant to be
/// layered as a partial override on top of a built-in default, not
/// necessarily specifying every token (`docs/config-schema.md` §4's
/// "Theme file loading ... overriding the built-in default where the file
/// specifies them" -- a custom theme can be as small as one `[color]`
/// entry).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ThemeTokensDocument {
    /// Migration marker; see [`THEME_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Human-readable theme name, e.g. `"Duet Dark"`.
    pub name: String,
    /// `"dark"` or `"light"` -- which of `appearance.theme_light`/
    /// `theme_dark` this file is meant to fill, per `docs/config-schema.md`
    /// §4's worked example. Left as a plain string (not a closed enum) since
    /// this crate only passes it through; the caller interprets it.
    pub variant: String,
    /// `[color]` table: token name -> `#rrggbb`/`#rrggbbaa` hex string.
    pub color: HashMap<String, String>,
    /// `[syntax]` table: token name -> `#rrggbb`/`#rrggbbaa` hex string.
    pub syntax: HashMap<String, String>,
    /// `[spacing]` table: token name -> integer pixels at 1x scale.
    pub spacing: HashMap<String, f64>,
}

impl Default for ThemeTokensDocument {
    fn default() -> Self {
        Self {
            schema_version: THEME_SCHEMA_VERSION,
            name: String::new(),
            variant: String::new(),
            color: HashMap::new(),
            syntax: HashMap::new(),
            spacing: HashMap::new(),
        }
    }
}

/// Loads a `themes/*.toml` file at `path` into [`ThemeTokensDocument`].
/// Convenience wrapper over [`load`] pinning `T = ThemeTokensDocument`, the
/// shape every real Duet theme file uses.
pub fn load_tokens(path: &std::path::Path) -> Result<ThemeDocument<ThemeTokensDocument>> {
    load::<ThemeTokensDocument>(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/config-schema.md` §4's own worked example, verbatim, must
    /// parse into [`ThemeTokensDocument`] -- the same "doc is the source of
    /// truth, verify against it" discipline `duet-commands::catalogue`
    /// applies to `docs/commands.md`.
    #[test]
    fn parses_config_schema_worked_example() {
        let text = r##"
schema_version = 1
name    = "Duet Dark"
variant = "dark"

[color]
panel_bg_active    = "#1e1e2e"
panel_bg_inactive  = "#181825"
accent             = "#89b4fa"

[syntax]
keyword     = "#cba6f7"
string      = "#a6e3a1"

[spacing]
xs        = 2
sm        = 4
radius_sm = 2
"##;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duet-dark.toml");
        std::fs::write(&path, text).unwrap();

        let file = load_tokens(&path).expect("config-schema.md's worked example must parse");
        let doc = file.typed().unwrap();
        assert_eq!(doc.name, "Duet Dark");
        assert_eq!(doc.variant, "dark");
        assert_eq!(
            doc.color.get("panel_bg_active"),
            Some(&"#1e1e2e".to_string())
        );
        assert_eq!(doc.color.get("accent"), Some(&"#89b4fa".to_string()));
        assert_eq!(doc.syntax.get("keyword"), Some(&"#cba6f7".to_string()));
        assert_eq!(doc.spacing.get("xs"), Some(&2.0));
    }

    /// A theme file overriding just a couple of tokens (the intended
    /// "partial override" use case) parses cleanly, with every other
    /// section defaulting to empty maps.
    #[test]
    fn partial_theme_file_parses_with_empty_defaults_for_missing_sections() {
        let text = "schema_version = 1\nname = \"My accent\"\nvariant = \"dark\"\n\n[color]\naccent = \"#ff00ff\"\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, text).unwrap();

        let file = load_tokens(&path).unwrap();
        let doc = file.typed().unwrap();
        assert_eq!(doc.color.len(), 1);
        assert!(doc.syntax.is_empty());
        assert!(doc.spacing.is_empty());
    }
}
