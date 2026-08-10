//! Creates the fixture files used by write-copy / write-cut and returns their
//! `file://` URIs, matching design.md §9.10's `text/uri-list` format.

use std::fs;
use std::path::PathBuf;

pub const FIXTURE_DIR: &str = "/tmp/duet-s2-test";

/// Ensures the fixture directory and files exist, returning their `file://` URIs
/// in the exact order Nautilus/Dolphin expect for `text/uri-list` (CRLF-terminated
/// lines per RFC 2483, though most Linux desktops tolerate bare `\n`).
pub fn ensure_fixtures() -> Vec<String> {
    fs::create_dir_all(FIXTURE_DIR).expect("failed to create fixture dir");

    let files = ["a.txt", "b.txt"];
    let mut uris = Vec::new();
    for name in files {
        let path: PathBuf = [FIXTURE_DIR, name].iter().collect();
        if !path.exists() {
            fs::write(&path, format!("duet S-2 clipboard spike fixture: {name}\n"))
                .unwrap_or_else(|e| panic!("failed to write fixture {path:?}: {e}"));
        }
        uris.push(format!("file://{}", path.display()));
    }
    uris
}
