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

## FR-CFG-04 / T-4.2.6: file icons are type icons only, from a theme read once at startup

**Symptom:** every entry gets the icon its MIME type (by name, via
shared-mime-info's `globs2`) or its kind (folder, symlink, special file)
maps to in the XDG icon theme. Nothing else is drawn: no emblems (a
symlink shows `inode-symlink`, never its target's icon; an unreadable
or read-only entry gets no overlay), no per-file sniffing (an
extension-less script is `text-x-generic`), and no `.xpm` icons (no
decoder; modern themes ship SVG/PNG). Changing the desktop's icon theme
or `appearance.icon_theme` takes effect on the next launch, not live.

**Root cause:** scope. T-4.2.6's ACs are type icons for common types,
NFR-05 scrolling, and a bounded atlas; emblems, magic-byte sniffing and
thumbnails are design.md §9.8's later `duet-meta` tasks (T-9.1.5 for
thumbnails), and live theme reload needs the settings watcher that
`appearance.*` keys don't have yet (they are all read once at startup).

**Why deferred:** the panel is already correct for the overwhelming
majority of entries; each of the gaps is its own bounded task with its
own AC and none blocks the view-mode work that follows.

**To close this out:** emblem compositing (a second `img` layered in the
16 px slot) once `duet-meta` reports link targets and permissions per
entry; hook `appearance.icon_theme` to the config watcher and rebuild
the `IconCache` (drop every image, clear the memo) on change.

## FR-NAV-04 / T-4.2.5: view-mode gaps -- Tree has no selection or quick-search, Thumbnails show type icons

**Symptom:** in Tree mode the selection keys (Insert, Space, `*`,
Ctrl+A, Shift+arrows) and typed quick-search do nothing -- the tree
navigates directories, it doesn't select entries; the listing they
would act on is the hidden one. Thumbnails mode draws each entry's
64 px *type* icon, not a rendering of the file. Switching the desktop
between light and dark keeps the current mode; there is no per-mode
column layout for Brief/Thumbnails (they have no columns).

**Root cause:** scope. T-4.2.5's brief says "Thumbnails (placeholder
icons for now)": real thumbnails (freedesktop cache, decoders,
sandboxed `.thumbnailer` subprocesses) are T-9.1.5. Tree is a
directory chooser in TC as well; selecting inside it is not a TC
behaviour this task set out to copy.

**Why deferred:** each list mode already carries the whole selection/
quick-search model; the tree deliberately doesn't, and a thumbnailer
pipeline is its own multi-day task with its own safety requirements
(design.md §11: untrusted decoders in subprocesses).

**To close this out:** T-9.1.5 swaps the icon slot in
`FileTableDelegate::list_cell` (`CellStyle::Thumbnail`) for a thumbnail
image once one is cached; if UAT wants it, `Space` in the tree could
mark a directory for the next operation the way TC's tree `Space`
computes its size.

## FR-TOOL-08 / T-5.3.4: associations are read once per session; "Open With" has no default key

**Symptom:** a `mimeapps.list` or `.desktop` change made while Duet
runs (installing an application, changing a default in GNOME Settings)
is not seen until the next launch. "Open With" is reachable from the
row context menu and the command palette (`file.open_with`) only. On a
selection of mixed types the chooser lists only applications that
handle every type; there is no "open each with its own default" entry
in the chooser itself (that is what "Open" does).
`DBusActivatable=true` entries are started through their `Exec` line
rather than D-Bus activation, and the desktop-specific
`<desktop>-mimeapps.list` variants are not consulted.

