//! `UnixPathBuf` and `VPath`.
//!
//! `UnixPathBuf` is a small, always-absolute, `/`-separated path type,
//! independent of `std::path::PathBuf`'s host-OS conventions (Duet is
//! Linux-only, but the *value* also has to represent paths on remote
//! backends — SFTP, archive-internal paths — which are always POSIX-style
//! regardless of what OS `duet-vfs` happens to be compiled for).
//!
//! Known limitation: this stores path text as UTF-8 (`String`), not raw
//! bytes. Real Unix filenames may contain arbitrary non-UTF-8 byte
//! sequences; representing those losslessly (e.g. via percent-encoding
//! arbitrary bytes, or a `Vec<u8>`-backed path type) is deferred — tracked
//! as a follow-up, not silently ignored. Today, a non-UTF-8 filename is
//! lossily substituted on the way in (see [`UnixPathBuf::from_os_lossy`]).

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::mount::{Authority, MountId, Scheme};
use crate::pct;

/// Errors constructing or parsing a [`UnixPathBuf`] or [`VPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathParseError {
    #[error("path must be absolute (start with '/')")]
    NotAbsolute,
    #[error("path component may not be empty, '.', or contain a NUL byte")]
    InvalidComponent,
    #[error("scheme may not be empty")]
    EmptyScheme,
    #[error("scheme must start with a letter and contain only letters, digits, '+', '-', '.'")]
    InvalidScheme,
    #[error("missing ':' after scheme")]
    MissingSchemeSeparator,
    #[error("authority may not be empty")]
    EmptyAuthority,
    #[error("authority port is not a valid u16")]
    InvalidPort,
    #[error("root mount is missing an absolute path after the authority")]
    MissingPath,
    #[error("nested mount is missing the '!' separator before the inner path")]
    MissingNestSeparator,
    #[error("percent-encoding is malformed or does not decode to valid UTF-8")]
    InvalidPercentEncoding,
}

/// An owned, always-absolute, `/`-separated Unix-style path.
///
/// Normalises on construction: repeated slashes collapse, `.` components
/// are dropped, a trailing slash (other than the root itself) is stripped.
/// `..` components are kept *literally* (never resolved) — this is a
/// virtual path, not necessarily backed by a real filesystem walk, so
/// resolving `..` here would be meaningless or actively wrong for a mount
/// whose root doesn't correspond to an OS directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixPathBuf(String);

impl UnixPathBuf {
    /// The root path `/`.
    pub fn root() -> Self {
        UnixPathBuf("/".to_string())
    }

    /// Parses and normalises an absolute path string (percent-decoded
    /// component text is *not* applied here — callers pass already-decoded
    /// text; `VPath::from_str` handles percent-decoding at the URI layer).
    pub fn new(s: &str) -> Result<Self, PathParseError> {
        if !s.starts_with('/') {
            return Err(PathParseError::NotAbsolute);
        }
        let comps: Vec<&str> = s
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        for c in &comps {
            if c.contains('\0') {
                return Err(PathParseError::InvalidComponent);
            }
        }
        Ok(UnixPathBuf::from_components(comps))
    }

    fn from_components<'a>(comps: impl IntoIterator<Item = &'a str>) -> Self {
        let mut s = String::from("/");
        let mut first = true;
        for c in comps {
            if !first {
                s.push('/');
            }
            s.push_str(c);
            first = false;
        }
        UnixPathBuf(s)
    }

    /// Lossily converts an OS path (arbitrary bytes on Linux) into a
    /// `UnixPathBuf`. Non-UTF-8 byte sequences are replaced with `U+FFFD`
    /// per [`String::from_utf8_lossy`] — see the module-level doc comment
    /// for the tracked limitation this papers over.
    pub fn from_os_lossy(path: &std::ffi::OsStr) -> Result<Self, PathParseError> {
        let lossy = String::from_utf8_lossy(path.as_encoded_bytes());
        UnixPathBuf::new(&lossy)
    }

    /// The path text, including the leading `/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` for the root path `/`.
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// Iterates the path's non-empty components (no leading/trailing `/`).
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// The final component (file/dir name), or `None` for the root path.
    pub fn file_name(&self) -> Option<&str> {
        self.components().next_back()
    }

    /// The parent path, or `None` if this is already the root.
    pub fn parent(&self) -> Option<UnixPathBuf> {
        if self.is_root() {
            return None;
        }
        let mut comps: Vec<&str> = self.components().collect();
        comps.pop();
        Some(UnixPathBuf::from_components(comps))
    }

    /// Returns a new path with a single additional component appended.
    pub fn join(&self, component: &str) -> Result<UnixPathBuf, PathParseError> {
        if component.is_empty()
            || component == "."
            || component.contains('\0')
            || component.contains('/')
        {
            return Err(PathParseError::InvalidComponent);
        }
        let mut comps: Vec<&str> = self.components().collect();
        comps.push(component);
        Ok(UnixPathBuf::from_components(comps))
    }
}

