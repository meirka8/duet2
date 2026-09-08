// SPDX-License-Identifier: MIT
//! MIME-type to application associations (T-5.3.4, FR-TOOL-08): which
//! desktop entries can open a type, and which one is the default -- the
//! freedesktop "Association between MIME types and applications" spec,
//! as `gio`/`xdg-mime` implement it.
//!
//! Sources, in precedence order (first wins):
//!
//! 1. `mimeapps.list` files: `$XDG_CONFIG_HOME/mimeapps.list`, each
//!    `$XDG_CONFIG_DIRS/mimeapps.list`, `$XDG_DATA_HOME/applications/
//!    mimeapps.list`, each `$XDG_DATA_DIRS/applications/mimeapps.list`.
//!    Three groups: `[Default Applications]` (the preferred entry),
//!    `[Added Associations]` (extra candidates) and `[Removed
//!    Associations]` (entries never to offer for the type, whatever a
//!    lower-precedence file or the cache says).
//! 2. `mimeinfo.cache` in every `applications/` directory: the index of
//!    `MimeType=` lines across the installed entries, i.e. every
//!    application that *claims* the type.
//! 3. `subclasses` from the shared-mime-info database: a type's parents
//!    (`text/x-python` is a `text/plain`), whose applications are offered
//!    after the type's own.
//!
//! The desktop-specific variants (`gnome-mimeapps.list` per
//! `$XDG_CURRENT_DESKTOP`) are not read; they are rare outside distro
//! defaults and the plain file overrides them anyway.
//!
//! A candidate is only offered when its entry is usable
//! ([`DesktopDb::entry`]) and installed ([`DesktopEntry::is_installed`]),
//! so a stale association to an uninstalled program silently drops out
//! and the next candidate becomes the default -- the same recovery
//! `gio open` performs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::desktop::{DesktopDb, DesktopEntry};

/// One parsed `mimeapps.list`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MimeAppsList {
    pub defaults: HashMap<String, Vec<String>>,
    pub added: HashMap<String, Vec<String>>,
    pub removed: HashMap<String, Vec<String>>,
}

impl MimeAppsList {
    pub fn parse(text: &str) -> Self {
        let mut list = Self::default();
        let mut group: Option<&str> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                group = match name.trim() {
                    "Default Applications" => Some("default"),
                    "Added Associations" => Some("added"),
                    "Removed Associations" => Some("removed"),
                    _ => None,
                };
                continue;
            }
            let (Some(group), Some((mime, ids))) = (group, line.split_once('=')) else {
                continue;
            };
            let ids: Vec<String> = ids
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let target = match group {
                "default" => &mut list.defaults,
                "added" => &mut list.added,
                _ => &mut list.removed,
            };
            target
                .entry(mime.trim().to_string())
                .or_default()
                .extend(ids);
        }
        list
    }
}

/// Parses a `mimeinfo.cache` (`[MIME Cache]` group, `mime=id;id;`).
pub fn parse_mimeinfo_cache(text: &str) -> HashMap<String, Vec<String>> {
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((mime, ids)) = line.split_once('=') {
            cache.entry(mime.trim().to_string()).or_default().extend(
                ids.split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
        }
    }
    cache
}

/// Parses a shared-mime-info `subclasses` file (`child parent` lines).
pub fn parse_subclasses(text: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(child), Some(parent)) = (parts.next(), parts.next()) {
            map.entry(child.to_string())
                .or_default()
                .push(parent.to_string());
        }
    }
    map
}

/// The merged association tables plus the desktop-entry lookup -- see
/// the module doc comment.
pub struct AssociationDb {
    /// `mimeapps.list` files, highest precedence first.
    lists: Vec<MimeAppsList>,
    /// `mimeinfo.cache` contents, highest precedence first.
    caches: Vec<HashMap<String, Vec<String>>>,
    subclasses: HashMap<String, Vec<String>>,
    desktop: DesktopDb,
}

impl std::fmt::Debug for AssociationDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssociationDb")
            .field("lists", &self.lists.len())
            .field("caches", &self.caches.len())
            .field("desktop", &self.desktop)
            .finish_non_exhaustive()
    }
}

/// An application offered for a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub entry: DesktopEntry,
    /// Whether this is the type's default (always the first candidate
    /// when present).
    pub is_default: bool,
}