**Root cause:** scope. The association tables are a process-wide
`OnceLock` loaded on the first launch; there is no watcher on the XDG
directories yet. Total Commander has no default chord for "Open With",
so T-5.3.4 adopted none. D-Bus activation would add a bus dependency
for a result that is identical for a file manager (GLib itself falls
back to `Exec` when the bus name isn't owned).

**Why deferred:** all four of T-5.3.4's AC cases work with the tables
as loaded at startup; a reload is a convenience, not a correctness
gap, and the same watcher will serve the icon theme (T-4.2.6's note).

**To close this out:** watch the `applications/` directories and
`mimeapps.list` with the config watcher and drop the tables on change;
if UAT wants a chord, bind `file.open_with` (Shift+Enter is free and
TC uses it for "run with parameters", which is close in spirit).

## FR-CFG-05 / T-5.3.3: the file clipboard is Wayland-only; X11 sessions exchange text paths

**Symptom:** under X11 (or XWayland), Ctrl+C in a panel puts the paths
on the clipboard as plain text only; other file managers see no file
list, and Ctrl+V in Duet only pastes if the clipboard text is a list of
absolute paths or `file://` URIs. Under native Wayland (Duet's target
session) `text/uri-list` and the GNOME/KDE cut markers are offered and
read.

**Root cause:** gpui's X11 backend serves the selection through the
`x11-clipboard` crate, whose `store` owns a single target; offering
several MIME types needs a custom ICCCM selection owner (with `INCR`
for large lists) -- S-2 already priced that as unbudgeted X11 work.

**Why deferred:** every desktop in the platform matrix's primary row
runs Wayland; X11 interop is a real gap for the X11+i3 row only.

**To close this out:** replace the `x11-clipboard` `store` path in the
vendored gpui with a selection owner that answers `TARGETS` with every
custom type (and the text types), the same DUET PATCH shape as the
Wayland one.

## FR-TOOL-06 / T-5.3.5: the command line has no job control and captures output only

**Symptom:** a command started with Enter runs to completion; there is
no key to interrupt or kill it (Ctrl+C in the field is text copy, per
Appendix A.1.1), and a command that waits for input (`sudo`, `vim`,
`less` without a pipe) sits there with stdin closed. The output pane
shows the last 2000 lines of combined stdout/stderr as plain text:
no colours, no cursor control. Alt+F8 focuses the line and recalls the
newest entry and Ctrl+` toggles focus between panel and command line;
there is no scrollable history *dropdown* as in TC's command line.

**Root cause:** scope. The capture pane is a pipe reader, not a PTY;
the real terminal (T-9.1.11, `alacritty_terminal`) is where interactive
and screen-oriented commands belong, and Shift+Enter already hands such
a command to the user's terminal emulator. A kill needs process-group
signalling plumbed through the pane header, a small follow-up once the
embedded terminal's own job model exists.

**Why deferred:** the AC (`make` from the panel, Ctrl+Enter, persistent
history) is met; interactive commands have a working route (Shift+Enter).

**To close this out:** a "stop" control in the output pane header that
sends SIGTERM to the command's process group; the history dropdown as a
`List` overlay reusing the hotlist pattern when a TC user asks for it.

## FR-TOOL-01 / T-5.3.6: the viewer's gaps -- detection, wrapping, persistence, zoom

**Symptom:** (1) a legacy single-byte file that isn't valid UTF-8 opens
as windows-1252; Cyrillic or Hebrew text in another code page shows
mojibake until Ctrl+E (`view.set_encoding`) cycles to the right one
(UTF-8, windows-1252/1251/1255, KOI8-R, UTF-16 LE/BE). (2) Wrap mode
counts characters, not display cells, so a row of CJK or emoji wraps a
little late. (3) A UTF-16 surrogate pair that straddles a 4 KiB chop in a
long line shows as two replacement characters (UTF-8 seams are
repaired). (4) Wrap, hex width, offset base and the encoding override
reset when the viewer closes; nothing is saved to `settings.toml`. (5)
Find-next/previous live in the find bar (Enter / Shift+Enter) and the
palette (`view.search_next/prev`); there is no body key for them, since
Appendix A gives F3 to "close". (6) Images fit the window; there is no
zoom or pan. (7) The file is shown as it was when opened; a growing log
needs Ctrl+PgDn/PgUp (or reopen) to show new data.

**Root cause:** scope. `chardetng` is a new dependency and the manual
cycle covers the AC ("encoding override works"); cell-width wrapping
needs `unicode-width`; the rest are small features past the task's AC.

**Why deferred:** the AC is met and measured (see `task.md`); each gap
has a working route.

**To close this out:** `chardetng` behind the heuristic; `unicode-width`
in `text::wrap_pieces`; a `[viewer]` settings section; `n`/`Shift+N` or
a confirmed TC Lister key for find-next once Appendix A's F3 row is
verified hands-on; zoom/pan in image mode (T-5.3.8's quick view will
want the same); a reload on Ctrl+R / F2 as TC's Lister does.

## FR-TOOL-02 / T-5.3.8: quick view is not persisted and previews files only

**Symptom:** (1) Ctrl+Q's preview is gone after a restart; `session.json`
records the panels' tabs, not that one of them was a quick view. (2)
While the preview is up, the replaced panel's tab strip is hidden; its
tabs and directory still exist (F5 still targets its directory) but
cannot be switched until Ctrl+Q restores it. (3) A directory under the
cursor shows a label with its name, not a listing or an entry count.
(4) The preview inherits the viewer's own gaps (encoding fallback, wrap
by characters, no image zoom). (5) In the Tree view (Ctrl+F10) the
cursor lives in the tree widget, which the preview does not observe;
it follows the table's last cursor instead.

**Root cause:** scope. Persisting the mode needs a per-panel field in the
session model; a directory preview needs a listing off the UI thread,
which is T-5.3.7's search/listing plumbing or a `DirectoryModel` read.

**Why deferred:** the AC (cursor movement updates the preview without
stutter; large files preview lazily) is met: the update is debounced and
read on the worker thread, and the embedded viewer builds no index.

**To close this out:** a `quick_view: Option<side>` in `session.json`;
a lazy entry count or first-page listing for directories; keep the tab
strip visible above the preview if a TC user misses it.

## FR-TOOL-04 / T-5.3.7: search is local-only, and a fed listing is not a saved place

**Symptom:** (1) Alt+F7 does nothing on a stock GNOME session: Mutter
grabs `<Alt>F7` for "move window" (`org.gnome.desktop.wm.keybindings
begin-move`) before the application sees it. Ctrl+F in a panel opens
the same dialog (design.md Appendix A.4), as does the palette's
`tool.search` row; the palette lists only the TC chord. (2)
Search walks the local filesystem only -- not inside archives (that is
T-6.1.11) and not through other VFS backends. (3) A regex content search
is UTF-8 only (the dialog refuses a regex with another encoding chosen);
the UTF-16 and code-page encodings apply to literal text. Whole words
use ASCII word boundaries and apply to the UTF-8 form only. A match
cannot span a line break, and a line longer than 1 MiB is searched in
1 MiB windows overlapping by 4 KiB, so a match longer than 4 KiB inside
such a line can be missed. (4) With "binary files: skipped", a UTF-16
file without a byte-order mark counts as binary. (5) Attribute filters
are the kind (files/directories), hidden and symlink switches; there is
no permission, owner or "executable" filter. (6) A fed listing is not
persisted: after a restart the tab shows the search root's own listing.
It is not in the tab's history either -- Back after leaving it returns to
the root's listing, not to the results. A reread (Ctrl+R, or the refresh
after a job) drops entries that are gone but does not add new matches;
search again for those. That includes an entry renamed with Shift+F6
(which renames it in its own folder): it drops out of the listing. (7) Saved search definitions (FR-TOOL-05, P2)
and a "search in results" mode are not implemented. The result list
shows names, sizes and dates, not the matching line, and has no F3
view key: Enter goes to the file, Alt+L feeds the panel.

**Root cause:** scope. GNOME's grab is outside the application;
archive search is its own WBS row; VFS-backed traversal needs the async
`FileSystem` trait on the walker's hot path; translating an arbitrary
regex into UTF-16 or a code page is not something the regex engine
offers; the rest are features past the task's AC.

**Why deferred:** the AC is met and measured (`task.md`): content search
of a 5 GiB tree streams its first hit well inside 500 ms, and a fed
listing works with F5/F6/F8, Enter, F3, Ctrl+Q and the clipboard,
because its entries resolve through the same `current_dir.join(name)`
as a listed directory's.

**To close this out:** show Duet's own additions (Ctrl+F) in the
palette next to the TC chords; T-6.1.11 for archives; a `FileSystem`-based walker
for remote backends; per-encoding regex translation for the common
subset (literal runs plus classes); a `fed` record in `session.json`;
FR-TOOL-05's saved definitions on top of `SearchSpec`.
