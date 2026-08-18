// SPDX-License-Identifier: MIT
//! Per-side tab container (T-4.3.2, FR-NAV-03): each of the workspace's
//! two panels hosts N independent [`FileTable`] tabs, of which exactly one
//! (`active`) is rendered at a time. Total Commander tab semantics:
//! new/close/next/prev/duplicate/lock/lock_dir_change/close_others/
//! move_left/move_right/reopen_closed. Commands with a real
//! `docs/keymap-tc.csv` binding (`Ctrl+T`/`Ctrl+W`/`Ctrl+Tab`/
//! `Ctrl+Shift+Tab`/`Ctrl+Shift+T`) are wired up as GPUI actions in
//! [`bind_panel_keys`]; the rest (`tab.duplicate`, `tab.lock`,
//! `tab.lock_dir_change`, `tab.close_others`, `tab.move_left`,
//! `tab.move_right`) have no keymap-tc.csv row at all -- same as
//! T-4.3.1's `nav.open_parent_and_select` -- so they're implemented as
//! plain public methods, reachable only once T-4.3.6's command palette
//! exists to invoke them, not left unimplemented.

use std::path::PathBuf;
use std::rc::Rc;

use duet_config::{SessionPanel, SessionSortColumn, SessionTab};
use duet_index::SortColumn;
use duet_widgets::{
    layout::{h_flex, v_flex},
    theme::TokenPalette,
};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, actions, div, px,
};

use crate::file_table::{
    FileTable, FileTableEvent, FileTableSettings, LockedNavigationHandler, TabRestore,
};

/// `duet_config::SessionSortColumn` -> `duet_index::SortColumn`, for
/// restoring a tab's sort from `session.json`.
fn sort_column_from_session(column: SessionSortColumn) -> SortColumn {
    match column {
        SessionSortColumn::Name => SortColumn::Name,
        SessionSortColumn::Size => SortColumn::Size,
        SessionSortColumn::Modified => SortColumn::Modified,
    }
}

/// The inverse of [`sort_column_from_session`], for [`Panel::snapshot`].
/// `SortColumn::Kind` is a real variant but unreachable through any
/// column `FileTableDelegate` actually renders (see
/// `SessionSortColumn`'s own doc comment) -- falls back to `Name`,
/// matching what `FileTableDelegate::perform_sort`'s own
/// `_ => SortColumn::Name` arm already does for anything outside its
/// three known columns.
fn sort_column_to_session(column: SortColumn) -> SessionSortColumn {
    match column {
        SortColumn::Size => SessionSortColumn::Size,
        SortColumn::Modified => SessionSortColumn::Modified,
        SortColumn::Name | SortColumn::Kind => SessionSortColumn::Name,
    }
}

// T-4.3.2's tab commands with a real default binding (see the module doc
// comment for the rest). Scoped to whichever `Panel` currently contains
// keyboard focus: `Panel::render`'s root element carries
// `.key_context("Panel")`, and GPUI resolves a `KeyBinding`'s context
// against the *whole* ancestor chain of the focused element (confirmed
// already by `workspace.rs`'s `ResizeSplitterLeft`/`Right`, scoped to
// `"Workspace"` yet firing while a `FileTable` two levels deeper holds
// actual focus) -- so `Ctrl+T` on the right panel opens a tab in the
// right panel specifically, never both, with no explicit "which panel is
// active" plumbing needed: only one `Panel` is ever an ancestor of the
// currently-focused element at a time.
actions!(
    duet_panel,
    [TabNew, TabClose, TabNext, TabPrev, TabReopenClosed]
);

/// Registers [`Panel`]'s own keybindings. Called once from
/// `workspace::run`, before any window opens -- see
/// `file_table::bind_file_table_keys`'s doc comment for the identical
/// pattern this mirrors.
pub fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-t", TabNew, Some("Panel")),
        KeyBinding::new("ctrl-w", TabClose, Some("Panel")),
        KeyBinding::new("ctrl-tab", TabNext, Some("Panel")),
        KeyBinding::new("ctrl-shift-tab", TabPrev, Some("Panel")),
        // `inferred`, not `known`, in keymap-tc.csv (plausible by browser
        // convention, chord not hand-verified against real TC) -- bound
        // anyway since it's low-risk and this is otherwise the only way
        // to exercise `reopen_closed` at all before T-4.3.6 exists.
        KeyBinding::new("ctrl-shift-t", TabReopenClosed, Some("Panel")),
    ]);
}

/// One tab: its own independent [`FileTable`] plus the two TC lock flags
/// (design.md FR-NAV-03). `locked_navigation`'s callback (installed on
/// `table` via [`FileTable::set_locked_navigation`] whenever these flags
/// change -- see [`Panel::apply_lock_state`]) is what actually enforces
/// them; these two bools are the source of truth that callback is
/// recomputed from, and what `docs/commands.md`'s `tab.lock`/
/// `tab.lock_dir_change` toggle.
struct TabEntry {
    table: Entity<FileTable>,
    locked: bool,
    lock_dir_change: bool,
}

/// What [`Panel::reopen_closed`] needs to recreate a tab exactly as it was
/// when [`Panel::close_active`]/[`Panel::close_others`] removed it --
/// directory, lock flags, and (T-4.3.7) cursor position + sort, mirroring
/// `duet_config::session::SessionTab`'s own fields (native
/// `duet_index::SortColumn` here rather than `SessionSortColumn`, since
/// this never gets serialized -- it only ever feeds back into a
/// [`TabRestore`]).
struct ClosedTab {
    dir: PathBuf,
    locked: bool,
    lock_dir_change: bool,
    cursor_name: Option<String>,
    sort: (SortColumn, bool),
}

/// The per-side tab container. See the module doc comment.
pub struct Panel {
    tabs: Vec<TabEntry>,
    /// Index into `tabs` of the tab currently rendered/focused-into.
    /// Always in range whenever `tabs` is non-empty -- `Panel` is never
    /// constructed with zero tabs (see [`Panel::new`]'s doc comment) and
    /// every method that removes a tab immediately re-clamps this.
    active: usize,
    /// LIFO stack of tabs closed by [`Panel::close_active`]/
    /// [`Panel::close_others`], for [`Panel::reopen_closed`]
    /// (`tab.reopen_closed`, Ctrl+Shift+T).
    closed_stack: Vec<ClosedTab>,
    tokio_handle: tokio::runtime::Handle,
    /// See `file_table::FileTableSettings`'s doc comment -- read once by
    /// `Workspace::new` and passed down unchanged to every tab this panel
    /// creates.
    file_table_settings: FileTableSettings,
}

impl Panel {
    /// Builds a panel from `tabs` (as loaded from `session.json`, or a
    /// single synthetic entry at the process's cwd on first launch / a
    /// corrupt session -- see `workspace.rs`'s session-loading code).
    /// `tabs` must be non-empty; every caller in this codebase guarantees
    /// that (there is no real TC state where a panel has zero tabs), so
    /// this asserts rather than silently substituting a default, per this
    /// project's "trust internal callers, validate only at real
    /// boundaries" convention.
    ///
    /// `pub(crate)`, not `pub`: only `workspace::Workspace::new` ever
    /// constructs a `Panel` (no external caller exists) -- `pub` would
    /// leak `MouseMode` (`pub(crate)`, same reasoning as `TabRestore`)
    /// through a public signature.
    pub(crate) fn new(
        tabs: Vec<SessionTab>,
        active: usize,
        tokio_handle: tokio::runtime::Handle,
        file_table_settings: FileTableSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        debug_assert!(!tabs.is_empty(), "Panel must be constructed with >=1 tab");
        let mut panel = Self {
            tabs: Vec::new(),
            active: 0,
            closed_stack: Vec::new(),
            tokio_handle,
            file_table_settings,
        };
        for tab in tabs {
            let restore = TabRestore {
                cursor_name: tab.cursor_name,
                sort: (
                    sort_column_from_session(tab.sort_column),
                    tab.sort_ascending,
                ),
            };
            panel.add_tab_entry(
                tab.dir,
                tab.locked,
                tab.lock_dir_change,
                restore,
                window,
                cx,
            );
        }
        panel.active = active.min(panel.tabs.len().saturating_sub(1));
        panel
    }

