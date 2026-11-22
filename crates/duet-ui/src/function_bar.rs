// SPDX-License-Identifier: MIT
//! Builds the 12-slot function-key bar's labels from real data: the
//! built-in command catalogue and the TC keymap survey (both in
//! `duet-commands`), per `design.md` §9.4/Appendix A -- never a hardcoded
//! per-key label table (T-4.1.4).
//!
//! No `gpui` types here on purpose: this is pure data derivation, testable
//! headlessly and reusable by a future settings/keymap-editor UI without
//! dragging GPUI along.

use duet_commands::keymap::{self, KeyChord, tc_csv};
use duet_commands::{CommandId, CommandRegistry, predicate, register_builtin_commands};

/// One function-key bar slot: the key's display label (`"F1"`..`"F12"`) and
/// the short command label currently bound to it (empty if nothing is
/// bound -- true of bare `F12`, which no source document assigns a default
/// to).
#[derive(Debug, Clone, PartialEq)]
pub struct FKeySlot {
    pub key: &'static str,
    pub label: String,
}

const KEY_DISPLAY: [&str; 12] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
];
const KEY_CHORD: [&str; 12] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
];

/// `design.md` Appendix A.4: Duet-only additions layered on top of the TC
/// base, so they are deliberately *not* in `docs/keymap-tc.csv` (that file
/// is scoped to TC's own survey). F11 occupies a chord TC never claimed;
/// F9 replaces TC's "activate the menu above the source panel" -- Duet has
/// no menu bar -- with Find Files, because GNOME grabs Alt+F7. Each wins
/// over the survey's row for its key, the way a later keymap layer would.
/// Named here as the only manual entries in an otherwise data-derived bar.
const DUET_ADDITIONS: [(&str, &str); 2] =
    [("f9", "tool.search"), ("f11", "window.toggle_fullscreen")];

/// Short labels for the handful of default-keymap commands that are *not*
/// in the registered catalogue (`docs/commands.md`) -- `help.show` and
/// `menu.activate` exist only in `docs/keymap-tc.csv`'s survey data (see
/// that file's own `description` column, quoted per entry below), and
/// `window.toggle_fullscreen` is an Appendix-A.4 addition in neither data
/// file. Every other slot's label is derived live from the registered
/// [`duet_commands::Command::title`] -- this table exists only because
/// those specific three ids were never given a catalogue entry to derive a
/// title from.
fn fallback_label(id: &str) -> &'static str {
    match id {
        // docs/keymap-tc.csv: "Open context-sensitive help"
        "help.show" => "Help",
        // docs/keymap-tc.csv: "Activate/focus the main menu bar ..."
        "menu.activate" => "Menu",
        // design.md Appendix A.4.
        "window.toggle_fullscreen" => "Fullscreen",
        // Any other unregistered id would be a genuine gap in this
        // module's coverage of the default keymap, not an expected case --
        // fall back to something visibly non-fabricated rather than a
        // fake-looking short word.
        _ => "?",
    }
}

/// The first whitespace-delimited token of a catalogue command's title
/// (e.g. `"Copy selection to the target panel"` -> `"Copy"`). A uniform,
/// deterministic derivation from real title text -- not a hand-picked
/// per-key string -- so two commands whose titles start the same way (e.g.
/// `view.open`/`edit.open`, both "Open ...") legitimately show the same
/// short label, same as real title text would suggest.
fn short_label(title: &str) -> String {
    title.split_whitespace().next().unwrap_or(title).to_string()
}

fn command_label(id: &CommandId, registry: &CommandRegistry) -> String {
    match registry.get(id) {
        Some(cmd) => short_label(&cmd.title),
        None => fallback_label(id.as_str()).to_string(),
    }
}

