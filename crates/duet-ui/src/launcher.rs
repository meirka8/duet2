// SPDX-License-Identifier: MIT
//! T-5.3.4: opening files with their applications (FR-TOOL-08). The
//! policy layer over `duet_meta`'s associations and launching:
//!
//! 1. **Classify** the file: the shared-mime-info name rules first
//!    (`MimeDb::mime_for_name`), content sniffing (`duet_meta::sniff`) for
//!    names the database can't place -- an extension-less script, a
//!    downloaded binary.
//! 2. **Overrides** from `settings.toml`'s `[associations.overrides]`
//!    (FR-TOOL-08's "internal association overrides"): a command in
//!    `Exec` syntax keyed by MIME type (`"text/markdown"`) or extension
//!    glob (`"*.md"`); the most specific key wins (exact type, then glob,
//!    then the type's media class such as `"text/*"`).
//! 3. **Executables**: a file with an execute bit whose type is a binary
//!    (`application/x-executable`, `-pie-executable`) runs as itself; an
//!    executable *script* runs in a terminal that stays open until
//!    Enter, so its output is seen -- Total Commander's own behaviour for
//!    Enter on a program. A script without the execute bit is a
//!    document: it opens in the editor its type maps to.
//! 4. **Default application** from the association database
//!    ([`duet_meta::AssociationDb::default_for`]), else the first
//!    candidate for the type, else a clear notice naming the type.
//!
//! The tables (`MimeDb` + `AssociationDb`) are loaded once per process,
//! lazily, on the first launch -- on the Tokio runtime, never the UI
//! thread -- and cached in a `OnceLock`; a session doesn't pick up
//! `mimeapps.list` edits made while it runs (known_issues.md). The
//! spawned child is reaped by a small named thread so it never lingers
//! as a zombie.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use duet_meta::{AssociationDb, LaunchPlan, MimeDb};
use gpui::{App, Global};

/// `[associations.overrides]` from `settings.toml`, installed as a
/// global once at startup (like every other settings-derived value).
pub(crate) struct LaunchSettings {
    pub(crate) overrides: Arc<BTreeMap<String, String>>,
}

impl Global for LaunchSettings {}

impl LaunchSettings {
    pub(crate) fn install(cx: &mut App, overrides: BTreeMap<String, String>) {
        cx.set_global(Self {
            overrides: Arc::new(overrides),
        });
    }

    /// The installed overrides, or none.
    pub(crate) fn overrides(cx: &App) -> Arc<BTreeMap<String, String>> {
        cx.try_global::<Self>()
            .map(|s| s.overrides.clone())
            .unwrap_or_default()
    }
}

/// The lookup tables -- see the module doc comment.
pub(crate) struct LaunchTables {
    mime: MimeDb,
    associations: AssociationDb,
}

