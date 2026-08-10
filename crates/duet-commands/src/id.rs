// SPDX-License-Identifier: MIT
//! [`CommandId`]: the dot-namespaced identifier every registered command is
//! keyed by (e.g. `ops.copy`, `nav.parent`, `plugin.command.exif-columns.rotate`).
//!
//! See `docs/commands.md` (T-1.5.1) for the full 307-command catalogue this
//! type is meant to eventually hold, and `design.md` §9.4 for the shape
//! rationale (`{ id, title, category, args_schema, precondition, handler }`).

use std::fmt;
use std::str::FromStr;

/// A validated, dot-namespaced command identifier, e.g. `"ops.copy"` or
/// `"plugin.command.exif-columns.rotate"`.
///
/// Segments are ASCII, lower-case, and may contain digits, `_`, or `-`
/// (hyphens show up in plugin-id segments, e.g. `exif-columns`). At least one
/// `.` is required: a bare `"copy"` is not a valid id, it must be namespaced
/// (`"ops.copy"`). This mirrors every id in `docs/commands.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(Box<str>);

/// Why a candidate string failed to become a [`CommandId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandIdError {
    /// The string was empty.
    #[error("command id must not be empty")]
    Empty,
    /// No `.` separator was found (ids must be namespaced).
    #[error("command id {0:?} must contain at least one '.' namespace separator")]
    NotNamespaced(String),
    /// A segment (the text between/around dots) was empty, e.g. `"ops..copy"`
    /// or a leading/trailing dot.
    #[error("command id {0:?} has an empty segment")]
    EmptySegment(String),
    /// A segment contained a character outside `[a-z0-9_-]`, or didn't start
    /// with a lower-case letter.
    #[error(
        "command id {0:?} has an invalid segment {1:?}: must start with a lower-case letter and contain only [a-z0-9_-]"
    )]
    InvalidSegment(String, String),
}

impl CommandId {
    /// Validates and constructs a [`CommandId`] from any string-like input.
    ///
    /// ```
    /// use duet_commands::CommandId;
    /// assert!(CommandId::new("ops.copy").is_ok());
    /// assert!(CommandId::new("nav.parent").is_ok());
    /// assert!(CommandId::new("plugin.command.exif-columns.rotate").is_ok());
    /// assert!(CommandId::new("copy").is_err()); // not namespaced
    /// assert!(CommandId::new("Ops.Copy").is_err()); // must be lower-case
    /// ```
    pub fn new(id: impl AsRef<str>) -> Result<Self, CommandIdError> {
        let id = id.as_ref();
        if id.is_empty() {
            return Err(CommandIdError::Empty);
        }
        if !id.contains('.') {
            return Err(CommandIdError::NotNamespaced(id.to_string()));
        }
        for segment in id.split('.') {
            if segment.is_empty() {
                return Err(CommandIdError::EmptySegment(id.to_string()));
            }
            let mut chars = segment.chars();
            let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
            let rest_ok =
                chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !first_ok || !rest_ok {
                return Err(CommandIdError::InvalidSegment(
                    id.to_string(),
                    segment.to_string(),
                ));
            }
        }
        Ok(Self(id.into()))
    }

    /// Borrows the id as a plain string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CommandId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for CommandId {
    type Error = CommandIdError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for CommandId {
    type Error = CommandIdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl FromStr for CommandId {
    type Err = CommandIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Deserializes from a plain TOML/JSON string, e.g. `command = "ops.copy"`
/// in `keymap.toml` (`docs/config-schema.md` §2, `binding.command`).
impl<'de> serde::Deserialize<'de> for CommandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        CommandId::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_catalogue_style_ids() {
        for id in [
            "ops.copy",
            "nav.parent",
            "sel.toggle_and_advance",
            "tab.goto_index",
            "plugin.command.exif-columns.rotate",
            "cmdline.insert_name",
        ] {
            assert!(CommandId::new(id).is_ok(), "expected {id:?} to be valid");
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(CommandId::new(""), Err(CommandIdError::Empty));
    }

    #[test]
    fn rejects_unnamespaced() {
        assert!(matches!(
            CommandId::new("copy"),
            Err(CommandIdError::NotNamespaced(_))
        ));
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(matches!(
            CommandId::new("ops..copy"),
            Err(CommandIdError::EmptySegment(_))
        ));
        assert!(matches!(
            CommandId::new(".copy"),
            Err(CommandIdError::EmptySegment(_))
        ));
        assert!(matches!(
            CommandId::new("ops."),
            Err(CommandIdError::EmptySegment(_))
        ));
    }

    #[test]
    fn rejects_upper_case_and_bad_chars() {
        assert!(matches!(
            CommandId::new("Ops.Copy"),
            Err(CommandIdError::InvalidSegment(_, _))
        ));
        assert!(matches!(
            CommandId::new("ops.co py"),
            Err(CommandIdError::InvalidSegment(_, _))
        ));
        assert!(matches!(
            CommandId::new("ops.3copy"),
            Err(CommandIdError::InvalidSegment(_, _))
        ));
    }

    #[test]
    fn display_round_trips() {
        let id = CommandId::new("ops.copy").unwrap();
        assert_eq!(id.to_string(), "ops.copy");
        assert_eq!(id.as_str(), "ops.copy");
    }
}
