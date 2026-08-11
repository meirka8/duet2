// SPDX-License-Identifier: MIT
//! Keymap types and resolution: `binding`/`unbind`, base-keymap layering,
//! and load-time conflict detection, per `design.md` §9.4 and
//! `docs/config-schema.md` §2.
//!
//! ```toml
//! # keymap.toml -- the TC-faithful default, layered by user overrides
//! [[binding]]
//! context = "panel"
//! key     = "f5"
//! command = "ops.copy"
//!
//! [[binding]]
//! context = "panel && selection.nonempty"
//! key     = "shift-f6"
//! command = "ops.rename_in_place"
//! ```
//!
//! **Layering model** (`docs/config-schema.md` §2): the resolver reads
//! exactly one base file (`keymaps/{tc,mc,modern}.toml`) as the starting
//! binding set, then applies the user's `keymap.toml` on top, in file
//! order: every `[[binding]]` is appended (colliding with an existing
//! `(context, key)` pair reports the earlier one as shadowed rather than
//! silently dropping it -- see [`KeymapDiagnostic::Shadowed`]), then every
//! `[[unbind]]` removes a matching binding (a no-op unbind is a diagnostic,
//! not an error -- [`KeymapDiagnostic::UnbindNoMatch`]).
//!
//! Conflict/collision matching is **structural equality of the parsed
//! `(Predicate, KeyChord)` pair**, not raw string equality -- so
//! `"panel && selection.nonempty"` and `"selection.nonempty && panel"`
//! collide as expected... except they currently do *not*, since `&&` is
//! non-commutative in this AST (`And(a, b) != And(b, a)`). Only exact
//! structural matches collide; overlapping-but-differently-shaped contexts
//! (e.g. `"panel"` vs `"panel && selection.nonempty"`, which overlap
//! semantically) are not detected as conflicts by this design -- doing so
//! in general requires a satisfiability check, which is out of scope here
//! and noted as a documented limitation, not a silent gap.

mod key;

pub use key::{KeyChord, KeyParseError, KeyStep, Modifiers};

use std::collections::HashMap;
use std::fmt;

use crate::id::CommandId;
use crate::predicate::Predicate;

/// Which shipped base keymap a user's `keymap.toml` layers on top of
/// (`docs/config-schema.md` §2, the `base` key). `None` starts from an
/// empty binding set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaseKeymap {
    /// Total Commander-faithful default (design.md §9.4's default table).
    Tc,
    /// Midnight Commander-style bindings.
    Mc,
    /// A "modern" (non-orthodox-FM-legacy) binding set.
    Modern,
    /// Start from an empty binding set.
    None,
}

impl Default for BaseKeymap {
    /// `docs/config-schema.md` §2: `base` defaults to `"tc"`.
    fn default() -> Self {
        BaseKeymap::Tc
    }
}

impl fmt::Display for BaseKeymap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BaseKeymap::Tc => "tc",
            BaseKeymap::Mc => "mc",
            BaseKeymap::Modern => "modern",
            BaseKeymap::None => "none",
        };
        f.write_str(s)
    }
}

/// An untyped literal for `binding.args` (`docs/config-schema.md` §2, e.g.
/// `args = { mask = "*.rs" }`). Kept untyped here because validating it
/// against a specific command's [`crate::ArgsSchema`] happens at bind-resolve
/// time (T-3.3.2), once a `CommandRegistry` is available to look the command
/// up -- this module only needs to parse and carry the value.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(untagged)]
pub enum ArgLiteral {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// The fixed arguments an inline `binding.args` table supplies, keyed by
/// argument name.
pub type BindingArgs = HashMap<String, ArgLiteral>;

/// One `[[binding]]` entry: `context = "..."`, `key = "..."`, `command =
/// "..."`, optional `args = { ... }` (`docs/config-schema.md` §2).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Binding {
    pub context: Predicate,
    pub key: KeyChord,
    pub command: CommandId,
    #[serde(default)]
    pub args: Option<BindingArgs>,
}

/// One `[[unbind]]` entry: removes a binding matching `context` and `key`
/// (and `command`, if given, to disambiguate when several bindings share a
/// key across overlapping contexts).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Unbind {
    pub context: Predicate,
    pub key: KeyChord,
    #[serde(default)]
    pub command: Option<CommandId>,
}

/// The parsed contents of one keymap file (a base file like `tc.toml`, or
/// the user's `keymap.toml`). Matches `docs/config-schema.md` §2's schema
/// exactly, so `toml::from_str::<KeymapFile>(text)` works directly against
/// a real file on disk (see this module's tests for a worked example).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct KeymapFile {
    pub schema_version: u32,
    /// Only meaningful in the user's `keymap.toml`; base files "use the
    /// identical `[[binding]]` schema with no `base` key of their own"
    /// (§2), so this simply defaults to `Tc` when absent, which is correct
    /// either way: a base file's own `base` field is never consulted by
    /// [`resolve`].
    #[serde(default)]
    pub base: BaseKeymap,
    #[serde(default, rename = "binding")]
    pub bindings: Vec<Binding>,
    #[serde(default, rename = "unbind")]
    pub unbinds: Vec<Unbind>,
}

