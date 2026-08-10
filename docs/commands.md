# Duet — Command Catalogue

| Field | Value |
|---|---|
| Document ID | DUET-CMD-001 |
| Status | Draft, produced by T-1.5.1 |
| Traces | G-4 ("everything is a command"), FR-TOOL-11, `design.md` §9.4 |
| Companion | `documentation/design.md`, `documentation/task.md` |

## Purpose

Per `design.md` §9.4, a command is `{ id, title, category, args_schema, precondition, handler }`, and "everything the app can do is registered here, including plugin-provided commands." This document enumerates every command Duet 1.0 (plus the reduced-scope backlog items already implied by frozen requirements) will register with `duet-commands`. It is the source of truth that:

- The command palette (FR-TOOL-11) indexes directly.
- The default keymap (`design.md` Appendix A / §9.4) binds against — every id referenced in the §9.4 keymap extract table resolves to an entry below.
- Plugins extend, per FR-PLUG-02 — see the **Plugins** section for the placeholder classes plugin-registered commands fall into.

`args_schema` and `handler` are implementation details decided in T-2.4.1 (command registry design) and are out of scope for this catalogue; the columns tracked here are the ones a requirements-phase enumeration can responsibly fix: **id**, **title**, **category**, **precondition**, **notes**.

## Precondition grammar

Preconditions are boolean predicates over UI-state context terms, evaluated by the context predicate evaluator (§9.4, designed in T-2.4.1). Base contexts used below: `app` (always true — global scope, no panel/dialog focus required), `panel` (a file panel has focus), `dialog` (a modal dialog is open, optionally namespaced e.g. `dialog.conflict`), `viewer`, `editor`, `cmdline`, `palette`. State terms compose with `&&` / `||` / `!`, e.g. `panel && selection.nonempty`, `panel && cursor.is_dir`, `dialog.conflict`. Namespaced pseudo-contexts (`ops_manager`, `trash_browser`, `hotlist_overlay`, `connection_manager`, `plugin_manager`, `keymap_editor`, `settings_ui`, `drive_bar`, `buttonbar_config`) name the overlay/manager surface a command applies to, following the same substrate.

## Summary

Total entries: **307**, across 23 categories. Every id named in `design.md` §9.4's default-keymap extract table (`focus.other_panel`, `panel.reread`, `view.open`, `edit.open`, `ops.copy`, `ops.move_or_rename`, `ops.mkdir`, `ops.delete`, `sel.toggle_and_advance`, `sel.toggle_and_size`, `sel.by_mask`, `unsel.by_mask`, `sel.invert`, `panel.swap`, `panel.push_to_other`, `nav.parent`, `nav.root`, `hotlist.open`, `tab.new`, `tab.close`, `panel.branch_view`, `tool.multi_rename`, `panel.quick_view`, `tool.search`, `archive.pack`, `archive.unpack`, `drive.change_left`, `drive.change_right`, `file.properties`, `cmdline.insert_name`, `cmdline.insert_path`) is present below, as is `ops.rename_in_place` from the §9.4 keymap.toml worked example.

---

## Navigation

| id | title | category | precondition | notes |
|---|---|---|---|---|
| nav.enter_dir | Enter directory / open entry | Navigation | `panel && cursor.is_dir` | Descends into a directory or a VFS mount point (archive, remote); FR-NAV-01 |
| nav.parent | Go to parent directory | Navigation | `panel` | Ctrl+PgUp |
| nav.root | Go to filesystem root | Navigation | `panel` | Ctrl+\ |
| nav.home | Go to home directory | Navigation | `panel` | |
| nav.back | Navigate back in tab history | Navigation | `panel && history.has_back` | FR-NAV-08 |
| nav.forward | Navigate forward in tab history | Navigation | `panel && history.has_forward` | FR-NAV-08 |
| nav.history_open | Open the per-tab history overlay | Navigation | `panel` | |
| nav.goto_path | Focus the breadcrumb/path bar for manual entry | Navigation | `panel` | FR-NAV-09 |
| nav.path_complete | Autocomplete the path segment under the cursor | Navigation | `cmdline || dialog` | Shared completion engine with FR-NAV-09 |
| nav.quick_search_start | Start quick search (type-to-jump) | Navigation | `panel` | FR-NAV-07 |
| nav.quick_search_next | Jump to next quick-search match | Navigation | `panel && quicksearch.active` | |
| nav.quick_search_prev | Jump to previous quick-search match | Navigation | `panel && quicksearch.active` | |
| nav.quick_search_cancel | Cancel quick search | Navigation | `panel && quicksearch.active` | |
| nav.quick_filter_start | Start quick filter (type-to-filter) | Navigation | `panel` | Modifier-prefixed mode, FR-NAV-07 |
| nav.quick_filter_clear | Clear the active quick filter | Navigation | `panel && quickfilter.active` | |
| nav.cursor_up | Move cursor up one row | Navigation | `panel` | |
| nav.cursor_down | Move cursor down one row | Navigation | `panel` | |
| nav.cursor_left | Move cursor left (multi-column view) | Navigation | `panel && view.mode != 'full'` | Brief/thumbnail modes |
| nav.cursor_right | Move cursor right (multi-column view) | Navigation | `panel && view.mode != 'full'` | |
| nav.page_up | Move cursor up one page | Navigation | `panel` | |
| nav.page_down | Move cursor down one page | Navigation | `panel` | |
| nav.first_entry | Move cursor to first entry | Navigation | `panel` | Home |
| nav.last_entry | Move cursor to last entry | Navigation | `panel` | End |
| nav.scroll_to_cursor | Scroll the cursor row into view | Navigation | `panel` | |
| nav.follow_symlink | Enter a symlink's target directly | Navigation | `panel && cursor.is_symlink` | |
| nav.open_parent_and_select | Go up and land the cursor on the directory just left | Navigation | `panel` | TC-faithful behaviour, called out in `design.md` T-4.3.1 AC |

