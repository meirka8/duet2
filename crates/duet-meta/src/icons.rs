// SPDX-License-Identifier: MIT
//! XDG icon-theme resolution (T-4.2.6, FR-CFG-04, design.md §9.8): from
//! an icon *name* (`folder`, `text-x-generic`) and a wanted pixel size to
//! the *file* the current theme provides for it, following the
//! freedesktop Icon Theme Specification's lookup algorithm:
//!
//! 1. Themes are searched in the inheritance chain: the chosen theme,
//!    then each `Inherits=` theme in order (depth-first, each visited
//!    once), then `hicolor` last -- always, even when nothing names it.
//! 2. Within a theme, every `Directories=`/`ScaledDirectories=` entry in
//!    its `index.theme` is a candidate; one whose `Size`/`Type`/
//!    `MinSize`/`MaxSize`/`Threshold` (and `Scale`) accept the wanted
//!    size is an exact match, and the first exact match in directory
//!    order wins. Failing that, the directory whose size range is
//!    *closest* wins (so a theme with only 22 px and 48 px icons still
//!    answers a 16 px request with its 22 px file, which the caller
//!    rasterises to size).
//! 3. Each theme directory is probed under every base directory --
//!    `~/.icons`, `$XDG_DATA_HOME/icons`, each `$XDG_DATA_DIRS/icons`,
//!    and `/usr/share/pixmaps` last -- for `<name>.svg`, `.png`, `.xpm`.
//!    Themes are frequently split across base directories (a distro
//!    theme with a user overlay), which is why the base-dir loop is
//!    inside the theme loop, exactly as the specification orders it.
//!
//! A theme's `index.theme` is a plain INI file; it is parsed once and
//! kept. Lookups are memoised per `(name, size, scale)` in
//! [`IconResolver`], including misses, so a listing full of one file
//! type costs one directory probe, not one per row. Everything here is
//! synchronous file I/O and is meant to run off the UI thread (the
//! caller in `duet-ui` does so); nothing touches GPUI.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The theme every chain ends with, per the specification.
pub const FALLBACK_THEME: &str = "hicolor";

/// How a theme directory matches a requested size (`Type=` in
/// `index.theme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirType {
    Fixed,
    Scalable,
    Threshold,
}

/// One `[subdir]` section of an `index.theme`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDir {
    /// Path relative to the theme directory, e.g. `16x16/mimetypes`.
    pub path: String,
    pub size: u32,
    pub scale: u32,
    dir_type: DirType,
    min_size: u32,
    max_size: u32,
    threshold: u32,
}

impl ThemeDir {
    /// Whether this directory is an exact match for `size` at `scale`
    /// (the specification's `DirectoryMatchesSize`).
    fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.dir_type {
            DirType::Fixed => self.size == size,
            DirType::Scalable => self.min_size <= size && size <= self.max_size,
            DirType::Threshold => {
                self.size.saturating_sub(self.threshold) <= size
                    && size <= self.size + self.threshold
            }
        }
    }

    /// How far `size` at `scale` is from what this directory offers (the
    /// specification's `DirectorySizeDistance`), for the closest-match
    /// fallback. Sizes compare in device pixels (`size * scale`).
    fn distance(&self, size: u32, scale: u32) -> u32 {
        let wanted = size * scale;
        let own = |s: u32| s * self.scale;
        match self.dir_type {
            DirType::Fixed => own(self.size).abs_diff(wanted),
            // Distance outside a [lo, hi] range: exactly one of the two
            // terms is nonzero, both are zero inside it.
            DirType::Scalable => {
                own(self.min_size).saturating_sub(wanted)
                    + wanted.saturating_sub(own(self.max_size))
            }
            DirType::Threshold => {
                let lo = own(self.size.saturating_sub(self.threshold));
                let hi = own(self.size + self.threshold);
                lo.saturating_sub(wanted) + wanted.saturating_sub(hi)
            }
        }
    }
}

/// A parsed `index.theme`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeIndex {
    pub name: String,
    /// `Inherits=`, in order, as written (may name themes that don't
    /// exist; the resolver skips those).
    pub inherits: Vec<String>,
    pub directories: Vec<ThemeDir>,
}