    /// Creates a real, independent `FileTable` for `dir`, subscribes to
    /// its `FileTableEvent::DirectoryChanged` (so this panel re-notifies
    /// -- and therefore `workspace.rs`'s `cx.observe` persists
    /// `session.json` -- whenever any tab's directory actually changes,
    /// not on every cursor keystroke; see that event's doc comment),
    /// installs the lock-redirect callback if applicable, appends it, and
    /// returns its index. The one place every tab-creating command
    /// (`new_tab`, `duplicate_active`, `reopen_closed`, the locked-nav
    /// redirect itself, and this constructor) funnels through.
    fn add_tab_entry(
        &mut self,
        dir: PathBuf,
        locked: bool,
        lock_dir_change: bool,
        restore: TabRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Seeds the new tab's column widths from an already-measured
        // sibling in this same panel, if one exists -- see
        // `FileTable::responsive_seed`'s doc comment. Every tab in a
        // `Panel` renders at the same panel width, so any existing tab's
        // answer is exactly right for a new one too; this is what stops a
        // tab opened mid-session (Ctrl+T, a tab-strip click, ...) from
        // visibly flashing a too-narrow Name column for one frame before
        // some unrelated later action happens to trigger the repaint that
        // would otherwise be the first thing to reveal the corrected
        // width (UAT: "defaults to a much narrower name column... resizes
        // immediately after whatever navigation action").
        let width_seed = self
            .tabs
            .get(self.active)
            .and_then(|t| t.table.read(cx).responsive_seed(cx));
        let table = cx.new(|cx| {
            FileTable::new(
                dir,
                self.tokio_handle.clone(),
                width_seed,
                restore,
                self.file_table_settings,
                window,
                cx,
            )
        });
        cx.subscribe(
            &table,
            |_this, _table, event: &FileTableEvent, cx| match event {
                FileTableEvent::DirectoryChanged => cx.notify(),
            },
        )
        .detach();

        // T-4.3.8's middle-click "open in a new tab" -- see
        // `NewTabHandler`'s doc comment for why this captures the
        // table's own `WeakEntity` (to resolve the click's target name
        // against `current_dir` fresh, at click time) alongside
        // `weak_panel`. Reuses `open_redirected_tab` outright -- the
        // "make a new, unlocked tab active at this directory, inheriting
        // the current sort" behaviour it already gives the locked-tab
        // redirect is exactly what a middle-click should do here too.
        // Same `window.spawn`/`update_in` dance as `apply_lock_state`'s
        // handler, for the same reason (neither `Window` nor
        // `Context<FileTable>` implements `VisualContext`).
        let weak_table = table.downgrade();
        let weak_panel = cx.entity().downgrade();
        table.update(cx, |table, cx| {
            table.set_new_tab_handler(
                Some(Rc::new(
                    move |name: String, window: &mut Window, app: &mut App| {
                        let weak_table = weak_table.clone();
                        let weak_panel = weak_panel.clone();
                        window
                            .spawn(app, async move |cx| {
                                let Ok(dir) = weak_table
                                    .update(cx, |table, _cx| table.current_dir().join(&name))
                                else {
                                    return;
                                };
                                let _ =
                                    weak_panel.update_in(cx, |panel: &mut Panel, window, cx| {
                                        panel.open_redirected_tab(dir, window, cx);
                                    });
                            })
                            .detach();
                    },
                )),
                cx,
            );
        });

        self.tabs.push(TabEntry {
            table,
            locked,
            lock_dir_change,
        });
        let ix = self.tabs.len() - 1;
        self.apply_lock_state(ix, cx);
        ix
    }

    /// Pushes tab `ix`'s current `locked`/`lock_dir_change` flags into its
    /// `FileTable` as a redirect callback (or clears it). TC semantics:
    /// `locked && !lock_dir_change` is the state that actually blocks
    /// in-place navigation (`lock_dir_change` is a *modifier* of a locked
    /// tab that explicitly permits it to still change directory in
    /// place -- both flags are still recorded and shown either way, only
    /// the enforcement differs). Called after every mutation of either
    /// flag, and once at tab creation.
    fn apply_lock_state(&mut self, ix: usize, cx: &mut Context<Self>) {
        let tab = &self.tabs[ix];
        let handler: Option<LockedNavigationHandler> = if tab.locked && !tab.lock_dir_change {
            let weak_panel = cx.entity().downgrade();
            // Neither `Window` nor `Context<FileTable>` (what `navigate_to`
            // actually holds when it calls this) implements `VisualContext`
            // -- only `AsyncWindowContext` does (confirmed by reading
            // `gpui-0.2.2`'s `app/async_context.rs`) -- so reaching another
            // entity's `update_in` from here has to go through
            // `Window::spawn`, same as every cross-entity update in
            // `gpui-component` itself does (e.g. `dock/mod.rs`'s
            // `subscribe_panel`). The `spawn`ed future starts running on
            // GPUI's own executor as soon as it's next polled -- for all
            // practical purposes immediate, not a user-visible delay.
            Some(
                Rc::new(move |dir: PathBuf, window: &mut Window, app: &mut App| {
                    let weak_panel = weak_panel.clone();
                    window
                        .spawn(app, async move |cx| {
                            let _ = weak_panel.update_in(cx, |panel: &mut Panel, window, cx| {
                                panel.open_redirected_tab(dir, window, cx);
                            });
                        })
                        .detach();
                }) as LockedNavigationHandler,
            )
        } else {
            None
        };
        let table = self.tabs[ix].table.clone();
        table.update(cx, |table, _cx| table.set_locked_navigation(handler));
    }