## Selection

| id | title | category | precondition | notes |
|---|---|---|---|---|
| sel.toggle_and_advance | Toggle selection of cursor entry, advance | Selection | `panel` | Ins; FR-SEL-02 |
| sel.toggle_and_size | Toggle selection, compute directory size | Selection | `panel` | Space; FR-SEL-02 |
| sel.by_mask | Select by wildcard mask | Selection | `panel` | Numpad +; FR-SEL-03 |
| unsel.by_mask | Unselect by wildcard mask | Selection | `panel` | Numpad −; FR-SEL-03 |
| sel.invert | Invert selection | Selection | `panel` | Numpad *; FR-SEL-03 |
| sel.all | Select all entries | Selection | `panel` | FR-SEL-03 |
| unsel.all | Unselect all entries | Selection | `panel` | FR-SEL-03 |
| sel.same_extension | Select all entries with cursor's extension | Selection | `panel && entry.has_extension` | FR-SEL-03 |
| sel.same_name | Select all entries sharing cursor's base name | Selection | `panel` | FR-SEL-03 ("select all with same name") |
| sel.extend_up | Extend selection range upward | Selection | `panel` | Shift+Up |
| sel.extend_down | Extend selection range downward | Selection | `panel` | Shift+Down |
| sel.extend_to_top | Extend selection to first entry | Selection | `panel` | Shift+Home |
| sel.extend_to_bottom | Extend selection to last entry | Selection | `panel` | Shift+End |
| sel.extend_page_up | Extend selection one page up | Selection | `panel` | Shift+PgUp |
| sel.extend_page_down | Extend selection one page down | Selection | `panel` | Shift+PgDn |
| sel.toggle_single | Toggle selection of a single entry without moving cursor | Selection | `panel` | Ctrl+click |
| sel.range_to_cursor | Select a contiguous range from anchor to cursor | Selection | `panel` | Shift+click |
| sel.mask_history_show | Show select/unselect mask history | Selection | `dialog` | FR-SEL-03 ("mask history") |

## Tabs

| id | title | category | precondition | notes |
|---|---|---|---|---|
| tab.new | Open a new tab | Tabs | `panel` | Ctrl+T |
| tab.close | Close the current tab | Tabs | `panel && tab.count > 1` | Ctrl+W |
| tab.next | Switch to the next tab | Tabs | `panel && tab.count > 1` | |
| tab.prev | Switch to the previous tab | Tabs | `panel && tab.count > 1` | |
| tab.duplicate | Duplicate the current tab | Tabs | `panel` | |
| tab.lock | Toggle tab lock (navigation opens a new tab instead) | Tabs | `panel` | FR-NAV-03 |
| tab.lock_dir_change | Toggle lock-with-directory-change-allowed | Tabs | `panel` | FR-NAV-03, TC semantics |
| tab.close_others | Close all tabs but the current one | Tabs | `panel && tab.count > 1` | |
| tab.reopen_closed | Reopen the most recently closed tab | Tabs | `panel` | |
| tab.rename | Rename the current tab's label | Tabs | `panel` | |
| tab.move_left | Move the current tab one position left | Tabs | `panel && tab.count > 1` | |
| tab.move_right | Move the current tab one position right | Tabs | `panel && tab.count > 1` | |
| tab.goto_index | Jump directly to tab N | Tabs | `panel` | `args_schema: { index: u32 }` |
| tab.restore_session | Restore all tabs from the last saved session | Tabs | `app` | FR-CFG-01, T-4.3.7 |

## Panel & view

