// SPDX-License-Identifier: MIT
//! The file clipboard's wire formats (T-5.3.3, FR-CFG-05, design.md
//! §9.10): what Duet puts on the clipboard when files are copied or cut,
//! and how it reads what other file managers put there. Pure encode /
//! decode over byte payloads keyed by MIME type -- the transport (the
//! Wayland data device, through the vendored gpui) is `duet-ui`'s job.
//!
//! Three formats, all of which Nautilus, Dolphin, Thunar and PCManFM
//! understand between them:
//!
//! - `text/uri-list` (RFC 2483): one `file://` URI per line, CRLF
//!   terminated, percent-encoded. Every file manager reads it; it carries
//!   no cut/copy distinction of its own.
//! - `x-special/gnome-copied-files`: the GNOME marker. First line `copy`
//!   or `cut`, then one URI per line (LF). Nautilus, Thunar and PCManFM
//!   write and read it; it is the most complete single format, so it is
//!   preferred when offered.
//! - `application/x-kde-cutselection`: the KDE marker, a single `1` for a
//!   cut (`0` otherwise), always alongside `text/uri-list`. Dolphin's
//!   convention.
//!
//! A plain `text/plain` list of paths is offered as well so a paste into
//! an editor or terminal yields something useful, and is accepted on
//! read as a last resort when it looks like absolute paths or URIs --
//! the case of a path copied from a terminal.
//!
//! `settings.toml`'s `clipboard.cut_marker_convention` picks which
//! markers to *emit* (`auto` = both); reading always accepts either.

use std::path::{Path, PathBuf};

pub const URI_LIST_MIME: &str = "text/uri-list";
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";
pub const KDE_CUT_SELECTION_MIME: &str = "application/x-kde-cutselection";
pub const TEXT_PLAIN_MIME: &str = "text/plain";

/// Which cut markers to emit -- `clipboard.cut_marker_convention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CutMarkerConvention {
    /// Both GNOME's and KDE's.
    #[default]
    Auto,
    Gnome,
    Kde,
}

impl CutMarkerConvention {
    /// Lenient parse of the settings value; unknown -> `Auto`.
    pub fn from_settings_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "gnome" => Self::Gnome,
            "kde" => Self::Kde,
            _ => Self::Auto,
        }
    }
}