impl ThemeIndex {
    /// Parses an `index.theme` text. `Directories=` and
    /// `ScaledDirectories=` together define which sections count; a
    /// listed directory with no section of its own gets the defaults
    /// (`Type=Threshold`, `Size` unknown -> skipped).
    pub fn parse(text: &str) -> Self {
        let sections = parse_ini(text);
        let main = sections.get("Icon Theme");
        let get = |key: &str| main.and_then(|s| s.get(key)).map(String::as_str);
        let name = get("Name").unwrap_or("").to_string();
        let inherits: Vec<String> = get("Inherits")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut listed: Vec<String> = Vec::new();
        for key in ["Directories", "ScaledDirectories"] {
            if let Some(v) = get(key) {
                listed.extend(
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                );
            }
        }
        let directories = listed
            .into_iter()
            .filter_map(|path| {
                let section = sections.get(&path)?;
                let num = |key: &str| section.get(key).and_then(|v| v.trim().parse::<u32>().ok());
                let size = num("Size")?;
                let dir_type = match section.get("Type").map(|t| t.trim()) {
                    Some("Fixed") => DirType::Fixed,
                    Some("Scalable") => DirType::Scalable,
                    _ => DirType::Threshold,
                };
                Some(ThemeDir {
                    path,
                    size,
                    scale: num("Scale").unwrap_or(1),
                    dir_type,
                    min_size: num("MinSize").unwrap_or(size),
                    max_size: num("MaxSize").unwrap_or(size),
                    threshold: num("Threshold").unwrap_or(2),
                })
            })
            .collect();
        Self {
            name,
            inherits,
            directories,
        }
    }
}

/// Minimal INI reader for `index.theme`: `[section]` headers, `key=value`
/// lines, `#` comments. Localised keys (`Name[de]`) are kept verbatim and
/// simply never asked for.
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(name.trim().to_string());
            sections.entry(name.trim().to_string()).or_default();
        } else if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            sections
                .get_mut(section)
                .expect("section entry inserted on header")
                .insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    sections
}

/// The file extensions probed for an icon, in preference order. SVG
/// first: it rasterises crisply at any size, which is what a HiDPI panel
/// wants; PNG next; XPM last (legacy, rarely present in modern themes).
pub const ICON_EXTENSIONS: [&str; 3] = ["svg", "png", "xpm"];