    /// Opens a fresh, unlocked tab at `dir` and makes it active -- what a
    /// locked tab's blocked navigation attempt redirects into (TC's real
    /// locked-tab behaviour: the locked tab itself never moves). Inherits
    /// the locked tab's current sort (it *is* `self.active` at the moment
    /// its own navigation attempt triggered this -- the user was
    /// interacting with it), same "carry sort forward, start fresh on
    /// cursor position" split as `new_tab`.
    fn open_redirected_tab(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let restore = TabRestore {
            cursor_name: None,
            sort: self.active_table().read(cx).sort_state(cx),
        };
        let ix = self.add_tab_entry(dir, false, false, restore, window, cx);
        self.active = ix;
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Ctrl+T (`tab.new`): opens a new, unlocked tab at the *active* tab's
    /// current directory (`docs/keymap-tc.csv`'s own description: "a new
    /// tab (duplicate of current directory)") and switches to it. Always
    /// unlocked regardless of the active tab's own lock state -- TC's
    /// "new tab" is a plain new tab, not a clone of the source tab's lock
    /// flags (that's `tab.duplicate`'s job, below). Inherits the active
    /// tab's current sort (continuing to browse the same directory sorted
    /// the same way is the expected feel) but starts at row 0, not the
    /// source tab's cursor position -- a genuinely new tab, not a clone.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active_table().read(cx).current_dir().to_path_buf();
        let restore = TabRestore {
            cursor_name: None,
            sort: self.active_table().read(cx).sort_state(cx),
        };
        let ix = self.add_tab_entry(dir, false, false, restore, window, cx);
        self.active = ix;
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// `tab.duplicate` (no default key -- see the module doc comment):
    /// opens a new tab at the active tab's directory *and* copies its
    /// lock flags, cursor position, and sort -- unlike `tab.new`, this is
    /// a literal clone.
    pub fn duplicate_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = &self.tabs[self.active];
        let dir = active.table.read(cx).current_dir().to_path_buf();
        let (locked, lock_dir_change) = (active.locked, active.lock_dir_change);
        let restore = TabRestore {
            cursor_name: active.table.read(cx).cursor_entry_name(cx),
            sort: active.table.read(cx).sort_state(cx),
        };
        let ix = self.add_tab_entry(dir, locked, lock_dir_change, restore, window, cx);
        self.active = ix;
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Ctrl+W (`tab.close`): closes the active tab, remembering it on
    /// `closed_stack` for `reopen_closed`. A no-op on the last remaining
    /// tab -- matches `docs/commands.md`'s own context predicate
    /// (`panel && tab.count > 1`); TC never lets a panel drop to zero
    /// tabs.
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let closed = self.tabs.remove(self.active);
        self.closed_stack.push(ClosedTab {
            dir: closed.table.read(cx).current_dir().to_path_buf(),
            locked: closed.locked,
            lock_dir_change: closed.lock_dir_change,
            cursor_name: closed.table.read(cx).cursor_entry_name(cx),
            sort: closed.table.read(cx).sort_state(cx),
        });
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// `tab.close_others` (no default key): closes every tab except the
    /// active one, pushing each onto `closed_stack` in closing order (so
    /// `reopen_closed` brings the most-recently-active-before-closing one
    /// back first).
    pub fn close_others(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let keep = self.tabs.remove(self.active);
        let removed: Vec<TabEntry> = self.tabs.drain(..).collect();
        for t in removed {
            self.closed_stack.push(ClosedTab {
                dir: t.table.read(cx).current_dir().to_path_buf(),
                locked: t.locked,
                lock_dir_change: t.lock_dir_change,
                cursor_name: t.table.read(cx).cursor_entry_name(cx),
                sort: t.table.read(cx).sort_state(cx),
            });
        }
        self.tabs.push(keep);
        self.active = 0;
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Ctrl+Shift+T (`tab.reopen_closed`): pops the most recently closed
    /// tab and reopens it with its original directory and lock flags, as
    /// a new tab (not, say, re-inserted at its old position -- matching
    /// the browser convention this binding is borrowed from). A no-op if
    /// nothing has been closed this session.
    pub fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.closed_stack.pop() else {
            return;
        };
        let restore = TabRestore {
            cursor_name: closed.cursor_name,
            sort: closed.sort,
        };
        let ix = self.add_tab_entry(
            closed.dir,
            closed.locked,
            closed.lock_dir_change,
            restore,
            window,
            cx,
        );
        self.active = ix;
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Ctrl+Tab (`tab.next`): wraps from the last tab to the first.
    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Ctrl+Shift+Tab (`tab.prev`): wraps from the first tab to the last.
    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.focus_active_tab(window, cx);
        cx.notify();
    }

    /// Tab-strip click-to-switch, and also `tab.goto_index`'s
    /// implementation (`docs/commands.md`'s `args_schema: { index: u32 }`
    /// -- no default key, reachable once T-4.3.6 exists). Out-of-range
    /// `ix` is a silent no-op rather than a panic: a stale click (the tab
    /// strip changed between the click starting and landing) is a normal
    /// race, not a bug.
    pub fn switch_to(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() && ix != self.active {
            self.active = ix;
            self.focus_active_tab(window, cx);
            cx.notify();
        }
    }

    /// Moves real keyboard focus onto the active tab's `FileTable`.
    /// Called after every command that changes which tab is active
    /// (`new_tab`, `close_active`, `next_tab`, the tab strip's click
    /// handler, ...): without this, focus stays on whichever `FileTable`
    /// held it before the switch -- which, for a *closed* or no-longer-
    /// rendered tab, is an element no longer in the dispatch tree at all,
    /// so no key press reaches anything and the panel reads as neither
    /// active nor inactive (T-4.3.2 UAT: "neither panel is active any
    /// more" after switching tabs). `move_active_left`/`_right` don't call
    /// this -- they reorder `tabs` but never change *which* `FileTable`
    /// entity is active, so its focus state is untouched by them.
    fn focus_active_tab(&self, window: &mut Window, cx: &App) {
        let handle = self.active_table().read(cx).focus_handle(cx);
        window.focus(&handle);
    }

    /// `tab.lock` (no default key): toggles the active tab's base lock.
    /// Unlocking also clears `lock_dir_change` -- that flag only means
    /// anything for a locked tab.
    pub fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        let ix = self.active;
        self.tabs[ix].locked = !self.tabs[ix].locked;
        if !self.tabs[ix].locked {
            self.tabs[ix].lock_dir_change = false;
        }
        self.apply_lock_state(ix, cx);
        cx.notify();
    }

    /// `tab.lock_dir_change` (no default key): toggles the "allow
    /// directory changes while locked" modifier. Turning it on implies
    /// `locked` (TC semantics: this is a *kind* of locked tab, not an
    /// independent state) -- turning it off leaves `locked` exactly as it
    /// was.
    pub fn toggle_lock_dir_change(&mut self, cx: &mut Context<Self>) {
        let ix = self.active;
        self.tabs[ix].lock_dir_change = !self.tabs[ix].lock_dir_change;
        if self.tabs[ix].lock_dir_change {
            self.tabs[ix].locked = true;
        }
        self.apply_lock_state(ix, cx);
        cx.notify();
    }

    /// `tab.move_left` (no default key): swaps the active tab one
    /// position earlier. A no-op already at the leftmost position.
    pub fn move_active_left(&mut self, cx: &mut Context<Self>) {
        if self.active == 0 {
            return;
        }
        self.tabs.swap(self.active, self.active - 1);
        self.active -= 1;
        cx.notify();
    }

    /// `tab.move_right` (no default key): the mirror of
    /// [`Self::move_active_left`].
    pub fn move_active_right(&mut self, cx: &mut Context<Self>) {
        if self.active + 1 >= self.tabs.len() {
            return;
        }
        self.tabs.swap(self.active, self.active + 1);
        self.active += 1;
        cx.notify();
    }

    /// The active tab's `FileTable` -- what `workspace.rs` renders as this
    /// panel's body, and reads path/footer/volume-stats text from.
    pub fn active_table(&self) -> &Entity<FileTable> {
        &self.tabs[self.active].table
    }

    /// The active tab's real keyboard focus target -- `workspace.rs` uses
    /// this both at startup (focusing the left panel's first tab) and for
    /// `focus.other_panel` (Tab: focusing whichever panel *doesn't*
    /// currently hold it).
    pub fn active_focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_table().read(cx).focus_handle(cx)
    }

    /// Reads every tab's *live* current directory (not whatever it was
    /// when last persisted) plus its lock flags, for `workspace.rs`'s
    /// `persist_session` to write into `session.json`. `App`, not
    /// `Context<Self>`, because this only reads -- callers hold this
    /// panel entity itself borrowed via `.read(cx)` already.
    pub fn snapshot(&self, cx: &App) -> SessionPanel {
        SessionPanel {
            tabs: self
                .tabs
                .iter()
                .map(|t| {
                    let table = t.table.read(cx);
                    let (sort_column, sort_ascending) = table.sort_state(cx);
                    SessionTab {
                        dir: table.current_dir().to_path_buf(),
                        locked: t.locked,
                        lock_dir_change: t.lock_dir_change,
                        cursor_name: table.cursor_entry_name(cx),
                        sort_column: sort_column_to_session(sort_column),
                        sort_ascending,
                    }
                })
                .collect(),
            active_tab: self.active,
        }
    }

    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tokens = TokenPalette::current(cx);
        let active_ix = self.active;
        h_flex()
            .w_full()
            .gap_px()
            .bg(tokens.color.statusbar_bg)
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let active = ix == active_ix;
                let label = tab_label(tab, cx);
                let (bg, fg) = if active {
                    (tokens.color.panel_bg_active, tokens.color.panel_fg_active)
                } else {
                    (tokens.color.statusbar_bg, tokens.color.statusbar_fg)
                };
                div()
                    .id(("tab", ix))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .bg(bg)
                    .text_color(fg)
                    .text_size(px(11.))
                    .border_b_2()
                    .border_color(if active {
                        tokens.color.border_focus
                    } else {
                        bg
                    })
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.switch_to(ix, window, cx);
                    }))
                    .child(label)
            }))
    }
}

