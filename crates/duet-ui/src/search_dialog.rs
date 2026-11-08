// SPDX-License-Identifier: MIT
//! T-5.3.7's Find Files dialog (Alt+F7 or Ctrl+F, `tool.search`, FR-TOOL-04): name
//! masks or a name regex, content search, size/date/kind filters, results
//! streamed in while the search runs, "go to file", and **feed to panel**.
//!
//! The search itself is `duet_search` (masks, content matcher, the
//! parallel walk -- see that crate's docs); this module is the form in
//! front of it and the list behind it.
//!
//! # Fields and toggles
//!
//! Eight text fields, in Tab order: *Search for* (a Total Commander mask
//! such as `*.c;*.h | *.bak .git/`, or with Alt+E a regex on the file
//! name), *Search in* (`;`-separated folders, pre-filled with the panel's
//! folder), *Containing* (text to find inside files; empty means no
//! content search), size from/to (`10k`, `1.5G`), modified after/before
//! (`2026-01-31`, `2026-01-31 14:00`, or an age: `7d`, `12h`), and a
//! depth limit (empty = unlimited, `0` = the folder itself only).
//!
//! The switches are `Alt+letter` chords, Total Commander's own dialog
//! accelerator style, each also clickable in the dialog:
//!
//! | Key | Switch |
//! |---|---|
//! | Alt+E | name is a regular expression |
//! | Alt+X | text is a regular expression |
//! | Alt+V | case sensitive (names and text) |
//! | Alt+W | whole words (text) |
//! | Alt+O | find files *not* containing the text |
//! | Ctrl+E | text encoding (UTF-8, +UTF-16, UTF-16, windows-1252/1251/1255, KOI8-R) -- the viewer's key |
//! | Alt+B | skip binary files |
//! | Alt+K | files and directories / files only / directories only |
//! | Alt+H | include hidden entries |
//! | Alt+Y | follow symlinks |
//! | Alt+M | stay on one filesystem |
//! | Alt+L | feed the results to the panel (TC's "Feed to listbox", same accelerator) |
//!
//! None of these chords is claimed by the text input's own key context
//! (`gpui-component`'s `InputState` binds only Alt+arrows/Backspace/
//! Delete on Linux), so they work with a field focused, which is how they
//! are used. They are Duet's own choices except Alt+L; `docs/keymap-tc.csv`
//! has no rows for the dialog's internals.
//!
//! # Keyboard model
//!
//! Enter in any field starts the search (a running one is replaced).
//! Escape stops a running search, or closes the dialog when none runs.
//! Tab/Shift+Tab walk the fields and then the result list -- caught as the
//! `IndentInline`/`OutdentInline` actions a single-line `InputState`
//! propagates, the way `conflict_dialog` does it. In the list,
//! Up/Down/PageUp/PageDown/Home/End move, and Enter goes to the result:
//! the dialog closes and the panel opens the result's folder with the
//! cursor on it (TC's "Go to file"). When a search finishes with results
//! while the focus is still in a field, the focus moves to the list, as
//! in TC.
//!
//! # Streaming
//!
//! `duet_search::start` runs the walk on its own threads and calls a sink
//! from them; the sink forwards each event into a Tokio unbounded channel
//! that one `cx.spawn` task drains, applying everything queued since its
//! last wake-up in one update (so a burst of hit batches costs one
//! re-render, not one per batch). The UI thread never touches the
//! filesystem. Each search gets its own channel and a generation number,
//! so nothing from a replaced search can land in the next one's list;
//! dropping the `SearchHandle` (a new search, Escape, closing the dialog)
//! cancels the walk.
//!
//! # Feed to panel
//!
//! Alt+L stops a running search and hands what has been found to
//! `Workspace::feed_search_results`, which shows it in the panel the
//! dialog was opened from as a synthetic listing (`file_table::FedListing`):
//! every entry named by its path relative to the search root, so the
//! listing supports the whole selection and operation model -- F5/F6/F8,
//! Enter, F3, Ctrl+Q, the clipboard -- with no second code path.
//!
//! # What persists
//!
//! The fields (except *Search in*, which always starts at the panel's
//! folder) and switches survive closing and reopening the dialog within a
//! session ([`SearchMemory`]), so refining a search is one Alt+F7 away.
//! The results do not, as in TC.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duet_search::{
    ContentEncodings, ContentSpec, KindFilter, SearchEvent, SearchHandle, SearchHit,
    SearchProgress, SearchSink, SearchSpec, parse_roots, parse_size, parse_time, relative_name,
};
use duet_types::EntryKind;
use duet_widgets::input::{Escape, IndentInline, Input, InputEvent, InputState, OutdentInline};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, WeakEntity, Window, actions, div, px, uniform_list,
};