/// Builds all 12 function-key slots from `docs/commands.md`'s catalogue and
/// `docs/keymap-tc.csv`'s TC keymap survey, resolved exactly the way
/// `duet-commands`'s own keymap layer does (base-keymap layering +
/// conflict-aware `find`). Cheap enough (parses ~151 CSV rows + ~302
/// catalogue rows) to call once at startup; not cached, since a future
/// keymap-editor UI will want to rebuild this after a live rebind anyway.
pub fn build_function_bar() -> Vec<FKeySlot> {
    let mut registry = CommandRegistry::new();
    register_builtin_commands(&mut registry).expect(
        "docs/commands.md's catalogue is embedded at compile time and covered by \
         duet-commands' own parse tests -- registration failing here would mean the \
         checked-in document itself is malformed",
    );

    let loaded = tc_csv::load();
    let resolved = keymap::resolve_with_locations([loaded.layer]);

    let panel_ctx = predicate::parse("panel").expect("\"panel\" is a valid predicate literal");
    let app_ctx = predicate::parse("app").expect("\"app\" is a valid predicate literal");

    KEY_DISPLAY
        .iter()
        .zip(KEY_CHORD.iter())
        .map(|(&display, &chord_name)| {
            let chord = KeyChord::parse(chord_name).expect("bare Fn key names always parse");
            let command_id = DUET_ADDITIONS
                .iter()
                .find(|(addition, _)| *addition == chord_name)
                .map(|(_, id)| CommandId::new(id).expect("valid command id"))
                .or_else(|| {
                    resolved
                        .find(&panel_ctx, &chord)
                        .or_else(|| resolved.find(&app_ctx, &chord))
                        .map(|b| b.binding.command.clone())
                });

            let label = command_id
                .as_ref()
                .map(|id| command_label(id, &registry))
                .unwrap_or_default();

            FKeySlot {
                key: display,
                label,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_all_twelve_slots_in_order() {
        let slots = build_function_bar();
        assert_eq!(slots.len(), 12);
        assert_eq!(
            slots.iter().map(|s| s.key).collect::<Vec<_>>(),
            vec![
                "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"
            ]
        );
    }

    /// The known TC-faithful bar: F2 Refresh, F5 Copy, F6 Move, F7 Create
    /// (mkdir), F8 Delete -- design.md's explicit "F2 = Refresh, not
    /// Rename" / "F5 = Copy, not Refresh" deviations from generic
    /// file-manager convention, verified as real data flowed through, not
    /// asserted from memory.
    #[test]
    fn known_slots_carry_real_catalogue_derived_labels() {
        let slots = build_function_bar();
        let by_key = |k: &str| slots.iter().find(|s| s.key == k).unwrap().label.clone();

        assert_eq!(by_key("F2"), "Refresh/reread");
        assert_eq!(by_key("F3"), "Open");
        assert_eq!(by_key("F4"), "Open");
        assert_eq!(by_key("F5"), "Copy");
        assert_eq!(by_key("F6"), "Move");
        assert_eq!(by_key("F8"), "Delete");
    }

    /// F1/F10 are real default TC bindings whose commands are not in the
    /// registered catalogue -- exercised via the documented fallback table,
    /// not silently blank.
    #[test]
    fn unregistered_default_bindings_get_documented_fallback_labels() {
        let slots = build_function_bar();
        let by_key = |k: &str| slots.iter().find(|s| s.key == k).unwrap().label.clone();

        assert_eq!(by_key("F1"), "Help");
        assert_eq!(by_key("F10"), "Menu");
    }

    /// F9 and F11 are the design.md Appendix A.4 additions (in neither data
    /// file this loads; F9's wins over the TC survey's own F9 row); F12 has
    /// no documented default anywhere and must stay honestly blank rather
    /// than fabricate a label.
    #[test]
    fn additions_and_the_f12_gap_are_handled_honestly() {
        let slots = build_function_bar();
        let by_key = |k: &str| slots.iter().find(|s| s.key == k).unwrap().label.clone();

        assert_eq!(by_key("F9"), "Search");
        assert_eq!(by_key("F11"), "Fullscreen");
        assert_eq!(by_key("F12"), "");
    }
}
