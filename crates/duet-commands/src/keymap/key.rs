// SPDX-License-Identifier: MIT
//! Key and key-chord types: parses `keymap.toml`'s `binding.key` strings
//! (`docs/config-schema.md` §2) into structured, comparable values.
//!
//! ```text
//! "f5"              -> one step, no modifiers, key "f5"
//! "shift-f6"         -> one step, SHIFT, key "f6"
//! "alt-shift-i"      -> one step, ALT|SHIFT, key "i"
//! "ctrl-k ctrl-b"    -> a two-step chord (multi-key sequence, §9.4)
//! ```

use std::fmt;
use std::str::FromStr;

/// A bitset of held modifier keys. Hand-rolled rather than pulling in the
/// `bitflags` crate for four bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL: Modifiers = Modifiers(0b0001);
    pub const ALT: Modifiers = Modifiers(0b0010);
    pub const SHIFT: Modifiers = Modifiers(0b0100);
    /// The "super"/"meta"/"cmd"/Windows key.
    pub const SUPER: Modifiers = Modifiers(0b1000);

    /// Returns whether every bit set in `other` is also set in `self`.
    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether no modifiers are held.
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Modifiers) {
        self.0 |= rhs.0;
    }
}

impl fmt::Display for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Canonical order: ctrl, alt, shift, super. Chosen so any two
        // `Modifiers` with the same bits always print identically,
        // regardless of the order the source string listed them in --
        // that canonicalisation is what lets `KeyChord` equality (used by
        // conflict detection) be structural rather than string-based.
        let mut parts = Vec::new();
        if self.contains(Modifiers::CTRL) {
            parts.push("ctrl");
        }
        if self.contains(Modifiers::ALT) {
            parts.push("alt");
        }
        if self.contains(Modifiers::SHIFT) {
            parts.push("shift");
        }
        if self.contains(Modifiers::SUPER) {
            parts.push("super");
        }
        f.write_str(&parts.join("-"))
    }
}

/// A single key-press within a chord: zero or more modifiers plus a key
/// name (e.g. `"f5"`, `"enter"`, `"i"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyStep {
    pub modifiers: Modifiers,
    /// The base key name, normalised to lower-case (e.g. `"f5"`, `"pgup"`,
    /// `"enter"`, `"\\"`). Not validated against a fixed keyboard-key
    /// vocabulary here -- that mapping is a `duet-ui`/GPUI concern (actual
    /// `Keystroke` matching); this type only needs to compare and print.
    pub key: Box<str>,
}

impl fmt::Display for KeyStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.is_none() {
            f.write_str(&self.key)
        } else {
            write!(f, "{}-{}", self.modifiers, self.key)
        }
    }
}

/// A key chord: one key-press, or a multi-key sequence (`design.md` §9.4:
/// "Chord support for multi-key sequences, with a pending-chord
/// indicator"), e.g. `ctrl-k` then `ctrl-b`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub steps: Vec<KeyStep>,
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let printed: Vec<String> = self.steps.iter().map(|s| s.to_string()).collect();
        f.write_str(&printed.join(" "))
    }
}

/// A `binding.key` string failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyParseError {
    #[error("key string must not be empty")]
    Empty,
    #[error("chord step {0:?} has no key (only modifiers)")]
    MissingKey(String),
    #[error("unknown modifier {0:?} in step {1:?} (expected one of ctrl, alt, shift, super)")]
    UnknownModifier(String, String),
}

impl KeyChord {
    /// Parses a `binding.key` string per `docs/config-schema.md` §2:
    /// modifiers joined with `-`, then the key name; chord steps separated
    /// by whitespace.
    ///
    /// ```
    /// use duet_commands::keymap::KeyChord;
    /// let chord = KeyChord::parse("ctrl-k ctrl-b").unwrap();
    /// assert_eq!(chord.steps.len(), 2);
    /// assert_eq!(chord.to_string(), "ctrl-k ctrl-b");
    ///
    /// let single = KeyChord::parse("shift-f6").unwrap();
    /// assert_eq!(single.steps.len(), 1);
    /// ```
    ///
    /// Note: a literal `-` as the key itself (e.g. binding to the minus key
    /// with a modifier, `"ctrl--"`) is not supported by this simple
    /// hyphen-splitting scheme; that edge case is left for a follow-up if
    /// it turns out to matter for a real keymap.
    pub fn parse(input: &str) -> Result<Self, KeyParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(KeyParseError::Empty);
        }
        let steps = trimmed
            .split_whitespace()
            .map(parse_step)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(KeyChord { steps })
    }
}

