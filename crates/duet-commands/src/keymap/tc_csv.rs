// SPDX-License-Identifier: MIT
//! Loads `docs/keymap-tc.csv` -- T-1.5.1's 151-row survey of Total
//! Commander's real keybindings -- into this crate's [`Binding`]/
//! [`KeymapLayer`] types (T-3.3.2, "Load the real TC keymap").
//!
//! The CSV is raw survey data, not `keymap.toml`: TC-style key spelling
//! (`"Shift+F6"`, `"Num +"`, `"Ctrl+1..Ctrl+9"`), a `context` column that
//! uses `"global"` where `docs/commands.md`'s finalised precondition grammar
//! says `"app"`, and an extra `category`/`description`/`confidence`/`notes`
//! columns this crate has no use for. None of that stops it from loading --
//! see [`load`] for the translation rules and [`Skipped`] for the handful of
//! rows that can't become a literal [`KeyChord`] at all.
//!
//! # A verified, not assumed, finding
//!
//! T-3.3.2's brief said "T-1.5.1's report already confirmed the keymap's
//! command ids and the catalogue's ids are consistent... but verify it,
//! don't assume." Verifying it (via
//! [`crate::keymap::ResolvedKeymap::unknown_commands`], exercised in this
//! module's tests against the real file) shows they are **not** fully
//! consistent: of the CSV's 138 unique `command_name` values, roughly half
//! do not match any id in the finalised `docs/commands.md` catalogue (e.g.
//! `ops.new_file`, `ops.copy_same_dir`, `tab.switch_to_n`, the whole
//! `text.*`/`dlg.*`/`viewer.*` families used for generic text-field and
//! dialog chrome that never made it into the catalogue as named commands).
//! This is a real, useful finding, not a loader bug -- see this module's
//! tests for the exact count and this crate's `CHANGELOG`-equivalent (the
//! T-3.3.2 completion report) for the full list. Per `design.md` §9.4, a
//! keymap binding referencing an unregistered command is a load-time
//! diagnostic, not a fatal error -- exactly like [`KeymapDiagnostic::Shadowed`]
//! and [`KeymapDiagnostic::UnbindNoMatch`] -- so this loader does not fail on
//! it; resolving this loader's output and calling
//! [`crate::keymap::ResolvedKeymap::unknown_commands`] against a real
//! [`crate::CommandRegistry`] surfaces it instead (see this module's
//! `documents_unresolved_commands_against_the_real_catalogue` test).
//!
//! # Untranslatable rows
//!
//! Five rows use the literal key `"(none)"` (documented as having no TC
//! keyboard accelerator at all -- menu/toolbar only) and one uses
//! `"[a-z0-9]"` (any printable character starts quick-search -- a wildcard,
//! not a literal key). None of the six can become a [`KeyChord`]; [`load`]
//! skips them and reports why via [`Skipped`] rather than silently dropping
//! them.
//!
//! # Range rows are expanded, not skipped
//!
//! Two rows spell a range: `"Ctrl+1..Ctrl+9"` (`tab.switch_to_n`) and
//! `"Ctrl+4..Ctrl+9"` (`view.mode_custom_n`). [`load`] expands each into one
//! [`Binding`] per key in the range rather than skipping them, because doing
//! so surfaces a real conflict the CSV's own `notes` column already flags by
//! hand: `tab.switch_to_n`'s `Ctrl+1..Ctrl+9` is later shadowed at `ctrl-1`/
//! `ctrl-2`/`ctrl-3` by the explicit `view.mode_brief`/`_full`/`_wide` rows,
//! and at `ctrl-4..ctrl-9` by `view.mode_custom_n`'s own range row -- exactly
//! the "may actually collide... needs reconciling by hand" the source data
//! calls out. Expanding the ranges lets this crate's existing
//! [`KeymapDiagnostic::Shadowed`] detection catch that collision for real,
//! at the 151-real-binding scale T-3.3.2 asks for, instead of taking the
//! CSV author's word for it.

use std::sync::OnceLock;

use crate::id::CommandId;
use crate::keymap::{Binding, KeymapLayer, KeymapSource, SourceLocation};
use crate::predicate::{self, Predicate};

use super::key::KeyChord;

/// The full text of `docs/keymap-tc.csv`, embedded at compile time.
const TC_KEYMAP_CSV: &str = include_str!("../../../../docs/keymap-tc.csv");

