// SPDX-License-Identifier: MIT
//! Desktop entries (`.desktop` files, the freedesktop Desktop Entry
//! Specification) -- the applications an "Open With" menu lists and the
//! `Exec` lines that launch them (T-5.3.4, FR-TOOL-08, design.md §9.8).
//!
//! Two halves:
//!
//! - [`DesktopEntry::parse`] reads the `[Desktop Entry]` group: `Name`,
//!   `Exec`, `TryExec`, `Path`, `Terminal`, `Icon`, `MimeType`,
//!   `NoDisplay`, `Hidden`, `Type`. Only `Type=Application` entries with
//!   an `Exec` are usable; `Hidden=true` means "deleted" per the spec and
//!   is treated as absent. `NoDisplay` is *not* a reason to skip: it only
//!   hides the entry from menus, and an association to it is still valid
//!   (this is what GLib does too).
//! - [`expand_exec`] turns an `Exec` value plus the files to open into an
//!   `argv`, following the spec's quoting rules (double-quoted arguments
//!   with backslash escapes) and field codes: `%f`/`%F` paths, `%u`/`%U`
//!   `file://` URIs, `%i` the `--icon <Icon>` pair, `%c` the name, `%k`
//!   the entry's own path, `%%` a literal percent; the deprecated codes
//!   (`%d %D %n %N %v %m`) are dropped. An `Exec` with no file code gets
//!   the files appended -- strictly the spec says such an entry takes no
//!   arguments, but `xdg-open`'s fallback appends and that is what users
//!   of hand-written entries expect.
//!
//! [`DesktopDb`] finds an entry by its desktop id (`org.gnome.Evince.desktop`)
//! across every `applications/` directory, including the spec's
//! subdirectory convention (`foo-bar.desktop` may live at
//! `foo/bar.desktop`), and caches what it parsed. Every function here is
//! synchronous file I/O for the caller to run off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A parsed, usable application entry -- see the module doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The desktop id (`org.gnome.Evince.desktop`).
    pub id: String,
    /// Where it was read from (`%k`).
    pub path: PathBuf,
    pub name: String,
    pub exec: String,
    pub try_exec: Option<String>,
    /// `Path=`: the working directory to launch in.
    pub working_dir: Option<PathBuf>,
    pub terminal: bool,
    pub icon: Option<String>,
    pub no_display: bool,
    pub mime_types: Vec<String>,
}