impl LaunchTables {
    /// Loads the running user's databases. File I/O; call off the UI
    /// thread.
    fn load() -> Self {
        let data_dirs = mime_data_dirs();
        Self {
            mime: MimeDb::load(&data_dirs),
            associations: AssociationDb::load(
                &AssociationDb::default_config_dirs(),
                duet_meta::default_application_dirs(),
                &data_dirs,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(mime: MimeDb, associations: AssociationDb) -> Self {
        Self { mime, associations }
    }

    /// The MIME type of `path`: by name, else by content, else
    /// `application/octet-stream`.
    pub(crate) fn mime_of(&self, path: &Path) -> String {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(mime) = self.mime.mime_for_name(&name) {
            return mime.to_string();
        }
        duet_meta::sniff_file(path)
            .unwrap_or("application/octet-stream")
            .to_string()
    }
}

/// `$XDG_DATA_HOME` then each `$XDG_DATA_DIRS`, where `mime/` lives.
fn mime_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share")))
    {
        dirs.push(home);
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

static TABLES: OnceLock<Arc<LaunchTables>> = OnceLock::new();

/// The process-wide tables, loaded on first use (blocking file I/O --
/// call on the Tokio runtime).
pub(crate) fn tables() -> Arc<LaunchTables> {
    TABLES
        .get_or_init(|| Arc::new(LaunchTables::load()))
        .clone()
}

/// An application offered in the "Open With" chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppChoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) is_default: bool,
    pub(crate) terminal: bool,
}

/// The override command for `mime`/`file_name`, most specific key first
/// -- see the module doc comment.
pub(crate) fn override_for<'a>(
    overrides: &'a BTreeMap<String, String>,
    mime: &str,
    file_name: &str,
) -> Option<&'a str> {
    if let Some(cmd) = overrides.get(mime) {
        return Some(cmd);
    }
    let lower = file_name.to_lowercase();
    for (ix, _) in lower.match_indices('.') {
        if ix == 0 {
            continue;
        }
        if let Some(cmd) = overrides.get(&format!("*.{}", &lower[ix + 1..])) {
            return Some(cmd);
        }
    }
    if let Some((media, _)) = mime.split_once('/')
        && let Some(cmd) = overrides.get(&format!("{media}/*"))
    {
        return Some(cmd);
    }
    None
}

fn is_binary_executable(mime: &str) -> bool {
    matches!(
        mime,
        "application/x-executable"
            | "application/x-pie-executable"
            | "application/vnd.appimage"
            | "application/x-appimage"
    )
}

fn is_script(mime: &str) -> bool {
    matches!(
        mime,
        "application/x-shellscript"
            | "text/x-shellscript"
            | "text/x-python"
            | "text/x-python3"
            | "application/x-python"
            | "application/x-perl"
            | "text/x-perl"
            | "application/x-ruby"
            | "text/x-ruby"
            | "application/javascript"
            | "text/javascript"
            | "text/x-lua"
    )
}

/// The plan Enter on `path` runs -- see the module doc comment.
/// `executable`: the file has an execute bit for the user.
pub(crate) fn resolve_open(
    tables: &LaunchTables,
    overrides: &BTreeMap<String, String>,
    path: &Path,
    executable: bool,
) -> Result<LaunchPlan, String> {
    let mime = tables.mime_of(path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(command) = override_for(overrides, &mime, &file_name) {
        return LaunchPlan::for_command(command, std::slice::from_ref(&path.to_path_buf()))
            .ok_or_else(|| format!("empty override command for {mime}"));
    }
    if executable && is_binary_executable(&mime) {
        return Ok(LaunchPlan::for_executable(path, false));
    }
    if executable && is_script(&mime) {
        return Ok(LaunchPlan::for_executable(path, true));
    }
    let entry = tables
        .associations
        .default_for(&mime)
        .ok_or_else(|| format!("No application is associated with {mime} ({file_name})"))?;
    LaunchPlan::for_entry(&entry, std::slice::from_ref(&path.to_path_buf()))
        .ok_or_else(|| format!("{} has no usable Exec line", entry.name))
}

/// The applications to offer for `path`, default first, and its type.
pub(crate) fn choices_for(tables: &LaunchTables, path: &Path) -> (String, Vec<AppChoice>) {
    let mime = tables.mime_of(path);
    let choices = tables
        .associations
        .candidates(&mime)
        .into_iter()
        .map(|c| AppChoice {
            id: c.entry.id.clone(),
            name: c.entry.name.clone(),
            is_default: c.is_default,
            terminal: c.entry.terminal,
        })
        .collect();
    (mime, choices)
}

/// The plan for opening `path` with the application `id` picked in the
/// chooser.
pub(crate) fn plan_for_choice(
    tables: &LaunchTables,
    id: &str,
    path: &Path,
) -> Result<LaunchPlan, String> {
    let entry = tables
        .associations
        .desktop()
        .entry(id)
        .ok_or_else(|| format!("{id} is no longer installed"))?;
    LaunchPlan::for_entry(&entry, std::slice::from_ref(&path.to_path_buf()))
        .ok_or_else(|| format!("{} has no usable Exec line", entry.name))
}

/// Starts `plan` detached and reaps it in the background. The error is
/// user-readable ("program not found: evince").
pub(crate) fn launch(plan: &LaunchPlan) -> Result<(), String> {
    let mut child = duet_meta::spawn(plan).map_err(|err| format!("{}: {err}", plan.label))?;
    let label = plan.label.clone();
    // A thread rather than a Tokio task: it may outlive the runtime that
    // called us, and `wait` blocks.
    let _ = std::thread::Builder::new()
        .name("duet-reap".to_string())
        .spawn(move || {
            let _ = child.wait();
            tracing::debug!(target: "duet_ui::launcher", "{label} exited");
        });
    Ok(())
}

/// [`resolve_open`] + [`launch`] with the process-wide tables: the whole
/// of Enter-on-a-file, for the Tokio runtime.
pub(crate) fn open_default(
    overrides: &BTreeMap<String, String>,
    path: &Path,
    executable: bool,
) -> Result<LaunchPlan, String> {
    let tables = tables();
    let plan = resolve_open(&tables, overrides, path, executable)?;
    launch(&plan)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, LaunchTables) {
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(
            apps.join("editor.desktop"),
            "[Desktop Entry]\nType=Application\nName=Editor\nExec=sh -c true %f\nMimeType=text/plain;\n",
        )
        .unwrap();
        std::fs::write(
            apps.join("viewer.desktop"),
            "[Desktop Entry]\nType=Application\nName=Viewer\nExec=sh -c true %U\nTerminal=false\nMimeType=application/pdf;\n",
        )
        .unwrap();
        std::fs::write(
            apps.join("mimeinfo.cache"),
            "[MIME Cache]\ntext/plain=editor.desktop;\napplication/pdf=viewer.desktop;\n",
        )
        .unwrap();
        let mime_dir = root.path().join("mime");
        std::fs::create_dir_all(&mime_dir).unwrap();
        std::fs::write(
            mime_dir.join("subclasses"),
            "application/x-shellscript text/plain\ntext/x-python text/plain\n",
        )
        .unwrap();
        let mut mime = MimeDb::default();
        mime.parse_globs2("50:text/plain:*.txt\n50:application/pdf:*.pdf\n50:application/x-shellscript:*.sh\n50:text/markdown:*.md\n");
        let associations = AssociationDb::load(&[], vec![apps], &[root.path().to_path_buf()]);
        (root, LaunchTables::from_parts(mime, associations))
    }

    #[test]
    fn classification_uses_names_then_content() {
        let (root, tables) = fixture();
        let txt = root.path().join("a.txt");
        std::fs::write(&txt, "hi").unwrap();
        assert_eq!(tables.mime_of(&txt), "text/plain");
        let script = root.path().join("run");
        std::fs::write(&script, "#!/bin/sh\necho\n").unwrap();
        assert_eq!(tables.mime_of(&script), "application/x-shellscript");
        let blob = root.path().join("blob");
        std::fs::write(&blob, [0u8, 1, 2, 3, 0xff]).unwrap();
        assert_eq!(tables.mime_of(&blob), "application/octet-stream");
        assert_eq!(
            tables.mime_of(Path::new("/nonexistent/x")),
            "application/octet-stream"
        );
    }

    #[test]
    fn overrides_match_type_then_extension_then_media_class() {
        let mut overrides = BTreeMap::new();
        overrides.insert("text/markdown".to_string(), "mdviewer %f".to_string());
        overrides.insert("*.tar.gz".to_string(), "untar %f".to_string());
        overrides.insert("image/*".to_string(), "imgview %f".to_string());
        assert_eq!(
            override_for(&overrides, "text/markdown", "README.md"),
            Some("mdviewer %f")
        );
        assert_eq!(
            override_for(&overrides, "application/gzip", "A.TAR.GZ"),
            Some("untar %f")
        );
        assert_eq!(
            override_for(&overrides, "image/png", "x.png"),
            Some("imgview %f")
        );
        assert_eq!(override_for(&overrides, "text/plain", "x.txt"), None);
    }

    #[test]
    fn resolve_open_applies_the_policy_in_order() {
        let (root, tables) = fixture();
        let overrides: BTreeMap<String, String> = BTreeMap::new();

        let pdf = root.path().join("doc.pdf");
        std::fs::write(&pdf, "%PDF-1.4").unwrap();
        let plan = resolve_open(&tables, &overrides, &pdf, false).unwrap();
        assert_eq!(plan.label, "Viewer");
        assert_eq!(plan.argv[..3], ["sh", "-c", "true"]);
        assert!(plan.argv[3].starts_with("file://"), "%U gives a URI");

        // An executable script runs in a terminal; the same script
        // without the bit opens as a document (subclass of text/plain).
        let script = root.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let plan = resolve_open(&tables, &overrides, &script, true).unwrap();
        assert_eq!(plan.argv, [script.to_string_lossy().into_owned()]);
        assert!(plan.terminal);
        let plan = resolve_open(&tables, &overrides, &script, false).unwrap();
        assert_eq!(plan.label, "Editor");

        // An executable binary runs as itself, no terminal.
        let bin = root.path().join("prog");
        std::fs::write(&bin, b"\x7fELF\x02").unwrap();
        let plan = resolve_open(&tables, &overrides, &bin, true).unwrap();
        assert_eq!(plan.argv, [bin.to_string_lossy().into_owned()]);
        assert!(!plan.terminal);
        // ...but not without the bit.
        let err = resolve_open(&tables, &overrides, &bin, false).unwrap_err();
        assert!(err.contains("application/x-executable"), "{err}");

        // An override beats everything.
        let mut overrides = BTreeMap::new();
        overrides.insert("*.pdf".to_string(), "mypdf --view %f".to_string());
        let plan = resolve_open(&tables, &overrides, &pdf, false).unwrap();
        assert_eq!(
            plan.argv,
            vec![
                "mypdf".to_string(),
                "--view".to_string(),
                pdf.to_string_lossy().into_owned()
            ]
        );

        // No association: a notice naming the type and file.
        let md = root.path().join("notes.md");
        std::fs::write(&md, "# x").unwrap();
        let err = resolve_open(&tables, &BTreeMap::new(), &md, false).unwrap_err();
        assert!(
            err.contains("text/markdown") && err.contains("notes.md"),
            "{err}"
        );
    }

    #[test]
    fn choices_list_the_default_first_and_plans_for_a_choice() {
        let (root, tables) = fixture();
        let script = root.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let (mime, choices) = choices_for(&tables, &script);
        assert_eq!(mime, "application/x-shellscript");
        assert_eq!(choices.len(), 1, "the parent type's editor");
        assert_eq!(choices[0].name, "Editor");
        assert!(
            !choices[0].is_default,
            "inherited, not the type's own default"
        );
        let plan = plan_for_choice(&tables, "editor.desktop", &script).unwrap();
        assert_eq!(plan.label, "Editor");
        assert!(plan_for_choice(&tables, "gone.desktop", &script).is_err());
    }

    #[test]
    fn launch_runs_the_plan_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let plan = LaunchPlan {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("echo launched > '{}'", marker.display()),
            ],
            cwd: None,
            terminal: false,
            label: "sh".into(),
        };
        launch(&plan).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists(), "the detached child ran");
        let missing = LaunchPlan {
            argv: vec!["definitely-not-installed-xyz".into()],
            cwd: None,
            terminal: false,
            label: "Nope".into(),
        };
        let err = launch(&missing).unwrap_err();
        assert!(err.starts_with("Nope: program not found"), "{err}");
    }
}