| id | title | category | precondition | notes |
|---|---|---|---|---|
| focus.other_panel | Move keyboard focus to the other panel | Panel & view | `app` | Tab |
| panel.reread | Refresh/reread the panel listing | Panel & view | `panel` | F2 |
| panel.swap | Swap left and right panel contents | Panel & view | `app` | Ctrl+U |
| panel.push_to_other | Copy the active panel's path to the other panel | Panel & view | `panel` | Ctrl+←/→ |
| panel.branch_view | Toggle branch view (flat recursive listing) | Panel & view | `panel` | Ctrl+B; FR-NAV-10 |
| panel.quick_view | Toggle quick-view preview in the inactive panel | Panel & view | `panel` | Ctrl+Q; FR-TOOL-02 |
| panel.sync_to_other | Change the other panel's directory to match active | Panel & view | `panel` | |
| panel.change_drive | Open the drive/mount picker for the active panel | Panel & view | `panel` | |
| drive.change_left | Change the left panel's drive/mount | Panel & view | `panel` | Alt+F1 |
| drive.change_right | Change the right panel's drive/mount | Panel & view | `panel` | Alt+F2 |
| panel.view_full | Switch to Full view mode | Panel & view | `panel` | FR-NAV-04 |
| panel.view_brief | Switch to Brief view mode | Panel & view | `panel` | FR-NAV-04 |
| panel.view_thumbnails | Switch to Thumbnails view mode | Panel & view | `panel` | FR-NAV-04 |
| panel.view_tree | Switch to Tree view mode | Panel & view | `panel` | FR-NAV-04 |
| panel.toggle_hidden | Toggle display of hidden files | Panel & view | `panel` | |
| panel.toggle_horizontal_split | Toggle side-by-side vs. stacked panel layout | Panel & view | `app` | FR-NAV-01 |
| panel.resize_splitter_left | Move the splitter left (keyboard resize) | Panel & view | `app` | FR-NAV-01 |
| panel.resize_splitter_right | Move the splitter right (keyboard resize) | Panel & view | `app` | FR-NAV-01 |
| panel.toggle_tree_sidebar | Toggle the synchronised directory-tree sidebar | Panel & view | `panel` | FR-NAV-12 |
| panel.calculate_all_sizes | Calculate directory sizes for all entries | Panel & view | `panel` | FR-SEL-02 |
| panel.cancel_size_calc | Cancel an in-progress directory-size calculation | Panel & view | `panel && sizejob.running` | |
| column.configure | Open the column configuration dialog | Panel & view | `panel` | FR-NAV-05 |
| column.add | Add a column to the current view | Panel & view | `dialog` | FR-NAV-05 |
| column.remove | Remove a column from the current view | Panel & view | `dialog` | FR-NAV-05 |
| column.reorder | Reorder a column | Panel & view | `dialog` | FR-NAV-05 |
| column.resize | Resize a column | Panel & view | `panel` | FR-NAV-05 |
| column.save_layout | Save the current column set as a named layout | Panel & view | `panel` | FR-NAV-05 |
| column.load_layout | Switch to a named column layout | Panel & view | `panel` | FR-NAV-05 |
| sort.by_name | Sort the panel by name | Panel & view | `panel` | FR-NAV-06 |
| sort.by_extension | Sort the panel by extension | Panel & view | `panel` | FR-NAV-06 |
| sort.by_size | Sort the panel by size | Panel & view | `panel` | FR-NAV-06 |
| sort.by_date | Sort the panel by modification date | Panel & view | `panel` | FR-NAV-06 |
| sort.by_column | Sort by the clicked column header | Panel & view | `panel` | FR-NAV-06 |
| sort.toggle_direction | Toggle ascending/descending sort | Panel & view | `panel` | FR-NAV-06 |
| sort.toggle_dirs_first | Toggle directories-first policy | Panel & view | `panel` | FR-NAV-06 |
| sort.toggle_natural | Toggle natural (version) numeric sort | Panel & view | `panel` | FR-NAV-06 |

## File operations

| id | title | category | precondition | notes |
|---|---|---|---|---|
| ops.copy | Copy selection to the target panel | File operations | `panel && selection.nonempty` | F5; FR-OPS-01 |
| ops.move_or_rename | Move selection to the target panel | File operations | `panel && selection.nonempty` | F6; FR-OPS-01 |
| ops.rename_in_place | Rename the cursor entry in place | File operations | `panel && selection.nonempty` | Shift+F6, §9.4 worked example; selects the stem, not the extension |
| ops.mkdir | Create a new directory | File operations | `panel` | F7; FR-OPS-01 |
| ops.mkdir_nested | Create a nested directory path in one step | File operations | `dialog` | TC-style F7 behaviour |
| ops.delete | Delete selection per the configured default | File operations | `panel && selection.nonempty` | F8 / Del; FR-OPS-01 |
| ops.delete_permanent | Delete selection permanently, bypassing trash | File operations | `panel && selection.nonempty` | Shift+Del |
| ops.delete_to_trash | Move selection to trash | File operations | `panel && selection.nonempty` | FR-CFG-07 |
| ops.create_symlink | Create a symbolic link to the selection | File operations | `panel && selection.nonempty` | FR-OPS-01 |
| ops.create_hardlink | Create a hard link to the selection | File operations | `panel && selection.nonempty` | FR-OPS-01 |
| ops.copy_prompt | Open the copy-destination dialog | File operations | `panel && selection.nonempty` | Precedes `ops.copy` execution |
| ops.move_prompt | Open the move-destination dialog | File operations | `panel && selection.nonempty` | Precedes `ops.move_or_rename` execution |
| file.properties | Open the properties dialog for the selection | File operations | `panel && selection.nonempty` | Alt+Enter; FR-TOOL-10 |
| file.attributes | Open the attributes/permissions dialog | File operations | `panel && selection.nonempty` | FR-OPS-12 |
| file.chmod_recursive | Apply a permission change recursively | File operations | `dialog` | FR-OPS-12, runs through the operation queue |
| file.set_timestamps | Edit file timestamps | File operations | `panel && selection.nonempty` | FR-OPS-12 |
| ops.elevate_retry | Retry a failed operation via polkit elevation | File operations | `job.failed_permission` | FR-OPS-13 |
| ops.verify_after_copy_toggle | Toggle post-copy checksum verification for this job | File operations | `dialog` | FR-OPS-08 |
| ops.conflict_skip | Resolve conflict: skip | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_overwrite | Resolve conflict: overwrite | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_overwrite_if_older | Resolve conflict: overwrite if source is newer | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_overwrite_if_size_differs | Resolve conflict: overwrite if size differs | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_rename | Resolve conflict: rename target | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_auto_rename | Resolve conflict: auto-rename | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_apply_to_all | Apply the chosen resolution to all remaining conflicts | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.conflict_abort | Abort the operation from the conflict dialog | File operations | `dialog.conflict` | FR-OPS-04 |
| ops.undo | Undo the last reversible operation | File operations | `undo.available` | FR-OPS-14 |
| ops.undo_rename_batch | Undo the last multi-rename batch | File operations | `undo.rename_available` | FR-OPS-09 |