impl AssociationDb {
    /// Reads every source for the running user: config dirs for
    /// `mimeapps.list`, `application_dirs` (see
    /// `desktop::default_application_dirs`) for their own `mimeapps.list`
    /// and `mimeinfo.cache`, `mime_data_dirs` for `mime/subclasses`.
    pub fn load(
        config_dirs: &[PathBuf],
        application_dirs: Vec<PathBuf>,
        mime_data_dirs: &[PathBuf],
    ) -> Self {
        let mut lists = Vec::new();
        for dir in config_dirs {
            if let Ok(text) = std::fs::read_to_string(dir.join("mimeapps.list")) {
                lists.push(MimeAppsList::parse(&text));
            }
        }
        let mut caches = Vec::new();
        for dir in &application_dirs {
            if let Ok(text) = std::fs::read_to_string(dir.join("mimeapps.list")) {
                lists.push(MimeAppsList::parse(&text));
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("mimeinfo.cache")) {
                caches.push(parse_mimeinfo_cache(&text));
            }
        }
        let mut subclasses: HashMap<String, Vec<String>> = HashMap::new();
        for dir in mime_data_dirs {
            if let Ok(text) = std::fs::read_to_string(dir.join("mime").join("subclasses")) {
                for (child, parents) in parse_subclasses(&text) {
                    subclasses.entry(child).or_default().extend(parents);
                }
            }
        }
        Self {
            lists,
            caches,
            subclasses,
            desktop: DesktopDb::new(application_dirs),
        }
    }