impl DesktopEntry {
    /// Parses a `.desktop` file's text; `None` when it isn't a usable
    /// application entry (wrong `Type`, no `Exec`, `Hidden=true`).
    pub fn parse(id: &str, path: &Path, text: &str) -> Option<Self> {
        let mut in_entry = false;
        let mut fields: HashMap<String, String> = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if in_entry {
                    break; // the main group is done; actions follow
                }
                in_entry = group.trim() == "Desktop Entry";
                continue;
            }
            if !in_entry {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                fields
                    .entry(key.trim().to_string())
                    .or_insert_with(|| value.trim().to_string());
            }
        }
        let flag = |key: &str| fields.get(key).is_some_and(|v| v == "true");
        if fields.get("Type").map(String::as_str) != Some("Application") || flag("Hidden") {
            return None;
        }
        let exec = fields.get("Exec")?.clone();
        if exec.is_empty() {
            return None;
        }
        let name = fields
            .get("Name")
            .cloned()
            .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string());
        Some(Self {
            id: id.to_string(),
            path: path.to_path_buf(),
            name,
            exec,
            try_exec: fields.get("TryExec").cloned().filter(|v| !v.is_empty()),
            working_dir: fields
                .get("Path")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            terminal: flag("Terminal"),
            icon: fields.get("Icon").cloned().filter(|v| !v.is_empty()),
            no_display: flag("NoDisplay"),
            mime_types: fields
                .get("MimeType")
                .map(|v| {
                    v.split(';')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Whether one invocation takes every file (`%F`/`%U` in `Exec`), as
    /// opposed to one file per instance (`%f`/`%u`, or no code at all --
    /// the spec's rule for launching several files with such an entry).
    pub fn accepts_multiple(&self) -> bool {
        split_exec(&self.exec)
            .iter()
            .any(|token| token == "%F" || token == "%U")
    }

    /// Whether the program the entry launches can be found: `TryExec`
    /// when given, else the first `Exec` word, resolved as an absolute
    /// path or through `$PATH`.
    pub fn is_installed(&self) -> bool {
        let program = match &self.try_exec {
            Some(program) => program.clone(),
            None => match exec_argv(&self.exec, &[], self).first() {
                Some(program) => program.clone(),
                None => return false,
            },
        };
        program_exists(&program)
    }
}

/// Whether `program` names an executable: an absolute/relative path that
/// exists, or a bare name found on `$PATH`.
pub fn program_exists(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Splits an `Exec` value into arguments per the spec's quoting rules.
/// The spec unescapes the whole string first (`\s`, `\n`, `\t`, `\r`,
/// `\\`), then reads double-quoted arguments in which `\"`, `` \` ``,
/// `\$` and `\\` are escapes; in practice real entries only ever use
/// quotes around paths with spaces, and that is what this handles.
pub fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                in_arg = true;
            }
            '\\' if in_quotes => match chars.next() {
                Some(escaped @ ('"' | '`' | '$' | '\\')) => current.push(escaped),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            '\\' => match chars.peek() {
                Some('s') => {
                    chars.next();
                    current.push(' ');
                    in_arg = true;
                }
                Some('\\') => {
                    chars.next();
                    current.push('\\');
                    in_arg = true;
                }
                _ => {
                    current.push('\\');
                    in_arg = true;
                }
            },
            c if c.is_whitespace() && !in_quotes => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

/// `file://` URI for a local path, percent-encoding everything outside
/// the RFC 3986 unreserved set and `/`.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || b"-._~/".contains(&c) {
            uri.push(c as char);
        } else {
            uri.push_str(&format!("%{c:02X}"));
        }
    }
    uri
}

/// Expands `exec`'s field codes for `files` -- see the module doc
/// comment. The resulting `argv[0]` is the program.
pub fn expand_exec(exec: &str, files: &[PathBuf], entry: &DesktopEntry) -> Vec<String> {
    exec_argv(exec, files, entry)
}

fn exec_argv(exec: &str, files: &[PathBuf], entry: &DesktopEntry) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    let mut saw_file_code = false;
    for token in split_exec(exec) {
        match token.as_str() {
            "%f" => {
                saw_file_code = true;
                if let Some(file) = files.first() {
                    argv.push(file.to_string_lossy().into_owned());
                }
            }
            "%F" => {
                saw_file_code = true;
                argv.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
            }
            "%u" => {
                saw_file_code = true;
                if let Some(file) = files.first() {
                    argv.push(file_uri(file));
                }
            }
            "%U" => {
                saw_file_code = true;
                argv.extend(files.iter().map(|f| file_uri(f)));
            }
            "%i" => {
                if let Some(icon) = &entry.icon {
                    argv.push("--icon".to_string());
                    argv.push(icon.clone());
                }
            }
            "%c" => argv.push(entry.name.clone()),
            "%k" => argv.push(entry.path.to_string_lossy().into_owned()),
            "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            other => {
                // `%%` inside a longer token, and stray deprecated codes
                // embedded in one, are the only in-token cases the spec
                // allows.
                argv.push(other.replace("%%", "%"));
            }
        }
    }
    if !saw_file_code {
        argv.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
    }
    argv
}