## Operations queue

| id | title | category | precondition | notes |
|---|---|---|---|---|
| ops.queue.open_manager | Open the operation manager | Operations queue | `app` | FR-OPS-02 |
| ops.queue.pause_job | Pause a running job | Operations queue | `ops_manager && job.running` | FR-OPS-02 |
| ops.queue.resume_job | Resume a paused job | Operations queue | `ops_manager && job.paused` | FR-OPS-02 |
| ops.queue.cancel_job | Cancel a job | Operations queue | `ops_manager && job.active` | FR-OPS-02 |
| ops.queue.pause_all | Pause all queued/running jobs | Operations queue | `ops_manager` | FR-OPS-10, ENOSPC handling |
| ops.queue.resume_all | Resume all paused jobs | Operations queue | `ops_manager` | |
| ops.queue.reorder_up | Move a queued job up in priority | Operations queue | `ops_manager && job.queued` | FR-OPS-02 |
| ops.queue.reorder_down | Move a queued job down in priority | Operations queue | `ops_manager && job.queued` | FR-OPS-02 |
| ops.queue.retry_failed | Re-run the failed/skipped items of a job | Operations queue | `ops_manager && job.has_errors` | |
| ops.queue.remove_completed | Clear completed jobs from the manager | Operations queue | `ops_manager` | |
| ops.queue.show_errors | Show the error/skip report for a job | Operations queue | `ops_manager && job.has_errors` | |
| ops.queue.toggle_tray | Toggle the status-bar operation tray | Operations queue | `app` | FR-OPS-02 |
| ops.recovery_resume | Resume an interrupted operation found at startup | Operations queue | `startup.has_interrupted_jobs` | FR-OPS-07 |
| ops.recovery_discard | Discard an interrupted operation's partial files | Operations queue | `startup.has_interrupted_jobs` | FR-OPS-07 |
| ops.recovery_inspect | Inspect an interrupted operation's journal | Operations queue | `startup.has_interrupted_jobs` | FR-OPS-07 |

## Archives

| id | title | category | precondition | notes |
|---|---|---|---|---|
| archive.pack | Pack selection into a new archive | Archives | `panel && selection.nonempty` | Alt+F5; FR-VFS-03 |
| archive.unpack | Unpack archive contents | Archives | `panel && cursor.is_archive` | Alt+F9; FR-VFS-03 |
| archive.unpack_here | Unpack archive into the current directory | Archives | `panel && cursor.is_archive` | |
| archive.unpack_to_new_folder | Unpack archive into a new same-named folder | Archives | `panel && cursor.is_archive` | |
| archive.enter | Enter an archive as a directory | Archives | `panel && cursor.is_archive` | FR-VFS-02 |
| archive.add_to_existing | Add selection to an existing open archive | Archives | `panel && selection.nonempty` | FR-VFS-03 |
| archive.move_to_archive | Pack then delete originals ("move to archive") | Archives | `panel && selection.nonempty` | TC feature |
| archive.set_password | Set/enter password for an encrypted archive | Archives | `dialog && archive.encrypted` | |
| archive.test_integrity | Test archive integrity without extracting | Archives | `panel && cursor.is_archive` | |
| archive.choose_format | Choose the target archive format in the pack dialog | Archives | `dialog` | FR-VFS-03 |

## VFS & drives

| id | title | category | precondition | notes |
|---|---|---|---|---|
| drive.open_bar | Open the drive/mount bar | VFS & drives | `panel` | FR-NAV-11 |
| drive.mount | Mount the selected block device | VFS & drives | `drive_bar && device.unmounted` | FR-NAV-11 |
| drive.unmount | Unmount the selected device | VFS & drives | `drive_bar && device.mounted` | FR-NAV-11 |
| drive.eject | Eject removable media | VFS & drives | `drive_bar && device.removable` | FR-NAV-11 |
| vfs.connect_remote | Open a new remote connection | VFS & drives | `panel` | FR-VFS-04 |
| vfs.connection_manager_open | Open the connection manager | VFS & drives | `app` | FR-VFS-04 |
| vfs.connection_new | Create a new saved connection profile | VFS & drives | `connection_manager` | FR-VFS-04 |
| vfs.connection_edit | Edit a saved connection profile | VFS & drives | `connection_manager && profile.selected` | |
| vfs.connection_delete | Delete a saved connection profile | VFS & drives | `connection_manager && profile.selected` | |
| vfs.connection_test | Test a connection profile | VFS & drives | `connection_manager && profile.selected` | |
| vfs.connection_import_ssh_config | Import profiles from `~/.ssh/config` | VFS & drives | `connection_manager` | |
| vfs.disconnect | Disconnect an active remote session | VFS & drives | `panel && vfs.scheme != 'file'` | |