    /// The default config directories: `$XDG_CONFIG_HOME` then each
    /// `$XDG_CONFIG_DIRS` (default `/etc/xdg`).
    pub fn default_config_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")));
        if let Some(home) = config_home {
            dirs.push(home);
        }
        let config_dirs = std::env::var("XDG_CONFIG_DIRS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/etc/xdg".to_string());
        dirs.extend(
            config_dirs
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
        );
        dirs
    }

    pub fn desktop(&self) -> &DesktopDb {
        &self.desktop
    }

    /// Whether `id` is removed for `mime` by any `mimeapps.list`.
    fn is_removed(&self, mime: &str, id: &str) -> bool {
        self.lists.iter().any(|l| {
            l.removed
                .get(mime)
                .is_some_and(|ids| ids.iter().any(|r| r == id))
        })
    }

    /// A usable, installed entry for `id`, unless removed for `mime`.
    fn usable(&self, mime: &str, id: &str) -> Option<DesktopEntry> {
        if self.is_removed(mime, id) {
            return None;
        }
        self.desktop.entry(id).filter(DesktopEntry::is_installed)
    }

    /// The type's own candidates (no parents): the first valid default,
    /// then added associations and cache entries in precedence order.
    fn own_candidates(&self, mime: &str) -> Vec<Candidate> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<Candidate> = Vec::new();
        let mut push = |id: &str, is_default: bool, out: &mut Vec<Candidate>| {
            if seen.contains(id) {
                return false;
            }
            match self.usable(mime, id) {
                Some(entry) => {
                    seen.insert(id.to_string());
                    out.push(Candidate { entry, is_default });
                    true
                }
                None => false,
            }
        };
        'default: for list in &self.lists {
            if let Some(ids) = list.defaults.get(mime) {
                for id in ids {
                    if push(id, true, &mut out) {
                        break 'default;
                    }
                }
            }
        }
        for list in &self.lists {
            if let Some(ids) = list.added.get(mime) {
                for id in ids {
                    push(id, false, &mut out);
                }
            }
        }
        for cache in &self.caches {
            if let Some(ids) = cache.get(mime) {
                for id in ids {
                    push(id, false, &mut out);
                }
            }
        }
        out
    }

    /// Every application that can open `mime`, best first: the type's
    /// default (marked), then its other associations, then those of its
    /// parent types, deduplicated. Empty when nothing is installed for it.
    pub fn candidates(&self, mime: &str) -> Vec<Candidate> {
        let mut out = self.own_candidates(mime);
        let mut seen: HashSet<String> = out.iter().map(|c| c.entry.id.clone()).collect();
        let mut queue: Vec<String> = self.subclasses.get(mime).cloned().unwrap_or_default();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(mime.to_string());
        while !queue.is_empty() {
            let parent = queue.remove(0);
            if !visited.insert(parent.clone()) {
                continue;
            }
            for candidate in self.own_candidates(&parent) {
                if seen.insert(candidate.entry.id.clone()) {
                    out.push(Candidate {
                        entry: candidate.entry,
                        is_default: false,
                    });
                }
            }
            queue.extend(self.subclasses.get(&parent).cloned().unwrap_or_default());
        }
        out
    }

    /// The application to open `mime` with: its default, else the first
    /// candidate.
    pub fn default_for(&self, mime: &str) -> Option<DesktopEntry> {
        self.candidates(mime).into_iter().next().map(|c| c.entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_entry(dir: &Path, id: &str, name: &str, exec: &str, mime: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(id),
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nMimeType={mime};\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn mimeapps_list_and_caches_parse() {
        let list = MimeAppsList::parse(
            "[Default Applications]\ntext/plain=a.desktop;b.desktop;\n\n[Added Associations]\ntext/plain=c.desktop\n[Removed Associations]\ntext/plain=d.desktop;\n[Other]\nx=y\n",
        );
        assert_eq!(list.defaults["text/plain"], ["a.desktop", "b.desktop"]);
        assert_eq!(list.added["text/plain"], ["c.desktop"]);
        assert_eq!(list.removed["text/plain"], ["d.desktop"]);
        let cache = parse_mimeinfo_cache("[MIME Cache]\nimage/png=x.desktop;y.desktop;\n");
        assert_eq!(cache["image/png"], ["x.desktop", "y.desktop"]);
        let sub =
            parse_subclasses("text/x-python text/plain\ntext/x-python application/x-executable\n");
        assert_eq!(
            sub["text/x-python"],
            ["text/plain", "application/x-executable"]
        );
    }

    #[test]
    fn candidates_follow_precedence_removals_installation_and_parents() {
        let config = tempfile::tempdir().unwrap();
        let apps = tempfile::tempdir().unwrap();
        let mime_dir = tempfile::tempdir().unwrap();
        // Installed apps: sh-based so `is_installed` holds; one uninstalled.
        write_entry(
            apps.path(),
            "editor.desktop",
            "Editor",
            "sh -c true %f",
            "text/plain",
        );
        write_entry(
            apps.path(),
            "python-ide.desktop",
            "Python IDE",
            "sh %f",
            "text/x-python",
        );
        write_entry(
            apps.path(),
            "gone.desktop",
            "Gone",
            "definitely-not-installed-xyz %f",
            "text/plain",
        );
        write_entry(
            apps.path(),
            "banned.desktop",
            "Banned",
            "sh %f",
            "text/plain",
        );
        std::fs::write(
            apps.path().join("mimeinfo.cache"),
            "[MIME Cache]\ntext/plain=gone.desktop;banned.desktop;editor.desktop;\ntext/x-python=python-ide.desktop;\n",
        )
        .unwrap();
        std::fs::write(
            config.path().join("mimeapps.list"),
            "[Default Applications]\ntext/plain=gone.desktop;editor.desktop;\n[Removed Associations]\ntext/plain=banned.desktop;\n",
        )
        .unwrap();
        std::fs::create_dir_all(mime_dir.path().join("mime")).unwrap();
        std::fs::write(
            mime_dir.path().join("mime/subclasses"),
            "text/x-python text/plain\n",
        )
        .unwrap();

        let db = AssociationDb::load(
            &[config.path().to_path_buf()],
            vec![apps.path().to_path_buf()],
            &[mime_dir.path().to_path_buf()],
        );
        // gone.desktop is the listed default but isn't installed: editor
        // becomes the default; banned is removed; gone never appears.
        let plain = db.candidates("text/plain");
        let ids: Vec<&str> = plain.iter().map(|c| c.entry.id.as_str()).collect();
        assert_eq!(ids, ["editor.desktop"]);
        assert!(plain[0].is_default);
        assert_eq!(db.default_for("text/plain").unwrap().name, "Editor");

        // Python: its own app first, then the parent's.
        let py = db.candidates("text/x-python");
        let ids: Vec<&str> = py.iter().map(|c| c.entry.id.as_str()).collect();
        assert_eq!(ids, ["python-ide.desktop", "editor.desktop"]);
        assert!(!py[0].is_default, "no default set for the type itself");
        assert_eq!(db.default_for("text/x-python").unwrap().name, "Python IDE");

        assert!(db.candidates("audio/flac").is_empty());
        assert!(db.default_for("audio/flac").is_none());
    }
}