/// The default XDG icon base directories for the running user: `~/.icons`,
/// `$XDG_DATA_HOME/icons`, every `$XDG_DATA_DIRS/icons`, then
/// `/usr/share/pixmaps`. Only existing directories are returned.
pub fn default_base_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".icons"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share")));
    if let Some(data_home) = data_home {
        dirs.push(data_home.join("icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs.push(Path::new(dir).join("icons"));
    }
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs.retain(|d| d.is_dir());
    dirs
}

/// The user's preferred icon theme name, from the first of: the
/// `DUET_ICON_THEME` environment variable; `gtk-icon-theme-name` in
/// `$XDG_CONFIG_HOME/gtk-4.0/settings.ini` or `gtk-3.0/settings.ini`;
/// `gsettings get org.gnome.desktop.interface icon-theme` (GNOME keeps
/// the real value in dconf, which the INI files usually don't mirror --
/// one short subprocess, run off the UI thread by the caller); else
/// `Adwaita`. `explicit` (a `settings.toml` value other than `"system"`)
/// short-circuits all of it.
pub fn detect_theme_name(explicit: Option<&str>) -> String {
    if let Some(name) = explicit
        .map(str::trim)
        .filter(|n| !n.is_empty() && *n != "system")
    {
        return name.to_string();
    }
    if let Some(name) = std::env::var("DUET_ICON_THEME")
        .ok()
        .filter(|n| !n.trim().is_empty())
    {
        return name.trim().to_string();
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")));
    if let Some(config_home) = config_home {
        for gtk in ["gtk-4.0", "gtk-3.0"] {
            let path = config_home.join(gtk).join("settings.ini");
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(name) = parse_ini(&text)
                    .get("Settings")
                    .and_then(|s| s.get("gtk-icon-theme-name"))
                && !name.is_empty()
            {
                return name.clone();
            }
        }
    }
    if let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        && output.status.success()
    {
        let raw = String::from_utf8_lossy(&output.stdout);
        let name = raw.trim().trim_matches('\'').trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "Adwaita".to_string()
}

/// Memoising icon lookup over a theme chain -- see the module doc
/// comment. `Sync` (a `Mutex` around the memo) so one instance can be
/// shared by every panel and queried from any worker thread.
pub struct IconResolver {
    base_dirs: Vec<PathBuf>,
    /// The chain, resolved: each theme's index, in lookup order.
    chain: Vec<ThemeIndex>,
    memo: Mutex<HashMap<(String, u32, u32), Option<PathBuf>>>,
}

impl std::fmt::Debug for IconResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IconResolver")
            .field("base_dirs", &self.base_dirs)
            .field(
                "chain",
                &self
                    .chain
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl IconResolver {
    /// Builds the chain for `theme` over `base_dirs` (see
    /// [`default_base_dirs`]), reading every reachable `index.theme` once.
    /// A theme that exists in no base directory is dropped from the chain;
    /// `hicolor` is always appended. With no readable theme at all the
    /// resolver is empty and every lookup misses -- a panel without icons,
    /// not an error.
    pub fn new(theme: &str, base_dirs: Vec<PathBuf>) -> Self {
        let mut chain: Vec<ThemeIndex> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut pending: Vec<String> = vec![theme.to_string()];
        while let Some(name) = pending.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            if let Some(index) = read_index(&base_dirs, &name) {
                // Depth-first: a theme's own parents come before whatever
                // was queued after it, so push them in reverse so the
                // first-listed parent pops first.
                for parent in index.inherits.iter().rev() {
                    pending.push(parent.clone());
                }
                chain.push(index);
            }
        }
        if !visited.contains(FALLBACK_THEME)
            && let Some(index) = read_index(&base_dirs, FALLBACK_THEME)
        {
            chain.push(index);
        }
        Self {
            base_dirs,
            chain,
            memo: Mutex::new(HashMap::new()),
        }
    }

    /// The themes in lookup order, by name.
    pub fn chain(&self) -> Vec<&str> {
        self.chain.iter().map(|t| t.name.as_str()).collect()
    }

    /// Whether at least one theme index was found.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    /// The file for icon `name` at `size` logical pixels and integer
    /// `scale`, or `None` if no theme in the chain provides one.
    /// Memoised, misses included.
    pub fn locate(&self, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        let key = (name.to_string(), size, scale);
        if let Some(hit) = self
            .memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return hit.clone();
        }
        let found = self.locate_uncached(name, size, scale);
        self.memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, found.clone());
        found
    }

    /// [`Self::locate`] for the first of `names` that any theme provides
    /// -- the caller's preference order (a MIME type's own icon before its
    /// generic fallback) beats theme order, per the specification's
    /// `FindBestIcon` note.
    pub fn locate_first(&self, names: &[String], size: u32, scale: u32) -> Option<PathBuf> {
        names.iter().find_map(|name| self.locate(name, size, scale))
    }

    fn locate_uncached(&self, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        for theme in &self.chain {
            if let Some(path) = self.lookup_in_theme(theme, name, size, scale) {
                return Some(path);
            }
        }
        // Unthemed fallback: bare files in the base directories
        // (`/usr/share/pixmaps/<name>.png`).
        for base in &self.base_dirs {
            for ext in ICON_EXTENSIONS {
                let candidate = base.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn lookup_in_theme(
        &self,
        theme: &ThemeIndex,
        name: &str,
        size: u32,
        scale: u32,
    ) -> Option<PathBuf> {
        let theme_dirs: Vec<PathBuf> = self.base_dirs.iter().map(|b| b.join(&theme.name)).collect();
        let probe = |dir: &ThemeDir| -> Option<PathBuf> {
            for theme_dir in &theme_dirs {
                for ext in ICON_EXTENSIONS {
                    let candidate = theme_dir.join(&dir.path).join(format!("{name}.{ext}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            None
        };
        for dir in theme.directories.iter().filter(|d| d.matches(size, scale)) {
            if let Some(path) = probe(dir) {
                return Some(path);
            }
        }
        let mut best: Option<(u32, PathBuf)> = None;
        for dir in &theme.directories {
            let distance = dir.distance(size, scale);
            if best.as_ref().is_some_and(|(d, _)| *d <= distance) {
                continue;
            }
            if let Some(path) = probe(dir) {
                best = Some((distance, path));
            }
        }
        best.map(|(_, path)| path)
    }
}

/// Reads `<base>/<theme>/index.theme` from the first base directory that
/// has one; the theme's directory name is what `Inherits=` and the file
/// system agree on, so it is stored as `name` regardless of the
/// human-readable `Name=` inside.
fn read_index(base_dirs: &[PathBuf], theme: &str) -> Option<ThemeIndex> {
    base_dirs.iter().find_map(|base| {
        let text = std::fs::read_to_string(base.join(theme).join("index.theme")).ok()?;
        let mut index = ThemeIndex::parse(&text);
        index.name = theme.to_string();
        Some(index)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEME: &str = "[Icon Theme]\n\
        Name=Fixture\n\
        Inherits=Parent\n\
        Directories=16x16/places,22x22/places,scalable/mimetypes,48x48/apps,16x16@2x/places\n\
        \n\
        [16x16/places]\nSize=16\nContext=Places\nType=Fixed\n\n\
        [22x22/places]\nSize=22\nType=Fixed\n\n\
        [scalable/mimetypes]\nSize=64\nType=Scalable\nMinSize=8\nMaxSize=512\n\n\
        [48x48/apps]\nSize=48\nType=Threshold\nThreshold=4\n\n\
        [16x16@2x/places]\nSize=16\nScale=2\nType=Fixed\n";

    #[test]
    fn index_theme_parses_inheritance_and_directory_rules() {
        let index = ThemeIndex::parse(THEME);
        assert_eq!(index.name, "Fixture");
        assert_eq!(index.inherits, ["Parent"]);
        let paths: Vec<&str> = index.directories.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "16x16/places",
                "22x22/places",
                "scalable/mimetypes",
                "48x48/apps",
                "16x16@2x/places"
            ]
        );
        let dir = |p: &str| index.directories.iter().find(|d| d.path == p).unwrap();
        assert!(dir("16x16/places").matches(16, 1));
        assert!(!dir("16x16/places").matches(17, 1));
        assert!(!dir("16x16/places").matches(16, 2));
        assert!(dir("16x16@2x/places").matches(16, 2));
        assert!(dir("scalable/mimetypes").matches(16, 1));
        assert!(dir("scalable/mimetypes").matches(500, 1));
        assert!(dir("48x48/apps").matches(45, 1));
        assert!(!dir("48x48/apps").matches(40, 1));
        assert_eq!(dir("22x22/places").distance(16, 1), 6);
        assert_eq!(dir("48x48/apps").distance(16, 1), 28);
        assert_eq!(dir("scalable/mimetypes").distance(16, 1), 0);
    }

    /// Builds `<base>/<theme>/index.theme` plus the given icon files.
    fn write_theme(base: &Path, theme: &str, index: &str, files: &[&str]) {
        let root = base.join(theme);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.theme"), index).unwrap();
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"<svg/>").unwrap();
        }
    }

    #[test]
    fn lookup_walks_the_chain_prefers_exact_size_then_closest_and_ends_at_hicolor() {
        let base = tempfile::tempdir().unwrap();
        write_theme(
            base.path(),
            "Fixture",
            THEME,
            &[
                "16x16/places/folder.svg",
                "22x22/places/folder-remote.png",
                "48x48/apps/editor.svg",
            ],
        );
        write_theme(
            base.path(),
            "Parent",
            "[Icon Theme]\nName=Parent\nDirectories=16x16/mimetypes\n\n[16x16/mimetypes]\nSize=16\nType=Fixed\n",
            &["16x16/mimetypes/text-x-generic.png"],
        );
        write_theme(
            base.path(),
            "hicolor",
            "[Icon Theme]\nName=hicolor\nDirectories=16x16/apps\n\n[16x16/apps]\nSize=16\nType=Fixed\n",
            &["16x16/apps/last-resort.png"],
        );
        let resolver = IconResolver::new("Fixture", vec![base.path().to_path_buf()]);
        assert_eq!(resolver.chain(), ["Fixture", "Parent", "hicolor"]);

        let root = base.path().join("Fixture");
        assert_eq!(
            resolver.locate("folder", 16, 1),
            Some(root.join("16x16/places/folder.svg"))
        );
        // No exact 16 px file: the closest directory (22 px) answers.
        assert_eq!(
            resolver.locate("folder-remote", 16, 1),
            Some(root.join("22x22/places/folder-remote.png"))
        );
        // Threshold directory: 48 px accepts 45, and is the closest for 16.
        assert_eq!(
            resolver.locate("editor", 45, 1),
            Some(root.join("48x48/apps/editor.svg"))
        );
        assert_eq!(
            resolver.locate("editor", 16, 1),
            Some(root.join("48x48/apps/editor.svg"))
        );
        // Inherited theme, then hicolor.
        assert_eq!(
            resolver.locate("text-x-generic", 16, 1),
            Some(
                base.path()
                    .join("Parent/16x16/mimetypes/text-x-generic.png")
            )
        );
        assert_eq!(
            resolver.locate("last-resort", 16, 1),
            Some(base.path().join("hicolor/16x16/apps/last-resort.png"))
        );
        assert_eq!(resolver.locate("nope", 16, 1), None);
        // Memoised miss: deleting the file after a hit doesn't change the answer.
        std::fs::remove_file(root.join("16x16/places/folder.svg")).unwrap();
        assert!(resolver.locate("folder", 16, 1).is_some());

        assert_eq!(
            resolver.locate_first(&["nope".to_string(), "folder-remote".to_string()], 16, 1),
            Some(root.join("22x22/places/folder-remote.png"))
        );
    }

    #[test]
    fn a_theme_split_across_base_dirs_and_a_missing_theme_are_handled() {
        let user = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_theme(
            system.path(),
            "Fixture",
            "[Icon Theme]\nName=Fixture\nInherits=Ghost\nDirectories=16x16/places\n\n[16x16/places]\nSize=16\nType=Fixed\n",
            &["16x16/places/folder.svg"],
        );
        // The user overlay has no index.theme, only an extra icon in the
        // same theme directory layout.
        let overlay = user.path().join("Fixture/16x16/places");
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::write(overlay.join("folder.svg"), b"<svg/>").unwrap();
        std::fs::write(overlay.join("user-only.png"), b"png").unwrap();

        let resolver = IconResolver::new(
            "Fixture",
            vec![user.path().to_path_buf(), system.path().to_path_buf()],
        );
        assert_eq!(
            resolver.chain(),
            ["Fixture"],
            "Ghost and hicolor don't exist"
        );
        assert_eq!(
            resolver.locate("folder", 16, 1),
            Some(overlay.join("folder.svg")),
            "the user base dir is probed first"
        );
        assert_eq!(
            resolver.locate("user-only", 16, 1),
            Some(overlay.join("user-only.png"))
        );
        let empty = IconResolver::new("Nothing", vec![user.path().join("void")]);
        assert!(empty.is_empty());
        assert_eq!(empty.locate("folder", 16, 1), None);
    }

    #[test]
    fn explicit_theme_name_and_env_override_beat_detection() {
        assert_eq!(detect_theme_name(Some("Papirus")), "Papirus");
        assert_eq!(detect_theme_name(Some("  Papirus ")), "Papirus");
        // "system" and "" mean "detect"; whatever detection returns, it
        // is never empty.
        assert!(!detect_theme_name(Some("system")).is_empty());
        assert!(!detect_theme_name(Some("")).is_empty());
    }
}