impl FromStr for KeyChord {
    type Err = KeyParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Deserializes from a plain TOML/JSON string, e.g. `key = "ctrl-k ctrl-b"`
/// in `keymap.toml` (`docs/config-schema.md` §2, `binding.key`).
impl<'de> serde::Deserialize<'de> for KeyChord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        KeyChord::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn parse_step(step: &str) -> Result<KeyStep, KeyParseError> {
    let mut parts: Vec<&str> = step.split('-').collect();
    let Some(key) = parts.pop() else {
        return Err(KeyParseError::MissingKey(step.to_string()));
    };
    if key.is_empty() {
        return Err(KeyParseError::MissingKey(step.to_string()));
    }
    let mut modifiers = Modifiers::NONE;
    for part in parts {
        modifiers |= match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Modifiers::CTRL,
            "alt" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "super" | "meta" | "cmd" | "win" => Modifiers::SUPER,
            other => {
                return Err(KeyParseError::UnknownModifier(
                    other.to_string(),
                    step.to_string(),
                ));
            }
        };
    }
    Ok(KeyStep {
        modifiers,
        key: key.to_ascii_lowercase().into_boxed_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_key() {
        let c = KeyChord::parse("f5").unwrap();
        assert_eq!(
            c.steps,
            vec![KeyStep {
                modifiers: Modifiers::NONE,
                key: "f5".into()
            }]
        );
    }

    #[test]
    fn parses_single_modifier() {
        let c = KeyChord::parse("shift-f6").unwrap();
        assert_eq!(
            c.steps,
            vec![KeyStep {
                modifiers: Modifiers::SHIFT,
                key: "f6".into()
            }]
        );
    }

    #[test]
    fn parses_multiple_modifiers() {
        let c = KeyChord::parse("alt-shift-i").unwrap();
        assert_eq!(
            c.steps,
            vec![KeyStep {
                modifiers: Modifiers::ALT | Modifiers::SHIFT,
                key: "i".into()
            }]
        );
    }

    #[test]
    fn parses_chord_of_two_steps() {
        let c = KeyChord::parse("ctrl-k ctrl-b").unwrap();
        assert_eq!(c.steps.len(), 2);
        assert_eq!(
            c.steps[0],
            KeyStep {
                modifiers: Modifiers::CTRL,
                key: "k".into()
            }
        );
        assert_eq!(
            c.steps[1],
            KeyStep {
                modifiers: Modifiers::CTRL,
                key: "b".into()
            }
        );
    }

    #[test]
    fn display_round_trips() {
        for src in ["f5", "shift-f6", "alt-shift-i", "ctrl-k ctrl-b", "ctrl-\\"] {
            let c = KeyChord::parse(src).unwrap();
            assert_eq!(c.to_string(), src);
        }
    }

    #[test]
    fn modifier_order_is_canonicalised() {
        // "shift-ctrl-enter" and "ctrl-shift-enter" must be the *same* key,
        // since conflict detection relies on structural equality.
        let a = KeyChord::parse("shift-ctrl-enter").unwrap();
        let b = KeyChord::parse("ctrl-shift-enter").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "ctrl-shift-enter");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(KeyChord::parse(""), Err(KeyParseError::Empty));
        assert_eq!(KeyChord::parse("   "), Err(KeyParseError::Empty));
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert!(matches!(
            KeyChord::parse("hyper-a"),
            Err(KeyParseError::UnknownModifier(_, _))
        ));
    }

    #[test]
    fn rejects_modifiers_with_no_key() {
        assert!(matches!(
            KeyChord::parse("ctrl-"),
            Err(KeyParseError::MissingKey(_))
        ));
    }
}
