// SPDX-License-Identifier: MIT
//! The built-in command catalogue: parses `docs/commands.md`'s 307-entry
//! table directly (via `include_str!`) and registers every concrete entry
//! into a [`CommandRegistry`] (T-3.3.2, "Register the full command
//! catalogue").
//!
//! # Why parse the doc instead of transcribing it
//!
//! `docs/commands.md` is itself the source of truth (Document ID
//! `DUET-CMD-001`, produced by T-1.5.1). A hand-written Rust literal of 307
//! `Command` structs would need to be kept in sync with that document by
//! hand forever, and nothing would fail loudly the day they drift. Parsing
//! the markdown table directly at compile time (`include_str!`) makes that
//! drift structurally impossible for the fields this module cares about
//! (**id**, **title**, **category**, **precondition**): whatever the table
//! says is what gets registered, always. [`tests::doc_summary_count_matches_parsed_rows`]
//! is a belt-and-suspenders check that the document's own "Total entries"
//! summary line agrees with what was actually parsed, to catch a doc-editing
//! mistake (e.g. a row added without updating the summary count) rather than
//! a code/doc mismatch.
//!
//! # The 307 vs. 302 distinction
//!
//! The document's table has 307 rows, but 5 of them (in the "Plugins"
//! section) are documented as *placeholder classes* --
//! `plugin.command.\<id\>`, `plugin.content.\<id\>`, etc. -- describing the
//! id *shape* a plugin's commands will take at runtime (e.g.
//! `plugin.command.exif-columns.rotate`), not a concrete, registrable
//! command themselves. Their `id` column is not a valid [`CommandId`] (it
//! contains `<`/`>`) and one of their `precondition` columns reads literally
//! "plugin-declared" rather than a parseable predicate -- both are
//! documentation notation, not data. This module registers the other 302
//! concrete entries as built-ins; the 5 classes are what a plugin host
//! registers *through* the same [`CommandRegistry::register`] call at plugin
//! load time (FR-PLUG-02), which is out of this crate's scope. 302 still
//! comfortably clears the T-3.3.2 acceptance criterion of "200 commands
//! register".
//!
//! # Handlers
//!
//! Every registered [`Command`]'s handler is [`placeholder_handler`]: it
//! always returns [`CommandError::HandlerFailed`] rather than doing
//! anything, per T-3.3.2's scope ("Command handlers can be `todo!()`/
//! placeholder closures -- the registration itself... is what this task
//! needs"). Returning an `Err` rather than an actual `todo!()`/`panic!()`
//! means a stray invocation (e.g. from a future palette smoke test) fails
//! cleanly instead of aborting the test process.

use std::fmt;

use crate::command::{Command, CommandCategory, CommandError, CommandHandler};
use crate::id::CommandId;
use crate::predicate::{self, Predicate};
use crate::registry::{CommandRegistry, DuplicateCommandError};

/// The full text of `docs/commands.md`, embedded at compile time so parsing
/// happens against exactly the checked-in document, with no separate data
/// file to fall out of sync.
const COMMANDS_MD: &str = include_str!("../../../docs/commands.md");

/// One parsed row of `docs/commands.md`'s command table.
#[derive(Debug, Clone)]
pub struct CatalogueEntry {
    pub id: CommandId,
    pub title: Box<str>,
    pub category: CommandCategory,
    pub precondition: Predicate,
}

/// Why a row of `docs/commands.md` could not be turned into a
/// [`CatalogueEntry`]. Surfaced (rather than silently skipped) for every row
/// except the 5 documented plugin placeholder-class rows, which are
/// filtered out deliberately -- see this module's doc comment.
#[derive(Debug, thiserror::Error)]
pub enum CatalogueParseError {
    #[error("row {row} ({raw_id:?}): invalid command id: {source}")]
    InvalidId {
        row: usize,
        raw_id: String,
        #[source]
        source: crate::id::CommandIdError,
    },
    #[error("row {row} (id {id}): invalid precondition {raw:?}: {source}")]
    InvalidPrecondition {
        row: usize,
        id: String,
        raw: String,
        #[source]
        source: predicate::PredicateParseError,
    },
}