impl Default for KeymapFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            base: BaseKeymap::default(),
            bindings: Vec::new(),
            unbinds: Vec::new(),
        }
    }
}

/// Where a resolved binding came from -- a shipped base file, or the user's
/// layer. Threaded through so [`KeymapDiagnostic`] can say *which* file is
/// responsible for a shadowed binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapSource {
    Base(BaseKeymap),
    User,
}

impl fmt::Display for KeymapSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapSource::Base(base) => write!(f, "{base}.toml"),
            KeymapSource::User => write!(f, "keymap.toml"),
        }
    }
}

/// Where in a file a binding/unbind was declared, for diagnostics. `line`
/// is `None` until a loader that tracks TOML spans (a later Phase 3
/// concern -- `toml`'s `from_str` alone does not report spans) fills it in;
/// the type exists now so the diagnostic surface doesn't need to change
/// shape when that lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: Box<str>,
    pub line: Option<u32>,
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}", self.file),
            None => write!(f, "{}", self.file),
        }
    }
}

/// A binding after layering, tagged with where it came from.
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub binding: Binding,
    pub source: KeymapSource,
    pub location: Option<SourceLocation>,
}

/// A load-time keymap diagnostic. §9.4: "Conflict detection at load time
/// with a diagnostic surface -- silently shadowed bindings are a classic
/// source of 'the app is broken' reports." Neither variant is fatal to
/// loading; both are meant to be surfaced to the user (a startup toast, a
/// panel in the keymap editor), never swallowed.
#[derive(Debug, Clone)]
pub enum KeymapDiagnostic {
    /// A later binding with the same `(context, key)` replaced an earlier
    /// one.
    Shadowed {
        context: Predicate,
        key: KeyChord,
        shadowed_command: CommandId,
        shadowed_source: KeymapSource,
        winning_command: CommandId,
        winning_source: KeymapSource,
    },
    /// An `[[unbind]]` didn't match any currently-resolved binding.
    UnbindNoMatch {
        context: Predicate,
        key: KeyChord,
        command: Option<CommandId>,
    },
}

impl fmt::Display for KeymapDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapDiagnostic::Shadowed {
                context,
                key,
                shadowed_command,
                shadowed_source,
                winning_command,
                winning_source,
            } => {
                write!(
                    f,
                    "binding conflict: key `{key}` in context `{context}` -- \
                     `{shadowed_command}` ({shadowed_source}) is shadowed by `{winning_command}` ({winning_source})"
                )
            }
            KeymapDiagnostic::UnbindNoMatch {
                context,
                key,
                command,
            } => match command {
                Some(cmd) => write!(
                    f,
                    "unbind has no matching binding: key `{key}` in context `{context}` for command `{cmd}`"
                ),
                None => write!(
                    f,
                    "unbind has no matching binding: key `{key}` in context `{context}`"
                ),
            },
        }
    }
}

/// The result of layering a set of keymap files: the final binding set plus
/// every conflict/no-op diagnostic produced along the way.
#[derive(Debug, Clone, Default)]
pub struct ResolvedKeymap {
    pub bindings: Vec<ResolvedBinding>,
    pub diagnostics: Vec<KeymapDiagnostic>,
}

impl ResolvedKeymap {
    /// Convenience lookup: the resolved binding (if any) for an exact
    /// `(context, key)` pair. Real dispatch (T-3.3.2) additionally needs to
    /// evaluate `context` predicates against live UI state to find the
    /// *active* binding for a keystroke when several contexts could match;
    /// this is the load-time, structural-equality view.
    pub fn find(&self, context: &Predicate, key: &KeyChord) -> Option<&ResolvedBinding> {
        self.bindings
            .iter()
            .find(|b| &b.binding.context == context && &b.binding.key == key)
    }
}