use crate::dialog_job::focus_at_end;
use crate::file_table::{write_date, write_size};
use crate::workspace::Workspace;

actions!(
    duet_search_dialog,
    [
        SearchToggleNameRegex,
        SearchToggleTextRegex,
        SearchToggleCase,
        SearchToggleWholeWords,
        SearchToggleNotContaining,
        SearchCycleEncoding,
        SearchToggleSkipBinary,
        SearchCycleKind,
        SearchToggleHidden,
        SearchToggleSymlinks,
        SearchToggleOneFilesystem,
        SearchFeedToPanel,
        SearchResultsUp,
        SearchResultsDown,
        SearchResultsPageUp,
        SearchResultsPageDown,
        SearchResultsHome,
        SearchResultsEnd,
        SearchResultsOpen,
        SearchResultsEscape,
        SearchFocusNext,
        SearchFocusPrev,
    ]
);

/// Rows the result list shows at once (it is a fixed-height viewport);
/// also what PageUp/PageDown move by.
const RESULT_ROWS: usize = 14;
const ROW_HEIGHT: f32 = 20.0;

/// Registers the dialog's chords -- see the module doc comment's table.
/// Called once from `workspace::run`.
pub(crate) fn bind_search_dialog_keys(cx: &mut App) {
    const DIALOG: Option<&str> = Some("SearchDialog");
    const LIST: Option<&str> = Some("SearchResults");
    cx.bind_keys([
        KeyBinding::new("alt-e", SearchToggleNameRegex, DIALOG),
        KeyBinding::new("alt-x", SearchToggleTextRegex, DIALOG),
        KeyBinding::new("alt-v", SearchToggleCase, DIALOG),
        KeyBinding::new("alt-w", SearchToggleWholeWords, DIALOG),
        KeyBinding::new("alt-o", SearchToggleNotContaining, DIALOG),
        KeyBinding::new("ctrl-e", SearchCycleEncoding, DIALOG),
        KeyBinding::new("alt-b", SearchToggleSkipBinary, DIALOG),
        KeyBinding::new("alt-k", SearchCycleKind, DIALOG),
        KeyBinding::new("alt-h", SearchToggleHidden, DIALOG),
        KeyBinding::new("alt-y", SearchToggleSymlinks, DIALOG),
        KeyBinding::new("alt-m", SearchToggleOneFilesystem, DIALOG),
        KeyBinding::new("alt-l", SearchFeedToPanel, DIALOG),
        KeyBinding::new("up", SearchResultsUp, LIST),
        KeyBinding::new("down", SearchResultsDown, LIST),
        KeyBinding::new("pageup", SearchResultsPageUp, LIST),
        KeyBinding::new("pagedown", SearchResultsPageDown, LIST),
        KeyBinding::new("home", SearchResultsHome, LIST),
        KeyBinding::new("end", SearchResultsEnd, LIST),
        KeyBinding::new("enter", SearchResultsOpen, LIST),
        KeyBinding::new("escape", SearchResultsEscape, LIST),
        KeyBinding::new("tab", SearchFocusNext, LIST),
        KeyBinding::new("shift-tab", SearchFocusPrev, LIST),
    ]);
}

/// The dialog's switches -- see the module doc comment's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchOptions {
    pub(crate) name_regex: bool,
    pub(crate) text_regex: bool,
    pub(crate) case_sensitive: bool,
    pub(crate) whole_words: bool,
    pub(crate) not_containing: bool,
    pub(crate) encodings: ContentEncodings,
    pub(crate) skip_binary: bool,
    pub(crate) kind: KindFilter,
    pub(crate) include_hidden: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) one_filesystem: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            name_regex: false,
            text_regex: false,
            case_sensitive: false,
            whole_words: false,
            not_containing: false,
            encodings: ContentEncodings::Utf8,
            skip_binary: false,
            kind: KindFilter::Any,
            // TC finds hidden files too; a search that silently skipped
            // every dotfile would read as "not found".
            include_hidden: true,
            follow_symlinks: false,
            one_filesystem: false,
        }
    }
}

/// The text fields, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchFields {
    pub(crate) name: String,
    pub(crate) roots: String,
    pub(crate) content: String,
    pub(crate) size_min: String,
    pub(crate) size_max: String,
    pub(crate) after: String,
    pub(crate) before: String,
    pub(crate) depth: String,
}

impl Default for SearchFields {
    fn default() -> Self {
        Self {
            name: "*".to_string(),
            roots: String::new(),
            content: String::new(),
            size_min: String::new(),
            size_max: String::new(),
            after: String::new(),
            before: String::new(),
            depth: String::new(),
        }
    }
}

