// SPDX-License-Identifier: MIT
//! A search request as plain data ([`SearchSpec`]), the parsers for the
//! dialog's free-text fields, and compilation into what the walker runs
//! ([`CompiledSearch`]).

use std::path::{Path, PathBuf};

use regex::bytes::{Regex, RegexBuilder};

use crate::content::{ContentError, ContentMatcher, ContentSpec};
use crate::mask::{MaskError, NameMask};

/// Which kinds of entry may be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindFilter {
    #[default]
    Any,
    FilesOnly,
    DirsOnly,
}

impl KindFilter {
    pub fn label(self) -> &'static str {
        match self {
            KindFilter::Any => "files and directories",
            KindFilter::FilesOnly => "files only",
            KindFilter::DirsOnly => "directories only",
        }
    }

    pub fn next(self) -> Self {
        match self {
            KindFilter::Any => KindFilter::FilesOnly,
            KindFilter::FilesOnly => KindFilter::DirsOnly,
            KindFilter::DirsOnly => KindFilter::Any,
        }
    }
}

/// Everything a search needs, as the dialog collected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSpec {
    /// Where to start. Each is searched; none is itself reported.
    pub roots: Vec<PathBuf>,
    /// A TC name mask ([`crate::mask`]) or, with `name_regex`, a regex
    /// matched against the file name (not the path).
    pub name: String,
    pub name_regex: bool,
    pub name_case_sensitive: bool,
    pub content: Option<ContentSpec>,
    /// Inclusive byte bounds. Either one set limits results to non-
    /// directories (a directory has no meaningful size here).
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    /// Inclusive Unix-second bounds on the modification time.
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub kind: KindFilter,
    /// Report and descend into dot-entries.
    pub include_hidden: bool,
    /// Treat a symlink to a directory as that directory (loops are
    /// detected), and filter a symlink to a file by its target.
    pub follow_symlinks: bool,
    /// Never cross into another mounted filesystem.
    pub one_filesystem: bool,
    /// `Some(0)`: only the roots' own entries; `None`: unlimited.
    pub max_depth: Option<u32>,
}

impl Default for SearchSpec {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            name: "*".to_string(),
            name_regex: false,
            name_case_sensitive: false,
            content: None,
            size_min: None,
            size_max: None,
            modified_after: None,
            modified_before: None,
            kind: KindFilter::Any,
            include_hidden: true,
            follow_symlinks: false,
            one_filesystem: false,
            max_depth: None,
        }
    }
}

/// Why a [`SearchSpec`] cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("no folder to search in")]
    NoRoots,
    #[error(transparent)]
    Mask(#[from] MaskError),
    #[error("invalid file-name regular expression: {0}")]
    NameRegex(String),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("the minimum size is larger than the maximum")]
    SizeRange,
    #[error("the \u{201c}after\u{201d} date is later than the \u{201c}before\u{201d} date")]
    DateRange,
}

/// How file names are matched.
#[derive(Debug, Clone)]
pub enum NameMatcher {
    Mask(NameMask),
    Regex(Regex),
}

impl NameMatcher {
    pub fn matches(&self, name: &[u8]) -> bool {
        match self {
            NameMatcher::Mask(mask) => mask.matches(name),
            NameMatcher::Regex(re) => re.is_match(name),
        }
    }

    pub fn prunes_dir(&self, name: &[u8]) -> bool {
        match self {
            NameMatcher::Mask(mask) => mask.prunes_dir(name),
            NameMatcher::Regex(_) => false,
        }
    }
}

/// A validated, compiled [`SearchSpec`] -- what [`crate::start`] runs.
#[derive(Debug, Clone)]
pub struct CompiledSearch {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) name: NameMatcher,
    pub(crate) content: Option<ContentMatcher>,
    pub(crate) size: (Option<u64>, Option<u64>),
    pub(crate) modified: (Option<i64>, Option<i64>),
    pub(crate) kind: KindFilter,
    pub(crate) include_hidden: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) one_filesystem: bool,
    pub(crate) max_depth: Option<u32>,
}

