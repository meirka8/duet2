// SPDX-License-Identifier: MIT
//! MIME detection, icon lookup, thumbnails, desktop entries, associations
//! (design.md §9.8).
//!
//! T-4.2.6 landed the first two: [`mime`] reads the shared-mime-info
//! database to map a file name to a MIME type and the icon names the
//! Icon Naming Specification derives from it, and [`icons`] resolves an
//! icon name to a file through the XDG icon theme's inheritance chain.
//! [`entry_icon_names`] ties them together for a directory listing.
//! Thumbnails, desktop entries and associations are still their own
//! later tasks; this crate has no GPUI dependency and does only
//! synchronous file I/O, meant to run off the UI thread.

pub mod icons;
pub mod mime;

pub use icons::{IconResolver, ThemeIndex, default_base_dirs, detect_theme_name};
pub use mime::{MimeDb, mime_icon_names};

/// What kind of entry an icon is wanted for -- a subset of
/// `duet_types::EntryKind` that avoids a dependency on it: this crate
/// only needs to know whether a name should be looked up as a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryClass {
    Directory,
    File,
    Symlink,
    /// FIFOs, sockets, device nodes.
    Special,
}

/// The icon names to ask the theme for, best first, for a directory
/// entry: `folder` for directories, `inode-symlink` for symlinks (the
/// listing doesn't follow them, so the target's type is unknown here),
/// `inode-blockdevice`-style names for the rest, and the MIME-derived
/// chain ([`mime_icon_names`]) for files -- `unknown` when the database
/// has no rule for the name.
pub fn entry_icon_names(db: &MimeDb, file_name: &str, class: EntryClass) -> Vec<String> {
    match class {
        EntryClass::Directory => vec!["folder".to_string(), "inode-directory".to_string()],
        EntryClass::Symlink => vec![
            "inode-symlink".to_string(),
            "emblem-symbolic-link".to_string(),
            "unknown".to_string(),
        ],
        EntryClass::Special => vec!["inode-blockdevice".to_string(), "unknown".to_string()],
        EntryClass::File => match db.mime_for_name(file_name) {
            Some(mime) => mime_icon_names(db, mime),
            None => vec!["text-x-generic".to_string(), "unknown".to_string()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_icon_names_cover_every_class() {
        let mut db = MimeDb::default();
        db.parse_globs2("50:text/plain:*.txt\n");
        db.parse_generic_icons("text/plain:text-x-generic\n");
        assert_eq!(
            entry_icon_names(&db, "x", EntryClass::Directory),
            ["folder", "inode-directory"]
        );
        assert_eq!(
            entry_icon_names(&db, "a.txt", EntryClass::File),
            ["text-plain", "text-x-generic", "unknown"]
        );
        assert_eq!(
            entry_icon_names(&db, "noext", EntryClass::File),
            ["text-x-generic", "unknown"]
        );
        assert_eq!(
            entry_icon_names(&db, "l", EntryClass::Symlink)[0],
            "inode-symlink"
        );
        assert_eq!(
            entry_icon_names(&db, "s", EntryClass::Special)[0],
            "inode-blockdevice"
        );
    }
}