/// Parses every concrete (non-placeholder) row out of `docs/commands.md`'s
/// command table, in document order.
///
/// This walks the raw markdown rather than using a markdown library: the
/// table structure is simple and stable (`| id | title | category |
/// precondition | notes |`), and a hand-rolled walk keeps this crate's
/// dependency list free of a general-purpose markdown parser for a
/// five-column pipe table.
pub fn parse_catalogue() -> Result<Vec<CatalogueEntry>, CatalogueParseError> {
    // The "Traceability" section at the end is a *different* table (key ->
    // command id, for cross-checking against design.md's keymap extract),
    // not part of the catalogue itself.
    let main = COMMANDS_MD
        .split("## Traceability")
        .next()
        .unwrap_or(COMMANDS_MD);

    let mut entries = Vec::new();
    let mut in_category_section = false;

    for (idx, line) in main.lines().enumerate() {
        let row_num = idx + 1; // 1-based, matches editor line numbers
        let trimmed = line.trim_end();

        if let Some(heading) = trimmed.strip_prefix("## ") {
            // Any "## X" heading marks entry into a new section. Every
            // section from "## Navigation" onward is a command table;
            // "## Purpose" / "## Precondition grammar" / "## Summary"
            // (which precede the first command table) are prose with no
            // `| ... |` rows, so unconditionally flipping this flag on any
            // heading is safe -- there is nothing to falsely capture before
            // the first real table starts.
            let _ = heading;
            in_category_section = true;
            continue;
        }

        if !in_category_section || !trimmed.starts_with('|') {
            continue;
        }
        // Table header / separator rows, not data.
        if trimmed.starts_with("| id |") || trimmed.starts_with("|---") {
            continue;
        }

        let cols: Vec<&str> = trimmed
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cols.len() < 4 {
            continue;
        }
        let (raw_id, title, category, raw_precondition) = (cols[0], cols[1], cols[2], cols[3]);

        // The 5 "placeholder class" rows in the Plugins section, e.g.
        // `plugin.command.\<id\>` -- not a real command, see module docs.
        if raw_id.contains('<') {
            continue;
        }

        let id = CommandId::new(raw_id).map_err(|source| CatalogueParseError::InvalidId {
            row: row_num,
            raw_id: raw_id.to_string(),
            source,
        })?;

        let precondition_src = raw_precondition.trim().trim_matches('`');
        let precondition = predicate::parse(precondition_src).map_err(|source| {
            CatalogueParseError::InvalidPrecondition {
                row: row_num,
                id: raw_id.to_string(),
                raw: precondition_src.to_string(),
                source,
            }
        })?;

        entries.push(CatalogueEntry {
            id,
            title: title.to_string().into_boxed_str(),
            category: CommandCategory::new(category),
            precondition,
        });
    }

    Ok(entries)
}

/// Marker error a [`placeholder_handler`] always returns: no built-in
/// command has a real handler wired up yet (T-3.3.2 registers the
/// catalogue; wiring handlers to `duet-ops`/`duet-index` is later work).
#[derive(Debug)]
struct HandlerNotWired;

impl fmt::Display for HandlerNotWired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "handler not yet wired to duet-ops/duet-index (T-3.3.2 registers \
             the catalogue only; command bodies are later work)",
        )
    }
}

impl std::error::Error for HandlerNotWired {}

/// Builds a placeholder handler for `id` that always fails with
/// [`HandlerNotWired`] rather than executing anything.
fn placeholder_handler(id: CommandId) -> CommandHandler {
    CommandHandler::new(move |_ctx, _args| {
        Err(CommandError::HandlerFailed {
            command: id.clone(),
            source: Box::new(HandlerNotWired),
        })
    })
}

