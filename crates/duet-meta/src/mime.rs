// SPDX-License-Identifier: MIT
//! The shared-mime-info database, read for one purpose: turning a file
//! name into the icon names the XDG icon theme should be asked for
//! (T-4.2.6, design.md §9.8 "MIME by extension").
//!
//! Two files under each `<data dir>/mime/` are parsed, exactly as
//! `xdg-mime`/GLib do:
//!
//! - `globs2`: `weight:mime/type:pattern[:flags]` lines. Only the two
//!   pattern shapes that cover the overwhelming majority of entries are
//!   honoured -- `*.ext` (an extension suffix, matched case-insensitively
//!   unless the `cs` flag is set) and a literal file name (`Makefile`,
//!   `.bashrc`). Anything with other wildcard characters is skipped:
//!   a panel icon is a hint, not a content-type verdict, and the
//!   magic-byte sniffing design.md mentions is a later, per-file job.
//! - `generic-icons`: `mime/type:icon-name` lines, the theme icon a type
//!   falls back to when no icon named after the type itself exists.
//!
//! Later data dirs (user before system, per `$XDG_DATA_DIRS` order) are
//! merged; a higher `weight` wins for the same extension, and among equal
//! weights the longer literal pattern wins (so `*.tar.gz` beats `*.gz`).
//! Everything is loaded once into plain maps -- the whole database is a
//! few thousand lines -- so a lookup is a couple of hash probes with no
//! allocation on the hit path (`icon_names_for` returns an iterator over
//! borrowed names).

use std::collections::HashMap;
use std::path::Path;

/// One `globs2` rule for an extension or a literal name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobRule {
    weight: u32,
    /// Pattern length, the tie-breaker among equal weights.
    specificity: usize,
    mime: String,
}

/// The parsed database -- see the module doc comment.
#[derive(Debug, Default, Clone)]
pub struct MimeDb {
    /// Lower-cased extension (without the dot; `tar.gz` for `*.tar.gz`)
    /// to the winning rule.
    by_extension: HashMap<String, GlobRule>,
    /// Case-sensitive extensions (`cs` flag), keyed as written.
    by_extension_cs: HashMap<String, GlobRule>,
    /// Literal whole-name patterns (`Makefile`, `.bashrc`).
    by_literal: HashMap<String, GlobRule>,
    /// `generic-icons`: mime type to fallback icon name.
    generic_icons: HashMap<String, String>,
}

impl MimeDb {
    /// Loads `<dir>/mime/globs2` and `<dir>/mime/generic-icons` from each
    /// of `data_dirs`, in order of precedence (first wins on equal
    /// weight and specificity). A missing file is simply skipped; an
    /// unreadable one too -- no icon database is a degraded panel, not a
    /// failure.
    pub fn load(data_dirs: &[impl AsRef<Path>]) -> Self {
        let mut db = Self::default();
        // Walk from lowest precedence to highest so a later insert (higher
        // precedence) can overwrite on ties.
        for dir in data_dirs.iter().rev() {
            let mime_dir = dir.as_ref().join("mime");
            if let Ok(text) = std::fs::read_to_string(mime_dir.join("globs2")) {
                db.parse_globs2(&text);
            }
            if let Ok(text) = std::fs::read_to_string(mime_dir.join("generic-icons")) {
                db.parse_generic_icons(&text);
            }
        }
        db
    }

    /// Parses one `globs2` file's text into this database (see the
    /// module doc comment for the precedence rules).
    pub fn parse_globs2(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(weight) = weight.parse::<u32>() else {
                continue;
            };
            let case_sensitive = parts.next().is_some_and(|flags| flags.contains("cs"));
            let rule = GlobRule {
                weight,
                specificity: pattern.len(),
                mime: mime.to_string(),
            };
            if let Some(ext) = pattern.strip_prefix("*.") {
                if ext.is_empty() || ext.contains(['*', '?', '[']) {
                    continue;
                }
                if case_sensitive {
                    insert_if_better(&mut self.by_extension_cs, ext.to_string(), rule);
                } else {
                    insert_if_better(&mut self.by_extension, ext.to_lowercase(), rule);
                }
            } else if !pattern.contains(['*', '?', '[']) {
                insert_if_better(&mut self.by_literal, pattern.to_string(), rule);
            }
        }
    }

    /// Parses one `generic-icons` file's text into this database.
    pub fn parse_generic_icons(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((mime, icon)) = line.split_once(':') {
                self.generic_icons
                    .insert(mime.to_string(), icon.to_string());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_extension.is_empty() && self.by_literal.is_empty()
    }

    /// The MIME type `globs2` assigns to `file_name`, if any: a literal
    /// rule for the whole name first (`Makefile`), then the longest
    /// matching extension suffix (`tar.gz` before `gz`), case-sensitive
    /// rules before case-insensitive ones at the same length.
    pub fn mime_for_name(&self, file_name: &str) -> Option<&str> {
        let mut best: Option<&GlobRule> = self.by_literal.get(file_name);
        // Every suffix after a dot, longest first: "a.tar.gz" -> "tar.gz", "gz".
        for (ix, _) in file_name.match_indices('.') {
            if ix == 0 {
                continue; // a leading dot is a dotfile, not an extension
            }
            let ext = &file_name[ix + 1..];
            if ext.is_empty() {
                continue;
            }
            let candidate = self.by_extension_cs.get(ext).or_else(|| {
                let lower = ext.to_lowercase();
                self.by_extension.get(lower.as_str())
            });
            if let Some(rule) = candidate
                && best.is_none_or(|b| (rule.weight, rule.specificity) > (b.weight, b.specificity))
            {
                best = Some(rule);
            }
        }
        best.map(|rule| rule.mime.as_str())
    }

    /// Whether `file_name` has a whole-name rule of its own (`Makefile`,
    /// `.bashrc`) -- what decides whether an extension-less name needs
    /// its own icon lookup or can share the generic one.
    pub fn is_literal(&self, file_name: &str) -> bool {
        self.by_literal.contains_key(file_name)
    }

    /// The `generic-icons` fallback for `mime`, if the database names one.
    pub fn generic_icon(&self, mime: &str) -> Option<&str> {
        self.generic_icons.get(mime).map(String::as_str)
    }
}