/// Layers a sequence of keymap files in order -- typically `[(Base(base),
/// base_file), (User, user_file)]` per `docs/config-schema.md` §2 -- into a
/// single [`ResolvedKeymap`].
///
/// Within each file, all of its `bindings` are applied (in array order)
/// before any of its `unbinds` are applied, matching §2's numbered
/// description. Across files, later files layer on top of earlier ones. A
/// later binding with the same `(context, key)` as an earlier one replaces
/// it and produces a [`KeymapDiagnostic::Shadowed`] rather than silently
/// dropping the earlier binding.
pub fn resolve(layers: impl IntoIterator<Item = (KeymapSource, KeymapFile)>) -> ResolvedKeymap {
    let mut resolved: Vec<ResolvedBinding> = Vec::new();
    let mut diagnostics: Vec<KeymapDiagnostic> = Vec::new();

    for (source, file) in layers {
        for binding in file.bindings {
            let candidate = ResolvedBinding {
                binding,
                source: source.clone(),
                location: None,
            };
            if let Some(pos) = resolved.iter().position(|existing| {
                existing.binding.context == candidate.binding.context
                    && existing.binding.key == candidate.binding.key
            }) {
                let shadowed = resolved.remove(pos);
                diagnostics.push(KeymapDiagnostic::Shadowed {
                    context: candidate.binding.context.clone(),
                    key: candidate.binding.key.clone(),
                    shadowed_command: shadowed.binding.command.clone(),
                    shadowed_source: shadowed.source,
                    winning_command: candidate.binding.command.clone(),
                    winning_source: candidate.source.clone(),
                });
            }
            resolved.push(candidate);
        }

        for unbind in file.unbinds {
            let mut matched_any = false;
            resolved.retain(|existing| {
                let context_and_key_match = existing.binding.context == unbind.context
                    && existing.binding.key == unbind.key;
                let command_matches = match &unbind.command {
                    Some(cmd) => *cmd == existing.binding.command,
                    None => true,
                };
                let is_match = context_and_key_match && command_matches;
                if is_match {
                    matched_any = true;
                }
                !is_match
            });
            if !matched_any {
                diagnostics.push(KeymapDiagnostic::UnbindNoMatch {
                    context: unbind.context,
                    key: unbind.key,
                    command: unbind.command,
                });
            }
        }
    }

    ResolvedKeymap {
        bindings: resolved,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::parse as parse_predicate;

    fn binding(context: &str, key: &str, command: &str) -> Binding {
        Binding {
            context: parse_predicate(context).unwrap(),
            key: KeyChord::parse(key).unwrap(),
            command: CommandId::new(command).unwrap(),
            args: None,
        }
    }

    fn unbind(context: &str, key: &str, command: Option<&str>) -> Unbind {
        Unbind {
            context: parse_predicate(context).unwrap(),
            key: KeyChord::parse(key).unwrap(),
            command: command.map(|c| CommandId::new(c).unwrap()),
        }
    }

    fn file(bindings: Vec<Binding>, unbinds: Vec<Unbind>) -> KeymapFile {
        KeymapFile {
            schema_version: 1,
            base: BaseKeymap::Tc,
            bindings,
            unbinds,
        }
    }

    #[test]
    fn parses_config_schema_worked_example() {
        // Verbatim from docs/config-schema.md §2.
        let text = r#"
schema_version = 1
base           = "tc"

[[binding]]
context = "panel"
key     = "f5"
command = "ops.copy"

[[binding]]
context = "panel && selection.nonempty"
key     = "shift-f6"
command = "ops.rename_in_place"

[[binding]]
context = "panel"
key     = "ctrl-k ctrl-b"
command = "panel.branch_view"

[[binding]]
context = "panel"
key     = "alt-shift-i"
command = "sel.by_mask"
args    = { mask = "*.rs" }

[[unbind]]
context = "panel"
key     = "ctrl-w"
"#;
        let parsed: KeymapFile = toml::from_str(text).expect("config-schema.md example must parse");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.base, BaseKeymap::Tc);
        assert_eq!(parsed.bindings.len(), 4);
        assert_eq!(parsed.unbinds.len(), 1);

        let chord_binding = &parsed.bindings[2];
        assert_eq!(
            chord_binding.key.steps.len(),
            2,
            "ctrl-k ctrl-b must parse as a 2-step chord"
        );

        let masked = &parsed.bindings[3];
        assert_eq!(
            masked.args.as_ref().unwrap().get("mask"),
            Some(&ArgLiteral::Str("*.rs".to_string()))
        );
    }

    #[test]
    fn user_binding_shadows_base_binding_and_is_reported() {
        let base = file(vec![binding("panel", "f5", "ops.copy")], vec![]);
        let user = file(vec![binding("panel", "f5", "custom.replacement")], vec![]);

        let resolved = resolve([
            (KeymapSource::Base(BaseKeymap::Tc), base),
            (KeymapSource::User, user),
        ]);

        assert_eq!(resolved.bindings.len(), 1);
        assert_eq!(
            resolved.bindings[0].binding.command,
            CommandId::new("custom.replacement").unwrap()
        );
        assert_eq!(resolved.bindings[0].source, KeymapSource::User);

        assert_eq!(resolved.diagnostics.len(), 1);
        match &resolved.diagnostics[0] {
            KeymapDiagnostic::Shadowed {
                shadowed_command,
                winning_command,
                ..
            } => {
                assert_eq!(*shadowed_command, CommandId::new("ops.copy").unwrap());
                assert_eq!(
                    *winning_command,
                    CommandId::new("custom.replacement").unwrap()
                );
            }
            other => panic!("expected Shadowed, got {other:?}"),
        }
    }

    #[test]
    fn non_colliding_bindings_do_not_conflict() {
        let base = file(
            vec![
                binding("panel", "f5", "ops.copy"),
                binding("panel", "f6", "ops.move_or_rename"),
            ],
            vec![],
        );
        let resolved = resolve([(KeymapSource::Base(BaseKeymap::Tc), base)]);
        assert_eq!(resolved.bindings.len(), 2);
        assert!(resolved.diagnostics.is_empty());
    }

    #[test]
    fn different_context_with_same_key_does_not_conflict() {
        // Same key, genuinely different (non-identical) context -- per this
        // module's documented limitation, structurally different contexts
        // never collide even if they overlap at runtime.
        let base = file(
            vec![
                binding("panel", "enter", "nav.enter_dir"),
                binding("cmdline", "enter", "cmdline.execute"),
            ],
            vec![],
        );
        let resolved = resolve([(KeymapSource::Base(BaseKeymap::Tc), base)]);
        assert_eq!(resolved.bindings.len(), 2);
        assert!(resolved.diagnostics.is_empty());
    }

    #[test]
    fn unbind_removes_inherited_binding() {
        let base = file(vec![binding("panel", "ctrl-w", "tab.close")], vec![]);
        let user = file(vec![], vec![unbind("panel", "ctrl-w", None)]);

        let resolved = resolve([
            (KeymapSource::Base(BaseKeymap::Tc), base),
            (KeymapSource::User, user),
        ]);

        assert!(resolved.bindings.is_empty());
        assert!(
            resolved.diagnostics.is_empty(),
            "a successful unbind is not itself a diagnostic"
        );
    }

    #[test]
    fn unbind_restricted_by_command_only_removes_matching_binding() {
        // Two bindings would collide on (context, key) if both were present
        // at once, so exercise the command-restricted unbind on a single
        // binding plus a no-match case to prove the command filter is
        // actually applied (not ignored).
        let base = file(vec![binding("panel", "ctrl-w", "tab.close")], vec![]);
        let user = file(
            vec![],
            vec![unbind("panel", "ctrl-w", Some("some.other_command"))],
        );

        let resolved = resolve([
            (KeymapSource::Base(BaseKeymap::Tc), base),
            (KeymapSource::User, user),
        ]);

        // The unbind named a different command than what's actually bound,
        // so it should not match, and the binding survives.
        assert_eq!(resolved.bindings.len(), 1);
        assert_eq!(resolved.diagnostics.len(), 1);
        assert!(matches!(
            resolved.diagnostics[0],
            KeymapDiagnostic::UnbindNoMatch { .. }
        ));
    }

    #[test]
    fn unbind_with_no_match_is_a_diagnostic_not_an_error() {
        let user = file(vec![], vec![unbind("panel", "ctrl-z", None)]);
        let resolved = resolve([(KeymapSource::User, user)]);
        assert!(resolved.bindings.is_empty());
        assert_eq!(resolved.diagnostics.len(), 1);
        match &resolved.diagnostics[0] {
            KeymapDiagnostic::UnbindNoMatch { key, .. } => assert_eq!(key.to_string(), "ctrl-z"),
            other => panic!("expected UnbindNoMatch, got {other:?}"),
        }
    }

    #[test]
    fn find_looks_up_exact_context_and_key() {
        let base = file(vec![binding("panel", "f5", "ops.copy")], vec![]);
        let resolved = resolve([(KeymapSource::Base(BaseKeymap::Tc), base)]);
        let ctx = parse_predicate("panel").unwrap();
        let key = KeyChord::parse("f5").unwrap();
        assert!(resolved.find(&ctx, &key).is_some());

        let other_key = KeyChord::parse("f6").unwrap();
        assert!(resolved.find(&ctx, &other_key).is_none());
    }

    #[test]
    fn diagnostic_display_is_human_readable() {
        let base = file(vec![binding("panel", "f5", "ops.copy")], vec![]);
        let user = file(vec![binding("panel", "f5", "custom.replacement")], vec![]);
        let resolved = resolve([
            (KeymapSource::Base(BaseKeymap::Tc), base),
            (KeymapSource::User, user),
        ]);
        let text = resolved.diagnostics[0].to_string();
        assert!(text.contains("ops.copy"), "{text}");
        assert!(text.contains("custom.replacement"), "{text}");
        assert!(text.contains("f5"), "{text}");
    }
}