impl CompiledSearch {
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

impl SearchSpec {
    pub fn compile(&self) -> Result<CompiledSearch, SpecError> {
        if self.roots.is_empty() {
            return Err(SpecError::NoRoots);
        }
        let name = if self.name_regex {
            let re = RegexBuilder::new(&self.name)
                .case_insensitive(!self.name_case_sensitive)
                .build()
                .map_err(|e| SpecError::NameRegex(e.to_string()))?;
            NameMatcher::Regex(re)
        } else {
            NameMatcher::Mask(NameMask::parse(&self.name, self.name_case_sensitive)?)
        };
        let content = self.content.as_ref().map(ContentMatcher::new).transpose()?;
        if let (Some(min), Some(max)) = (self.size_min, self.size_max)
            && min > max
        {
            return Err(SpecError::SizeRange);
        }
        if let (Some(after), Some(before)) = (self.modified_after, self.modified_before)
            && after > before
        {
            return Err(SpecError::DateRange);
        }
        Ok(CompiledSearch {
            roots: self.roots.clone(),
            name,
            content,
            size: (self.size_min, self.size_max),
            modified: (self.modified_after, self.modified_before),
            kind: self.kind,
            include_hidden: self.include_hidden,
            follow_symlinks: self.follow_symlinks,
            one_filesystem: self.one_filesystem,
            max_depth: self.max_depth,
        })
    }
}

/// The "Search in" field: `;`-separated folders, `~` expanded against
/// `home`, relative ones resolved against `base` (the panel's folder).
pub fn parse_roots(text: &str, base: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let expanded = match (s.strip_prefix('~'), home) {
                (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                    home.join(rest.trim_start_matches('/'))
                }
                _ => PathBuf::from(s),
            };
            if expanded.is_absolute() {
                expanded
            } else {
                base.join(expanded)
            }
        })
        .collect()
}

/// A size field: a number with an optional binary-unit suffix --
/// `512`, `10k`, `1.5M`, `2 GiB`, `3g`. Units are powers of 1024 (what
/// the panel's size column shows). Empty means "no bound".
pub fn parse_size(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("\u{201c}{text}\u{201d} is not a size (try 10k, 5M, 1.5G)"))?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => {
            return Err(format!(
                "\u{201c}{text}\u{201d}: unknown unit (use k, M, G or T)"
            ));
        }
    };
    Ok(Some((value * multiplier as f64).round() as u64))
}

/// A date field, in the panel's own `YYYY-MM-DD[ HH:MM]` (UTC, like the
/// date column), or an age back from `now`: `30min`, `12h`, `7d`, `2w`,
/// `6mo`, `1y`. Empty means "no bound". Returns Unix seconds.
pub fn parse_time(text: &str, now: i64) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(age) = parse_age(text) {
        return Ok(Some(now - age));
    }
    parse_civil(text).map(Some).ok_or_else(|| {
        format!(
            "\u{201c}{text}\u{201d} is not a date (YYYY-MM-DD, YYYY-MM-DD HH:MM, or an age like 7d)"
        )
    })
}

/// `7d` -> seconds. `None` if `text` is not an age.
fn parse_age(text: &str) -> Option<i64> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let n: i64 = number.parse().ok()?;
    let unit_secs = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" => 1,
        "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "mo" => 30 * 86_400,
        "y" => 365 * 86_400,
        _ => return None,
    };
    n.checked_mul(unit_secs)
}

/// `YYYY-MM-DD[ HH:MM]` -> Unix seconds, rejecting impossible dates by
/// round-tripping (the approach `attributes_dialog::parse_date_time`
/// takes for the same format).
fn parse_civil(text: &str) -> Option<i64> {
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t.trim())),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (hh, mm) = match time {
        Some(t) => {
            let (h, mi) = t.split_once(':')?;
            (h.parse().ok()?, mi.parse().ok()?)
        }
        None => (0, 0),
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 {
        return None;
    }
    let secs = unix_from_civil(y, m, d, hh, mm);
    (civil_from_unix(secs) == (y, m, d, hh, mm)).then_some(secs)
}

/// Howard Hinnant's `days_from_civil` -- a private twin of
/// `duet_ui::file_table::unix_from_civil` (this crate cannot depend on the
/// UI crate, and the panel's date column must agree with these fields).
fn unix_from_civil(y: i64, m: u32, d: u32, hh: u32, mm: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + i64::from(hh) * 3600 + i64::from(mm) * 60
}

