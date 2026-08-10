# Acceptance-test sketches — P0 requirements

**Task:** T-1.3.1 (`documentation/task.md`). One or two sentences per P0 requirement: how would we know it works? These feed T-10.4.1's full acceptance pass — treat each as the seed of a real test case, not the test itself.

Coverage: 100% of P0-priority rows in `documentation/design.md` §6, including the NFR table's newly-added `Pri` column (see the T-1.2.1 review note at the top of §6). Requirements without a P0 marking are out of scope for this pass by design.

## FR-NAV (panels and navigation)

- **FR-NAV-01** (dual panels + splitter): open Duet, drag the splitter to 30/70, restart the app, confirm the ratio is still 30/70. Resize the window and confirm both panels stay proportional rather than one panel clamping to a fixed width.
- **FR-NAV-02** (active/target panel): with the left panel active, press Tab, and confirm the right panel's cursor row now has the active-panel visual treatment and the left panel's no longer does — verified against a single ground-truth "active panel id" in the model, not per-view guessing.
- **FR-NAV-03** (tabs, incl. locked): open 3 tabs in a panel, lock one, navigate into a subdirectory from the locked tab, and confirm a *new* tab opened instead of the locked tab's directory changing; restart the app and confirm all tabs and their directories restored.
- **FR-NAV-04** (Full/Tree view modes — the P0 half): switch a panel between Full and Tree, confirm each renders correctly and the choice is retained per-tab after switching away and back.
- **FR-NAV-06** (sorting): sort a directory containing `file2.txt`, `file10.txt`, `file1.txt` by name with natural-numeric mode on, confirm order is 1, 2, 10 (not lexicographic 1, 10, 2); toggle directories-first and confirm dirs move as a block without breaking the sort within each group.
- **FR-NAV-07 / FR-NAV-13** (quick search/filter): in a 1000-entry panel, type a 3-character fuzzy query that matches a file roughly in the middle of the list; confirm the cursor jumps directly there within one frame and an indicator shows the query and match ordinal; press Escape and confirm the indicator disappears and the cursor stays put (does not revert).
- **FR-NAV-08** (history + hotlist): navigate A → B → C, press Back twice, confirm cursor is at A; open the hotlist overlay (Ctrl+D), arrow to a bookmarked path, press Enter, confirm navigation occurs and the overlay closes.

## FR-SEL (selection)

- **FR-SEL-01** (cursor vs. selection): move the cursor with arrow keys over 5 files without pressing Insert/Space; confirm the selection set is still empty (cursor movement never implicitly selects).
- **FR-SEL-02** (Insert/Space semantics): press Insert on 3 consecutive files, confirm all 3 are selected and the cursor advanced 3 rows; press Space on a directory, confirm its recursive size populates in the size column without changing the selection set.
- **FR-SEL-03** (mask selection): select-by-mask `*.rs`, confirm exactly the `.rs` files are selected; invert selection, confirm exactly the previously-unselected files are now selected and vice versa.
- **FR-SEL-04** (selection survives sort/refresh): select 5 files, change sort column, confirm the same 5 files (by identity, not position) remain selected; touch an external file to trigger a watch-driven refresh, confirm selection is unaffected.
- **FR-SEL-05** (footer stats): select files totaling a known byte count, confirm the footer shows the correct "n of m selected, x of y bytes" and updates live as selection changes, with no perceptible lag.

## FR-OPS (file operations)

