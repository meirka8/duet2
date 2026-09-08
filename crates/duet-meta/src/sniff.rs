// SPDX-License-Identifier: MIT
//! Content sniffing for files the name-based database can't classify
//! (T-5.3.4): a script with no extension, a stray download, a binary.
//! Deliberately small -- a dozen unmistakable signatures plus a text
//! heuristic -- rather than the full `magic` database: the name rules
//! already cover almost everything, and the point here is to make Enter
//! do the right thing on `run.sh`-without-the-`.sh` and on an ELF
//! binary, not to compete with `file(1)`.

/// The MIME type suggested by the first bytes of a file, or `None` when
/// nothing here recognises them.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if head.is_empty() {
        return Some("application/x-zerosize");
    }
    if let Some(rest) = head.strip_prefix(b"#!") {
        let line = rest.split(|&b| b == b'\n').next().unwrap_or(rest);
        let line = String::from_utf8_lossy(line).to_ascii_lowercase();
        return Some(if line.contains("python") {
            "text/x-python"
        } else if line.contains("perl") {
            "application/x-perl"
        } else if line.contains("ruby") {
            "application/x-ruby"
        } else if line.contains("node") {
            "application/javascript"
        } else {
            "application/x-shellscript"
        });
    }
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\x7fELF", "application/x-executable"),
        (b"%PDF", "application/pdf"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"BZh", "application/x-bzip2"),
        (b"\xfd7zXZ\x00", "application/x-xz"),
        (b"7z\xbc\xaf\x27\x1c", "application/x-7z-compressed"),
        (b"Rar!\x1a\x07", "application/vnd.rar"),
        (b"SQLite format 3\0", "application/vnd.sqlite3"),
        (b"<?xml", "application/xml"),
        (b"{\\rtf", "application/rtf"),
        (b"OggS", "application/ogg"),
        (b"fLaC", "audio/flac"),
        (b"ID3", "audio/mpeg"),
        (b"RIFF", "audio/x-wav"),
        (b"\x1aE\xdf\xa3", "video/x-matroska"),
    ];
    for (magic, mime) in SIGNATURES {
        if head.starts_with(magic) {
            return Some(mime);
        }
    }
    if head[..head.len().min(4096)].contains(&0) {
        return Some("application/octet-stream");
    }
    // Valid UTF-8, allowing for a multi-byte character cut off by the
    // 4 KiB window (only when there is enough text for that to be the
    // explanation).
    if std::str::from_utf8(head).is_ok()
        || (head.len() >= 8 && std::str::from_utf8(&head[..head.len() - 4]).is_ok())
    {
        return Some("text/plain");
    }
    None
}

/// [`sniff`] on the first 4 KiB of the file at `path`.
pub fn sniff_file(path: &std::path::Path) -> Option<&'static str> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 4096];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    sniff(&head[..read])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_shebangs_and_text_are_recognised() {
        assert_eq!(
            sniff(b"#!/bin/sh\necho hi\n"),
            Some("application/x-shellscript")
        );
        assert_eq!(sniff(b"#!/usr/bin/env python3\n"), Some("text/x-python"));
        assert_eq!(sniff(b"\x7fELF\x02\x01"), Some("application/x-executable"));
        assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(b"PK\x03\x04junk"), Some("application/zip"));
        assert_eq!(sniff(b"plain words\n"), Some("text/plain"));
        assert_eq!(sniff("héllo wörld".as_bytes()), Some("text/plain"));
        assert_eq!(sniff(b"ab\0cd"), Some("application/octet-stream"));
        assert_eq!(sniff(b""), Some("application/x-zerosize"));
        assert_eq!(sniff(&[0xff, 0xfe, 0x80, 0x81]), None);
    }

    #[test]
    fn sniff_file_reads_the_head_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        let mut body = b"#!/bin/bash\n".to_vec();
        body.extend(std::iter::repeat_n(b'x', 100_000));
        std::fs::write(&path, body).unwrap();
        assert_eq!(sniff_file(&path), Some("application/x-shellscript"));
        assert_eq!(sniff_file(&dir.path().join("missing")), None);
    }
}