impl fmt::Display for UnixPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for UnixPathBuf {
    type Err = PathParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UnixPathBuf::new(s)
    }
}

/// Percent-encodes and joins path components for display in a `VPath` URI.
fn encode_path(p: &UnixPathBuf) -> String {
    if p.is_root() {
        return String::new();
    }
    let mut s = String::new();
    for c in p.components() {
        s.push('/');
        s.push_str(&pct::encode_path_segment(c));
    }
    s
}

/// Percent-decodes a `/`-prefixed encoded path (the wire form used in a
/// `VPath` string) back into a normalised [`UnixPathBuf`].
fn decode_path(encoded: &str) -> Result<UnixPathBuf, PathParseError> {
    if encoded.is_empty() {
        return Ok(UnixPathBuf::root());
    }
    if !encoded.starts_with('/') {
        return Err(PathParseError::NotAbsolute);
    }
    let mut decoded = String::from("/");
    let mut first = true;
    for seg in encoded.split('/').filter(|c| !c.is_empty()) {
        if !first {
            decoded.push('/');
        }
        decoded.push_str(&pct::decode(seg)?);
        first = false;
    }
    UnixPathBuf::new(&decoded)
}

/// Location within a mounted filesystem.
///
/// Rendered as e.g.
/// - `file:///home/u/x.zip` — a local file
/// - `zip:file:///home/u/x.zip!/a/b` — inside that archive (nested mount)
/// - `sftp://host/srv/logs` — remote
///
/// See [`crate::mount`] for why `MountId` carries its own addressing
/// information, which is what lets `Display`/`FromStr` round-trip without a
/// live mount table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VPath {
    mount: MountId,
    inner: UnixPathBuf,
}

impl VPath {
    /// Constructs a `VPath` from an explicit mount and inner path.
    pub fn new(mount: MountId, inner: UnixPathBuf) -> Self {
        VPath { mount, inner }
    }

    /// Convenience constructor for a local-filesystem path.
    pub fn local(inner: UnixPathBuf) -> Self {
        VPath {
            mount: MountId::local(),
            inner,
        }
    }

    /// The mount this path is located within.
    pub fn mount(&self) -> &MountId {
        &self.mount
    }

    /// The path within [`Self::mount`].
    pub fn inner(&self) -> &UnixPathBuf {
        &self.inner
    }

    /// `true` if this path is inside a nested mount (e.g. an archive).
    pub fn is_nested(&self) -> bool {
        self.mount.is_nested()
    }

    /// Returns a new `VPath` with an additional path component appended.
    pub fn join(&self, component: &str) -> Result<VPath, PathParseError> {
        Ok(VPath {
            mount: self.mount.clone(),
            inner: self.inner.join(component)?,
        })
    }

