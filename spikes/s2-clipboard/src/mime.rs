//! MIME type constants and payload builders for the clipboard formats named in
//! design.md §9.10: `text/uri-list` (the cross-desktop file list) plus the
//! GNOME and KDE cut markers that distinguish copy from cut/move.

pub const URI_LIST: &str = "text/uri-list";
pub const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
pub const KDE_CUT_SELECTION: &str = "application/x-kde-cutselection";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Cut,
}

impl Mode {
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Copy => "copy",
            Mode::Cut => "cut",
        }
    }

    /// The MIME types we advertise to the data source for this mode.
    pub fn offered_mime_types(self) -> Vec<&'static str> {
        match self {
            Mode::Copy => vec![URI_LIST, GNOME_COPIED_FILES],
            Mode::Cut => vec![URI_LIST, GNOME_COPIED_FILES, KDE_CUT_SELECTION],
        }
    }
}

/// `text/uri-list` payload: one URI per line.
pub fn uri_list_payload(uris: &[String]) -> Vec<u8> {
    let mut s = uris.join("\n");
    s.push('\n');
    s.into_bytes()
}

/// `x-special/gnome-copied-files` payload: `copy\n` or `cut\n` followed by one
/// URI per line, per Nautilus's format.
pub fn gnome_copied_files_payload(mode: Mode, uris: &[String]) -> Vec<u8> {
    let mut s = String::new();
    s.push_str(mode.verb());
    s.push('\n');
    s.push_str(&uris.join("\n"));
    s.push('\n');
    s.into_bytes()
}

/// `application/x-kde-cutselection` payload: KDE/Dolphin convention is the
/// literal string "1" to mark a cut; the MIME type is only offered at all
/// when cutting.
pub fn kde_cut_selection_payload() -> Vec<u8> {
    b"1".to_vec()
}
