// SPDX-License-Identifier: MIT
//! The command palette index (FR-TOOL-11): a fuzzy-searchable listing of
//! every registered command together with its current keybinding(s), per
//! `docs/commands.md`'s "The command palette (FR-TOOL-11) indexes directly"
//! and design.md §9.4.
//!
//! Fuzzy scoring reuses `nucleo-matcher`, the same crate family design.md
//! §9.2 already settles on for FR-NAV-13's panel quick-search ("`nucleo` --
//! the same crate family Zed/GPUI's own fuzzy finder already depends on
//! transitively, so no new dependency weight"), so the app doesn't carry two
//! different fuzzy-matching implementations for what is conceptually the
//! same feature.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::command::CommandCategory;
use crate::id::CommandId;
use crate::keymap::{KeyChord, ResolvedKeymap};
use crate::registry::CommandRegistry;

/// One entry in the palette: a command's identity plus every currently
/// resolved keybinding for it (a command can be bound multiple times, e.g.
/// once in `panel` context and once in `dialog` context).
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub id: CommandId,
    pub title: Box<str>,
    pub category: CommandCategory,
    pub bindings: Vec<KeyChord>,
}

/// A scored search result: a reference into the index plus the fuzzy-match
/// score nucleo assigned (higher is a better match; see
/// [`nucleo_matcher::pattern::Pattern::score`]).
#[derive(Debug, Clone, Copy)]
pub struct PaletteMatch<'a> {
    pub entry: &'a PaletteEntry,
    pub score: u32,
}

/// The command palette index: every registered command, snapshotted
/// together with its resolved keybindings, searchable by fuzzy query.
///
/// This is a **snapshot**, not a live view -- it is rebuilt (via
/// [`PaletteIndex::build`]) whenever the registry or the resolved keymap
/// changes (command registration/unregistration from a plugin
/// enable/disable, or a keymap reload). That mirrors how `duet-index`'s
/// `DirectoryModel` handles change (§9.2's `generation` counter): rebuild
/// on mutation, not incremental patching, because the data is small (hundreds
/// of entries, not millions) and rebuilding is cheap relative to a
/// keystroke's fuzzy-search cost anyway.
pub struct PaletteIndex {
    entries: Vec<PaletteEntry>,
}

impl PaletteIndex {
    /// Builds a palette index from every command currently in `registry`,
    /// annotated with whatever bindings currently resolve to each command
    /// id in `keymap`.
    pub fn build(registry: &CommandRegistry, keymap: &ResolvedKeymap) -> Self {
        let entries = registry
            .iter()
            .map(|command| {
                let bindings = keymap
                    .bindings
                    .iter()
                    .filter(|b| b.binding.command == command.id)
                    .map(|b| b.binding.key.clone())
                    .collect();
                PaletteEntry {
                    id: command.id.clone(),
                    title: command.title.clone(),
                    category: command.category.clone(),
                    bindings,
                }
            })
            .collect();
        Self { entries }
    }

    /// The number of indexed commands.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no commands.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates every entry, unfiltered, in registration order (useful for
    /// an "all commands" browse view before the user types a query).
    pub fn iter(&self) -> impl Iterator<Item = &PaletteEntry> {
        self.entries.iter()
    }

    /// Fuzzy-searches the index by `query`, matching against each entry's
    /// title and id (so both `"copy sel"` and `"ops.copy"` find `ops.copy`
    /// -- "Copy selection to the target panel"), sorted best-match-first.
    ///
    /// An empty query returns every entry, in index order (score 0), which
    /// is the palette's default "just opened, no input yet" state.
    pub fn search(&self, query: &str) -> Vec<PaletteMatch<'_>> {
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();

        let mut scored: Vec<PaletteMatch<'_>> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let haystack = format!("{} {}", entry.title, entry.id);
                let score = pattern.score(Utf32Str::new(&haystack, &mut buf), &mut matcher)?;
                Some(PaletteMatch { entry, score })
            })
            .collect();

        scored.sort_by_key(|m| std::cmp::Reverse(m.score));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, CommandHandler};
    use crate::keymap::{BaseKeymap, KeymapSource, resolve};
    use crate::predicate::parse as parse_predicate;

    fn command(id: &str, title: &str, category: &str) -> Command {
        Command::new(
            CommandId::new(id).unwrap(),
            title,
            category,
            parse_predicate("app").unwrap(),
            CommandHandler::new(|_ctx, _args| Ok(())),
        )
    }

    fn sample_registry() -> CommandRegistry {
        let mut reg = CommandRegistry::new();
        reg.register(command(
            "ops.copy",
            "Copy selection to the target panel",
            "File operations",
        ))
        .unwrap();
        reg.register(command(
            "ops.move_or_rename",
            "Move selection to the target panel",
            "File operations",
        ))
        .unwrap();
        reg.register(command(
            "nav.parent",
            "Go to parent directory",
            "Navigation",
        ))
        .unwrap();
        reg
    }

    fn sample_keymap() -> ResolvedKeymap {
        use crate::keymap::{Binding, KeyChord, KeymapFile};
        let base = KeymapFile {
            schema_version: 1,
            base: BaseKeymap::Tc,
            bindings: vec![
                Binding {
                    context: parse_predicate("panel").unwrap(),
                    key: KeyChord::parse("f5").unwrap(),
                    command: CommandId::new("ops.copy").unwrap(),
                    args: None,
                },
                Binding {
                    context: parse_predicate("panel").unwrap(),
                    key: KeyChord::parse("f6").unwrap(),
                    command: CommandId::new("ops.move_or_rename").unwrap(),
                    args: None,
                },
            ],
            unbinds: vec![],
        };
        resolve([(KeymapSource::Base(BaseKeymap::Tc), base)])
    }

    #[test]
    fn build_indexes_every_registered_command() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn build_attaches_resolved_bindings() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        let copy_entry = index
            .iter()
            .find(|e| e.id == CommandId::new("ops.copy").unwrap())
            .unwrap();
        assert_eq!(copy_entry.bindings.len(), 1);
        assert_eq!(copy_entry.bindings[0].to_string(), "f5");

        let nav_entry = index
            .iter()
            .find(|e| e.id == CommandId::new("nav.parent").unwrap())
            .unwrap();
        assert!(
            nav_entry.bindings.is_empty(),
            "nav.parent has no binding in the sample keymap"
        );
    }

    #[test]
    fn search_finds_by_title_substring() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        let results = index.search("copy selection");
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.id, CommandId::new("ops.copy").unwrap());
    }

    #[test]
    fn search_finds_by_command_id() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        let results = index.search("nav.parent");
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.id, CommandId::new("nav.parent").unwrap());
    }

    #[test]
    fn empty_query_returns_everything() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        let results = index.search("");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn nonsense_query_returns_no_results() {
        let index = PaletteIndex::build(&sample_registry(), &sample_keymap());
        let results = index.search("zzzzzznotarealcommandxyz");
        assert!(results.is_empty());
    }
}