/// The `applications/` directories for the running user, in precedence
/// order: `$XDG_DATA_HOME/applications`, then each
/// `$XDG_DATA_DIRS/applications`. Only existing directories.
pub fn default_application_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share")));
    if let Some(data_home) = data_home {
        dirs.push(data_home.join("applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs.push(Path::new(dir).join("applications"));
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Desktop-id lookup over a set of `applications/` directories, with the
/// parsed entries cached (misses included).
pub struct DesktopDb {
    dirs: Vec<PathBuf>,
    cache: Mutex<HashMap<String, Option<DesktopEntry>>>,
}

impl std::fmt::Debug for DesktopDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopDb")
            .field("dirs", &self.dirs)
            .finish_non_exhaustive()
    }
}

impl DesktopDb {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The usable entry for `id`, from the first directory that has it
    /// (a user-installed copy shadows the system one). `None` when no
    /// directory has it or the file isn't a usable application.
    pub fn entry(&self, id: &str) -> Option<DesktopEntry> {
        if let Some(hit) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return hit.clone();
        }
        let found = self.dirs.iter().find_map(|dir| {
            desktop_file_candidates(dir, id)
                .into_iter()
                .find(|p| p.is_file())
                .and_then(|path| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    DesktopEntry::parse(id, &path, &text)
                })
        });
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), found.clone());
        found
    }
}

