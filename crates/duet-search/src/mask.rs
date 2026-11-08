// SPDX-License-Identifier: MIT
//! Total Commander's file-name mask syntax, compiled to byte regexes.
//!
//! What the "Search for" field of TC's Find Files dialog accepts, and so
//! what P1 types without thinking:
//!
//! - several masks separated by spaces or `;` -- `*.rs *.toml`,
//!   `*.c;*.h` -- any one of which may match;
//! - a mask with spaces in it in double quotes -- `"my notes*"`;
//! - `*` (any run of characters), `?` (one character), and `[a-z]` /
//!   `[!0-9]` character classes;
//! - a word with no wildcard at all means "contains": `report` finds
//!   `q3-report.ods` (TC searches `*report*` for it);
//! - `*.*` matches every name, extension or not, as on Windows;
//! - everything after a `|` excludes: `*.* | *.bak *.tmp`. An excluded
//!   mask ending in `/` names *directories* to skip altogether, contents
//!   included -- `| .git/ node_modules/` (TC writes `.git\`). Directory
//!   masks are taken literally (no "contains" widening), so
//!   `node_modules/` does not also skip `old_node_modules_backup`.
//!
//! Matching is case-insensitive unless asked otherwise (TC's default, and
//! a Linux user typing `*.jpg` wants `IMG_0001.JPG` too). Names are
//! matched as bytes, so a file whose name is not valid UTF-8 is still
//! found by `*`; `?` and classes match one UTF-8 character.

use regex::bytes::{Regex, RegexBuilder};

/// A compiled mask -- see the module doc comment for the syntax.
#[derive(Debug, Clone)]
pub struct NameMask {
    /// `None`: every name (an empty mask, `*` or `*.*`).
    include: Option<Regex>,
    exclude: Option<Regex>,
    /// Directories never entered nor reported.
    prune_dirs: Option<Regex>,
}

/// A mask that would not compile -- in practice only a pathological
/// pattern big enough to exceed the regex size limit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid name mask: {0}")]
pub struct MaskError(pub String);

impl NameMask {
    /// Compiles `text`. Never fails for ordinary input: every character
    /// either is mask syntax or is matched literally.
    pub fn parse(text: &str, case_sensitive: bool) -> Result<Self, MaskError> {
        let (include_part, exclude_part) = match split_unquoted(text, '|') {
            Some((inc, exc)) => (inc, exc),
            None => (text, ""),
        };
        let includes: Vec<String> = tokens(include_part)
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect();
        let match_all = includes.is_empty() || includes.iter().any(|t| t == "*" || t == "*.*");
        let include = if match_all {
            None
        } else {
            Some(compile(
                includes.iter().map(|t| glob_to_regex(t, true)),
                case_sensitive,
            )?)
        };

        let mut excludes = Vec::new();
        let mut dirs = Vec::new();
        for term in tokens(exclude_part) {
            match term.strip_suffix('/') {
                Some(dir) if !dir.is_empty() => dirs.push(glob_to_regex(dir, false)),
                Some(_) => {}
                None if term.is_empty() => {}
                None => excludes.push(glob_to_regex(&term, true)),
            }
        }
        let exclude = if excludes.is_empty() {
            None
        } else {
            Some(compile(excludes.into_iter(), case_sensitive)?)
        };
        let prune_dirs = if dirs.is_empty() {
            None
        } else {
            Some(compile(dirs.into_iter(), case_sensitive)?)
        };
        Ok(Self {
            include,
            exclude,
            prune_dirs,
        })
    }

    /// Whether a file or directory called `name` is a match.
    pub fn matches(&self, name: &[u8]) -> bool {
        self.include.as_ref().is_none_or(|re| re.is_match(name))
            && !self.exclude.as_ref().is_some_and(|re| re.is_match(name))
    }

    /// Whether a directory called `name` is excluded with its contents.
    pub fn prunes_dir(&self, name: &[u8]) -> bool {
        self.prune_dirs.as_ref().is_some_and(|re| re.is_match(name))
    }

    /// Whether this mask matches every name (and so costs nothing).
    pub fn is_match_all(&self) -> bool {
        self.include.is_none() && self.exclude.is_none()
    }
}

/// Splits `text` at the first `sep` outside double quotes.
fn split_unquoted(text: &str, sep: char) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (ix, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == sep && !quoted => return Some((&text[..ix], &text[ix + c.len_utf8()..])),
            _ => {}
        }
    }
    None
}

/// Space/`;`-separated terms, double quotes grouping (and being removed).
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if !quoted && (c.is_whitespace() || c == ';') => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One glob term as an unanchored regex fragment. `widen`: a term with no
/// wildcard becomes `*term*` (the "contains" rule).
fn glob_to_regex(term: &str, widen: bool) -> String {
    let has_wildcard = term.contains(['*', '?', '[']);
    let mut out = String::with_capacity(term.len() * 2 + 16);
    if widen && !has_wildcard {
        out.push_str("(?s-u:.)*");
        out.push_str(&regex::escape(term));
        out.push_str("(?s-u:.)*");
        return out;
    }
    let chars: Vec<char> = term.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Any bytes, so a name that is not valid UTF-8 still matches.
            '*' => out.push_str("(?s-u:.)*"),
            '?' => out.push_str("(?s:.)"),
            '[' => match class_end(&chars, i) {
                Some(end) => {
                    out.push('[');
                    let mut j = i + 1;
                    if matches!(chars[j], '!' | '^') {
                        out.push('^');
                        j += 1;
                    }
                    for &c in &chars[j..end] {
                        if c == '-' {
                            out.push('-');
                        } else {
                            push_class_char(&mut out, c);
                        }
                    }
                    out.push(']');
                    i = end;
                }
                None => out.push_str(&regex::escape("[")),
            },
            c => out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
        i += 1;
    }
    out
}