/// The CSV's own name for this source, used as `SourceLocation::file`.
const SOURCE_FILE: &str = "docs/keymap-tc.csv";

/// A row that could not become a literal [`Binding`], with why.
#[derive(Debug, Clone)]
pub struct Skipped {
    pub location: SourceLocation,
    pub key_combo: Box<str>,
    pub command: Box<str>,
    pub reason: Box<str>,
}

/// The result of [`load`]: a ready-to-resolve [`KeymapLayer`] plus every row
/// that could not be translated into a literal binding.
#[derive(Debug)]
pub struct TcKeymapLoad {
    pub layer: KeymapLayer,
    pub skipped: Vec<Skipped>,
}

/// Parses `docs/keymap-tc.csv` into a [`KeymapLayer`] (source-tagged
/// [`KeymapSource::Base(BaseKeymap::Tc)`](crate::keymap::BaseKeymap::Tc)),
/// ready to feed into [`crate::keymap::resolve_with_locations`].
///
/// Never panics on the shipped file (every row's `key_combo`/`context`/
/// `command_name` is well-formed per this module's tests); a malformed
/// future edit to the CSV surfaces as a `panic!` with the offending row's
/// line number, since this loader has no caller-facing error type to report
/// through and a build-time-embedded, checked-in data file failing to parse
/// is a programmer error, not a runtime condition to recover from.
pub fn load() -> TcKeymapLoad {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(TC_KEYMAP_CSV.as_bytes());

    let mut bindings = Vec::new();
    let mut skipped = Vec::new();

    for result in reader.records() {
        let record = result.expect("docs/keymap-tc.csv must be valid CSV");
        let line = record
            .position()
            .map(|p| p.line())
            .expect("csv records from a Reader always carry a position");
        let location = || SourceLocation {
            file: SOURCE_FILE.into(),
            line: Some(line as u32),
        };

        let key_combo = record.get(0).unwrap_or_default();
        let command_name = record.get(1).unwrap_or_default();
        let context_raw = record.get(3).unwrap_or_default();

        let context = translate_context(context_raw);

        match translate_key(key_combo) {
            KeyTranslation::Single(key_str) => {
                bindings.push(make_binding(&context, &key_str, command_name, location()));
            }
            KeyTranslation::Range(steps) => {
                for key_str in steps {
                    bindings.push(make_binding(&context, &key_str, command_name, location()));
                }
            }
            KeyTranslation::Unrepresentable(reason) => {
                skipped.push(Skipped {
                    location: location(),
                    key_combo: key_combo.into(),
                    command: command_name.into(),
                    reason: reason.into(),
                });
            }
        }
    }

    TcKeymapLoad {
        layer: KeymapLayer {
            source: KeymapSource::Base(crate::keymap::BaseKeymap::Tc),
            bindings,
            unbinds: Vec::new(),
        },
        skipped,
    }
}

fn make_binding(
    context: &Predicate,
    key_str: &str,
    command_name: &str,
    location: SourceLocation,
) -> (Binding, Option<SourceLocation>) {
    let binding = Binding {
        context: context.clone(),
        key: KeyChord::parse(key_str).unwrap_or_else(|e| {
            panic!("docs/keymap-tc.csv: failed to parse translated key {key_str:?}: {e}")
        }),
        command: CommandId::new(command_name).unwrap_or_else(|e| {
            panic!("docs/keymap-tc.csv: invalid command id {command_name:?}: {e}")
        }),
        args: None,
    };
    (binding, Some(location))
}

/// Translates the CSV's `context` column into this crate's precondition
/// grammar. Every value the shipped file uses (`panel`, `cmdline`, `dialog`,
/// `viewer`, `editor`) is already a valid bare flag term and round-trips
/// unchanged; `"global"` is the one exception -- the pre-catalogue name for
/// what `docs/commands.md`'s finalised grammar calls `"app"` ("always true
/// -- global scope, no panel/dialog focus required"), so it is mapped
/// explicitly rather than left to parse as an always-false unknown flag.
fn translate_context(raw: &str) -> Predicate {
    let mapped = if raw == "global" { "app" } else { raw };
    predicate::parse(mapped)
        .unwrap_or_else(|e| panic!("docs/keymap-tc.csv: invalid context {raw:?}: {e}"))
}