/// The tab strip's label for `tab`: its directory's file name (or the
/// full path for a root-like directory with no file name, e.g. `/`),
/// prefixed with a plain-text lock marker -- no icon rendering exists yet
/// anywhere in this app (T-4.2.6 is still pending), so this matches the
/// rest of the UI's current all-text chrome rather than introducing the
/// first icon glyph in a tab label specifically.
fn tab_label(tab: &TabEntry, cx: &App) -> SharedString {
    let dir = tab.table.read(cx).current_dir();
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    match (tab.locked, tab.lock_dir_change) {
        (true, true) => format!("[L+] {name}").into(),
        (true, false) => format!("[L] {name}").into(),
        (false, _) => name.into(),
    }
}

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_strip = self.render_tab_strip(cx);
        let active_table = self.active_table().clone();
        v_flex()
            .size_full()
            .key_context("Panel")
            .on_action(cx.listener(|this, _: &TabNew, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, _: &TabClose, window, cx| this.close_active(window, cx)))
            .on_action(cx.listener(|this, _: &TabNext, window, cx| this.next_tab(window, cx)))
            .on_action(cx.listener(|this, _: &TabPrev, window, cx| this.prev_tab(window, cx)))
            .on_action(cx.listener(|this, _: &TabReopenClosed, window, cx| {
                this.reopen_closed(window, cx);
            }))
            .child(tab_strip)
            .child(div().flex_1().min_h(px(0.)).child(active_table))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use duet_types::EntryId;
    use duet_widgets::layout::Root;
    use duet_widgets::table::{TableDelegate, TableEvent};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use tempfile::TempDir;

    use super::*;
    use crate::file_table::{CursorDown, EnterDirectory, MouseMode, NavigateRoot, QuickSearchMode};

    fn session_tab(dir: PathBuf, locked: bool, lock_dir_change: bool) -> SessionTab {
        SessionTab {
            dir,
            locked,
            lock_dir_change,
            cursor_name: None,
            sort_column: SessionSortColumn::Name,
            sort_ascending: true,
        }
    }

    /// Builds a real window (wrapped in `Root`, same as `workspace::run`
    /// does -- `gpui-component` widgets, which `FileTable`'s `TableState`
    /// is one of, panic if the window root isn't one) hosting a real
    /// `Panel`, and hands both it and the `VisualTestContext` needed to
    /// drive it to `f`. Backed by a real (if minimal) multi-thread Tokio
    /// runtime -- `FileTable::new`/`navigate_to` unconditionally call
    /// `tokio_handle.spawn(..)`, which needs one to exist, even though
    /// none of these tests wait for a spawned directory listing to
    /// actually complete (every assertion here is about tab structure --
    /// count/order/active-index/lock-flags/`current_dir` -- all set
    /// synchronously, never about listing contents).
    fn with_panel(
        cx: &mut TestAppContext,
        tabs: Vec<SessionTab>,
        active: usize,
        f: impl FnOnce(Entity<Panel>, &mut VisualTestContext),
    ) {
        with_panel_ext(cx, tabs, active, MouseMode::Windows, f);
    }

    /// [`with_panel`], with an explicit [`MouseMode`] -- T-4.3.8's mouse
    /// tests need to exercise all three; every other (pre-existing) test
    /// is indifferent to it, so `with_panel` itself just fixes it at the
    /// default (`Windows`) rather than making every call site plumb one
    /// through.
    fn with_panel_ext(
        cx: &mut TestAppContext,
        tabs: Vec<SessionTab>,
        active: usize,
        mouse_mode: MouseMode,
        f: impl FnOnce(Entity<Panel>, &mut VisualTestContext),
    ) {
        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("failed to start a test Tokio runtime");
        let tokio_handle = tokio_rt.handle().clone();

        cx.update(|cx| {
            duet_widgets::init(cx);
            // `TokenPalette::current` (used by `render_tab_strip`) panics
            // if no palette was ever installed -- every real window-open
            // path installs one via `theme_controller::ThemeController`
            // before building any view; this is that same prerequisite's
            // minimal stand-in for a test with no real desktop theme to
            // follow.
            duet_widgets::theme::TokenPalette::built_in(duet_widgets::theme::ThemeMode::Dark)
                .install(cx);
            crate::file_table::bind_file_table_keys(cx);
            bind_panel_keys(cx);
        });

        let file_table_settings = FileTableSettings {
            mouse_mode,
            quick_search_default_mode: QuickSearchMode::default(),
            quick_search_idle_timeout: Duration::from_millis(1200),
        };
        let mut panel_cell: Option<Entity<Panel>> = None;
        let (_root, vcx) = cx.add_window_view(|window, cx| {
            let panel = cx.new(|cx| {
                Panel::new(
                    tabs,
                    active,
                    tokio_handle.clone(),
                    file_table_settings,
                    window,
                    cx,
                )
            });
            panel_cell = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = panel_cell.expect("the window-build closure always constructs one");

        f(panel, vcx);
    }

    /// Two distinct, really-existing directories -- enough for every test
    /// below that needs more than one real path (`tempfile::tempdir()`
    /// each time would work too, but two independently named ones make
    /// assertions read more clearly than `dir_a`/`dir_a` twice would).
    fn two_dirs() -> (TempDir, TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    /// Waits for `condition` to become true, alternating `run_until_parked`
    /// with a short real sleep -- a single `run_until_parked()` call isn't
    /// enough for anything that depends on `spawn_directory_load`'s
    /// result: that work crosses onto a genuinely separate OS thread (the
    /// real, if minimal, Tokio runtime `with_panel` builds), and
    /// `run_until_parked` only guarantees GPUI's own executor has nothing
    /// left to do *right now* -- it can return while the Tokio-side
    /// listing is still in flight, before the oneshot channel it
    /// eventually completes through has been sent. Panics with a clear
    /// message if `condition` never becomes true within the timeout,
    /// rather than hanging a test run indefinitely.
    fn wait_until(
        vcx: &mut VisualTestContext,
        mut condition: impl FnMut(&mut VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            vcx.run_until_parked();
            if condition(vcx) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("wait_until: condition did not become true within 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[gpui::test]
    fn panel_new_clamps_active_index_into_range(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            99, // out of range -- must clamp, not panic
            |panel, vcx| {
                panel.read_with(vcx, |panel, _| {
                    assert_eq!(panel.active, 1, "clamped to the last real tab");
                    assert_eq!(panel.tabs.len(), 2);
                });
            },
        );
    }

    #[gpui::test]
    fn new_tab_duplicates_active_dir_and_becomes_active(cx: &mut TestAppContext) {
        let (dir_a, _dir_b) = two_dirs();
        with_panel(
            cx,
            vec![session_tab(dir_a.path().to_path_buf(), false, false)],
            0,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.new_tab(window, cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs.len(), 2);
                    assert_eq!(panel.active, 1);
                    assert_eq!(
                        panel.tabs[1].table.read(cx).current_dir(),
                        dir_a.path(),
                        "Ctrl+T duplicates the active tab's directory"
                    );
                    assert!(
                        !panel.tabs[1].locked,
                        "a plain new tab is never locked, even if the source tab was"
                    );
                });
            },
        );
    }

    /// UAT regression: a brand-new tab must start with the *same* column
    /// widths an already-measured sibling in the same panel has, not the
    /// narrow default `FileTableDelegate::new` otherwise starts every
    /// table at. Without seeding, the new tab renders one narrow-Name-
    /// column frame that only self-corrects once some unrelated later
    /// action happens to trigger a repaint (cursor movement, another
    /// navigation, ...) -- exactly what was reported.
    #[gpui::test]
    fn new_tab_seeds_column_widths_from_an_already_measured_sibling(cx: &mut TestAppContext) {
        let (dir_a, _dir_b) = two_dirs();
        with_panel(
            cx,
            vec![session_tab(dir_a.path().to_path_buf(), false, false)],
            0,
            |panel, vcx| {
                // A real paint, so the first tab's canvas-based width
                // measurement actually runs once -- mirrors what happens
                // on the real window's first frame.
                let _ = vcx.update(|window, cx| window.draw(cx));

                let seed_before = panel.read_with(vcx, |panel, cx| {
                    panel.tabs[0].table.read(cx).responsive_seed(cx)
                });
                assert!(
                    seed_before.is_some(),
                    "the first tab must be measured after a real paint"
                );

                panel.update_in(vcx, |panel, window, cx| panel.new_tab(window, cx));

                let (seed_a, seed_b) = panel.read_with(vcx, |panel, cx| {
                    (
                        panel.tabs[0].table.read(cx).responsive_seed(cx),
                        panel.tabs[1].table.read(cx).responsive_seed(cx),
                    )
                });
                assert_eq!(
                    seed_a, seed_b,
                    "the new tab must start seeded with its sibling's already-measured \
                     widths, not the narrow default"
                );
            },
        );
    }

    #[gpui::test]
    fn close_active_removes_tab_and_never_drops_below_one(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            1,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.close_active(window, cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs.len(), 1);
                    assert_eq!(panel.tabs[0].table.read(cx).current_dir(), dir_a.path());
                });
                // Closing the last remaining tab must be a no-op.
                panel.update_in(vcx, |panel, window, cx| panel.close_active(window, cx));
                panel.read_with(vcx, |panel, _| {
                    assert_eq!(panel.tabs.len(), 1, "never closes the last tab");
                });
            },
        );
    }

    #[gpui::test]
    fn next_tab_and_prev_tab_wrap_around(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            0,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.next_tab(window, cx));
                panel.read_with(vcx, |panel, _| assert_eq!(panel.active, 1));
                panel.update_in(vcx, |panel, window, cx| panel.next_tab(window, cx));
                panel.read_with(vcx, |panel, _| {
                    assert_eq!(panel.active, 0, "wraps from the last tab back to the first")
                });
                panel.update_in(vcx, |panel, window, cx| panel.prev_tab(window, cx));
                panel.read_with(vcx, |panel, _| {
                    assert_eq!(panel.active, 1, "wraps from the first tab back to the last")
                });
            },
        );
    }

    /// UAT regression: switching tabs (by any means -- a new tab, closing
    /// one, next/prev, or a tab-strip click) must move real keyboard focus
    /// onto the newly-active tab's `FileTable`. Without that, focus is
    /// left pointing at whichever tab held it before the switch -- for a
    /// tab that's no longer the one being rendered, an element outside the
    /// current dispatch tree, so no key press reaches anything and (per
    /// the report) *neither* panel reads as active any more. Checked via
    /// real `FocusHandle::is_focused`, not just `panel.active`'s index --
    /// the index alone wouldn't have caught this bug at all.
    #[gpui::test]
    fn switching_tabs_moves_real_keyboard_focus_to_the_new_active_tab(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            0,
            |panel, vcx| {
                let first_handle = panel.read_with(vcx, |panel, cx| panel.active_focus_handle(cx));
                vcx.update(|window, _cx| window.focus(&first_handle));
                let _ = vcx.update(|window, cx| window.draw(cx));
                vcx.update(|window, _cx| assert!(first_handle.is_focused(window)));

                panel.update_in(vcx, |panel, window, cx| panel.next_tab(window, cx));
                let _ = vcx.update(|window, cx| window.draw(cx));

                let second_handle = panel.read_with(vcx, |panel, cx| panel.active_focus_handle(cx));
                vcx.update(|window, _cx| {
                    assert!(
                        second_handle.is_focused(window),
                        "the newly-active tab must hold real keyboard focus after switching"
                    );
                    assert!(
                        !first_handle.is_focused(window),
                        "the previous tab must not still hold focus"
                    );
                });

                // And switching back must move focus back, not leave it
                // stranded on the tab that was active in between.
                panel.update_in(vcx, |panel, window, cx| panel.prev_tab(window, cx));
                let _ = vcx.update(|window, cx| window.draw(cx));
                vcx.update(|window, _cx| assert!(first_handle.is_focused(window)));
            },
        );
    }

    #[gpui::test]
    fn duplicate_active_copies_lock_flags(cx: &mut TestAppContext) {
        let (dir_a, _dir_b) = two_dirs();
        with_panel(
            cx,
            vec![session_tab(dir_a.path().to_path_buf(), true, true)],
            0,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.duplicate_active(window, cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs.len(), 2);
                    assert_eq!(panel.active, 1);
                    assert!(panel.tabs[1].locked);
                    assert!(panel.tabs[1].lock_dir_change);
                    assert_eq!(panel.tabs[1].table.read(cx).current_dir(), dir_a.path());
                });
            },
        );
    }

    #[gpui::test]
    fn toggle_lock_and_lock_dir_change_semantics(cx: &mut TestAppContext) {
        let (dir_a, _dir_b) = two_dirs();
        with_panel(
            cx,
            vec![session_tab(dir_a.path().to_path_buf(), false, false)],
            0,
            |panel, vcx| {
                panel.update_in(vcx, |panel, _window, cx| panel.toggle_lock(cx));
                panel.read_with(vcx, |panel, _| {
                    assert!(panel.tabs[0].locked);
                    assert!(!panel.tabs[0].lock_dir_change);
                });

                // Turning lock_dir_change on implies locked (TC semantics:
                // it's a kind of locked tab, not independent).
                panel.update_in(vcx, |panel, _window, cx| panel.toggle_lock(cx)); // unlock first
                panel.update_in(vcx, |panel, _window, cx| panel.toggle_lock_dir_change(cx));
                panel.read_with(vcx, |panel, _| {
                    assert!(panel.tabs[0].locked, "lock_dir_change implies locked");
                    assert!(panel.tabs[0].lock_dir_change);
                });

                // Unlocking clears lock_dir_change too -- it means nothing
                // on an unlocked tab.
                panel.update_in(vcx, |panel, _window, cx| panel.toggle_lock(cx));
                panel.read_with(vcx, |panel, _| {
                    assert!(!panel.tabs[0].locked);
                    assert!(!panel.tabs[0].lock_dir_change);
                });
            },
        );
    }

    #[gpui::test]
    fn close_others_then_reopen_closed_round_trips_dir_and_lock_flags(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), true, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            1,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.close_others(window, cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs.len(), 1);
                    assert_eq!(panel.active, 0);
                    assert_eq!(
                        panel.tabs[0].table.read(cx).current_dir(),
                        dir_b.path(),
                        "the active tab (dir_b) is the one kept"
                    );
                });

                panel.update_in(vcx, |panel, window, cx| panel.reopen_closed(window, cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs.len(), 2);
                    assert_eq!(panel.active, 1);
                    assert_eq!(panel.tabs[1].table.read(cx).current_dir(), dir_a.path());
                    assert!(
                        panel.tabs[1].locked,
                        "reopening restores the closed tab's lock flag"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn move_active_left_and_right_reorders_tabs(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            0,
            |panel, vcx| {
                // Already leftmost: a no-op.
                panel.update_in(vcx, |panel, _window, cx| panel.move_active_left(cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.tabs[0].table.read(cx).current_dir(), dir_a.path());
                    assert_eq!(panel.active, 0);
                });

                panel.update_in(vcx, |panel, _window, cx| panel.move_active_right(cx));
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(panel.active, 1);
                    assert_eq!(panel.tabs[0].table.read(cx).current_dir(), dir_b.path());
                    assert_eq!(panel.tabs[1].table.read(cx).current_dir(), dir_a.path());
                });

                // Already rightmost: a no-op.
                panel.update_in(vcx, |panel, _window, cx| panel.move_active_right(cx));
                panel.read_with(vcx, |panel, _| assert_eq!(panel.active, 1));
            },
        );
    }

    #[gpui::test]
    fn switch_to_out_of_range_is_a_no_op(cx: &mut TestAppContext) {
        let (dir_a, dir_b) = two_dirs();
        with_panel(
            cx,
            vec![
                session_tab(dir_a.path().to_path_buf(), false, false),
                session_tab(dir_b.path().to_path_buf(), false, false),
            ],
            0,
            |panel, vcx| {
                panel.update_in(vcx, |panel, window, cx| panel.switch_to(50, window, cx));
                panel.read_with(vcx, |panel, _| assert_eq!(panel.active, 0));
                panel.update_in(vcx, |panel, window, cx| panel.switch_to(1, window, cx));
                panel.read_with(vcx, |panel, _| assert_eq!(panel.active, 1));
            },
        );
    }

    /// The end-to-end case: a locked tab (without `lock_dir_change`) that
    /// receives a real dispatched navigation action must not move at all --
    /// instead a brand-new, unlocked tab opens at the target directory and
    /// becomes active. Drives this through a real focused `FileTable` and
    /// a real dispatched `NavigateRoot` action (`Ctrl+\`'s handler), not a
    /// direct method call -- this is specifically what TC's "locked tab"
    /// affordance means, and the part of this feature most worth proving
    /// actually wired up correctly end to end.
    #[gpui::test]
    fn locked_tab_navigation_opens_a_new_unlocked_tab_instead(cx: &mut TestAppContext) {
        let (dir_a, _dir_b) = two_dirs();
        with_panel(
            cx,
            vec![session_tab(dir_a.path().to_path_buf(), true, false)],
            0,
            |panel, vcx| {
                panel.read_with(vcx, |panel, _| assert!(panel.tabs[0].locked));

                let handle = panel.read_with(vcx, |panel, cx| panel.active_focus_handle(cx));
                vcx.update(|window, _cx| window.focus(&handle));
                let _ = vcx.update(|window, cx| window.draw(cx));

                vcx.dispatch_action(NavigateRoot);
                vcx.run_until_parked();

                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(
                        panel.tabs.len(),
                        2,
                        "the locked tab's navigation attempt must open a new tab, not just \
                         mutate the locked one"
                    );
                    assert_eq!(
                        panel.tabs[0].table.read(cx).current_dir(),
                        dir_a.path(),
                        "the locked tab itself must never move"
                    );
                    assert!(panel.tabs[0].locked, "the locked tab stays locked");
                    assert_eq!(panel.active, 1, "the new tab becomes active");
                    assert_eq!(
                        panel.tabs[1].table.read(cx).current_dir(),
                        std::path::Path::new("/"),
                        "the new tab lands at the target the locked tab tried to reach"
                    );
                    assert!(!panel.tabs[1].locked, "the redirected new tab is unlocked");
                });
            },
        );
    }

    /// T-4.3.7's session-restore path end to end: a tab built from a
    /// `SessionTab` with `cursor_name`/`sort_column`/`sort_ascending` set
    /// must, once its (real, async) directory listing finishes loading,
    /// land the cursor on the named entry and sort by the saved column
    /// and direction -- not just carry those values around unapplied.
    #[gpui::test]
    fn session_restored_cursor_and_sort_apply_once_the_listing_loads(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("b_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("c_file.txt"), "").unwrap();

        with_panel(
            cx,
            vec![SessionTab {
                dir: dir.path().to_path_buf(),
                locked: false,
                lock_dir_change: false,
                cursor_name: Some("b_file.txt".to_string()),
                sort_column: SessionSortColumn::Name,
                sort_ascending: false,
            }],
            0,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                panel.read_with(vcx, |panel, cx| {
                    let table = panel.tabs[0].table.read(cx);
                    assert_eq!(
                        table.cursor_entry_name(cx),
                        Some("b_file.txt".to_string()),
                        "cursor must land on the saved entry once the listing loads"
                    );
                    assert_eq!(
                        table.sort_state(cx),
                        (SortColumn::Name, false),
                        "sort must match what was saved, not the Name-ascending default"
                    );
                });
            },
        );
    }

    /// The graceful-miss half of the same path: a saved cursor name that
    /// no longer exists in the directory (renamed/deleted since the
    /// session was saved) must fall back to row 0, never panic or leave
    /// the cursor unset.
    #[gpui::test]
    fn session_restored_cursor_falls_back_to_row_zero_when_the_named_entry_is_gone(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("z_file.txt"), "").unwrap();

        with_panel(
            cx,
            vec![SessionTab {
                dir: dir.path().to_path_buf(),
                locked: false,
                lock_dir_change: false,
                cursor_name: Some("this_file_no_longer_exists.txt".to_string()),
                sort_column: SessionSortColumn::Name,
                sort_ascending: true,
            }],
            0,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                panel.read_with(vcx, |panel, cx| {
                    let table = panel.tabs[0].table.read(cx);
                    assert_eq!(
                        table.cursor_entry_name(cx),
                        Some("a_file.txt".to_string()),
                        "falls back to row 0 (name-ascending: a_file.txt) rather than \
                         leaving the cursor unset"
                    );
                });
            },
        );
    }

    /// Sort persists across ordinary navigation within a tab, not just
    /// across a restart -- `FileTable::navigate_to` reads the *current*
    /// sort back out of the model instead of resetting to the
    /// Name-ascending default on every directory change (otherwise,
    /// restored sort state would revert the moment the user navigated
    /// anywhere after a restore).
    #[gpui::test]
    fn sort_persists_across_navigation_within_a_tab(cx: &mut TestAppContext) {
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("x_file.txt"), "").unwrap();

        with_panel(
            cx,
            vec![session_tab(parent.path().to_path_buf(), false, false)],
            0,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });

                // Sort descending by Size via a real dispatched click-
                // equivalent isn't available headlessly without a real
                // header-click simulation, so drive the same effect
                // `perform_sort` produces directly through the model --
                // this is exercising `navigate_to`'s carry-forward
                // behavior, not header-click dispatch (already covered
                // elsewhere).
                panel.update_in(vcx, |panel, _window, cx| {
                    panel.tabs[0].table.update(cx, |table, cx| {
                        table.state().update(cx, |state, cx| {
                            state
                                .delegate_mut()
                                .model_mut()
                                .sort_by(SortColumn::Size, false);
                            cx.notify();
                        });
                    });
                });
                panel.read_with(vcx, |panel, cx| {
                    assert_eq!(
                        panel.tabs[0].table.read(cx).sort_state(cx),
                        (SortColumn::Size, false)
                    );
                });

                // Navigate into a subdirectory -- the new listing must
                // still be sorted by Size descending, not reset to Name
                // ascending.
                let handle = panel.read_with(vcx, |panel, cx| panel.active_focus_handle(cx));
                vcx.update(|window, _cx| window.focus(&handle));
                let _ = vcx.update(|window, cx| window.draw(cx));
                vcx.dispatch_action(EnterDirectory);
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).current_dir() == child
                    })
                });

                panel.read_with(vcx, |panel, cx| {
                    let table = panel.tabs[0].table.read(cx);
                    assert_eq!(table.current_dir(), child, "sanity: actually navigated");
                    assert_eq!(
                        table.sort_state(cx),
                        (SortColumn::Size, false),
                        "sort must carry forward across navigation, not reset"
                    );
                });
            },
        );
    }

    // -- T-4.3.8 mouse support -----------------------------------------
    //
    // `FileTable::handle_left_click`/the `TableEvent` subscription it's
    // wired through are private to `file_table.rs` (same-crate,
    // different-module privacy) -- unreachable directly from here. These
    // tests instead emit the exact `TableEvent` a real click would
    // (`Table`'s own `on_row_left_click`/state.rs emits precisely
    // `SelectRow`/`DoubleClickedRow` with a display-row index, confirmed
    // by reading `gpui-component-0.5.1/src/table/state.rs`) straight onto
    // `FileTable::state()` (`pub`), which drives the real production
    // subscription exactly as a genuine click would -- only the upstream
    // library's own pixel-to-row hit-testing goes unexercised, which is
    // its concern, not this codebase's. `vcx.simulate_modifiers_change`
    // sets the *real* `Window::modifiers()` T-4.3.8's click handler reads
    // live (a `TableEvent` itself carries none) through the same
    // `ModifiersChangedEvent` a real keyboard would send.

    fn emit_table_event(table: &Entity<FileTable>, event: TableEvent, vcx: &mut VisualTestContext) {
        let state = table.read_with(vcx, |table, _| table.state().clone());
        state.update(vcx, |_, cx| cx.emit(event));
        vcx.run_until_parked();
    }

    /// Every currently-selected entry's name, in `order()` (display)
    /// order -- `DirectoryModel::selection()`/`is_selected` are both
    /// `pub`, same as every other accessor these tests reach through.
    fn selected_names(table: &FileTable, cx: &App) -> Vec<String> {
        let delegate = table.state().read(cx).delegate();
        let model = delegate.model();
        model
            .order()
            .iter()
            .copied()
            .map(EntryId::new)
            .filter(|&id| model.is_selected(id))
            .map(|id| model.entries().name(id).to_string())
            .collect()
    }

    /// Whether `table`'s current listing has finished loading --
    /// `TableDelegate::loading` (a real trait method, not something
    /// mouse-support-specific) is a more general "is there anything real
    /// to click on yet" signal than `cursor_entry_name(cx).is_some()`,
    /// which stays `None` forever for a directory that loads with no
    /// entries at all (T-4.3.8's empty `child_dir` fixture).
    fn listing_loaded(table: &FileTable, cx: &App) -> bool {
        !table.state().read(cx).delegate().loading(cx)
    }

    /// Three real files, name-ascending order (`a_file.txt`,
    /// `b_file.txt`, `c_file.txt`) -- every mouse test's shared fixture.
    /// With `has_parent_row` (true for any non-root directory), display
    /// rows are `0=".."`, `1=a_file.txt`, `2=b_file.txt`, `3=c_file.txt`.
    fn three_file_dir() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("b_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("c_file.txt"), "").unwrap();
        dir
    }

    #[gpui::test]
    fn plain_click_in_windows_mode_selects_only_the_clicked_row(cx: &mut TestAppContext) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                vcx.simulate_modifiers_change(Modifiers::default());
                emit_table_event(&table, TableEvent::SelectRow(2), vcx); // b_file.txt

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("b_file.txt".to_string()));
                    assert_eq!(selected_names(table, cx), vec!["b_file.txt".to_string()]);
                });
            },
        );
    }

    #[gpui::test]
    fn plain_click_in_norton_mode_moves_the_cursor_but_never_selects(cx: &mut TestAppContext) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Norton,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                vcx.simulate_modifiers_change(Modifiers::default());
                emit_table_event(&table, TableEvent::SelectRow(3), vcx); // c_file.txt

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("c_file.txt".to_string()));
                    assert!(
                        selected_names(table, cx).is_empty(),
                        "Norton mode: a plain click never selects"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn plain_click_in_none_mode_moves_the_cursor_but_never_selects(cx: &mut TestAppContext) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::None,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                vcx.simulate_modifiers_change(Modifiers::default());
                emit_table_event(&table, TableEvent::SelectRow(1), vcx); // a_file.txt

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("a_file.txt".to_string()));
                    assert!(selected_names(table, cx).is_empty());
                });
            },
        );
    }

    #[gpui::test]
    fn shift_click_extends_selection_from_the_cursor_to_the_clicked_row(cx: &mut TestAppContext) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.cursor_entry_name(cx),
                        Some("a_file.txt".to_string()),
                        "sanity: a fresh listing starts the cursor on the first entry"
                    );
                });

                vcx.simulate_modifiers_change(Modifiers {
                    shift: true,
                    ..Default::default()
                });
                emit_table_event(&table, TableEvent::SelectRow(3), vcx); // c_file.txt

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("c_file.txt".to_string()));
                    let mut names = selected_names(table, cx);
                    names.sort();
                    assert_eq!(
                        names,
                        vec![
                            "a_file.txt".to_string(),
                            "b_file.txt".to_string(),
                            "c_file.txt".to_string(),
                        ],
                        "the whole range from the cursor's starting row to the clicked row"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn ctrl_click_toggles_only_the_clicked_row_leaving_other_selections_alone(
        cx: &mut TestAppContext,
    ) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());

                vcx.simulate_modifiers_change(Modifiers::default());
                emit_table_event(&table, TableEvent::SelectRow(2), vcx); // plain click b_file.txt

                vcx.simulate_modifiers_change(Modifiers {
                    control: true,
                    ..Default::default()
                });
                emit_table_event(&table, TableEvent::SelectRow(3), vcx); // ctrl+click c_file.txt

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("c_file.txt".to_string()));
                    let mut names = selected_names(table, cx);
                    names.sort();
                    assert_eq!(
                        names,
                        vec!["b_file.txt".to_string(), "c_file.txt".to_string()],
                        "ctrl+click adds to the existing selection, it never replaces it"
                    );
                });

                emit_table_event(&table, TableEvent::SelectRow(3), vcx); // ctrl+click c_file.txt again
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        selected_names(table, cx),
                        vec!["b_file.txt".to_string()],
                        "a second ctrl+click on the same row toggles it back off"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn clicking_the_parent_row_moves_the_cursor_and_never_touches_selection(
        cx: &mut TestAppContext,
    ) {
        let dir = three_file_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                wait_until(vcx, |vcx| {
                    panel.read_with(vcx, |panel, cx| {
                        panel.tabs[0].table.read(cx).cursor_entry_name(cx).is_some()
                    })
                });
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());

                vcx.simulate_modifiers_change(Modifiers::default());
                emit_table_event(&table, TableEvent::SelectRow(1), vcx); // select a_file.txt

                vcx.simulate_modifiers_change(Modifiers {
                    control: true,
                    ..Default::default()
                });
                emit_table_event(&table, TableEvent::SelectRow(0), vcx); // ctrl+click ".."

                table.read_with(vcx, |table, cx| {
                    let delegate = table.state().read(cx).delegate();
                    assert!(
                        delegate.cursor_on_parent(),
                        "clicking \"..\" must move the display cursor onto it"
                    );
                    assert_eq!(
                        selected_names(table, cx),
                        vec!["a_file.txt".to_string()],
                        "the \"..\" row has no entry to select -- selection must be untouched"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn double_click_on_a_directory_enters_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child_dir");
        std::fs::create_dir(&child).unwrap();

        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));
                emit_table_event(&table, TableEvent::DoubleClickedRow(1), vcx); // the only real entry

                wait_until(vcx, |vcx| {
                    table.read_with(vcx, |table, _| table.current_dir() == child)
                });
                table.read_with(vcx, |table, _| {
                    assert_eq!(
                        table.current_dir(),
                        child,
                        "double-click must descend into it"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn double_click_on_the_parent_row_navigates_up(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child_dir");
        std::fs::create_dir(&child).unwrap();

        with_panel_ext(
            cx,
            vec![session_tab(child.clone(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                // Must wait for the *load* to finish, not just for
                // `current_dir` to already read as `child` (that field is
                // set synchronously by `navigate_to`/`FileTable::new`,
                // before the background listing completes) --
                // `has_parent_row` (what makes row 0 the ".." row at all)
                // is only set once the async load actually applies.
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));
                emit_table_event(&table, TableEvent::DoubleClickedRow(0), vcx); // ".."

                let parent = dir.path().to_path_buf();
                wait_until(vcx, |vcx| {
                    table.read_with(vcx, |table, _| table.current_dir() == parent)
                });
            },
        );
    }

    /// T-4.3.8's middle-click "open in a new tab" -- exercises the real
    /// closure `add_tab_entry` installs (fetched via
    /// `FileTableDelegate::new_tab_handler`, a `#[cfg(test)]`-only
    /// accessor -- see its doc comment for why calling this directly,
    /// rather than simulating a real mouse click at some row's computed
    /// pixel position, is the right level to test this codebase's own
    /// logic at).
    #[gpui::test]
    fn middle_click_handler_opens_the_directorys_own_tab_and_makes_it_active(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("child_dir")).unwrap();

        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                let handler = table.read_with(vcx, |table, cx| {
                    table
                        .state()
                        .read(cx)
                        .delegate()
                        .new_tab_handler()
                        .expect("Panel::add_tab_entry always installs one")
                });
                vcx.update(|window, cx| handler("child_dir".to_string(), window, cx));
                vcx.run_until_parked();

                panel.read_with(vcx, |panel, _| {
                    assert_eq!(
                        panel.tabs.len(),
                        2,
                        "middle-click opens a new tab, it never replaces the current one"
                    );
                    assert_eq!(panel.active, 1, "the new tab becomes active");
                });
                let new_tab = panel.read_with(vcx, |panel, _| panel.tabs[1].table.clone());
                assert_eq!(
                    new_tab.read_with(vcx, |table, _| table.current_dir().to_path_buf()),
                    dir.path().join("child_dir"),
                    "must open the clicked entry's own directory, resolved against \
                     current_dir at click time"
                );
            },
        );
    }

    // -- T-4.3.3 quick search / quick filter ----------------------------

    /// Moves real keyboard focus onto `panel`'s active tab and forces a
    /// render pass -- `dispatch_action`/`simulate_input`/`simulate_keystrokes`
    /// all resolve against whatever the *last drawn frame's* dispatch
    /// tree says has focus, so a bare `window.focus(&handle)` with no
    /// follow-up `window.draw(cx)` isn't reliably picked up (same
    /// pattern `sort_persists_across_navigation_within_a_tab` already
    /// established for `dispatch_action(EnterDirectory)`).
    fn focus_active_table(panel: &Entity<Panel>, vcx: &mut VisualTestContext) {
        let handle = panel.read_with(vcx, |panel, cx| panel.active_focus_handle(cx));
        vcx.update(|window, _cx| window.focus(&handle));
        let _ = vcx.update(|window, cx| window.draw(cx));
    }

    /// Three real files, deliberately named so a fuzzy subsequence query
    /// for "gam" only plausibly matches `gamma.txt`.
    fn three_named_files_dir() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "").unwrap();
        std::fs::write(dir.path().join("gamma.txt"), "").unwrap();
        dir
    }

    #[gpui::test]
    fn typing_in_jump_mode_moves_the_cursor_to_the_best_fuzzy_match(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("gam");

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("gamma.txt".to_string()));
                    assert_eq!(
                        table.state().read(cx).delegate().quick_search_mode(),
                        Some(QuickSearchMode::Jump)
                    );
                });
            },
        );
    }

    /// Unlike "gam" (only ever matches `gamma.txt`), "a" is a fuzzy
    /// subsequence of all three fixture names -- exercising the actual
    /// score comparison, not just "there was exactly one candidate".
    /// `alpha.txt`'s leading "a" is expected to outscore `beta.txt`'s
    /// (3rd character) and `gamma.txt`'s (2nd character) under nucleo's
    /// own prefix-match bonus.
    #[gpui::test]
    fn typing_in_jump_mode_picks_the_highest_scoring_match_among_several(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("a");

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.cursor_entry_name(cx), Some("alpha.txt".to_string()));
                });
            },
        );
    }

    #[gpui::test]
    fn escape_exits_the_quick_search_regime(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("gam");
                table.read_with(vcx, |table, cx| {
                    assert!(
                        table
                            .state()
                            .read(cx)
                            .delegate()
                            .quick_search_mode()
                            .is_some()
                    );
                });

                vcx.simulate_keystrokes("escape");
                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.state().read(cx).delegate().quick_search_mode(), None);
                });
            },
        );
    }

    #[gpui::test]
    fn idle_timeout_exits_the_quick_search_regime(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("g");
                table.read_with(vcx, |table, cx| {
                    assert!(
                        table
                            .state()
                            .read(cx)
                            .delegate()
                            .quick_search_mode()
                            .is_some()
                    );
                });

                vcx.executor().advance_clock(Duration::from_millis(1300));
                vcx.run_until_parked();
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.state().read(cx).delegate().quick_search_mode(),
                        None,
                        "1300ms with no further keystroke must exceed the 1200ms default idle timeout"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn a_new_keystroke_resets_the_idle_timer(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("g");
                vcx.executor().advance_clock(Duration::from_millis(1000)); // < 1200ms
                vcx.run_until_parked();
                vcx.simulate_input("a"); // resets the timer

                vcx.executor().advance_clock(Duration::from_millis(1000)); // 2000ms since 1st keystroke
                vcx.run_until_parked();
                table.read_with(vcx, |table, cx| {
                    assert!(
                        table
                            .state()
                            .read(cx)
                            .delegate()
                            .quick_search_mode()
                            .is_some(),
                        "the 2nd keystroke's own timer (1000ms elapsed) must still be pending, \
                         even though 2000ms have passed since the 1st keystroke's stale timer \
                         would have fired"
                    );
                });

                vcx.executor().advance_clock(Duration::from_millis(300)); // 1300ms since 2nd
                vcx.run_until_parked();
                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.state().read(cx).delegate().quick_search_mode(), None);
                });
            },
        );
    }

    #[gpui::test]
    fn arrow_key_exits_jump_mode_but_not_filter_mode(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("g");
                vcx.dispatch_action(CursorDown);
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.state().read(cx).delegate().quick_search_mode(),
                        None,
                        "jump mode must exit on arrow-key movement (FR-NAV-13)"
                    );
                });

                vcx.simulate_keystrokes("ctrl-p");
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.state().read(cx).delegate().quick_search_mode(),
                        Some(QuickSearchMode::Filter)
                    );
                });
                vcx.dispatch_action(CursorDown);
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.state().read(cx).delegate().quick_search_mode(),
                        Some(QuickSearchMode::Filter),
                        "filter mode must NOT exit on arrow-key movement -- own judgment call, \
                         see QuickSearchMode's doc comment"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn ctrl_p_starts_filter_mode_and_narrows_the_listing(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_keystrokes("ctrl-p");
                vcx.simulate_input("ga");

                table.read_with(vcx, |table, cx| {
                    let delegate = table.state().read(cx).delegate();
                    assert_eq!(
                        delegate.model().order().len(),
                        1,
                        "only gamma.txt contains \"ga\""
                    );
                    assert_eq!(delegate.quick_search_mode(), Some(QuickSearchMode::Filter));
                });

                vcx.simulate_keystrokes("escape");
                table.read_with(vcx, |table, cx| {
                    assert_eq!(
                        table.state().read(cx).delegate().model().order().len(),
                        3,
                        "escape must restore the full, unfiltered listing"
                    );
                });
            },
        );
    }

    #[gpui::test]
    fn mouse_click_exits_the_quick_search_regime(cx: &mut TestAppContext) {
        let dir = three_named_files_dir();
        with_panel_ext(
            cx,
            vec![session_tab(dir.path().to_path_buf(), false, false)],
            0,
            MouseMode::Windows,
            |panel, vcx| {
                let table = panel.read_with(vcx, |panel, _| panel.tabs[0].table.clone());
                wait_until(vcx, |vcx| table.read_with(vcx, listing_loaded));

                focus_active_table(&panel, vcx);
                vcx.simulate_input("g");
                emit_table_event(&table, TableEvent::SelectRow(1), vcx);

                table.read_with(vcx, |table, cx| {
                    assert_eq!(table.state().read(cx).delegate().quick_search_mode(), None);
                });
            },
        );
    }

    // `losing_focus_exits_the_quick_search_regime` (FR-NAV-13's "panel
    // losing focus" exit condition) is intentionally not tested here --
    // see `FileTable::new`'s `on_focus_out` wiring for why: `Window`'s
    // real focus-change events only report a non-empty
    // `previous_focus_path` while `window_active` (the OS window's own
    // foreground state) is true, and the only way to simulate that in a
    // test is `TestWindow::simulate_active_status_change`, which is
    // `pub(crate)` *inside gpui itself* -- unreachable from `duet-ui`'s
    // own tests (confirmed by reading `gpui-0.2.2/src/platform/test/
    // window.rs` and `src/app/test_context.rs`; no public wrapper
    // exists). Verified instead that `window.on_focus_out` is the
    // documented, correct API for this exact purpose and that every
    // *other* exit condition (Escape, idle timeout, jump-mode cursor
    // movement, mouse click, Ctrl+P) is both implemented and tested
    // above -- this one is flagged honestly for live UAT instead.
}