## Viewer

| id | title | category | precondition | notes |
|---|---|---|---|---|
| view.open | Open the internal viewer on the cursor entry | Viewer | `panel && cursor.is_file` | F3; FR-TOOL-01 |
| view.close | Close the viewer | Viewer | `viewer` | |
| view.mode_text | Switch viewer to text mode | Viewer | `viewer` | FR-TOOL-01 |
| view.mode_hex | Switch viewer to hex mode | Viewer | `viewer` | FR-TOOL-01 |
| view.mode_image | Switch viewer to image mode | Viewer | `viewer` | FR-TOOL-01 |
| view.mode_auto | Switch viewer to auto-detect mode | Viewer | `viewer` | FR-TOOL-01 |
| view.set_encoding | Override the detected text encoding manually | Viewer | `viewer` | FR-TOOL-01 |
| view.search | Search within the viewer | Viewer | `viewer` | §9.6 |
| view.search_next | Find the next match in the viewer | Viewer | `viewer && viewer.search_active` | |
| view.search_prev | Find the previous match in the viewer | Viewer | `viewer && viewer.search_active` | |
| view.hex_set_width | Set hex-mode byte width | Viewer | `viewer && viewer.mode == 'hex'` | |
| view.hex_toggle_offset_base | Toggle hex offset base (decimal/hex) | Viewer | `viewer && viewer.mode == 'hex'` | |
| view.next_file | View the next file in the panel without closing the viewer | Viewer | `viewer` | |
| view.prev_file | View the previous file in the panel without closing the viewer | Viewer | `viewer` | |
| view.wrap_toggle | Toggle line wrapping | Viewer | `viewer && viewer.mode == 'text'` | |

## Editor

| id | title | category | precondition | notes |
|---|---|---|---|---|
| edit.open | Open the internal editor on the cursor entry | Editor | `panel && cursor.is_file` | F4; FR-TOOL-03 |
| edit.save | Save the current file | Editor | `editor && editor.dirty` | FR-TOOL-03 |
| edit.save_as | Save the current file to a new path | Editor | `editor` | |
| edit.close | Close the editor, prompting if unsaved | Editor | `editor` | |
| edit.set_encoding | Set the file encoding for save | Editor | `editor` | FR-TOOL-03 |
| edit.set_line_ending | Set the line-ending convention (LF/CRLF) | Editor | `editor` | FR-TOOL-03 |
| edit.open_external | Delegate to the configured external editor | Editor | `panel && cursor.is_file` | FR-TOOL-03 |
| edit.find | Find within the editor buffer | Editor | `editor` | |
| edit.find_replace | Find and replace within the editor buffer | Editor | `editor` | |
| edit.undo | Undo the last edit | Editor | `editor && editor.can_undo` | |
| edit.redo | Redo the last undone edit | Editor | `editor && editor.can_redo` | |

## Search

| id | title | category | precondition | notes |
|---|---|---|---|---|
| tool.search | Open the search dialog | Search | `panel` | Alt+F7; FR-TOOL-04 |
| search.start | Execute the configured search | Search | `dialog` | |
| search.cancel | Cancel a running search | Search | `search.running` | |
| search.by_name_mask | Add a name-mask filter | Search | `dialog` | FR-TOOL-04 |
| search.by_regex | Use regex for name matching | Search | `dialog` | FR-TOOL-04 |
| search.by_content | Enable content search (literal/regex) | Search | `dialog` | FR-TOOL-04 |
| search.by_size | Add a size-range filter | Search | `dialog` | FR-TOOL-04 |
| search.by_date | Add a date-range filter | Search | `dialog` | FR-TOOL-04 |
| search.by_attributes | Add an attribute filter | Search | `dialog` | FR-TOOL-04 |
| search.in_archives | Include archive contents in the search | Search | `dialog` | FR-TOOL-04, T-6.1.11 |
| search.feed_to_panel | Feed search results into a panel as a synthetic listing | Search | `search.has_results` | FR-TOOL-04 |
| search.save_definition | Save the current search as a reusable definition | Search | `dialog` | FR-TOOL-05 |
| search.load_definition | Load a saved search definition | Search | `dialog` | FR-TOOL-05 |
| search.open_result | Open the location of a selected result | Search | `search.has_results` | |

## Multi-rename

| id | title | category | precondition | notes |
|---|---|---|---|---|
| tool.multi_rename | Open the multi-rename tool | Multi-rename | `panel && selection.nonempty` | Ctrl+M; FR-OPS-09 |
| rename.add_counter | Insert a numeric counter placeholder | Multi-rename | `dialog` | FR-OPS-09 |
| rename.add_metadata_field | Insert a metadata placeholder (date, size, EXIF, etc.) | Multi-rename | `dialog` | FR-OPS-09 |
| rename.regex_search_replace | Apply regex search/replace to names | Multi-rename | `dialog` | FR-OPS-09 |
| rename.case_upper | Convert the matched name segment to uppercase | Multi-rename | `dialog` | FR-OPS-09 |
| rename.case_lower | Convert the matched name segment to lowercase | Multi-rename | `dialog` | FR-OPS-09 |
| rename.case_title | Convert the matched name segment to title case | Multi-rename | `dialog` | FR-OPS-09 |
| rename.preview_refresh | Refresh the live rename preview | Multi-rename | `dialog` | FR-OPS-09 |
| rename.execute | Execute the rename batch | Multi-rename | `dialog && rename.no_conflicts` | Runs through the operation queue |
| rename.load_mask | Load a saved rename mask/pattern | Multi-rename | `dialog` | |
| rename.save_mask | Save the current rename mask/pattern | Multi-rename | `dialog` | |