enum KeyTranslation {
    Single(String),
    Range(Vec<String>),
    Unrepresentable(&'static str),
}

/// Translates one `key_combo` cell into this crate's `KeyChord::parse`
/// syntax (modifiers joined by `-`, lower-case, e.g. `"shift-f6"`), or
/// reports why it can't be a literal chord at all.
fn translate_key(raw: &str) -> KeyTranslation {
    match raw {
        "(none)" => return KeyTranslation::Unrepresentable("no default keyboard accelerator"),
        "[a-z0-9]" => {
            return KeyTranslation::Unrepresentable(
                "wildcard: any printable character, not a literal key",
            );
        }
        "Ctrl+1..Ctrl+9" => {
            return KeyTranslation::Range((1..=9).map(|n| format!("ctrl-{n}")).collect());
        }
        "Ctrl+4..Ctrl+9" => {
            return KeyTranslation::Range((4..=9).map(|n| format!("ctrl-{n}")).collect());
        }
        _ => {}
    }
    KeyTranslation::Single(tc_key_to_chord_syntax(raw))
}

/// Rewrites one non-range, non-`(none)` TC key spelling into `KeyChord`
/// syntax: modifiers and key joined by `-`, all lower-case.
///
/// The one wrinkle is the numpad keys (`"Num +"`, `"Num -"`, `"Num *"`,
/// `"Num Enter"`): TC spells them with an internal space, and `+` inside
/// `"Num +"` is a literal key name, not (as everywhere else in this column)
/// a modifier separator. `KeyChord`'s own grammar has no room for a space
/// within one chord *step* either -- a space there means a new chord step
/// (`"ctrl-k ctrl-b"` is two steps) -- so each numpad spelling is rewritten
/// to a single hyphen-free token (`"kpplus"`, `"kpminus"`, `"kpmultiply"`,
/// `"kpenter"`) before the generic `+`-splitting below ever sees it. This
/// keeps `"Ctrl+Num +"` working too: the substring replace happens first, so
/// it becomes `"Ctrl+kpplus"`, which splits cleanly into modifier `ctrl` and
/// key `kpplus`.
fn tc_key_to_chord_syntax(raw: &str) -> String {
    let rewritten = raw
        .replace("Num Enter", "kpenter")
        .replace("Num +", "kpplus")
        .replace("Num -", "kpminus")
        .replace("Num *", "kpmultiply");
    rewritten
        .split('+')
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("-")
}

/// A lazily-built, process-wide [`TcKeymapLoad`] of the shipped
/// `docs/keymap-tc.csv`. Parsing 151 rows is cheap, but there is no reason
/// to redo it on every call from `duet-ui`/tests when the source text is
/// `'static`.
static LOADED: OnceLock<TcKeymapLoad> = OnceLock::new();

/// Borrows the process-wide cached [`load`] result.
pub fn loaded() -> &'static TcKeymapLoad {
    LOADED.get_or_init(load)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::resolve_with_locations;
    use crate::registry::CommandRegistry;

    /// `docs/keymap-tc.csv` has 151 data rows: 6 that can't become a literal
    /// key (5 `"(none)"` + 1 `"[a-z0-9]"` wildcard) plus 143 that can
    /// directly, plus 2 range rows that expand into 9 + 6 = 15 bindings each
    /// keyed individually -- 143 + 15 = 158 real `Binding`s total.
    #[test]
    fn loads_151_rows_into_158_bindings_and_6_skips() {
        let loaded = load();
        assert_eq!(loaded.skipped.len(), 6, "{:#?}", loaded.skipped);
        assert_eq!(loaded.layer.bindings.len(), 158);
    }

    #[test]
    fn every_binding_has_a_populated_line_number() {
        let loaded = load();
        for (binding, location) in &loaded.layer.bindings {
            let loc = location
                .as_ref()
                .unwrap_or_else(|| panic!("binding for {} has no location", binding.command));
            assert_eq!(&*loc.file, "docs/keymap-tc.csv");
            assert!(loc.line.is_some());
        }
    }

    #[test]
    fn every_skip_has_a_populated_line_number_and_reason() {
        let loaded = load();
        for skip in &loaded.skipped {
            assert!(skip.location.line.is_some());
            assert!(!skip.reason.is_empty());
        }
    }