/// The inverse, `civil_from_days`, for the round-trip check.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_with_binary_units() {
        assert_eq!(parse_size(""), Ok(None));
        assert_eq!(parse_size("  "), Ok(None));
        assert_eq!(parse_size("512"), Ok(Some(512)));
        assert_eq!(parse_size("10k"), Ok(Some(10 * 1024)));
        assert_eq!(parse_size("10 KiB"), Ok(Some(10 * 1024)));
        assert_eq!(parse_size("1.5M"), Ok(Some(1_572_864)));
        assert_eq!(parse_size("2G"), Ok(Some(2 << 30)));
        assert_eq!(parse_size("1tb"), Ok(Some(1 << 40)));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("10q").is_err());
    }

    #[test]
    fn dates_absolute_and_relative() {
        let now = 1_800_000_000;
        assert_eq!(parse_time("", now), Ok(None));
        assert_eq!(parse_time("2026-01-31", now), Ok(Some(1_769_817_600)));
        assert_eq!(
            parse_time("2026-01-31 12:30", now),
            Ok(Some(1_769_817_600 + 12 * 3600 + 30 * 60))
        );
        assert_eq!(parse_time("7d", now), Ok(Some(now - 7 * 86_400)));
        assert_eq!(parse_time("12h", now), Ok(Some(now - 12 * 3600)));
        assert_eq!(parse_time("30min", now), Ok(Some(now - 1800)));
        assert_eq!(parse_time("2w", now), Ok(Some(now - 14 * 86_400)));
        assert!(parse_time("2026-02-30", now).is_err());
        assert!(parse_time("2026-13-01", now).is_err());
        assert!(parse_time("yesterday", now).is_err());
        assert!(parse_time("7x", now).is_err());
    }

    #[test]
    fn civil_round_trips() {
        for secs in [0, 86_399, 951_782_400, 1_769_817_600, -86_400] {
            let (y, m, d, hh, mm) = civil_from_unix(secs);
            assert_eq!(unix_from_civil(y, m, d, hh, mm), secs - secs.rem_euclid(60));
        }
    }

    #[test]
    fn roots_split_expand_and_resolve() {
        let base = Path::new("/panel");
        let home = Path::new("/home/u");
        assert_eq!(
            parse_roots("/a; ~/b ;rel;; ~", base, Some(home)),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/home/u/b"),
                PathBuf::from("/panel/rel"),
                PathBuf::from("/home/u"),
            ]
        );
        assert_eq!(
            parse_roots("~other", base, Some(home)),
            vec![PathBuf::from("/panel/~other")]
        );
        assert!(parse_roots(" ; ", base, Some(home)).is_empty());
    }

    #[test]
    fn compile_validates() {
        let ok = SearchSpec {
            roots: vec![PathBuf::from("/")],
            ..SearchSpec::default()
        };
        assert!(ok.compile().is_ok());
        assert_eq!(
            SearchSpec::default().compile().unwrap_err(),
            SpecError::NoRoots
        );
        let bad_regex = SearchSpec {
            name: "(".into(),
            name_regex: true,
            ..ok.clone()
        };
        assert!(matches!(
            bad_regex.compile().unwrap_err(),
            SpecError::NameRegex(_)
        ));
        let sizes = SearchSpec {
            size_min: Some(10),
            size_max: Some(5),
            ..ok.clone()
        };
        assert_eq!(sizes.compile().unwrap_err(), SpecError::SizeRange);
        let dates = SearchSpec {
            modified_after: Some(10),
            modified_before: Some(5),
            ..ok.clone()
        };
        assert_eq!(dates.compile().unwrap_err(), SpecError::DateRange);
        let content = SearchSpec {
            content: Some(ContentSpec::default()),
            ..ok
        };
        assert_eq!(
            content.compile().unwrap_err(),
            SpecError::Content(ContentError::Empty)
        );
    }

    #[test]
    fn name_regex_matches_the_name() {
        let spec = SearchSpec {
            roots: vec![PathBuf::from("/")],
            name: r"^img_\d+\.jpe?g$".into(),
            name_regex: true,
            ..SearchSpec::default()
        };
        let compiled = spec.compile().unwrap();
        assert!(compiled.name.matches(b"IMG_0042.JPG"));
        assert!(!compiled.name.matches(b"xIMG_0042.JPG"));
    }

    #[test]
    fn kind_filter_cycles() {
        let k = KindFilter::default();
        assert_eq!(k.next().next().next(), k);
    }
}