/// Index of the `]` closing the class opened at `open`, if there is one.
/// A `]` right after `[` or `[!` is a literal member, as in POSIX globs.
fn class_end(chars: &[char], open: usize) -> Option<usize> {
    let mut j = open + 1;
    if chars.get(j).is_some_and(|c| matches!(c, '!' | '^')) {
        j += 1;
    }
    if chars.get(j) == Some(&']') {
        j += 1;
    }
    (j..chars.len()).find(|&k| chars[k] == ']')
}

fn push_class_char(out: &mut String, c: char) {
    if matches!(c, '\\' | ']' | '[' | '^' | '&' | '~') {
        out.push('\\');
    }
    out.push(c);
}

fn compile(
    fragments: impl Iterator<Item = String>,
    case_sensitive: bool,
) -> Result<Regex, MaskError> {
    let alternation: Vec<String> = fragments.collect();
    let pattern = format!("^(?:{})$", alternation.join("|"));
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| MaskError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(mask: &str) -> NameMask {
        NameMask::parse(mask, false).unwrap()
    }

    #[test]
    fn star_matches_everything_and_is_free() {
        for mask in ["", "*", "*.*", "  ", "*.rs *"] {
            let mask = m(mask);
            assert!(mask.is_match_all());
            assert!(mask.matches(b"Makefile"));
            assert!(mask.matches(b".hidden"));
        }
    }

    #[test]
    fn extension_masks_are_case_insensitive_by_default() {
        let mask = m("*.jpg");
        assert!(mask.matches(b"IMG_0001.JPG"));
        assert!(mask.matches(b"a.jpg"));
        assert!(!mask.matches(b"a.jpeg"));
        assert!(!mask.matches(b"jpg"));
        let sensitive = NameMask::parse("*.jpg", true).unwrap();
        assert!(!sensitive.matches(b"IMG_0001.JPG"));
    }

    #[test]
    fn several_masks_separated_by_spaces_or_semicolons() {
        for text in ["*.c *.h", "*.c;*.h", " *.c ; *.h "] {
            let mask = m(text);
            assert!(mask.matches(b"main.c"));
            assert!(mask.matches(b"main.h"));
            assert!(!mask.matches(b"main.o"));
        }
    }

    #[test]
    fn a_plain_word_means_contains() {
        let mask = m("report");
        assert!(mask.matches(b"q3-report.ods"));
        assert!(mask.matches(b"REPORT"));
        assert!(!mask.matches(b"rep-ort"));
    }

    #[test]
    fn question_mark_and_classes() {
        let mask = m("file?.[ch]");
        assert!(mask.matches(b"file1.c"));
        assert!(mask.matches(b"fileX.H"));
        assert!(!mask.matches(b"file12.c"));
        assert!(!mask.matches(b"file1.o"));

        let negated = m("[!0-9]*");
        assert!(negated.matches(b"a1"));
        assert!(!negated.matches(b"1a"));

        // `?` is one character, not one byte.
        assert!(m("caf?").matches("café".as_bytes()));
    }

    #[test]
    fn unclosed_bracket_is_literal() {
        let mask = m("a[b");
        assert!(mask.matches(b"a[b"));
        assert!(!mask.matches(b"ab"));
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let mask = m("a+b(1).txt");
        assert!(mask.matches(b"a+b(1).txt"));
        assert!(!mask.matches(b"aab1.txt"));
    }

    #[test]
    fn quoted_masks_keep_their_spaces() {
        let mask = m("\"my notes*\" *.md");
        assert!(mask.matches(b"my notes 2026.txt"));
        assert!(mask.matches(b"README.md"));
        assert!(!mask.matches(b"notes.txt"));
    }

    #[test]
    fn pipe_excludes_names_and_directories() {
        let mask = m("*.* | *.bak .git/ node_modules/");
        assert!(mask.matches(b"main.rs"));
        assert!(!mask.matches(b"main.rs.bak"));
        assert!(mask.prunes_dir(b".git"));
        assert!(mask.prunes_dir(b"node_modules"));
        // Directory masks are literal: no "contains" widening.
        assert!(!mask.prunes_dir(b"old_node_modules"));
        // A pruned directory name is not otherwise excluded as a *name*.
        assert!(mask.matches(b".git"));
        assert!(!mask.is_match_all());
    }

    #[test]
    fn exclusion_only_mask_includes_everything_else() {
        let mask = m("| *.o");
        assert!(mask.matches(b"main.c"));
        assert!(!mask.matches(b"main.o"));
    }

    #[test]
    fn a_pipe_inside_quotes_is_not_a_separator() {
        let mask = m("\"a|b\"");
        assert!(mask.matches(b"xa|by"));
        assert!(!mask.matches(b"a"));
    }

    #[test]
    fn non_utf8_names_still_match_star_masks() {
        let name = b"caf\xe9.txt";
        assert!(m("*.txt").matches(name));
        assert!(m("caf*").matches(name));
    }
}