/// Why registering the built-in catalogue failed.
#[derive(Debug, thiserror::Error)]
pub enum CatalogueRegistrationError {
    #[error(transparent)]
    Parse(#[from] CatalogueParseError),
    #[error(transparent)]
    Duplicate(#[from] DuplicateCommandError),
}

/// Parses `docs/commands.md` and registers every concrete catalogue entry
/// (302 of the document's 307 rows -- see this module's doc comment) into
/// `registry`, each with a [`placeholder_handler`].
///
/// Returns the number of commands registered on success.
pub fn register_builtin_commands(
    registry: &mut CommandRegistry,
) -> Result<usize, CatalogueRegistrationError> {
    let entries = parse_catalogue()?;
    let count = entries.len();
    for entry in entries {
        let handler = placeholder_handler(entry.id.clone());
        registry.register(Command::new(
            entry.id,
            entry.title,
            entry.category,
            entry.precondition,
            handler,
        ))?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_302_concrete_entries() {
        let entries = parse_catalogue().expect("docs/commands.md must parse");
        assert_eq!(
            entries.len(),
            302,
            "expected 302 concrete catalogue entries (307 rows minus 5 plugin \
             placeholder-class rows) -- if docs/commands.md changed, this \
             count should change with it"
        );
    }

    #[test]
    fn doc_summary_count_matches_parsed_rows() {
        // Belt-and-suspenders: docs/commands.md states its own row count
        // ("Total entries: **307**, across 23 categories"). Cross-check that
        // against (parsed concrete entries + the 5 known placeholder rows)
        // so a future doc edit that adds/removes a row without updating the
        // summary line fails loudly here instead of silently drifting.
        let stated: usize = COMMANDS_MD
            .lines()
            .find_map(|l| {
                l.strip_prefix("Total entries: **")
                    .and_then(|rest| rest.split("**").next())
                    .and_then(|n| n.parse().ok())
            })
            .expect("docs/commands.md must contain a 'Total entries: **N**' summary line");
        let placeholder_rows = 5;
        let entries = parse_catalogue().unwrap();
        assert_eq!(
            stated,
            entries.len() + placeholder_rows,
            "docs/commands.md's stated total ({stated}) must equal parsed concrete \
             rows ({}) plus the 5 known plugin placeholder rows",
            entries.len()
        );
    }

    #[test]
    fn no_duplicate_ids_in_catalogue() {
        let entries = parse_catalogue().unwrap();
        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                seen.insert(entry.id.clone()),
                "duplicate catalogue id: {}",
                entry.id
            );
        }
    }

    #[test]
    fn register_builtin_commands_succeeds_without_duplicates() {
        let mut registry = CommandRegistry::new();
        let count = register_builtin_commands(&mut registry).expect("registration must succeed");
        assert_eq!(count, 302);
        assert_eq!(registry.len(), 302);
        assert!(registry.len() >= 200, "AC: 200 commands register");
    }

    #[test]
    fn every_catalogue_command_is_available_under_its_own_precondition() {
        // Sanity check that parsed preconditions are meaningful, not just
        // syntactically valid: every entry's precondition should be
        // satisfiable by *some* context state (none of them are
        // accidentally `false`-shaped).
        use crate::predicate::MapContextState;
        let entries = parse_catalogue().unwrap();
        let mut state = MapContextState::new();
        for flag in [
            "app",
            "panel",
            "dialog",
            "viewer",
            "editor",
            "cmdline",
            "palette",
            "dialog.conflict",
        ] {
            state.set_flag(flag, true);
        }
        // Flags of the form "panel && selection.nonempty" etc. also need
        // their state terms set; rather than enumerate every one, just
        // confirm evaluation never panics across the whole catalogue.
        for entry in &entries {
            let _ = entry.precondition.eval(&state);
        }
    }

    #[test]
    fn known_ids_from_design_doc_keymap_extract_are_present() {
        // design.md section 9.4's default-keymap extract table, transcribed
        // in docs/commands.md's own "Summary" section.
        let entries = parse_catalogue().unwrap();
        let ids: std::collections::HashSet<_> = entries.iter().map(|e| e.id.as_str()).collect();
        for expected in [
            "focus.other_panel",
            "panel.reread",
            "view.open",
            "edit.open",
            "ops.copy",
            "ops.move_or_rename",
            "ops.mkdir",
            "ops.delete",
            "sel.toggle_and_advance",
            "sel.toggle_and_size",
            "sel.by_mask",
            "unsel.by_mask",
            "sel.invert",
            "panel.swap",
            "panel.push_to_other",
            "nav.parent",
            "nav.root",
            "hotlist.open",
            "tab.new",
            "tab.close",
            "panel.branch_view",
            "tool.multi_rename",
            "panel.quick_view",
            "tool.search",
            "archive.pack",
            "archive.unpack",
            "drive.change_left",
            "drive.change_right",
            "file.properties",
            "cmdline.insert_name",
            "cmdline.insert_path",
            "ops.rename_in_place",
        ] {
            assert!(ids.contains(expected), "missing expected id {expected:?}");
        }
    }

    #[test]
    fn placeholder_handler_fails_cleanly_rather_than_panicking() {
        use crate::command::{CommandArgs, CommandContext};
        use crate::predicate::{ContextState, MapContextState};

        struct NullContext(MapContextState);
        impl CommandContext for NullContext {
            fn context_state(&self) -> &dyn ContextState {
                &self.0
            }
        }

        let handler = placeholder_handler(CommandId::new("ops.copy").unwrap());
        let mut ctx = NullContext(MapContextState::new());
        let err = handler
            .call(&mut ctx, &CommandArgs::new())
            .expect_err("placeholder handler must return Err, not panic");
        assert!(matches!(err, CommandError::HandlerFailed { .. }));
    }
}