/// What reopening the dialog restores -- see the module doc comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SearchMemory {
    pub(crate) fields: SearchFields,
    pub(crate) options: SearchOptions,
}

/// The fields and switches as a [`SearchSpec`], or a message saying what
/// is wrong with them. `base` resolves relative folders (the panel's
/// folder), `home` expands `~`, `now` anchors ages like `7d`.
pub(crate) fn build_spec(
    fields: &SearchFields,
    options: &SearchOptions,
    base: &Path,
    home: Option<&Path>,
    now: i64,
) -> Result<SearchSpec, String> {
    let roots = parse_roots(&fields.roots, base, home);
    if roots.is_empty() {
        return Err("Enter a folder to search in.".to_string());
    }
    let content = (!fields.content.is_empty()).then(|| ContentSpec {
        text: fields.content.clone(),
        regex: options.text_regex,
        case_sensitive: options.case_sensitive,
        whole_words: options.whole_words,
        not_containing: options.not_containing,
        encodings: options.encodings,
        skip_binary: options.skip_binary,
    });
    let depth = match fields.depth.trim() {
        "" => None,
        text => Some(
            text.parse::<u32>()
                .map_err(|_| format!("Depth \u{201c}{text}\u{201d} is not a whole number."))?,
        ),
    };
    let name = fields.name.trim();
    Ok(SearchSpec {
        roots,
        name: if name.is_empty() { "*" } else { name }.to_string(),
        name_regex: options.name_regex,
        name_case_sensitive: options.case_sensitive,
        content,
        size_min: parse_size(&fields.size_min)?,
        size_max: parse_size(&fields.size_max)?,
        modified_after: parse_time(&fields.after, now)?,
        modified_before: parse_time(&fields.before, now)?,
        kind: options.kind,
        include_hidden: options.include_hidden,
        follow_symlinks: options.follow_symlinks,
        one_filesystem: options.one_filesystem,
        max_depth: depth,
    })
}

/// How a fed listing describes itself in the panel header and tab:
/// `*.rs`, `/^img_\d+/`, `*.rs containing "todo"`.
pub(crate) fn listing_label(spec: &SearchSpec) -> String {
    let mut label = if spec.name_regex {
        format!("/{}/", spec.name)
    } else {
        spec.name.clone()
    };
    if let Some(content) = &spec.content {
        let verb = if content.not_containing {
            "not containing"
        } else {
            "containing"
        };
        label.push_str(&format!(" {verb} \u{201c}{}\u{201d}", content.text));
    }
    label
}

/// The folder results are named relative to: the root itself, or the
/// deepest folder every root is under.
pub(crate) fn common_root(roots: &[PathBuf]) -> PathBuf {
    let Some(first) = roots.first() else {
        return PathBuf::from("/");
    };
    let mut common = first.clone();
    for root in &roots[1..] {
        while !root.starts_with(&common) {
            if !common.pop() {
                return PathBuf::from("/");
            }
        }
    }
    common
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Running,
    Done { elapsed: Duration, cancelled: bool },
}

/// Which of the dialog's focus stops has the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Field(usize),
    Results,
}