## Compare & sync

| id | title | category | precondition | notes |
|---|---|---|---|---|
| tool.compare_dirs | Compare the two panel directories | Compare & sync | `panel` | FR-OPS-10 |
| compare.by_name | Compare by name only | Compare & sync | `dialog` | FR-OPS-10 |
| compare.by_size | Compare by size | Compare & sync | `dialog` | FR-OPS-10 |
| compare.by_date | Compare by modification date | Compare & sync | `dialog` | FR-OPS-10 |
| compare.by_content | Compare by content (checksum) | Compare & sync | `dialog` | FR-OPS-10 |
| compare.select_diffs | Select all differing entries in both panels | Compare & sync | `compare.has_results` | FR-OPS-10 |
| tool.sync_dirs | Open the synchronise-directories tool | Compare & sync | `panel` | FR-OPS-10 |
| sync.set_direction | Set sync direction (left→right, right→left, both) | Compare & sync | `dialog` | FR-OPS-10 |
| sync.add_filter | Add an include/exclude filter to the sync plan | Compare & sync | `dialog` | FR-OPS-10 |
| sync.preview_plan | Preview the generated sync plan (dry run) | Compare & sync | `dialog` | FR-OPS-10 |
| sync.execute | Execute the sync plan through the operation queue | Compare & sync | `dialog && sync.plan_ready` | FR-OPS-10 |

## Split, merge & checksums

| id | title | category | precondition | notes |
|---|---|---|---|---|
| tool.split_file | Split a file into volumes | Split, merge & checksums | `panel && selection.single` | FR-OPS-11 |
| tool.merge_files | Merge split volumes back into one file | Split, merge & checksums | `panel && cursor.is_split_part` | FR-OPS-11, `.crc` verification |
| checksum.create | Create a checksum file (SFV/MD5/SHA-1/SHA-256/BLAKE3) | Split, merge & checksums | `panel && selection.nonempty` | FR-OPS-11 |
| checksum.verify | Verify files against a checksum file | Split, merge & checksums | `panel && cursor.is_checksum_file` | FR-OPS-11 |
| checksum.set_algorithm | Choose the checksum algorithm | Split, merge & checksums | `dialog` | FR-OPS-11 |

## Command line & terminal

| id | title | category | precondition | notes |
|---|---|---|---|---|
| cmdline.focus | Focus the command line | Command line & terminal | `app` | FR-TOOL-06 |
| cmdline.execute | Execute the current command-line input | Command line & terminal | `cmdline` | FR-TOOL-06, runs in the user's shell with the active panel as cwd |
| cmdline.insert_name | Insert the cursor entry's filename | Command line & terminal | `cmdline` | Ctrl+Enter |
| cmdline.insert_path | Insert the cursor entry's full path | Command line & terminal | `cmdline` | Ctrl+Shift+Enter |
| cmdline.history_prev | Recall the previous command-line history entry | Command line & terminal | `cmdline` | FR-TOOL-06 |
| cmdline.history_next | Recall the next command-line history entry | Command line & terminal | `cmdline` | FR-TOOL-06 |
| cmdline.complete | Autocomplete the current command-line token | Command line & terminal | `cmdline` | FR-TOOL-06 |
| cmdline.clear | Clear the command line | Command line & terminal | `cmdline` | |
| terminal.toggle | Toggle the embedded terminal panel | Command line & terminal | `app` | FR-TOOL-07 |
| terminal.focus | Focus the embedded terminal | Command line & terminal | `terminal.visible` | FR-TOOL-07 |
| terminal.sync_cwd | Sync the terminal's cwd to the active panel | Command line & terminal | `terminal.visible` | FR-TOOL-07 |

## Properties & associations

| id | title | category | precondition | notes |
|---|---|---|---|---|
| file.open_with | Open the "Open With" application chooser | Properties & associations | `panel && cursor.is_file` | FR-TOOL-08 |
| file.open_default | Open with the default associated application | Properties & associations | `panel && cursor.is_file` | FR-TOOL-08 |
| file.set_association | Set the default application for a file type | Properties & associations | `dialog` | FR-TOOL-08 |
| file.calculate_checksum_ondemand | Compute a checksum on demand in the properties dialog | Properties & associations | `dialog` | FR-TOOL-10 |
| file.thumbnail_regenerate | Regenerate the thumbnail for the cursor entry | Properties & associations | `panel && cursor.is_file` | FR-TOOL-09 |

## Clipboard & drag-drop