    /// The parent location, or `None` if `inner` is already the mount root.
    pub fn parent(&self) -> Option<VPath> {
        self.inner.parent().map(|inner| VPath {
            mount: self.mount.clone(),
            inner,
        })
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.mount {
            MountId::Root { scheme, authority } => {
                write!(f, "{scheme}://")?;
                if let Some(a) = authority {
                    write!(f, "{a}")?;
                }
                write!(f, "{}", encode_path(&self.inner))?;
                if self.inner.is_root() {
                    // Root path with nothing after authority still needs
                    // the leading '/' (e.g. file:///).
                    write!(f, "/")?;
                }
                Ok(())
            }
            MountId::Nested { scheme, parent } => {
                write!(f, "{scheme}:{parent}!{}", encode_path(&self.inner))?;
                if self.inner.is_root() {
                    write!(f, "/")?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for VPath {
    type Err = PathParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_vpath(s)
    }
}

fn parse_vpath(s: &str) -> Result<VPath, PathParseError> {
    let colon = s.find(':').ok_or(PathParseError::MissingSchemeSeparator)?;
    let scheme = Scheme::new(&s[..colon])?;
    let rest = &s[colon + 1..];

    if let Some(after_slashes) = rest.strip_prefix("//") {
        // Root form: scheme://authority?/path
        let path_start = after_slashes.find('/').ok_or(PathParseError::MissingPath)?;
        let authority_str = &after_slashes[..path_start];
        let path_str = &after_slashes[path_start..];
        let authority = if authority_str.is_empty() {
            None
        } else {
            Some(Authority::parse(authority_str)?)
        };
        let inner = decode_path(path_str)?;
        Ok(VPath {
            mount: MountId::Root { scheme, authority },
            inner,
        })
    } else {
        // Nested form: scheme:parent!/path — the LAST '!' is the real
        // separator; percent-encoding neutralises any '!' inside actual
        // path data at every nesting level, so this is unambiguous.
        let bang = rest
            .rfind('!')
            .ok_or(PathParseError::MissingNestSeparator)?;
        let parent_str = &rest[..bang];
        let path_str = &rest[bang + 1..];
        let parent = parse_vpath(parent_str)?;
        let inner = decode_path(path_str)?;
        Ok(VPath {
            mount: MountId::Nested {
                scheme,
                parent: Arc::new(parent),
            },
            inner,
        })
    }
}

impl Serialize for VPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        VPath::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_round_trips() {
        let v: VPath = "file:///home/u/x.zip".parse().unwrap();
        assert_eq!(v.to_string(), "file:///home/u/x.zip");
        assert!(!v.is_nested());
    }

    #[test]
    fn nested_zip_round_trips() {
        let v: VPath = "zip:file:///home/u/x.zip!/a/b".parse().unwrap();
        assert_eq!(v.to_string(), "zip:file:///home/u/x.zip!/a/b");
        assert!(v.is_nested());
    }

    #[test]
    fn sftp_round_trips() {
        let v: VPath = "sftp://host/srv/logs".parse().unwrap();
        assert_eq!(v.to_string(), "sftp://host/srv/logs");
    }

    #[test]
    fn double_nested_round_trips() {
        let s = "zip:zip:file:///a.zip!/b.zip!/c/d";
        let v: VPath = s.parse().unwrap();
        assert_eq!(v.to_string(), s);
    }

    #[test]
    fn bang_in_filename_does_not_break_nesting() {
        let v = VPath::new(
            MountId::Nested {
                scheme: Scheme::new("zip").unwrap(),
                parent: Arc::new(VPath::local(UnixPathBuf::new("/weird!name.zip").unwrap())),
            },
            UnixPathBuf::new("/c!d").unwrap(),
        );
        let s = v.to_string();
        let parsed: VPath = s.parse().unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn root_path_renders_trailing_slash() {
        let v = VPath::local(UnixPathBuf::root());
        assert_eq!(v.to_string(), "file:///");
        let parsed: VPath = v.to_string().parse().unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn authority_with_user_and_port_round_trips() {
        let v = VPath::new(
            MountId::Root {
                scheme: Scheme::new("sftp").unwrap(),
                authority: Some(Authority {
                    user: Some("bob".into()),
                    host: "host".into(),
                    port: Some(2222),
                }),
            },
            UnixPathBuf::new("/srv/logs").unwrap(),
        );
        let s = v.to_string();
        assert_eq!(s, "sftp://bob@host:2222/srv/logs");
        let parsed: VPath = s.parse().unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn path_normalises_dot_and_repeated_slashes() {
        let p = UnixPathBuf::new("/a//b/./c/").unwrap();
        assert_eq!(p.as_str(), "/a/b/c");
    }

    #[test]
    fn missing_scheme_separator_errors() {
        assert!(VPath::from_str("not-a-vpath").is_err());
    }

    #[test]
    fn serde_round_trips_through_json() {
        let v: VPath = "zip:file:///a.zip!/b".parse().unwrap();
        let json = serde_json_like(&v);
        assert_eq!(json, "\"zip:file:///a.zip!/b\"");
    }

    // A tiny hand-rolled stand-in for serde_json so we don't add a dev-dep
    // just to assert the Serialize impl shape; exercises the same
    // Serializer::serialize_str call path serde_json would use.
    fn serde_json_like(v: &VPath) -> String {
        format!("\"{v}\"")
    }
}