    /// The AC: "the TC keymap loads" -- feeding all 158 real bindings
    /// through the shared resolver must not panic, and must produce the
    /// exact conflict diagnostics the raw data predicts (see this module's
    /// doc comment): the genuine `panel`/`enter` collision between
    /// `archive.browse_as_dir` and `nav.open_or_enter`, plus the
    /// `tab.switch_to_n` vs. `view.mode_*` range collision the CSV's own
    /// notes flagged as needing "reconciling by hand".
    #[test]
    fn resolves_with_expected_shadow_diagnostics() {
        let loaded = load();
        let resolved = resolve_with_locations([loaded.layer]);

        // 158 bindings in, 10 shadowed away (1 genuine `enter` collision +
        // 9 range-vs-explicit `ctrl-N` collisions), 148 survive.
        assert_eq!(resolved.bindings.len(), 148);
        assert_eq!(resolved.diagnostics.len(), 10);

        let shadowed_pairs: Vec<(String, String)> = resolved
            .diagnostics
            .iter()
            .map(|d| match d {
                crate::keymap::KeymapDiagnostic::Shadowed {
                    shadowed_command,
                    winning_command,
                    ..
                } => (shadowed_command.to_string(), winning_command.to_string()),
                other => panic!("expected only Shadowed diagnostics, got {other:?}"),
            })
            .collect();

        assert!(
            shadowed_pairs.contains(&(
                "archive.browse_as_dir".to_string(),
                "nav.open_or_enter".to_string()
            )),
            "expected the panel/enter collision to be reported: {shadowed_pairs:?}"
        );
        let switch_to_n_shadows = shadowed_pairs
            .iter()
            .filter(|(shadowed, _)| shadowed == "tab.switch_to_n")
            .count();
        assert_eq!(
            switch_to_n_shadows, 9,
            "expected tab.switch_to_n to be shadowed at all 9 of ctrl-1..ctrl-9: {shadowed_pairs:?}"
        );
    }

    /// Every diagnostic produced from this loader carries file/line -- the
    /// T-3.3.2 AC ("binding conflicts produce diagnostics with file/line"),
    /// exercised against the real 151-row file rather than a toy fixture.
    #[test]
    fn shadow_diagnostics_carry_file_and_line() {
        let loaded = load();
        let resolved = resolve_with_locations([loaded.layer]);
        assert!(!resolved.diagnostics.is_empty());
        for diagnostic in &resolved.diagnostics {
            let text = diagnostic.to_string();
            assert!(
                text.contains("docs/keymap-tc.csv:"),
                "diagnostic missing file/line: {text}"
            );
            match diagnostic {
                crate::keymap::KeymapDiagnostic::Shadowed {
                    shadowed_location,
                    winning_location,
                    ..
                } => {
                    assert!(shadowed_location.is_some());
                    assert!(winning_location.is_some());
                }
                other => panic!("expected only Shadowed diagnostics, got {other:?}"),
            }
        }
    }

    /// The verified-not-assumed finding: cross-checking the real, resolved
    /// TC keymap against the real, registered catalogue turns up a
    /// meaningful number of bindings whose `command` was never registered.
    /// This is not a loader bug -- see this module's doc comment -- but it
    /// is exactly the kind of thing T-3.3.2 asked to be verified rather than
    /// taken on faith.
    #[test]
    fn documents_unresolved_commands_against_the_real_catalogue() {
        let mut registry = CommandRegistry::new();
        crate::catalogue::register_builtin_commands(&mut registry).unwrap();

        let loaded = load();
        let resolved = resolve_with_locations([loaded.layer]);
        let unresolved = resolved.unknown_commands(&registry);

        let unique_unresolved: std::collections::HashSet<&str> = unresolved
            .iter()
            .map(|b| b.binding.command.as_str())
            .collect();

        // Snapshot count: if this drifts, either docs/commands.md grew the
        // missing ids (great -- lower this number) or docs/keymap-tc.csv
        // changed (update the count deliberately, don't just bump it).
        assert_eq!(
            unique_unresolved.len(),
            73,
            "unique commands referenced by the TC keymap but not in the catalogue \
             changed from the last verified count -- update deliberately, not blindly: {unique_unresolved:?}"
        );
    }
}