fn insert_if_better(map: &mut HashMap<String, GlobRule>, key: String, rule: GlobRule) {
    match map.get(&key) {
        Some(existing)
            if (existing.weight, existing.specificity) > (rule.weight, rule.specificity) => {}
        _ => {
            map.insert(key, rule);
        }
    }
}

/// The icon names the theme should be asked for, best first, for a MIME
/// type -- the XDG Icon Naming Specification's rule for MIME icons:
/// the type with `/` turned into `-` (`text-plain`), then the database's
/// `generic-icons` fallback (`text-x-generic`), then the media type's own
/// generic icon (`text-x-generic`, `image-x-generic`, ...), then the
/// last-resort `unknown`. Duplicates are dropped so a theme is never
/// probed twice for the same name.
pub fn mime_icon_names(db: &MimeDb, mime: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(4);
    let mut push = |name: String| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    push(mime.replace('/', "-"));
    if let Some(generic) = db.generic_icon(mime) {
        push(generic.to_string());
    }
    if let Some((media, _)) = mime.split_once('/') {
        push(format!("{media}-x-generic"));
    }
    push("unknown".to_string());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> MimeDb {
        let mut db = MimeDb::default();
        db.parse_globs2(
            "# comment\n\
             50:text/x-makefile:Makefile\n\
             50:text/plain:*.txt\n\
             50:application/gzip:*.gz\n\
             55:application/x-compressed-tar:*.tar.gz\n\
             50:text/x-csrc:*.c:cs\n\
             50:text/x-c++src:*.C:cs\n\
             50:image/png:*.png\n\
             50:text/x-readme:readme*\n\
             bogus line\n",
        );
        db.parse_generic_icons("text/plain:text-x-generic\napplication/gzip:package-x-generic\n");
        db
    }

    #[test]
    fn extension_lookup_is_case_insensitive_unless_flagged() {
        let db = db();
        assert_eq!(db.mime_for_name("notes.TXT"), Some("text/plain"));
        assert_eq!(db.mime_for_name("main.c"), Some("text/x-csrc"));
        assert_eq!(db.mime_for_name("main.C"), Some("text/x-c++src"));
        assert_eq!(db.mime_for_name("photo.png"), Some("image/png"));
        assert_eq!(db.mime_for_name("Makefile"), Some("text/x-makefile"));
        assert!(db.is_literal("Makefile"));
        assert!(!db.is_literal("README"));
        assert_eq!(db.mime_for_name("noext"), None);
        assert_eq!(db.mime_for_name(".bashrc"), None, "dotfile, no extension");
        assert_eq!(
            db.mime_for_name("readme.md"),
            None,
            "wildcard patterns are skipped"
        );
    }

    #[test]
    fn longer_and_heavier_patterns_win() {
        let db = db();
        assert_eq!(
            db.mime_for_name("backup.tar.gz"),
            Some("application/x-compressed-tar")
        );
        assert_eq!(db.mime_for_name("file.gz"), Some("application/gzip"));
    }

    #[test]
    fn icon_name_candidates_follow_the_naming_spec_without_duplicates() {
        let db = db();
        assert_eq!(
            mime_icon_names(&db, "text/plain"),
            ["text-plain", "text-x-generic", "unknown"]
        );
        assert_eq!(
            mime_icon_names(&db, "application/gzip"),
            [
                "application-gzip",
                "package-x-generic",
                "application-x-generic",
                "unknown"
            ]
        );
        assert_eq!(
            mime_icon_names(&db, "image/png"),
            ["image-png", "image-x-generic", "unknown"]
        );
    }

    #[test]
    fn load_merges_data_dirs_with_the_first_taking_precedence() {
        let user = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user.path().join("mime")).unwrap();
        std::fs::create_dir_all(system.path().join("mime")).unwrap();
        std::fs::write(
            system.path().join("mime/globs2"),
            "50:text/plain:*.txt\n50:image/png:*.png\n",
        )
        .unwrap();
        std::fs::write(user.path().join("mime/globs2"), "50:text/x-mine:*.txt\n").unwrap();
        std::fs::write(
            system.path().join("mime/generic-icons"),
            "image/png:image-x-generic\n",
        )
        .unwrap();
        let db = MimeDb::load(&[user.path(), system.path()]);
        assert_eq!(
            db.mime_for_name("a.txt"),
            Some("text/x-mine"),
            "user dir wins ties"
        );
        assert_eq!(db.mime_for_name("a.png"), Some("image/png"));
        assert_eq!(db.generic_icon("image/png"), Some("image-x-generic"));
        assert!(MimeDb::load(&[Path::new("/nonexistent")]).is_empty());
    }
}