- **FR-OPS-01** (core ops): with panel A active and B as target, press F5 on a file, confirm it appears in B and still exists in A (copy, not move); press F6, confirm the reverse (moved, gone from A).
- **FR-OPS-02** (background queue): start a copy of a large directory, confirm the UI remains responsive (can navigate, open menus) while it runs; pause and resume the job, confirm it actually stops writing and correctly continues from where it left off.
- **FR-OPS-03** (progress/ETA): during a mixed-size-files copy, confirm the progress UI shows both a current-file and a total-job percentage, and that the refresh cadence is stable (not jumping wildly with each small file boundary).
- **FR-OPS-04** (conflict resolution): copy a file onto an existing same-named file, confirm a conflict prompt with side-by-side metadata appears; select "apply to all" with "overwrite-if-newer" on a multi-conflict batch, confirm every subsequent conflict resolves silently per that policy with no repeated prompts.
- **FR-OPS-05** (metadata preservation): copy a file with custom xattrs, an ACL entry, and a set mtime; confirm `getfattr`/`getfacl`/`stat` on source and destination match exactly (this is the same check as T-5.1.6's AC — this sketch is the acceptance-level framing of it).
- **FR-OPS-06** (copy strategy): on a btrfs volume, copy a large file and confirm via `filefrag` that a reflink was used (near-instant, shared extents) rather than a full data copy; on a filesystem without reflink support, confirm it falls back correctly and still completes.
- **FR-OPS-07** (crash safety): start a large copy, `SIGKILL` the process mid-transfer, restart Duet, confirm it offers to resume/discard/inspect and that the original source file is untouched regardless of which option is chosen (this sketch is the acceptance framing of the much more rigorous T-10.2.1 data-safety suite, which is the actual release gate).

## FR-VFS (virtual filesystems)

- **FR-VFS-01** (uniform VFS): open a zip archive as if it were a directory (no separate "extract first" step) and confirm the panel, viewer, and search all operate on its contents exactly as they would a real directory.
- **FR-VFS-06** (capability-driven strategy): connect to a backend lacking `RENAME` (e.g. certain archive or remote backends) and attempt an in-place rename; confirm the operation engine transparently falls back to a copy+delete strategy rather than surfacing a raw "unsupported" error to the user.

## FR-TOOL (viewing, search, tools)

- **FR-TOOL-01** (viewer): open a 5GB text file with F3, confirm it opens in well under a second and scrolling to the end is instant (no full-file load); switch to hex mode and confirm the same file opens without re-reading from scratch.
- **FR-TOOL-04** (search + feed to panel): search a directory tree for content matching a regex, confirm results start streaming in under a second on a multi-GB tree; use "feed to panel" and confirm the result set behaves as a normal, fully-operable panel listing (selectable, operable with F5/F6/etc).
- **FR-TOOL-06** (command line): with the active panel at `/home/user/project`, type `pwd` in the command line and press Enter; confirm the shell executes with that directory as cwd, and that Ctrl+Enter inserts the cursor file's name into the command line at the cursor position.
- **FR-TOOL-08** (associations): press Enter on a `.pdf` file and confirm the system default PDF viewer opens it; open "Open With" on the same file and confirm the list contains real installed applications from the desktop-entry database, not a hardcoded list.
- **FR-TOOL-11** (command palette): open the palette, type a fuzzy fragment of a command name, confirm the correct command surfaces near the top of results with its current keybinding shown, and confirm invoking it from the palette has the identical effect to using its keybinding directly.

## FR-CFG (configuration and integration)

- **FR-CFG-01** (config + hot reload): edit `settings.toml` in an external editor while Duet is running, save, confirm the change (e.g. a toggled `show_hidden`) takes effect in the running UI within 200ms with no restart.
- **FR-CFG-02** (keymap): rebind F5 to a different command in `keymap.toml`, restart, confirm F5 now invokes the new command and the palette reflects the new binding; introduce a duplicate binding and confirm a diagnostic with file/line is surfaced rather than one silently winning.
- **FR-CFG-05** (clipboard interop): copy a file in Duet, paste into Nautilus, confirm the file is copied there; cut a file in Nautilus, paste into Duet, confirm it's moved (not copied) and the source is gone from Nautilus's view.
- **FR-CFG-07** (trash): delete a file with the default policy (trash, not permanent), confirm it disappears from the panel and appears in the trash browser with its original path recorded; restore it and confirm it returns to the exact original location.

## NFR (non-functional, P0 only)

- **NFR-01** (cold start ≤150ms): `hyperfine --warmup 3 duet` on a warm page cache, confirm the reported mean is at or under 150ms to the "first frame with real listing" instrumented marker.
- **NFR-02** (input latency): instrument a keystroke-to-paint timer for cursor movement in a populated panel; confirm p50 ≤ 6ms and p99 ≤ 12ms over a few thousand samples of held-key scrolling.
- **NFR-03 / NFR-04** (listing scale): open a 100k-entry directory, confirm first paint ≤100ms and full sort ≤400ms; open a 1M-entry directory, confirm it's scrollable/sortable within 3s with no single UI stall over 16ms.
- **NFR-05** (scroll perf): scroll a 1M-row panel at speed with frame-time instrumentation active, confirm no frame exceeds 8.3ms on a genuine 120Hz display (per the G0 report, this specific check could not be completed in the Phase 0 environment and remains open pending real hardware).
- **NFR-06** (memory): with two panels each showing 100k entries and thumbnails off, sample `/proc/self/status` VmRSS, confirm ≤150MB (per the G0 report, S-1 measured gpui-component's own init cost alone at ~248MB in a single-window spike — this NFR needs re-validation against the real two-panel app, not just the spike, once T-4.2.1 exists).
- **NFR-07** (copy throughput): copy a 10GB file and a 100k-small-file tree on the same disk as a `cp`/`cp -a` baseline; confirm ≥95% and ≥80% of baseline throughput respectively.
- **NFR-08** (data integrity): this is the acceptance framing of T-10.2.1's data-safety suite directly — the sketch *is* "run the full SIGKILL/ENOSPC/EACCES injection matrix and confirm 100% pass, no exceptions."
- **NFR-10** (no GTK/Qt/KDE runtime dependency): run the built binary in a minimal container with no GTK/Qt/KDE packages installed, confirm it starts and renders a window (per the G0 report, S-8 could only produce `ldd`/`strings` proxy evidence for this — a real container run is still owed before this NFR can be marked verified).
- **NFR-11, keyboard-complete half**: for every dialog and view in the app, complete a full representative workflow (e.g. copy with a conflict, rename, create a connection profile) using only the keyboard, confirm no step requires a mouse click to proceed.