/// A set of files on the clipboard and whether they were cut.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// `file://` URI for a local path (RFC 3986 percent-encoding of every
/// byte outside the unreserved set and `/`).
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The local path a `file://` URI names, or `None` for any other scheme,
/// a remote host, or malformed escapes. Non-UTF-8 bytes are kept as-is
/// (Linux paths are bytes).
pub fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` (empty host) or `file://localhost/path`.
    let path_part = if let Some(after_host) = rest.strip_prefix("localhost/") {
        format!("/{after_host}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        return None;
    };
    let bytes = path_part.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

impl FileClipboard {
    /// The MIME payloads to offer, in offer order -- see the module doc
    /// comment. Empty when there are no paths.
    pub fn encode(&self, convention: CutMarkerConvention) -> Vec<(String, Vec<u8>)> {
        if self.paths.is_empty() {
            return Vec::new();
        }
        let uris: Vec<String> = self.paths.iter().map(|p| file_uri(p)).collect();
        let mut payloads: Vec<(String, Vec<u8>)> = Vec::new();
        let mut uri_list = String::new();
        for uri in &uris {
            uri_list.push_str(uri);
            uri_list.push_str("\r\n");
        }
        payloads.push((URI_LIST_MIME.to_string(), uri_list.into_bytes()));
        if matches!(
            convention,
            CutMarkerConvention::Auto | CutMarkerConvention::Gnome
        ) {
            let mut gnome = String::from(if self.cut { "cut" } else { "copy" });
            for uri in &uris {
                gnome.push('\n');
                gnome.push_str(uri);
            }
            payloads.push((GNOME_COPIED_FILES_MIME.to_string(), gnome.into_bytes()));
        }
        if matches!(
            convention,
            CutMarkerConvention::Auto | CutMarkerConvention::Kde
        ) {
            payloads.push((
                KDE_CUT_SELECTION_MIME.to_string(),
                if self.cut {
                    b"1".to_vec()
                } else {
                    b"0".to_vec()
                },
            ));
        }
        payloads
    }

    /// The `text/plain` companion: one path per line.
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for path in &self.paths {
            text.push_str(&path.to_string_lossy());
            text.push('\n');
        }
        text
    }

    /// Decodes whatever a clipboard owner offers, best format first.
    /// `offered` lists the owner's MIME types; `fetch` retrieves one
    /// payload (it is only called for types that are offered). `None`
    /// when nothing on the clipboard looks like files.
    pub fn decode(
        offered: &[String],
        mut fetch: impl FnMut(&str) -> Option<Vec<u8>>,
    ) -> Option<Self> {
        let has = |mime: &str| offered.iter().any(|m| m == mime);
        if has(GNOME_COPIED_FILES_MIME)
            && let Some(bytes) = fetch(GNOME_COPIED_FILES_MIME)
            && let Some(parsed) = parse_gnome_copied_files(&bytes)
        {
            return Some(parsed);
        }
        if has(URI_LIST_MIME)
            && let Some(bytes) = fetch(URI_LIST_MIME)
        {
            let paths = parse_uri_list(&bytes);
            if !paths.is_empty() {
                let cut = has(KDE_CUT_SELECTION_MIME)
                    && fetch(KDE_CUT_SELECTION_MIME)
                        .is_some_and(|b| b.first().is_some_and(|c| *c == b'1'));
                return Some(Self { paths, cut });
            }
        }
        for mime in ["text/plain;charset=utf-8", TEXT_PLAIN_MIME, "UTF8_STRING"] {
            if has(mime)
                && let Some(bytes) = fetch(mime)
            {
                let paths = parse_plain_paths(&bytes);
                if !paths.is_empty() {
                    return Some(Self { paths, cut: false });
                }
                break;
            }
        }
        None
    }
}

/// `x-special/gnome-copied-files`: `copy|cut` then URIs.
pub fn parse_gnome_copied_files(bytes: &[u8]) -> Option<FileClipboard> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    let cut = match lines.next()?.trim() {
        "cut" => true,
        "copy" => false,
        _ => return None,
    };
    let paths: Vec<PathBuf> = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(path_from_file_uri)
        .collect();
    (!paths.is_empty()).then_some(FileClipboard { paths, cut })
}

/// `text/uri-list`: one URI per line, `#` comments, CRLF or LF.
pub fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(path_from_file_uri)
        .collect()
}

