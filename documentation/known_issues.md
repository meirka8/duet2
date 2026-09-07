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

## FR-NAV-09: RTL (Arabic/Hebrew) path segments render in logical order in the path bar

**Found during:** S-6 (text input spike), scoped into T-4.3.4 (path bar).

**Behavior:** `gpui-component` 0.5.1's `Input` performs no BiDi visual
reordering: an RTL run inside the path bar's editable field is drawn in
logical (typing) order, so a Hebrew or Arabic directory name reads
backwards on screen while being edited. The breadcrumb face (plain text
elements) has the same limitation for the same reason. Navigation is
unaffected: the bytes round-trip exactly (S-6 verified paste, cursor and
IME-range plumbing on 4000-character and RTL paths), the panel lists the
right directory, and completion matches the right names.

**Root cause:** the widget's text layout is logical-order-only
(`documentation/spikes/S-6.md`, confirmed via `bounds_for_range`). Not
fixable from `duet-ui`: R-G7 forbids reaching past the façade, and the
fix belongs in the vendored widget's shaping.

**Why deferred:** correctness holds; only rendering of a minority of
paths is affected, and the fix is upstream work with a full
gpui-component bump behind it (ADR-003).

**To close this out:** re-test the S-6 RTL cases at the next
`gpui-component` bump; if still wrong, file upstream or patch the
vendored widget the way ADR-007 does for gpui.

## FR-NAV-09: CJK IME composition in the path bar is unverified headlessly

**Found during:** S-6, carried into T-4.3.4.

**Behavior:** unknown, not known-bad. GPUI's Wayland `zwp_text_input_v3`
and X11 XIM plumbing is present and S-6 proved the marked-text API works;
whether a real ibus/fcitx composition sequence lands correctly in the path
bar can only be checked by a person typing one.

**Why deferred:** the headless test platform implements no text-input
protocol, so no automated test can claim it.

**To close this out:** manual UAT step -- with ibus engaged, type a
pinyin/romaji sequence into the path bar, pick a candidate, confirm the
committed text and that `Enter` navigates to the matching directory.

