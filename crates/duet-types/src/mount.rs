//! `Scheme`, `Authority`, and `MountId` — the addressing half of a [`VPath`](crate::VPath).
//!
//! design.md §9.1 sketches `VPath` as a two-field struct: `{ mount: MountId,
//! inner: UnixPathBuf }`. For `VPath`'s `Display`/`FromStr` to round-trip
//! *without* consulting a live mount table (the AC for T-2.2.1), `MountId`
//! must carry its own addressing information rather than being a bare opaque
//! handle into a runtime registry. This module therefore defines `MountId`
//! as either:
//!
//! - a **root** mount, addressed by URI scheme plus an optional authority
//!   (`file:///…`, `sftp://host/…`), or
//! - a **nested** mount stacked on a location inside another mount
//!   (`zip:` opened on a `file:` path — `zip:file:///a.zip!/inner`).
//!
//! `duet-vfs`'s runtime mount table (T-6.1.1, `MountId → Arc<dyn FileSystem>`)
//! can still key off `MountId` directly for lookup/dedup/reference-counting —
//! two mount requests that resolve to the same `MountId` value name the same
//! mounted filesystem. Nested parents are held behind `Arc` so cloning a
//! `MountId` (which happens constantly as `VPath`s move through hot paths)
//! is a refcount bump, not a deep copy.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::path::PathParseError;

/// A URI-style scheme tag (`file`, `zip`, `sftp`, …), validated and
/// normalised to lowercase.
///
/// Grammar (RFC 3986 `scheme`, restricted to what we need): `ALPHA
/// *(ALPHA / DIGIT / "+" / "-" / ".")`. Not a closed enum: new backends
/// (webdav, s3, smb, …) are added by higher layers without touching this
/// crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scheme(Box<str>);

impl Scheme {
    /// Validates and constructs a scheme from its textual form. Rejects
    /// empty strings and anything outside the RFC 3986 `scheme` grammar.
    /// Input is normalised to lowercase (schemes are case-insensitive).
    pub fn new(s: &str) -> Result<Self, PathParseError> {
        let mut chars = s.chars();
        let first = chars.next().ok_or(PathParseError::EmptyScheme)?;
        if !first.is_ascii_alphabetic() {
            return Err(PathParseError::InvalidScheme);
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return Err(PathParseError::InvalidScheme);
        }
        Ok(Scheme(s.to_ascii_lowercase().into_boxed_str()))
    }

    /// The `file` scheme, used for local-filesystem root mounts.
    pub fn file() -> Self {
        Scheme(Box::from("file"))
    }

    /// Borrows the scheme's textual form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Scheme {
    type Err = PathParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scheme::new(s)
    }
}

/// The `[user@]host[:port]` authority component of a root mount (e.g. an
/// SFTP or SMB target). Local (`file:`) mounts have no authority.
///
/// `user` and `host` are percent-decoded on parse and percent-encoded on
/// display, so literal `:`/`@`/`!`/`%` in either never collide with the
/// surrounding `VPath` grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Authority {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl Authority {
    /// Constructs an authority with just a host, no user or port.
    pub fn host_only(host: impl Into<String>) -> Self {
        Authority {
            user: None,
            host: host.into(),
            port: None,
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, PathParseError> {
        if s.is_empty() {
            return Err(PathParseError::EmptyAuthority);
        }
        let (user_part, host_port) = match s.rsplit_once('@') {
            Some((u, rest)) => (Some(crate::pct::decode(u)?), rest),
            None => (None, s),
        };
        // `host_port` was percent-encoded on the way out, so any literal
        // ':' left in it is genuinely our port separator, never host data.
        let (host_str, port) = match host_port.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                let port: u16 = p.parse().map_err(|_| PathParseError::InvalidPort)?;
                (h, Some(port))
            }
            _ => (host_port, None),
        };
        let host = crate::pct::decode(host_str)?;
        if host.is_empty() {
            return Err(PathParseError::EmptyAuthority);
        }
        Ok(Authority {
            user: user_part,
            host,
            port,
        })
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "{}@", crate::pct::encode_authority(user))?;
        }
        write!(f, "{}", crate::pct::encode_authority(&self.host))?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// Identifies which mounted filesystem a [`VPath`](crate::VPath) refers to.
///
/// See the module docs for why this is a self-describing structure rather
/// than an opaque handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MountId {
    /// A top-level mount: local filesystem, or a remote backend addressed
    /// by an optional authority.
    Root {
        scheme: Scheme,
        authority: Option<Authority>,
    },
    /// A mount stacked on top of a location inside another mount, e.g. a
    /// `zip:` filesystem opened on a `file:` path.
    Nested {
        scheme: Scheme,
        parent: Arc<crate::path::VPath>,
    },
}

impl MountId {
    /// A local-filesystem root mount (`file:`).
    pub fn local() -> Self {
        MountId::Root {
            scheme: Scheme::file(),
            authority: None,
        }
    }

    /// The scheme of this mount level (not the whole nesting chain).
    pub fn scheme(&self) -> &Scheme {
        match self {
            MountId::Root { scheme, .. } => scheme,
            MountId::Nested { scheme, .. } => scheme,
        }
    }

    /// `true` if this mount is stacked on another mount rather than being
    /// a filesystem root.
    pub fn is_nested(&self) -> bool {
        matches!(self, MountId::Nested { .. })
    }

    /// The parent mount's `VPath`, if this is a nested mount.
    pub fn parent(&self) -> Option<&crate::path::VPath> {
        match self {
            MountId::Nested { parent, .. } => Some(parent),
            MountId::Root { .. } => None,
        }
    }
}
