// SPDX-License-Identifier: MIT
//! Content search: "does this file contain the text" (or, inverted,
//! "does it not").
//!
//! # One byte regex for every case
//!
//! Literal and regex searches, case folding and whole words all compile
//! to a single `regex::bytes::Regex` run over raw file bytes. The regex
//! crate already turns a literal (or an alternation of literals) into a
//! `memchr`/Teddy prefilter, so a literal search costs what a hand-rolled
//! `memmem` loop would, and there is one scanning loop to get right
//! instead of two.
//!
//! # Encodings
//!
//! A file is searched as bytes; what changes with the encoding is the
//! *pattern*. For a literal, each chosen encoding contributes the text
//! encoded that way as one more alternative -- UTF-16 LE and BE, or one
//! of the legacy code pages the viewer also offers (windows-1252/1251/
//! 1255, KOI8-R). Case-insensitivity in those alternatives is spelled out
//! per character (`[Aa]` as bytes, `(?:A\0|a\0)` for UTF-16), since the
//! regex engine only knows how to fold UTF-8. A regex can only be
//! searched as UTF-8 (translating an arbitrary regex into another
//! encoding's byte patterns is not something the engine offers), so a
//! regex with a non-UTF-8 encoding chosen is refused at compile time
//! rather than silently searched as UTF-8. Whole words use ASCII word
//! boundaries and apply to the UTF-8 alternative only.
//!
//! # Reading in windows, lines kept whole
//!
//! The file is read in [`CHUNK`]-byte reads and searched one window at a
//! time, each window ending at its last line break (the rest carries into
//! the next). Every window therefore starts and ends on a line boundary,
//! which makes `^`/`$` (multi-line, CRLF-aware) mean what they mean in
//! grep, and means a match never straddles two windows unless it spans a
//! line break itself -- which needs an explicit `\n` in a regex, and is
//! not supported (grep does not either). A single line longer than
//! [`MAX_LINE`] is searched in `MAX_LINE` windows overlapping by
//! [`OVERLAP`] bytes, so a match inside a megabyte-long minified line is
//! still found unless it is longer than `OVERLAP` itself.
//!
//! # Binary files
//!
//! Searched like any other file by default, as Total Commander does.
//! With `skip_binary` a file whose first 8 KiB contain a NUL byte (and
//! that does not start with a UTF-16 byte-order mark) is skipped -- it is
//! neither a match nor, for "not containing", a non-match.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use regex::bytes::{Regex, RegexBuilder};

/// Bytes per read.
pub(crate) const CHUNK: usize = 256 * 1024;
/// The longest line searched as one window.
pub(crate) const MAX_LINE: usize = 1024 * 1024;
/// How much of an over-long line the next window repeats.
pub(crate) const OVERLAP: usize = 4096;
/// How much of a file's head the binary heuristic looks at.
const BINARY_SNIFF: usize = 8192;

/// Which encodings a content search covers, in the order `Ctrl+E` in the
/// dialog cycles through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentEncodings {
    #[default]
    Utf8,
    /// UTF-8 plus UTF-16 LE and BE: what to pick when you don't know.
    Utf8AndUtf16,
    Utf16,
    Windows1252,
    Windows1251,
    Windows1255,
    Koi8R,
}

impl ContentEncodings {
    pub const ALL: [ContentEncodings; 7] = [
        ContentEncodings::Utf8,
        ContentEncodings::Utf8AndUtf16,
        ContentEncodings::Utf16,
        ContentEncodings::Windows1252,
        ContentEncodings::Windows1251,
        ContentEncodings::Windows1255,
        ContentEncodings::Koi8R,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ContentEncodings::Utf8 => "UTF-8",
            ContentEncodings::Utf8AndUtf16 => "UTF-8 + UTF-16",
            ContentEncodings::Utf16 => "UTF-16",
            ContentEncodings::Windows1252 => "windows-1252",
            ContentEncodings::Windows1251 => "windows-1251",
            ContentEncodings::Windows1255 => "windows-1255",
            ContentEncodings::Koi8R => "KOI8-R",
        }
    }

    pub fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|&e| e == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    fn includes_utf8(self) -> bool {
        matches!(
            self,
            ContentEncodings::Utf8 | ContentEncodings::Utf8AndUtf16
        )
    }

    fn includes_utf16(self) -> bool {
        matches!(
            self,
            ContentEncodings::Utf8AndUtf16 | ContentEncodings::Utf16
        )
    }

    fn legacy(self) -> Option<&'static encoding_rs::Encoding> {
        match self {
            ContentEncodings::Windows1252 => Some(encoding_rs::WINDOWS_1252),
            ContentEncodings::Windows1251 => Some(encoding_rs::WINDOWS_1251),
            ContentEncodings::Windows1255 => Some(encoding_rs::WINDOWS_1255),
            ContentEncodings::Koi8R => Some(encoding_rs::KOI8_R),
            _ => None,
        }
    }
}