| id | title | category | precondition | notes |
|---|---|---|---|---|
| clipboard.copy | Copy selection to the clipboard | Clipboard & drag-drop | `panel && selection.nonempty` | FR-CFG-05 |
| clipboard.cut | Cut selection to the clipboard | Clipboard & drag-drop | `panel && selection.nonempty` | FR-CFG-05, GNOME/KDE cut-marker conventions |
| clipboard.paste | Paste files from the clipboard into the panel | Clipboard & drag-drop | `panel && clipboard.has_files` | FR-CFG-05 |
| clipboard.paste_link | Paste as a symlink | Clipboard & drag-drop | `panel && clipboard.has_files` | |
| clipboard.copy_path | Copy selection's path(s) to the clipboard as text | Clipboard & drag-drop | `panel && selection.nonempty` | |
| clipboard.copy_name | Copy selection's filename(s) to the clipboard as text | Clipboard & drag-drop | `panel && selection.nonempty` | |
| dnd.drag_start | Begin a drag operation with the selection | Clipboard & drag-drop | `panel && selection.nonempty` | FR-CFG-06; outbound cross-app deferred to P1 per R-G3 |
| dnd.drop | Handle a drop of external files onto a panel | Clipboard & drag-drop | `panel` | FR-CFG-06; inbound implemented natively per R-G3 |

## Trash

| id | title | category | precondition | notes |
|---|---|---|---|---|
| trash.open_browser | Open the trash browser | Trash | `app` | FR-CFG-07 |
| trash.restore | Restore selected trashed entries | Trash | `trash_browser && selection.nonempty` | FR-CFG-07 |
| trash.empty | Permanently empty the trash | Trash | `trash_browser` | FR-CFG-07 |
| trash.delete_selected | Permanently delete selected trashed entries | Trash | `trash_browser && selection.nonempty` | FR-CFG-07 |

## Hotlist & history

| id | title | category | precondition | notes |
|---|---|---|---|---|
| hotlist.open | Open the hotlist (bookmarks) overlay | Hotlist & history | `panel` | Ctrl+D; FR-NAV-08 |
| hotlist.add | Add the current directory to the hotlist | Hotlist & history | `panel` | FR-NAV-08 |
| hotlist.remove | Remove the selected entry from the hotlist | Hotlist & history | `hotlist_overlay && entry.selected` | FR-NAV-08 |
| hotlist.reorder | Reorder a hotlist entry | Hotlist & history | `hotlist_overlay` | FR-NAV-08 |
| hotlist.rename_entry | Rename a hotlist entry's label | Hotlist & history | `hotlist_overlay && entry.selected` | |
| hotlist.navigate | Navigate to the selected hotlist entry | Hotlist & history | `hotlist_overlay && entry.selected` | Arrow+Enter |
| hotlist.add_submenu | Add a hotlist submenu/category | Hotlist & history | `hotlist_overlay` | |

## Command palette & help

| id | title | category | precondition | notes |
|---|---|---|---|---|
| cmd.palette_open | Open the command palette | Command palette & help | `app` | FR-TOOL-11 |
| cmd.palette_execute | Execute the highlighted palette command | Command palette & help | `palette` | FR-TOOL-11 |
| cmd.palette_close | Close the command palette | Command palette & help | `palette` | |
| help.show_manual | Open the user manual | Command palette & help | `app` | |
| help.show_keymap_reference | Open the keymap reference | Command palette & help | `app` | FR-CFG-02 |
| help.about | Show the About dialog | Command palette & help | `app` | |
| help.show_shortcuts_overlay | Show an on-screen keyboard-shortcut cheat sheet | Command palette & help | `app` | |
| help.report_issue | Open the issue-reporting page | Command palette & help | `app` | |
| help.check_for_updates | Check for a new release | Command palette & help | `app` | |

## Settings & configuration

| id | title | category | precondition | notes |
|---|---|---|---|---|
| settings.open | Open the settings UI | Settings & configuration | `app` | FR-CFG-01 |
| settings.open_file | Reveal the underlying TOML settings file | Settings & configuration | `settings_ui` | FR-CFG-01 |
| settings.reload | Force-reload configuration from disk | Settings & configuration | `app` | FR-CFG-01, hot reload |
| settings.keymap_edit | Open the keymap editor | Settings & configuration | `app` | FR-CFG-02 |
| settings.keymap_reset_binding | Reset a keybinding to its default | Settings & configuration | `keymap_editor` | FR-CFG-02 |
| settings.keymap_switch_base | Switch the base keymap (tc/mc/modern) | Settings & configuration | `settings_ui` | FR-CFG-02 |
| settings.theme_select | Select an application theme | Settings & configuration | `settings_ui` | FR-CFG-04 |
| settings.theme_follow_system | Toggle following the desktop light/dark preference | Settings & configuration | `settings_ui` | FR-CFG-04 |
| settings.import_wincmd_ini | Import settings from a Total Commander `wincmd.ini` | Settings & configuration | `settings_ui` | FR-CFG-03 |
| settings.locale_select | Select the application locale | Settings & configuration | `settings_ui` | FR-CFG-10 |
| settings.buttonbar_configure | Configure the button bar | Settings & configuration | `settings_ui` | FR-TOOL-12 |
| settings.buttonbar_add_button | Add a button to the button bar | Settings & configuration | `buttonbar_config` | FR-TOOL-12 |
| settings.buttonbar_remove_button | Remove a button from the button bar | Settings & configuration | `buttonbar_config` | FR-TOOL-12 |

## Application