/// The dialog. `Workspace::open_search_dialog` is the only constructor
/// call site.
pub(crate) struct SearchDialogState {
    /// Tab order: name, roots, content, size from, size to, after,
    /// before, depth.
    inputs: [Entity<InputState>; 8],
    options: SearchOptions,
    /// The panel's folder: what relative *Search in* entries mean.
    base: PathBuf,
    results: Vec<SearchHit>,
    /// What [`Self::results`] are named relative to, for this search.
    results_root: PathBuf,
    /// The search the results came from, for the fed listing's label.
    results_label: String,
    cursor: usize,
    results_focus: FocusHandle,
    scroll: UniformListScrollHandle,
    phase: Phase,
    progress: SearchProgress,
    /// The first error the walk reported (a folder it could not read...).
    first_error: Option<String>,
    /// What is wrong with the fields, shown until the next start.
    form_error: Option<String>,
    handle: Option<SearchHandle>,
    /// Bumped per search; events from an older one are dropped.
    generation: u64,
    started: Option<Instant>,
    /// Set when a search finishes with results; the next render moves the
    /// focus to the list if it is still in a field (TC's behaviour).
    pending_focus_results: bool,
    /// Whether the list had the focus at the start of this render --
    /// `render_row` paints the cursor row by it.
    results_focus_is_focused: bool,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl SearchDialogState {
    pub(crate) fn new(
        memory: SearchMemory,
        base: PathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = SearchFields {
            roots: base.to_string_lossy().into_owned(),
            ..memory.fields
        };
        let make = |value: String,
                    placeholder: &'static str,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
            })
        };
        let inputs = [
            make(fields.name, "*.txt *.md  |  *.bak .git/", window, cx),
            make(fields.roots, "Folder(s), separated by ;", window, cx),
            make(
                fields.content,
                "Text to find inside files (optional)",
                window,
                cx,
            ),
            make(fields.size_min, "e.g. 10k", window, cx),
            make(fields.size_max, "e.g. 2G", window, cx),
            make(fields.after, "2026-01-31 or 7d", window, cx),
            make(fields.before, "2026-12-31 14:00", window, cx),
            make(fields.depth, "all", window, cx),
        ];
        let _subscriptions = inputs
            .iter()
            .map(|input| cx.subscribe_in(input, window, Self::on_input_event))
            .collect();
        inputs[0].update(cx, |state, cx| focus_at_end(state, window, cx));
        Self {
            inputs,
            options: memory.options,
            base,
            results: Vec::new(),
            results_root: PathBuf::new(),
            results_label: String::new(),
            cursor: 0,
            results_focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            phase: Phase::Idle,
            progress: SearchProgress::default(),
            first_error: None,
            form_error: None,
            handle: None,
            generation: 0,
            started: None,
            pending_focus_results: false,
            results_focus_is_focused: false,
            workspace,
            _subscriptions,
        }
    }

    fn fields(&self, cx: &App) -> SearchFields {
        let value = |ix: usize| self.inputs[ix].read(cx).value().to_string();
        SearchFields {
            name: value(0),
            roots: value(1),
            content: value(2),
            size_min: value(3),
            size_max: value(4),
            after: value(5),
            before: value(6),
            depth: value(7),
        }
    }

    /// What reopening the dialog should restore.
    pub(crate) fn memory(&self, cx: &App) -> SearchMemory {
        SearchMemory {
            fields: self.fields(cx),
            options: self.options,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    #[cfg(test)]
    pub(crate) fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// Test-only: puts the focus back in the name field, where Enter
    /// starts a search (a finished search leaves it in the list).
    #[cfg(test)]
    pub(crate) fn focus_name_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_stop(Stop::Field(0), window, cx);
    }

    #[cfg(test)]
    pub(crate) fn form_error(&self) -> Option<&str> {
        self.form_error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn options_mut(&mut self) -> &mut SearchOptions {
        &mut self.options
    }

    /// Test-only: sets a field the way typing would (see
    /// `MkdirDialogState::set_destination_value` for why tests don't
    /// simulate keystrokes into a focused input).
    #[cfg(test)]
    pub(crate) fn set_field(&self, ix: usize, value: &str, window: &mut Window, cx: &mut App) {
        self.inputs[ix].update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx)
        });
    }

    fn on_input_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.start(window, cx);
        }
    }

    /// Enter: (re)starts the search from the fields. See the module doc
    /// comment's "Streaming" section.
    pub(crate) fn start(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let spec = build_spec(
            &self.fields(cx),
            &self.options,
            &self.base,
            home.as_deref(),
            now,
        )
        .and_then(|spec| spec.compile().map(|c| (spec, c)).map_err(|e| e.to_string()));
        let (spec, compiled) = match spec {
            Ok(pair) => pair,
            Err(message) => {
                self.form_error = Some(message);
                cx.notify();
                return;
            }
        };

        self.handle = None; // cancels a running search
        self.generation += 1;
        let generation = self.generation;
        self.form_error = None;
        self.first_error = None;
        self.results.clear();
        self.results_root = common_root(&spec.roots);
        self.results_label = listing_label(&spec);
        self.cursor = 0;
        self.progress = SearchProgress::default();
        self.phase = Phase::Running;
        self.started = Some(Instant::now());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SearchEvent>();
        let sink: SearchSink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        self.handle = Some(duet_search::start(compiled, sink));
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let mut batch = vec![event];
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                let current = this
                    .update(cx, |this, cx| this.apply_events(generation, batch, cx))
                    .unwrap_or(false);
                if !current {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Applies one drained batch. `false` once `generation` is stale (the
    /// draining task then stops).
    fn apply_events(
        &mut self,
        generation: u64,
        events: Vec<SearchEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        for event in events {
            match event {
                SearchEvent::Hits(hits) => self.results.extend(hits),
                SearchEvent::Progress(progress) => self.progress = progress,
                SearchEvent::Error { path, message } => {
                    if self.first_error.is_none() {
                        self.first_error = Some(format!("{}: {message}", path.display()));
                    }
                }
                SearchEvent::Finished {
                    progress,
                    elapsed,
                    cancelled,
                } => {
                    self.progress = progress;
                    self.phase = Phase::Done { elapsed, cancelled };
                    self.handle = None;
                    if !self.results.is_empty() {
                        self.pending_focus_results = true;
                    }
                }
            }
        }
        cx.notify();
        true
    }

    /// Escape: stops a running search, or closes the dialog.
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            self.stop(cx);
            return;
        }
        let memory = self.memory(cx);
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_search_dialog(memory, window, cx);
        });
    }

    /// Cancels the running search; what has been found stays listed.
    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
        if self.is_running() {
            self.phase = Phase::Done {
                elapsed: self.started.map_or(Duration::ZERO, |s| s.elapsed()),
                cancelled: true,
            };
            // The walk's own `Finished` would arrive after this; nothing
            // from it is needed any more.
            self.generation += 1;
        }
        cx.notify();
    }

    /// Alt+L: hands the results to the panel. See the module doc comment.
    pub(crate) fn feed_to_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            self.form_error = Some("Nothing to feed to the panel yet.".to_string());
            cx.notify();
            return;
        }
        self.stop(cx);
        let root = self.results_root.clone();
        let entries: Vec<(String, duet_types::Metadata)> = self
            .results
            .iter()
            .map(|hit| (relative_name(&root, &hit.path), hit.metadata.clone()))
            .collect();
        let label = self.results_label.clone();
        let memory = self.memory(cx);
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.feed_search_results(memory, root, label, entries, window, cx);
        });
    }

    /// Enter on a result: the panel goes to it. See the module doc comment.
    fn open_cursor_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.results.get(self.cursor) else {
            return;
        };
        let path = hit.path.clone();
        self.stop(cx);
        let memory = self.memory(cx);
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.go_to_search_result(memory, path, window, cx);
        });
    }

    fn move_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, last) as usize;
        self.scroll
            .scroll_to_item(self.cursor, ScrollStrategy::Center);
        cx.notify();
    }

    fn current_stop(&self, window: &Window, cx: &App) -> Option<Stop> {
        if self.results_focus.is_focused(window) {
            return Some(Stop::Results);
        }
        self.inputs
            .iter()
            .position(|input| input.read(cx).focus_handle(cx).is_focused(window))
            .map(Stop::Field)
    }

    fn focus_stop(&mut self, stop: Stop, window: &mut Window, cx: &mut Context<Self>) {
        match stop {
            Stop::Field(ix) => {
                self.inputs[ix].update(cx, |state, cx| focus_at_end(state, window, cx));
            }
            Stop::Results => window.focus(&self.results_focus),
        }
        cx.notify();
    }

    /// Tab / Shift+Tab: fields, then the result list, round and round.
    fn cycle_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let stops = self.inputs.len() + 1; // + the list
        let current = match self.current_stop(window, cx) {
            Some(Stop::Field(ix)) => ix,
            Some(Stop::Results) | None => self.inputs.len(),
        };
        let next = if forward {
            (current + 1) % stops
        } else {
            (current + stops - 1) % stops
        };
        let stop = if next == self.inputs.len() {
            Stop::Results
        } else {
            Stop::Field(next)
        };
        self.focus_stop(stop, window, cx);
    }

    fn toggle(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut SearchOptions)) {
        f(&mut self.options);
        cx.notify();
    }

    fn status_text(&self) -> String {
        let p = &self.progress;
        let found = match self.results.len() {
            1 => "1 found".to_string(),
            n => format!("{} found", group_digits(n as u64)),
        };
        let scanned = format!(
            "{} folders, {} entries",
            group_digits(p.dirs),
            group_digits(p.entries)
        );
        let read = if p.bytes > 0 {
            let mut bytes = String::new();
            crate::file_table::write_byte_count(&mut bytes, p.bytes);
            format!(", {bytes} read")
        } else {
            String::new()
        };
        let errors = match p.errors {
            0 => String::new(),
            1 => " \u{b7} 1 error".to_string(),
            n => format!(" \u{b7} {n} errors"),
        };
        match self.phase {
            Phase::Idle => "Enter starts the search.".to_string(),
            Phase::Running => {
                let current = p
                    .current
                    .as_ref()
                    .map(|c| format!(" \u{b7} {}", c.display()))
                    .unwrap_or_default();
                format!("Searching\u{2026} {found} \u{b7} {scanned}{read}{errors}{current}")
            }
            Phase::Done { elapsed, cancelled } => {
                let verb = if cancelled { "Stopped" } else { "Done" };
                format!(
                    "{verb}: {found} in {:.1} s \u{b7} {scanned}{read}{errors}",
                    elapsed.as_secs_f64()
                )
            }
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let tokens = TokenPalette::current(cx);
        let hit = &self.results[ix];
        let mut name = relative_name(&self.results_root, &hit.path);
        if hit.metadata.kind == EntryKind::Directory {
            name.push('/');
        }
        let mut size = String::new();
        write_size(&mut size, hit.metadata.kind, hit.metadata.size);
        let mut date = String::new();
        write_date(&mut date, hit.metadata.modified.map_or(0, |t| t.secs));
        let is_cursor = ix == self.cursor;
        let list_focused = self.results_focus_is_focused;
        div()
            .id(("search-result", ix))
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .gap_3()
            .text_size(px(12.))
            .when(is_cursor && list_focused, |row| {
                row.bg(tokens.color.cursor_bg)
                    .text_color(tokens.color.cursor_fg)
            })
            .when(is_cursor && !list_focused, |row| {
                row.bg(tokens.color.selection_bg)
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.cursor = ix;
                window.focus(&this.results_focus);
                if event.click_count() >= 2 {
                    this.open_cursor_result(window, cx);
                }
                cx.notify();
            }))
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(div().w(px(80.)).flex_none().text_right().child(size))
            .child(div().w(px(110.)).flex_none().child(date))
            .into_any_element()
    }
}