/// What the user asked for -- plain data, compiled by
/// [`ContentMatcher::new`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContentSpec {
    pub text: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_words: bool,
    /// Report the files that do *not* contain the text.
    pub not_containing: bool,
    pub encodings: ContentEncodings,
    pub skip_binary: bool,
}

/// Why a [`ContentSpec`] cannot be searched for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContentError {
    #[error("the text to search for is empty")]
    Empty,
    #[error("invalid regular expression: {0}")]
    Regex(String),
    #[error("a regular expression can only be searched as UTF-8 (choose UTF-8 with Ctrl+E)")]
    RegexNotUtf8,
    #[error("the text cannot be written in {0}")]
    Unrepresentable(&'static str),
}

/// A compiled content criterion. Cheap to share between workers (`Regex`
/// is `Sync`); each worker brings its own read buffer.
#[derive(Debug, Clone)]
pub struct ContentMatcher {
    re: Regex,
    not_containing: bool,
    skip_binary: bool,
    /// UTF-16 alternatives are in play, so a UTF-16 BOM marks text.
    utf16: bool,
}

/// The outcome of checking one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileVerdict {
    Match,
    NoMatch,
    /// Binary (with `skip_binary`) or the search was cancelled mid-file.
    Skipped,
}

impl ContentMatcher {
    pub fn new(spec: &ContentSpec) -> Result<Self, ContentError> {
        if spec.text.is_empty() {
            return Err(ContentError::Empty);
        }
        let pattern = if spec.regex {
            if spec.encodings != ContentEncodings::Utf8 {
                return Err(ContentError::RegexNotUtf8);
            }
            word_bounded(&spec.text, spec.whole_words)
        } else {
            literal_pattern(spec)?
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(spec.regex && !spec.case_sensitive)
            .multi_line(true)
            .crlf(true)
            // Generous: a long alternation of encodings and case variants
            // is still tiny, but a user regex may legitimately be large.
            .size_limit(64 * 1024 * 1024)
            .build()
            .map_err(|e| ContentError::Regex(e.to_string()))?;
        Ok(Self {
            re,
            not_containing: spec.not_containing,
            skip_binary: spec.skip_binary,
            utf16: spec.encodings.includes_utf16(),
        })
    }

    /// Reads `file` to the end (or the first match) and decides. `buf` is
    /// the worker's reusable window buffer; `bytes_read` accumulates for
    /// progress; `cancel` aborts between reads.
    pub fn check(
        &self,
        file: &mut impl Read,
        buf: &mut Vec<u8>,
        bytes_read: &AtomicU64,
        cancel: &AtomicBool,
    ) -> io::Result<FileVerdict> {
        match self.contains(file, buf, bytes_read, cancel)? {
            None => Ok(FileVerdict::Skipped),
            Some(found) if found != self.not_containing => Ok(FileVerdict::Match),
            Some(_) => Ok(FileVerdict::NoMatch),
        }
    }

    /// `Some(found)`, or `None` when the file was skipped.
    fn contains(
        &self,
        file: &mut impl Read,
        buf: &mut Vec<u8>,
        bytes_read: &AtomicU64,
        cancel: &AtomicBool,
    ) -> io::Result<Option<bool>> {
        let mut carry = 0usize;
        let mut first = true;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if buf.len() < carry + CHUNK {
                buf.resize(carry + CHUNK, 0);
            }
            let n = match file.read(&mut buf[carry..carry + CHUNK]) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            bytes_read.fetch_add(n as u64, Ordering::Relaxed);
            if first {
                first = false;
                if self.skip_binary && self.looks_binary(&buf[..n]) {
                    return Ok(None);
                }
            }
            if n == 0 {
                return Ok(Some(carry > 0 && self.re.is_match(&buf[..carry])));
            }
            let filled = carry + n;
            match memchr::memrchr(b'\n', &buf[..filled]) {
                Some(nl) => {
                    // Up to, not including, the newline: `$` then matches
                    // at the true end of the last line.
                    if self.re.is_match(&buf[..nl]) {
                        return Ok(Some(true));
                    }
                    buf.copy_within(nl + 1..filled, 0);
                    carry = filled - nl - 1;
                }
                None if filled >= MAX_LINE => {
                    if self.re.is_match(&buf[..filled]) {
                        return Ok(Some(true));
                    }
                    buf.copy_within(filled - OVERLAP..filled, 0);
                    carry = OVERLAP;
                }
                None => carry = filled,
            }
        }
    }