| id | title | category | precondition | notes |
|---|---|---|---|---|
| app.quit | Quit the application | Application | `app` | |
| app.new_instance | Launch a new independent instance | Application | `app` | FR-CFG-09, `--new-instance` |
| app.new_window | Open a new top-level window | Application | `app` | |
| app.toggle_fullscreen | Toggle fullscreen mode | Application | `app` | |
| app.minimize | Minimize the window | Application | `app` | |
| app.show_dbus_folder | Handle a `ShowFolders` request via `org.freedesktop.FileManager1` | Application | `app` | FR-CFG-08 |
| app.show_dbus_item_properties | Handle a `ShowItemProperties` request via `org.freedesktop.FileManager1` | Application | `app` | FR-CFG-08 |
| app.reload_plugins | Reload all dev-mode plugins | Application | `app && plugins.dev_mode` | FR-PLUG-04 |
| app.session_save | Save the current workspace session manually | Application | `app` | FR-CFG-01 |
| app.session_restore_default | Reset the session to the default layout | Application | `app` | |

## Plugins

Per FR-PLUG-02, plugins register commands through one of five WIT interfaces (`content-plugin`, `packer-plugin`, `fs-plugin`, `viewer-plugin`, `command-plugin`; §9.9). The rows below split into (a) host-provided commands for managing plugins, and (b) the placeholder command classes that plugin-registered commands themselves fall into — each installed plugin contributes concrete entries at `plugin.<class>.<plugin-id>.<name>` at runtime, indistinguishable from built-ins in the palette (G-4).

| id | title | category | precondition | notes |
|---|---|---|---|---|
| plugin.manager_open | Open the plugin manager | Plugins | `app` | FR-PLUG-04 |
| plugin.install | Install a plugin from the registry | Plugins | `plugin_manager` | FR-PLUG-04 |
| plugin.update | Update an installed plugin | Plugins | `plugin_manager && plugin.has_update` | FR-PLUG-04 |
| plugin.remove | Remove an installed plugin | Plugins | `plugin_manager && plugin.installed` | FR-PLUG-04 |
| plugin.disable | Disable an installed plugin | Plugins | `plugin_manager && plugin.installed` | FR-PLUG-06 |
| plugin.enable | Enable a disabled plugin | Plugins | `plugin_manager && plugin.disabled` | |
| plugin.review_capabilities | Review a plugin's requested capabilities | Plugins | `plugin_manager && plugin.selected` | FR-PLUG-03 |
| plugin.load_dev | Load a local plugin directory for development | Plugins | `app` | FR-PLUG-04, `--dev-plugin <dir>` |
| plugin.reload_dev | Hot-reload the active dev plugin | Plugins | `app && plugins.dev_mode` | T-8.1.3 |
| plugin.command.\<id\> | *(placeholder class)* a command registered by a **command** plugin | Plugins | plugin-declared | FR-PLUG-02; concrete id is `plugin.command.<plugin-id>.<name>` |
| plugin.content.\<id\> | *(placeholder class)* a computed-column/metadata action registered by a **content** plugin | Plugins | plugin-declared | FR-PLUG-02, mirrors TC `.wdx` |
| plugin.viewer.\<id\> | *(placeholder class)* an "open with plugin viewer" action registered by a **viewer** plugin | Plugins | `panel && cursor.is_file` | FR-PLUG-02, mirrors TC `.wlx` |
| plugin.packer.\<id\> | *(placeholder class)* an archive-format handler registered by a **packer** plugin | Plugins | `panel` | FR-PLUG-02, mirrors TC `.wcx` |
| plugin.fs.\<id\> | *(placeholder class)* a VFS-backend mount action registered by a **filesystem** plugin | Plugins | `panel` | FR-PLUG-02, mirrors TC `.wfx` |

---

## Traceability: §9.4 keymap extract → command id

Every binding in `design.md` §9.4's default-keymap extract table resolves to an id defined above:

| Key | Command id | Key | Command id |
|---|---|---|---|
| Tab | `focus.other_panel` | F2 | `panel.reread` |
| F3 | `view.open` | F4 | `edit.open` |
| F5 | `ops.copy` | F6 | `ops.move_or_rename` |
| F7 | `ops.mkdir` | F8 / Del | `ops.delete` |
| Ins | `sel.toggle_and_advance` | Space | `sel.toggle_and_size` |
| Num + | `sel.by_mask` | Num − | `unsel.by_mask` |
| Num * | `sel.invert` | Ctrl+U | `panel.swap` |
| Ctrl+←/→ | `panel.push_to_other` | Ctrl+PgUp | `nav.parent` |
| Ctrl+\\ | `nav.root` | Ctrl+D | `hotlist.open` |
| Ctrl+T | `tab.new` | Ctrl+W | `tab.close` |
| Ctrl+B | `panel.branch_view` | Ctrl+M | `tool.multi_rename` |
| Ctrl+Q | `panel.quick_view` | Alt+F7 | `tool.search` |
| Alt+F5 | `archive.pack` | Alt+F9 | `archive.unpack` |
| Alt+F1 | `drive.change_left` | Alt+F2 | `drive.change_right` |
| Alt+Enter | `file.properties` | Ctrl+Enter | `cmdline.insert_name` |
| Ctrl+Shift+Enter | `cmdline.insert_path` | Shift+F6 (§9.4 worked example) | `ops.rename_in_place` |
