# Duet — Known Issues

Tracked, deliberately-deferred gaps: real behavior that's confirmed
missing or wrong, judged not worth blocking the task that found it.
Each entry names the task it surfaced during, the actual behavior, why
it's deferred rather than fixed immediately, and what fixing it would
take. Not a place for design questions or TODOs with no confirmed impact
— see `task.md` for the WBS instead.

## FR-NAV-13: quick-search does not exit when the panel loses window focus

**Found during:** T-4.3.3 (quick search / quick filter), UAT.

**Behavior:** FR-NAV-13 lists "the panel losing focus" as one of quick
-search's exit conditions (alongside Escape, idle timeout, and non
-search cursor movement — all of which do work). Today, switching focus
away from the panel entirely (e.g. Alt+Tab to another application, or a
future feature that puts real keyboard focus somewhere outside the
window) leaves an active quick-search/quick-filter session running
instead of clearing it.

**Root cause:** implemented via `FileTable::new`'s `window.on_focus_out`
subscription (`crates/duet-ui/src/file_table.rs`) — the documented,
correct GPUI API for exactly this. It's wired up and should work in the
real app. It could not be verified in this session because GPUI's real
focus-change events only report a meaningful `previous_focus_path`
while the OS window itself is "active" (foregrounded); the only test
hook for simulating that (`TestWindow::simulate_active_status_change`)
is `pub(crate)` *inside the `gpui` crate itself*, unreachable from
`duet-ui`'s own tests. Every other quick-search exit condition has a
passing regression test in `crates/duet-ui/src/panel.rs`; this one does
not.

**Why deferred:** low real-world impact — switching apps away from Duet
while mid-search is a narrow window, and the session self-clears on the
very next keystroke, click, or idle timeout regardless. Not worth
blocking on a headless-test-only gap when the implementation already
follows the documented API contract.

**To close this out:** confirm via live UAT that Alt+Tabbing away from
the Duet window (or otherwise moving OS-level focus elsewhere) clears
an active quick-search session the next time the window regains focus.
If it doesn't, the bug is in the `on_focus_out` wiring itself, not the
test gap.

## FR-NAV-05: the column configuration menu is mouse-only

**Symptom:** T-4.2.4's add/remove-column menu opens on a right-click
on any table header cell (and columns are reordered by dragging a
header, resized by dragging the handle between two headers). There is
no keyboard route to any of the three: no default binding opens the
menu, and no `columns.*` commands exist for the command palette.

**Root cause:** Total Commander itself has no default accelerator for
column configuration (`docs/keymap-tc.csv` lists none -- it lives under
Configuration > Options > Custom columns), so T-4.2.4 had nothing to
adopt, and the `gpui-component` popup menu is anchored to a mouse
position. The layout *model* (`crates/duet-ui/src/columns.rs`) is
keyboard-agnostic already; only the entry point is missing.

**Why deferred:** the WBS row's ACs (survives restart, smooth
drag-resize, correct sort indicator) are all met by the mouse path, and
FR-NAV-05's "named layouts switchable by keyboard" is T-4.2.5's Ctrl+4..9
custom-view slots, which need view modes first. Adding palette commands
(`columns.toggle_ext`, `columns.reset`, ...) is a small follow-up once
the command palette's command registry is the natural home for them.

**To close this out:** register `columns.toggle_<key>` / `columns.reset`
commands in the palette (and, if TC users ask, a default chord), plus a
keyboard-driven reorder (Ctrl+Shift+Left/Right on the sorted column, say).