/// `1234567` -> `1,234,567`.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Focusable for SearchDialogState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.results_focus.clone()
    }
}

/// A clickable `label: value (key)` switch.
fn switch(
    id: &'static str,
    label: &str,
    value: &str,
    key: &str,
    on: bool,
    tokens: &TokenPalette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .hover(|style| style.bg(tokens.color.tab_active_bg))
        .when(on, |this| this.text_color(tokens.color.accent))
        .child(SharedString::from(format!("{label}: {value} ({key})")))
}

fn field_row(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(86.)).flex_none().child(label))
        .child(div().flex_1().child(Input::new(input)))
}

fn small_field(label: &'static str, input: &Entity<InputState>, width: f32) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().flex_none().child(label))
        .child(div().w(px(width)).child(Input::new(input)))
}

impl Render for SearchDialogState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.pending_focus_results)
            && matches!(self.current_stop(window, cx), Some(Stop::Field(_)))
        {
            window.focus(&self.results_focus);
        }
        self.results_focus_is_focused = self.results_focus.is_focused(window);
        let tokens = TokenPalette::current(cx);
        let o = self.options;
        let muted = tokens.color.statusbar_fg;
        let yes_no = |b: bool| if b { "yes" } else { "no" };

        let name_switches = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .text_size(px(11.))
            .text_color(muted)
            .child(
                switch(
                    "search-name-regex",
                    "Name",
                    if o.name_regex { "regex" } else { "mask" },
                    "Alt+E",
                    o.name_regex,
                    tokens,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.toggle(cx, |o| o.name_regex = !o.name_regex)),
                ),
            )
            .child(
                switch(
                    "search-case",
                    "Case",
                    if o.case_sensitive {
                        "matched"
                    } else {
                        "ignored"
                    },
                    "Alt+V",
                    o.case_sensitive,
                    tokens,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle(cx, |o| o.case_sensitive = !o.case_sensitive)
                })),
            )
            .child(
                switch(
                    "search-kind",
                    "Find",
                    o.kind.label(),
                    "Alt+K",
                    o.kind != KindFilter::Any,
                    tokens,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.toggle(cx, |o| o.kind = o.kind.next())),
                ),
            )
            .child(
                switch(
                    "search-hidden",
                    "Hidden",
                    if o.include_hidden {
                        "included"
                    } else {
                        "skipped"
                    },
                    "Alt+H",
                    !o.include_hidden,
                    tokens,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle(cx, |o| o.include_hidden = !o.include_hidden)
                })),
            )
            .child(
                switch(
                    "search-symlinks",
                    "Follow symlinks",
                    yes_no(o.follow_symlinks),
                    "Alt+Y",
                    o.follow_symlinks,
                    tokens,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle(cx, |o| o.follow_symlinks = !o.follow_symlinks)
                })),
            )
            .child(
                switch(
                    "search-onefs",
                    "One filesystem",
                    yes_no(o.one_filesystem),
                    "Alt+M",
                    o.one_filesystem,
                    tokens,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle(cx, |o| o.one_filesystem = !o.one_filesystem)
                })),
            );

        let text_switches =
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .text_size(px(11.))
                .text_color(muted)
                .child(
                    switch(
                        "search-text-regex",
                        "Text",
                        if o.text_regex { "regex" } else { "literal" },
                        "Alt+X",
                        o.text_regex,
                        tokens,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle(cx, |o| o.text_regex = !o.text_regex)
                    })),
                )
                .child(
                    switch(
                        "search-words",
                        "Whole words",
                        yes_no(o.whole_words),
                        "Alt+W",
                        o.whole_words,
                        tokens,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle(cx, |o| o.whole_words = !o.whole_words)
                    })),
                )
                .child(
                    switch(
                        "search-not",
                        "Not containing",
                        yes_no(o.not_containing),
                        "Alt+O",
                        o.not_containing,
                        tokens,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle(cx, |o| o.not_containing = !o.not_containing)
                    })),
                )
                .child(
                    switch(
                        "search-encoding",
                        "Encoding",
                        o.encodings.label(),
                        "Ctrl+E",
                        o.encodings != ContentEncodings::Utf8,
                        tokens,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle(cx, |o| o.encodings = o.encodings.next())
                    })),
                )
                .child(
                    switch(
                        "search-binary",
                        "Binary files",
                        if o.skip_binary { "skipped" } else { "searched" },
                        "Alt+B",
                        o.skip_binary,
                        tokens,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle(cx, |o| o.skip_binary = !o.skip_binary)
                    })),
                );

        let filters = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(small_field("Size from", &self.inputs[3], 80.))
            .child(small_field("to", &self.inputs[4], 80.))
            .child(small_field("Modified after", &self.inputs[5], 120.))
            .child(small_field("before", &self.inputs[6], 120.))
            .child(small_field("Depth", &self.inputs[7], 44.));

        let count = self.results.len();
        let results = div()
            .id("search-results")
            .key_context("SearchResults")
            .track_focus(&self.results_focus)
            .h(px(ROW_HEIGHT * RESULT_ROWS as f32 + 2.))
            .border_1()
            .border_color(if self.results_focus_is_focused {
                tokens.color.border_focus
            } else {
                tokens.color.border_default
            })
            .rounded_sm()
            .child(if count == 0 {
                div()
                    .p_2()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(match self.phase {
                        Phase::Done { .. } => "No files found.",
                        _ => "",
                    })
                    .into_any_element()
            } else {
                uniform_list(
                    "search-results-list",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                    }),
                )
                .size_full()
                .track_scroll(self.scroll.clone())
                .into_any_element()
            })
            .on_action(cx.listener(|this, _: &SearchResultsUp, _, cx| this.move_cursor(-1, cx)))
            .on_action(cx.listener(|this, _: &SearchResultsDown, _, cx| this.move_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &SearchResultsPageUp, _, cx| {
                this.move_cursor(-(RESULT_ROWS as i64 - 1), cx)
            }))
            .on_action(cx.listener(|this, _: &SearchResultsPageDown, _, cx| {
                this.move_cursor(RESULT_ROWS as i64 - 1, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SearchResultsHome, _, cx| {
                    this.move_cursor(i64::MIN / 2, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SearchResultsEnd, _, cx| this.move_cursor(i64::MAX / 2, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchResultsOpen, window, cx| {
                this.open_cursor_result(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SearchResultsEscape, window, cx| this.escape(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchFocusNext, window, cx| {
                this.cycle_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SearchFocusPrev, window, cx| {
                this.cycle_focus(false, window, cx)
            }));

        let status_color = match self.phase {
            Phase::Running => tokens.color.accent,
            _ => muted,
        };

        div()
            .id("search-dialog")
            .key_context("SearchDialog")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(div().font_weight(FontWeight::BOLD).child("Find files"))
            .child(field_row("Search for:", &self.inputs[0]))
            .child(name_switches)
            .child(field_row("Search in:", &self.inputs[1]))
            .child(field_row("Containing:", &self.inputs[2]))
            .child(text_switches)
            .child(filters)
            .when_some(self.form_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(tokens.color.error)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(status_color)
                    .truncate()
                    .child(SharedString::from(self.status_text())),
            )
            .when_some(self.first_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(tokens.color.warning)
                        .truncate()
                        .child(SharedString::from(error)),
                )
            })
            .child(results)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(
                        "Enter: search \u{b7} Esc: stop / close \u{b7} Tab: next field, then results \u{b7} \
                         Enter on a result: go to it \u{b7} Alt+L: feed to panel",
                    ),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.escape(window, cx)))
            .on_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.cycle_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                this.cycle_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleNameRegex, _, cx| {
                this.toggle(cx, |o| o.name_regex = !o.name_regex)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleTextRegex, _, cx| {
                this.toggle(cx, |o| o.text_regex = !o.text_regex)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleCase, _, cx| {
                this.toggle(cx, |o| o.case_sensitive = !o.case_sensitive)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleWholeWords, _, cx| {
                this.toggle(cx, |o| o.whole_words = !o.whole_words)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleNotContaining, _, cx| {
                this.toggle(cx, |o| o.not_containing = !o.not_containing)
            }))
            .on_action(cx.listener(|this, _: &SearchCycleEncoding, _, cx| {
                this.toggle(cx, |o| o.encodings = o.encodings.next())
            }))
            .on_action(cx.listener(|this, _: &SearchToggleSkipBinary, _, cx| {
                this.toggle(cx, |o| o.skip_binary = !o.skip_binary)
            }))
            .on_action(cx.listener(|this, _: &SearchCycleKind, _, cx| {
                this.toggle(cx, |o| o.kind = o.kind.next())
            }))
            .on_action(cx.listener(|this, _: &SearchToggleHidden, _, cx| {
                this.toggle(cx, |o| o.include_hidden = !o.include_hidden)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleSymlinks, _, cx| {
                this.toggle(cx, |o| o.follow_symlinks = !o.follow_symlinks)
            }))
            .on_action(cx.listener(|this, _: &SearchToggleOneFilesystem, _, cx| {
                this.toggle(cx, |o| o.one_filesystem = !o.one_filesystem)
            }))
            .on_action(cx.listener(|this, _: &SearchFeedToPanel, window, cx| {
                this.feed_to_panel(window, cx)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(name: &str, roots: &str) -> SearchFields {
        SearchFields {
            name: name.into(),
            roots: roots.into(),
            ..SearchFields::default()
        }
    }

    #[test]
    fn build_spec_maps_fields_and_switches() {
        let f = SearchFields {
            content: "todo".into(),
            size_min: "1k".into(),
            size_max: "2M".into(),
            after: "7d".into(),
            before: "2026-12-31".into(),
            depth: "3".into(),
            ..fields(" *.rs ", "src; ~/notes")
        };
        let o = SearchOptions {
            case_sensitive: true,
            whole_words: true,
            kind: KindFilter::FilesOnly,
            ..SearchOptions::default()
        };
        let spec = build_spec(&f, &o, Path::new("/p"), Some(Path::new("/h")), 1_000_000).unwrap();
        assert_eq!(
            spec.roots,
            [PathBuf::from("/p/src"), PathBuf::from("/h/notes")]
        );
        assert_eq!(spec.name, "*.rs");
        assert!(spec.name_case_sensitive);
        let content = spec.content.as_ref().unwrap();
        assert_eq!(content.text, "todo");
        assert!(content.case_sensitive && content.whole_words && !content.regex);
        assert_eq!(spec.size_min, Some(1024));
        assert_eq!(spec.size_max, Some(2 << 20));
        assert_eq!(spec.modified_after, Some(1_000_000 - 7 * 86_400));
        assert!(spec.modified_before.is_some());
        assert_eq!(spec.max_depth, Some(3));
        assert_eq!(spec.kind, KindFilter::FilesOnly);
        assert!(spec.include_hidden);
    }

    #[test]
    fn build_spec_defaults_and_errors() {
        let o = SearchOptions::default();
        let spec = build_spec(&fields("", "/x"), &o, Path::new("/p"), None, 0).unwrap();
        assert_eq!(spec.name, "*");
        assert!(spec.content.is_none());
        assert_eq!(spec.max_depth, None);

        assert!(build_spec(&fields("*", " ; "), &o, Path::new("/p"), None, 0).is_err());
        let bad_size = SearchFields {
            size_min: "huge".into(),
            ..fields("*", "/x")
        };
        assert!(build_spec(&bad_size, &o, Path::new("/p"), None, 0).is_err());
        let bad_depth = SearchFields {
            depth: "-1".into(),
            ..fields("*", "/x")
        };
        assert!(build_spec(&bad_depth, &o, Path::new("/p"), None, 0).is_err());
    }

    #[test]
    fn labels_describe_the_search() {
        let o = SearchOptions::default();
        let spec = build_spec(&fields("*.rs", "/x"), &o, Path::new("/"), None, 0).unwrap();
        assert_eq!(listing_label(&spec), "*.rs");
        let spec = build_spec(
            &SearchFields {
                content: "todo".into(),
                ..fields("*.rs", "/x")
            },
            &SearchOptions {
                not_containing: true,
                ..o
            },
            Path::new("/"),
            None,
            0,
        )
        .unwrap();
        assert_eq!(
            listing_label(&spec),
            "*.rs not containing \u{201c}todo\u{201d}"
        );
        let spec = build_spec(
            &fields(r"^a\d", "/x"),
            &SearchOptions {
                name_regex: true,
                ..o
            },
            Path::new("/"),
            None,
            0,
        )
        .unwrap();
        assert_eq!(listing_label(&spec), r"/^a\d/");
    }

    #[test]
    fn common_root_of_several_folders() {
        assert_eq!(common_root(&[PathBuf::from("/a/b")]), PathBuf::from("/a/b"));
        assert_eq!(
            common_root(&[
                PathBuf::from("/a/b/c"),
                PathBuf::from("/a/b/d"),
                PathBuf::from("/a/b")
            ]),
            PathBuf::from("/a/b")
        );
        assert_eq!(
            common_root(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            PathBuf::from("/")
        );
    }

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }
}