    fn looks_binary(&self, head: &[u8]) -> bool {
        let head = &head[..head.len().min(BINARY_SNIFF)];
        if self.utf16 && (head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF])) {
            return false;
        }
        memchr::memchr(0, head).is_some()
    }

    /// The regex run over in-memory bytes -- for tests and callers that
    /// already hold the content.
    pub fn is_match(&self, haystack: &[u8]) -> bool {
        self.re.is_match(haystack) != self.not_containing
    }
}

fn word_bounded(pattern: &str, whole_words: bool) -> String {
    if whole_words {
        format!(r"(?-u:\b)(?:{pattern})(?-u:\b)")
    } else {
        format!("(?:{pattern})")
    }
}

/// The alternation of `spec.text` in every encoding `spec.encodings`
/// covers. See the module doc comment.
fn literal_pattern(spec: &ContentSpec) -> Result<String, ContentError> {
    let fold = !spec.case_sensitive;
    let mut alternatives: Vec<String> = Vec::new();
    if spec.encodings.includes_utf8() {
        let escaped = regex::escape(&spec.text);
        let body = if fold {
            format!("(?i:{escaped})")
        } else {
            escaped
        };
        alternatives.push(word_bounded(&body, spec.whole_words));
    }
    if spec.encodings.includes_utf16() {
        alternatives.push(raw_alternative(&spec.text, fold, |c| {
            let mut units = [0u16; 2];
            Some(
                c.encode_utf16(&mut units)
                    .iter()
                    .flat_map(|u| u.to_le_bytes())
                    .collect(),
            )
        }));
        alternatives.push(raw_alternative(&spec.text, fold, |c| {
            let mut units = [0u16; 2];
            Some(
                c.encode_utf16(&mut units)
                    .iter()
                    .flat_map(|u| u.to_be_bytes())
                    .collect(),
            )
        }));
    }
    if let Some(encoding) = spec.encodings.legacy() {
        let encode = |c: char| {
            let mut s = [0u8; 4];
            let (bytes, _, had_errors) = encoding.encode(c.encode_utf8(&mut s));
            (!had_errors).then(|| bytes.into_owned())
        };
        if spec.text.chars().any(|c| encode(c).is_none()) {
            return Err(ContentError::Unrepresentable(encoding.name()));
        }
        alternatives.push(raw_alternative(&spec.text, fold, encode));
    }
    alternatives.dedup();
    Ok(alternatives.join("|"))
}

/// `text` as a raw-byte regex fragment, each character encoded by
/// `encode` -- with its other-case forms as alternatives when `fold`.
/// Characters `encode` cannot represent contribute only the forms it can
/// (the caller has already checked the characters themselves).
fn raw_alternative(text: &str, fold: bool, encode: impl Fn(char) -> Option<Vec<u8>>) -> String {
    let mut out = String::from("(?-iu:");
    for c in text.chars() {
        let mut forms: Vec<Vec<u8>> = Vec::with_capacity(3);
        for variant in case_variants(c, fold) {
            if let Some(bytes) = encode(variant)
                && !forms.contains(&bytes)
            {
                forms.push(bytes);
            }
        }
        match forms.as_slice() {
            [] => {}
            [only] => push_bytes(&mut out, only),
            many => {
                out.push_str("(?:");
                for (ix, form) in many.iter().enumerate() {
                    if ix > 0 {
                        out.push('|');
                    }
                    push_bytes(&mut out, form);
                }
                out.push(')');
            }
        }
    }
    out.push(')');
    out
}

/// `c`, plus its single-character lower- and upper-case forms when `fold`.
fn case_variants(c: char, fold: bool) -> Vec<char> {
    let mut out = vec![c];
    if fold {
        let mut lower = c.to_lowercase();
        if let (Some(l), None) = (lower.next(), lower.next())
            && !out.contains(&l)
        {
            out.push(l);
        }
        let mut upper = c.to_uppercase();
        if let (Some(u), None) = (upper.next(), upper.next())
            && !out.contains(&u)
        {
            out.push(u);
        }
    }
    out
}