/// Where `id` may live under `dir`: as is, then with each leading `-`
/// read as a directory separator (`foo-bar-baz.desktop` -> `foo/bar-baz`,
/// `foo/bar/baz`).
fn desktop_file_candidates(dir: &Path, id: &str) -> Vec<PathBuf> {
    let mut candidates = vec![dir.join(id)];
    let mut relative = id.to_string();
    while let Some(pos) = relative.find('-') {
        relative.replace_range(pos..pos + 1, "/");
        candidates.push(dir.join(&relative));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exec: &str) -> DesktopEntry {
        DesktopEntry {
            id: "app.desktop".into(),
            path: PathBuf::from("/usr/share/applications/app.desktop"),
            name: "My App".into(),
            exec: exec.into(),
            try_exec: None,
            working_dir: None,
            terminal: false,
            icon: Some("my-icon".into()),
            no_display: false,
            mime_types: Vec::new(),
        }
    }

    #[test]
    fn parse_reads_the_main_group_and_rejects_unusable_entries() {
        let text = "[Desktop Entry]\nType=Application\nName=Document Viewer\nName[de]=Dokumentbetrachter\n\
                    Exec=evince %U\nTryExec=evince\nTerminal=false\nIcon=org.gnome.Evince\n\
                    MimeType=application/pdf;image/tiff;\nNoDisplay=true\n\n\
                    [Desktop Action new-window]\nName=New Window\nExec=evince --new-window\n";
        let e =
            DesktopEntry::parse("org.gnome.Evince.desktop", Path::new("/x.desktop"), text).unwrap();
        assert_eq!(e.name, "Document Viewer");
        assert_eq!(e.exec, "evince %U", "the action's Exec must not override");
        assert_eq!(e.try_exec.as_deref(), Some("evince"));
        assert!(e.accepts_multiple(), "%U takes every file");
        assert_eq!(e.mime_types, ["application/pdf", "image/tiff"]);
        assert!(e.no_display);
        assert!(!e.terminal);

        let hidden = "[Desktop Entry]\nType=Application\nExec=x\nHidden=true\n";
        assert!(DesktopEntry::parse("h.desktop", Path::new("/h"), hidden).is_none());
        let link = "[Desktop Entry]\nType=Link\nURL=http://x\n";
        assert!(DesktopEntry::parse("l.desktop", Path::new("/l"), link).is_none());
        let no_exec = "[Desktop Entry]\nType=Application\nName=x\n";
        assert!(DesktopEntry::parse("n.desktop", Path::new("/n"), no_exec).is_none());
        let no_name = "[Desktop Entry]\nType=Application\nExec=x\n";
        assert_eq!(
            DesktopEntry::parse("bare.desktop", Path::new("/b"), no_name)
                .unwrap()
                .name,
            "bare"
        );
    }

    #[test]
    fn split_exec_honours_quotes_and_escapes() {
        assert_eq!(split_exec("evince %U"), ["evince", "%U"]);
        assert_eq!(
            split_exec(r#""/opt/My App/bin/app" --flag "a \"quoted\" arg" %f"#),
            ["/opt/My App/bin/app", "--flag", "a \"quoted\" arg", "%f"]
        );
        assert_eq!(
            split_exec(r"/opt/My\sApp/app  %F"),
            ["/opt/My App/app", "%F"]
        );
        assert_eq!(split_exec(""), Vec::<String>::new());
    }

    #[test]
    fn expand_exec_substitutes_every_field_code() {
        let files = vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")];
        assert_eq!(
            expand_exec("app %f", &files, &entry("")),
            ["app", "/tmp/a b.txt"]
        );
        assert_eq!(
            expand_exec("app %F --x", &files, &entry("")),
            ["app", "/tmp/a b.txt", "/tmp/c.txt", "--x"]
        );
        assert_eq!(
            expand_exec("app %u", &files, &entry("")),
            ["app", "file:///tmp/a%20b.txt"]
        );
        assert_eq!(
            expand_exec("app %U", &files, &entry("")),
            ["app", "file:///tmp/a%20b.txt", "file:///tmp/c.txt"]
        );
        assert_eq!(
            expand_exec("app %i %c %k %d %D 100%% %f", &files[..1], &entry("")),
            [
                "app",
                "--icon",
                "my-icon",
                "My App",
                "/usr/share/applications/app.desktop",
                "100%",
                "/tmp/a b.txt"
            ]
        );
        // No file code at all: the files are appended (xdg-open's rule).
        assert_eq!(
            expand_exec("app --new", &files[..1], &entry("")),
            ["app", "--new", "/tmp/a b.txt"]
        );
        // No files: the code just vanishes.
        assert_eq!(expand_exec("app %f", &[], &entry("")), ["app"]);
        assert_eq!(file_uri(Path::new("/ä/ö.txt")), "file:///%C3%A4/%C3%B6.txt");
    }

    #[test]
    fn program_exists_checks_path_and_absolute_paths() {
        assert!(program_exists("sh"));
        assert!(program_exists("/bin/sh"));
        assert!(!program_exists("definitely-not-a-program-xyz"));
        assert!(!program_exists(""));
    }

    #[test]
    fn desktop_db_finds_ids_in_subdirectories_and_shadows_by_precedence() {
        let user = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, name: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=sh %f\n"),
            )
            .unwrap();
        };
        write(system.path(), "org.example.App.desktop", "System App");
        write(user.path(), "org.example.App.desktop", "User App");
        write(system.path(), "kde/kate.desktop", "Kate");
        write(system.path(), "hidden.desktop", "Hidden");
        std::fs::write(
            system.path().join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nExec=x\nHidden=true\n",
        )
        .unwrap();

        let db = DesktopDb::new(vec![user.path().to_path_buf(), system.path().to_path_buf()]);
        assert_eq!(
            db.entry("org.example.App.desktop").unwrap().name,
            "User App"
        );
        assert_eq!(db.entry("kde-kate.desktop").unwrap().name, "Kate");
        assert!(db.entry("hidden.desktop").is_none());
        assert!(db.entry("missing.desktop").is_none());
        // Cached miss: creating the file afterwards doesn't change the answer.
        write(system.path(), "missing.desktop", "Late");
        assert!(db.entry("missing.desktop").is_none());

        assert_eq!(
            desktop_file_candidates(Path::new("/d"), "a-b-c.desktop"),
            [
                PathBuf::from("/d/a-b-c.desktop"),
                PathBuf::from("/d/a/b-c.desktop"),
                PathBuf::from("/d/a/b/c.desktop"),
            ]
        );
    }
}
