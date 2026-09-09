// SPDX-License-Identifier: MIT
//! T-5.3.3: the file clipboard (FR-CFG-05) -- Ctrl+C / Ctrl+X / Ctrl+V in
//! a panel exchange files with Nautilus, Dolphin, Thunar and PCManFM.
//!
//! The formats live in `duet_platform::clipboard`; this module is the
//! GPUI glue:
//!
//! - **Write**: a copy or cut puts one `ClipboardItem` on the system
//!   clipboard carrying the paths as text (for editors and terminals)
//!   plus the custom MIME payloads (`text/uri-list`, the GNOME and KDE
//!   cut markers) through the vendored gpui's `ClipboardEntry::Custom`
//!   (DUET PATCH, see `vendor/README.md`). Under Wayland gpui's own data
//!   device offers every type; other file managers negotiate the one
//!   they read.
//! - **Read**: a paste asks the clipboard owner which types it offers
//!   and decodes the best one, GNOME's marker first (it carries both the
//!   URIs and cut/copy), then `text/uri-list` with KDE's marker, then a
//!   plain list of paths. Reading blocks on the owner's transfer exactly
//!   as gpui's own text paste does.
//! - **Cut marks** ([`CutMarks`], a global): the entries cut and not yet
//!   pasted, kept per directory so a row's dimming is one hash lookup on
//!   the name. A new copy or cut replaces them; pasting a cut clears
//!   them and the clipboard, as Nautilus does.
//!
//! X11: gpui's X11 backend serves a single target through the
//! `x11-clipboard` crate, so the custom payloads are not offered there
//! -- a copy still puts the paths on the clipboard as text, and a paste
//! reads a text path list. Native Wayland is Duet's target session;
//! `known_issues.md` records the gap.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duet_platform::clipboard::{CutMarkerConvention, FileClipboard, parse_plain_paths};
use gpui::{App, ClipboardItem, Global};

/// The cut-and-not-yet-pasted entries, per directory -- see the module
/// doc comment.
pub(crate) struct CutMarks {
    by_dir: HashMap<PathBuf, Arc<HashSet<String>>>,
    /// Bumped on every change so a table can re-sync its cached set
    /// with one comparison per frame.
    generation: u64,
    convention: CutMarkerConvention,
}

impl Global for CutMarks {}

impl CutMarks {
    pub(crate) fn install(cx: &mut App, convention: CutMarkerConvention) {
        cx.set_global(Self {
            by_dir: HashMap::new(),
            generation: 0,
            convention,
        });
    }

    fn ensure(cx: &mut App) -> &mut Self {
        if !cx.has_global::<Self>() {
            Self::install(cx, CutMarkerConvention::default());
        }
        cx.global_mut::<Self>()
    }

    pub(crate) fn convention(cx: &App) -> CutMarkerConvention {
        cx.try_global::<Self>()
            .map(|m| m.convention)
            .unwrap_or_default()
    }

    /// `0` until anything was cut; changes on every set/clear.
    pub(crate) fn generation(cx: &App) -> u64 {
        cx.try_global::<Self>().map_or(0, |m| m.generation)
    }

    /// The cut entry names in `dir`, if any.
    pub(crate) fn names_for(cx: &App, dir: &Path) -> Option<Arc<HashSet<String>>> {
        cx.try_global::<Self>()
            .and_then(|m| m.by_dir.get(dir).cloned())
    }

    /// Replaces the marks with `paths` (when `cut`) or clears them.
    pub(crate) fn set(cx: &mut App, paths: &[PathBuf], cut: bool) {
        let marks = Self::ensure(cx);
        let mut by_dir: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        if cut {
            for path in paths {
                if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
                    by_dir
                        .entry(dir.to_path_buf())
                        .or_default()
                        .insert(name.to_string_lossy().into_owned());
                }
            }
        }
        marks.by_dir = by_dir
            .into_iter()
            .map(|(dir, names)| (dir, Arc::new(names)))
            .collect();
        marks.generation += 1;
    }

    pub(crate) fn clear(cx: &mut App) {
        let marks = Self::ensure(cx);
        if !marks.by_dir.is_empty() {
            marks.by_dir.clear();
            marks.generation += 1;
        }
    }
}

/// Puts `paths` on the system clipboard as a copy or a cut, and records
/// the cut marks.
pub(crate) fn write_files(cx: &mut App, paths: Vec<PathBuf>, cut: bool) {
    let clipboard = FileClipboard { paths, cut };
    let payloads = clipboard.encode(CutMarks::convention(cx));
    cx.write_to_clipboard(ClipboardItem::new_string_with_custom(
        clipboard.plain_text(),
        payloads,
    ));
    CutMarks::set(cx, &clipboard.paths, cut);
}

/// Copies plain text (paths or names) to the clipboard; clears any cut
/// marks, since the clipboard no longer holds those files.
pub(crate) fn write_text(cx: &mut App, text: String) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    CutMarks::clear(cx);
}

/// Empties the clipboard (after a cut was pasted) and the marks.
pub(crate) fn clear(cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    CutMarks::clear(cx);
}

/// The files on the system clipboard, if it holds any -- see the module
/// doc comment for the negotiation order.
pub(crate) fn read_files(cx: &App) -> Option<FileClipboard> {
    let offered = cx.clipboard_mime_types();
    if !offered.is_empty()
        && let Some(files) = FileClipboard::decode(&offered, |mime| cx.read_clipboard_mime(mime))
    {
        return Some(files);
    }
    // Backends without MIME negotiation (X11, tests with a plain text
    // item): a text clipboard that is a list of paths still pastes.
    let text = cx.read_from_clipboard()?.text()?;
    let paths = parse_plain_paths(text.as_bytes());
    (!paths.is_empty()).then_some(FileClipboard { paths, cut: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn copy_and_cut_round_trip_through_the_clipboard_and_mark_cuts(cx: &mut TestAppContext) {
        let a = PathBuf::from("/tmp/dir/a.txt");
        let b = PathBuf::from("/tmp/dir/b c.txt");
        cx.update(|cx| {
            write_files(cx, vec![a.clone(), b.clone()], false);
            let read = read_files(cx).expect("files on the clipboard");
            assert_eq!(read.paths, [a.clone(), b.clone()]);
            assert!(!read.cut);
            assert!(CutMarks::names_for(cx, Path::new("/tmp/dir")).is_none());
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("/tmp/dir/a.txt\n/tmp/dir/b c.txt\n"),
                "editors get the paths as text"
            );

            let generation = CutMarks::generation(cx);
            write_files(cx, vec![a.clone(), b.clone()], true);
            let read = read_files(cx).unwrap();
            assert!(read.cut);
            let marks = CutMarks::names_for(cx, Path::new("/tmp/dir")).expect("marked");
            assert!(marks.contains("a.txt") && marks.contains("b c.txt"));
            assert!(CutMarks::generation(cx) > generation);

            clear(cx);
            assert!(read_files(cx).is_none());
            assert!(CutMarks::names_for(cx, Path::new("/tmp/dir")).is_none());

            // A plain-text clipboard that lists paths pastes too.
            cx.write_to_clipboard(ClipboardItem::new_string("/etc/hosts\n".to_string()));
            assert_eq!(
                read_files(cx).map(|c| c.paths),
                Some(vec![PathBuf::from("/etc/hosts")])
            );
            cx.write_to_clipboard(ClipboardItem::new_string("hello".to_string()));
            assert!(read_files(cx).is_none());
        });
    }
}