fn push_bytes(out: &mut String, bytes: &[u8]) {
    use std::fmt::Write as _;
    for b in bytes {
        let _ = write!(out, "\\x{b:02X}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> ContentSpec {
        ContentSpec {
            text: text.to_string(),
            ..ContentSpec::default()
        }
    }

    fn check(matcher: &ContentMatcher, content: &[u8]) -> FileVerdict {
        let mut buf = Vec::new();
        matcher
            .check(
                &mut std::io::Cursor::new(content),
                &mut buf,
                &AtomicU64::new(0),
                &AtomicBool::new(false),
            )
            .unwrap()
    }

    #[test]
    fn literal_is_case_insensitive_unless_asked() {
        let m = ContentMatcher::new(&spec("Hello")).unwrap();
        assert!(m.is_match(b"say hello world"));
        let m = ContentMatcher::new(&ContentSpec {
            case_sensitive: true,
            ..spec("Hello")
        })
        .unwrap();
        assert!(!m.is_match(b"say hello world"));
        assert!(m.is_match(b"say Hello world"));
    }

    #[test]
    fn literal_metacharacters_are_literal() {
        let m = ContentMatcher::new(&spec("a.b*c")).unwrap();
        assert!(m.is_match(b"xa.b*cx"));
        assert!(!m.is_match(b"axbbbc"));
    }

    #[test]
    fn unicode_case_folding_in_utf8() {
        let m = ContentMatcher::new(&spec("ПРИВЕТ")).unwrap();
        assert!(m.is_match("скажи привет".as_bytes()));
    }

    #[test]
    fn whole_words() {
        let m = ContentMatcher::new(&ContentSpec {
            whole_words: true,
            ..spec("cat")
        })
        .unwrap();
        assert!(m.is_match(b"the cat sat"));
        assert!(m.is_match(b"cat"));
        assert!(!m.is_match(b"concatenate"));
    }

    #[test]
    fn regex_mode_with_line_anchors() {
        let m = ContentMatcher::new(&ContentSpec {
            regex: true,
            ..spec(r"^fn \w+\(\)$")
        })
        .unwrap();
        assert!(m.is_match(b"x\nfn main()\ny"));
        assert!(m.is_match(b"x\r\nfn main()\r\ny"));
        assert!(!m.is_match(b"x fn main()"));
        assert_eq!(check(&m, b"use x;\nfn main()\n"), FileVerdict::Match);
        assert_eq!(check(&m, b"use x;\nfn main()"), FileVerdict::Match);
        assert_eq!(check(&m, b"use x; fn main()\n"), FileVerdict::NoMatch);
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let err = ContentMatcher::new(&ContentSpec {
            regex: true,
            ..spec("(unclosed")
        })
        .unwrap_err();
        assert!(matches!(err, ContentError::Regex(_)));
    }

    #[test]
    fn regex_refuses_non_utf8_encodings() {
        let err = ContentMatcher::new(&ContentSpec {
            regex: true,
            encodings: ContentEncodings::Utf8AndUtf16,
            ..spec("a+")
        })
        .unwrap_err();
        assert_eq!(err, ContentError::RegexNotUtf8);
    }

    #[test]
    fn empty_text_is_an_error() {
        assert_eq!(
            ContentMatcher::new(&spec("")).unwrap_err(),
            ContentError::Empty
        );
    }

    #[test]
    fn utf16_both_byte_orders_case_folded() {
        let m = ContentMatcher::new(&ContentSpec {
            encodings: ContentEncodings::Utf8AndUtf16,
            ..spec("Hello")
        })
        .unwrap();
        let le: Vec<u8> = "xx HELLO yy"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let be: Vec<u8> = "xx hello yy"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        assert!(m.is_match(&le));
        assert!(m.is_match(&be));
        assert!(m.is_match(b"plain hello"));
        let only16 = ContentMatcher::new(&ContentSpec {
            encodings: ContentEncodings::Utf16,
            ..spec("Hello")
        })
        .unwrap();
        assert!(!only16.is_match(b"plain hello"));
        assert!(only16.is_match(&le));
    }

    #[test]
    fn legacy_code_pages_case_folded() {
        let m = ContentMatcher::new(&ContentSpec {
            encodings: ContentEncodings::Windows1251,
            ..spec("Привет")
        })
        .unwrap();
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode("скажи ПРИВЕТ");
        assert!(m.is_match(&bytes));
        assert!(!m.is_match("скажи привет".as_bytes()), "UTF-8 is not 1251");

        let koi = ContentMatcher::new(&ContentSpec {
            encodings: ContentEncodings::Koi8R,
            ..spec("привет")
        })
        .unwrap();
        let (bytes, _, _) = encoding_rs::KOI8_R.encode("ПРИВЕТ");
        assert!(koi.is_match(&bytes));
    }

    #[test]
    fn unrepresentable_text_is_an_error() {
        let err = ContentMatcher::new(&ContentSpec {
            encodings: ContentEncodings::Windows1255,
            ..spec("Привет")
        })
        .unwrap_err();
        assert_eq!(err, ContentError::Unrepresentable("windows-1255"));
    }

    #[test]
    fn not_containing_inverts() {
        let m = ContentMatcher::new(&ContentSpec {
            not_containing: true,
            ..spec("needle")
        })
        .unwrap();
        assert_eq!(check(&m, b"hay hay"), FileVerdict::Match);
        assert_eq!(check(&m, b"hay needle"), FileVerdict::NoMatch);
        assert_eq!(check(&m, b""), FileVerdict::Match);
    }

    #[test]
    fn binary_files_are_searched_unless_skipped() {
        let content = b"\x7fELF\0\0\0needle";
        let m = ContentMatcher::new(&spec("needle")).unwrap();
        assert_eq!(check(&m, content), FileVerdict::Match);
        let skip = ContentMatcher::new(&ContentSpec {
            skip_binary: true,
            ..spec("needle")
        })
        .unwrap();
        assert_eq!(check(&skip, content), FileVerdict::Skipped);
        assert_eq!(check(&skip, b"text needle"), FileVerdict::Match);
    }

    #[test]
    fn utf16_bom_is_not_binary_when_utf16_is_searched() {
        let mut content = vec![0xFF, 0xFE];
        content.extend("a needle".encode_utf16().flat_map(|u| u.to_le_bytes()));
        let m = ContentMatcher::new(&ContentSpec {
            skip_binary: true,
            encodings: ContentEncodings::Utf8AndUtf16,
            ..spec("needle")
        })
        .unwrap();
        assert_eq!(check(&m, &content), FileVerdict::Match);
    }

    #[test]
    fn matches_across_read_boundaries_are_found() {
        let m = ContentMatcher::new(&spec("needle")).unwrap();
        for offset in [CHUNK - 3, CHUNK - 6, CHUNK, 2 * CHUNK - 1] {
            // Short lines: the window boundary is a line boundary.
            let mut content = Vec::new();
            while content.len() < offset {
                content.extend_from_slice(b"haystack line\n");
            }
            content.truncate(offset);
            content.extend_from_slice(b"needle\n");
            assert_eq!(check(&m, &content), FileVerdict::Match, "offset {offset}");
        }
    }

    #[test]
    fn matches_inside_an_over_long_line_are_found() {
        let m = ContentMatcher::new(&spec("needle")).unwrap();
        for offset in [MAX_LINE - 3, MAX_LINE + 10, 3 * MAX_LINE] {
            let mut content = vec![b'x'; offset];
            content.extend_from_slice(b"needle");
            content.extend(std::iter::repeat_n(b'y', 1000));
            assert_eq!(check(&m, &content), FileVerdict::Match, "offset {offset}");
        }
        let content = vec![b'x'; 3 * MAX_LINE];
        assert_eq!(check(&m, &content), FileVerdict::NoMatch);
    }

    #[test]
    fn cancel_skips_the_file() {
        let m = ContentMatcher::new(&spec("needle")).unwrap();
        let mut buf = Vec::new();
        let verdict = m
            .check(
                &mut std::io::Cursor::new(b"needle".to_vec()),
                &mut buf,
                &AtomicU64::new(0),
                &AtomicBool::new(true),
            )
            .unwrap();
        assert_eq!(verdict, FileVerdict::Skipped);
    }

    #[test]
    fn encodings_cycle_through_every_option() {
        let mut e = ContentEncodings::default();
        let mut seen = vec![e];
        for _ in 0..ContentEncodings::ALL.len() - 1 {
            e = e.next();
            seen.push(e);
        }
        assert_eq!(seen, ContentEncodings::ALL);
        assert_eq!(e.next(), ContentEncodings::Utf8);
    }
}