/// A last resort for plain text: every line that is an absolute path or
/// a `file://` URI, and only if *every* non-empty line is one -- prose
/// that happens to contain a path is not a file list.
pub fn parse_plain_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(path) = path_from_file_uri(line) {
            paths.push(path);
        } else if line.starts_with('/') {
            paths.push(PathBuf::from(line));
        } else {
            return Vec::new();
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(cut: bool) -> FileClipboard {
        FileClipboard {
            paths: vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/ö/c.txt")],
            cut,
        }
    }

    #[test]
    fn file_uris_round_trip_with_percent_encoding() {
        let path = PathBuf::from("/tmp/a b/ö%#?.txt");
        let uri = file_uri(&path);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%B6%25%23%3F.txt");
        assert_eq!(path_from_file_uri(&uri), Some(path));
        assert_eq!(
            path_from_file_uri("file://localhost/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(
            path_from_file_uri("file://server/share/x"),
            None,
            "remote host"
        );
        assert_eq!(path_from_file_uri("http://x/y"), None);
        assert_eq!(path_from_file_uri("file:///bad%zz"), None);
    }

    #[test]
    fn encode_offers_every_format_the_convention_asks_for() {
        let payloads = files(true).encode(CutMarkerConvention::Auto);
        let mimes: Vec<&str> = payloads.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            mimes,
            [
                URI_LIST_MIME,
                GNOME_COPIED_FILES_MIME,
                KDE_CUT_SELECTION_MIME
            ]
        );
        assert_eq!(
            payloads[0].1,
            b"file:///tmp/a%20b.txt\r\nfile:///tmp/%C3%B6/c.txt\r\n"
        );
        assert_eq!(
            payloads[1].1,
            b"cut\nfile:///tmp/a%20b.txt\nfile:///tmp/%C3%B6/c.txt"
        );
        assert_eq!(payloads[2].1, b"1");

        let copy = files(false).encode(CutMarkerConvention::Gnome);
        let mimes: Vec<&str> = copy.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(mimes, [URI_LIST_MIME, GNOME_COPIED_FILES_MIME]);
        assert!(copy[1].1.starts_with(b"copy\n"));

        let kde = files(false).encode(CutMarkerConvention::Kde);
        let mimes: Vec<&str> = kde.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(mimes, [URI_LIST_MIME, KDE_CUT_SELECTION_MIME]);
        assert_eq!(kde[1].1, b"0");

        assert!(
            FileClipboard::default()
                .encode(CutMarkerConvention::Auto)
                .is_empty()
        );
        assert_eq!(files(false).plain_text(), "/tmp/a b.txt\n/tmp/ö/c.txt\n");
        assert_eq!(
            CutMarkerConvention::from_settings_str(" KDE "),
            CutMarkerConvention::Kde
        );
        assert_eq!(
            CutMarkerConvention::from_settings_str("x"),
            CutMarkerConvention::Auto
        );
    }

    #[test]
    fn decode_prefers_gnome_then_uri_list_with_kde_marker_then_plain_text() {
        let want = files(true);
        // Our own Auto encoding round-trips through every path.
        let payloads = want.encode(CutMarkerConvention::Auto);
        let offered: Vec<String> = payloads.iter().map(|(m, _)| m.clone()).collect();
        let fetch = |mime: &str| {
            payloads
                .iter()
                .find(|(m, _)| m == mime)
                .map(|(_, b)| b.clone())
        };
        assert_eq!(FileClipboard::decode(&offered, fetch), Some(want.clone()));

        // Dolphin-style: uri-list + KDE marker only.
        let kde = want.encode(CutMarkerConvention::Kde);
        let offered: Vec<String> = kde.iter().map(|(m, _)| m.clone()).collect();
        let fetch = |mime: &str| kde.iter().find(|(m, _)| m == mime).map(|(_, b)| b.clone());
        assert_eq!(FileClipboard::decode(&offered, fetch), Some(want.clone()));

        // uri-list alone: a copy.
        let offered = vec![URI_LIST_MIME.to_string()];
        let fetch = |_: &str| Some(b"# comment\r\nfile:///tmp/x\r\n".to_vec());
        assert_eq!(
            FileClipboard::decode(&offered, fetch),
            Some(FileClipboard {
                paths: vec![PathBuf::from("/tmp/x")],
                cut: false
            })
        );

        // Plain text that is a path list.
        let offered = vec!["text/plain;charset=utf-8".to_string()];
        let fetch = |_: &str| Some(b"/etc/hosts\nfile:///tmp/y\n".to_vec());
        assert_eq!(
            FileClipboard::decode(&offered, fetch).map(|c| c.paths),
            Some(vec![PathBuf::from("/etc/hosts"), PathBuf::from("/tmp/y")])
        );
        // Plain prose is not.
        let fetch = |_: &str| Some(b"see /etc/hosts for details\n".to_vec());
        assert_eq!(FileClipboard::decode(&offered, fetch), None);
        // Nothing offered.
        assert_eq!(FileClipboard::decode(&[], |_| None), None);
    }
}
