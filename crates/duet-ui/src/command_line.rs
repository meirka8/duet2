// SPDX-License-Identifier: MIT
//! T-5.3.5: the command line at the bottom of the window (FR-TOOL-06).
//!
//! What it does, in Total Commander's terms:
//!
//! - **Enter** runs the line in the user's shell (`terminal.shell`,
//!   `$SHELL` expanded, `/bin/sh` fallback) with the *active panel's
//!   directory* as cwd. Output is captured line by line, off the UI
//!   thread, into a collapsible pane above the input that shows the last
//!   [`OUTPUT_CAP`] lines, the exit status and the elapsed time. Both
//!   panels reread when the command exits, since commands change
//!   directories. A `cd [dir]` line never reaches the shell: it navigates
//!   the active panel, as TC does.
//! - **Shift+Enter** runs the same line in a terminal window instead
//!   (`duet_meta::TerminalLauncher`, held open until Enter), for
//!   interactive or screen-oriented commands the capture pane can't host.
//! - **Ctrl+Enter / Ctrl+Shift+Enter** insert the cursor entry's name /
//!   full path, shell-quoted, both from the panel and from inside the
//!   input (`cmdline.insert_name` / `insert_path`).
//! - **Up / Down** recall history; the unsent draft is kept and comes back
//!   at the end. History lives in `$XDG_STATE_HOME/duet/history/cmdline`
//!   (design.md §10), newest last, deduplicated, capped at
//!   [`HISTORY_CAP`], written atomically off the UI thread.
//! - **Tab / Shift+Tab** complete the last token as a path relative to
//!   cwd (files and directories; directories get a `/`), and the first
//!   token also against `$PATH` executables. An ambiguous match extends
//!   to the common prefix and lists candidates under the input; further
//!   Tabs cycle through them, exactly like the path bar.
//! - **Esc** peels one layer: the completion hint, then the text, then
//!   the output pane, then focus goes back to the panel.
//! - **Alt+F8** (TC's `cmdline.history`) focuses the command line and
//!   recalls the last entry -- TC has no dedicated "focus the command
//!   line" key because typed letters go there by default, and here typed
//!   letters are quick-search (FR-NAV-13).
//!
//! Everything that touches the filesystem or a process runs on the core
//! Tokio runtime; the UI thread only formats and renders.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use duet_widgets::input::{
    Escape, IndentInline, Input, InputEvent, InputState, OutdentInline, Position,
};
use duet_widgets::layout::{h_flex, v_flex};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    actions, div, px,
};

use crate::path_bar::{HINT_VISIBLE, common_prefix, cycle_index, scroll_hint_window};
use crate::workspace::{PanelSide, Workspace};

/// History entries kept (and persisted).
pub(crate) const HISTORY_CAP: usize = 500;
/// Output lines kept for the pane.
pub(crate) const OUTPUT_CAP: usize = 2000;
/// Height of the output pane.
const OUTPUT_PANE_HEIGHT: f32 = 180.0;

actions!(
    duet_command_line,
    [RunInTerminal, HistoryPrev, HistoryNext, InsertPath]
);

/// The command line's own bindings. `Enter`, `Escape`, `Tab`/`Shift+Tab`
/// are the `Input` widget's own actions, which it propagates for a
/// single-line field, so the `CommandLine` wrapper handles them with
/// `on_action`. `Up`/`Down` are different: the widget consumes `MoveUp`
/// / `MoveDown` without propagating, so history recall binds the keys
/// itself in the `CommandLine > Input` context -- same node, bound after
/// the widget's own bindings, so it takes precedence.
pub(crate) fn bind_command_line_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", HistoryPrev, Some("CommandLine > Input")),
        KeyBinding::new("down", HistoryNext, Some("CommandLine > Input")),
        KeyBinding::new("shift-enter", RunInTerminal, Some("CommandLine")),
        KeyBinding::new("ctrl-shift-enter", InsertPath, Some("CommandLine")),
    ]);
}

// ---------------------------------------------------------------------
// Pure helpers (unit-tested without GPUI)
// ---------------------------------------------------------------------

/// POSIX single-quoting, only when `s` needs it: a bare word of letters,
/// digits and `-_./+=:@~%,` is left alone, anything else is wrapped in
/// `'...'` with embedded `'` as `'\''`. Empty becomes `''`.
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+=:@~%,".contains(c))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Strips one layer of simple shell quoting from a token the user typed
/// (`'a b'`, `"a b"`, `a\ b`), for completion against real names.
pub(crate) fn shell_unquote(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            (Some(_), c) => out.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            (None, c) => out.push(c),
        }
    }
    out
}

/// `cd`, `cd <dir>` or `cd ~`: the directory to navigate to (`None` for
/// any other line). A bare `cd` goes home. Tolerates surrounding
/// whitespace and a quoted argument.
pub(crate) fn parse_cd(line: &str) -> Option<Option<String>> {
    let trimmed = line.trim();
    if trimmed == "cd" {
        return Some(None);
    }
    let rest = trimmed.strip_prefix("cd")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let arg = rest.trim();
    if arg.is_empty() || arg == "~" {
        return Some(None);
    }
    Some(Some(shell_unquote(arg)))
}

/// Resolves a `cd` target the way a shell would: `~`/`~/x` to home,
/// relative to `cwd`, `.`/`..` folded.
pub(crate) fn resolve_cd_target(
    arg: Option<&str>,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let raw = match arg {
        None => return home.map(Path::to_path_buf),
        Some(a) => a,
    };
    let joined = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Splits a line into the text before the token being completed and the
/// token itself: the last whitespace-separated word, or an empty token
/// when the line ends in whitespace. Returns `(head, token, is_first)`.
pub(crate) fn split_last_token(line: &str) -> (&str, &str, bool) {
    let trimmed_len = line.trim_end().len();
    if trimmed_len < line.len() {
        // Trailing whitespace: completing a fresh, empty token.
        let is_first = line.trim().is_empty();
        return (line, "", is_first);
    }
    let start = line
        .rfind(char::is_whitespace)
        .map(|ix| ix + line[ix..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let token = &line[start..];
    let is_first = line[..start].trim().is_empty();
    (&line[..start], token, is_first)
}

/// Where a path token's completion looks: the directory to list, the
/// part of the token to keep verbatim in front of the completed name
/// (`~/Doc` -> `~/`, `src/ma` -> `src/`, `ma` -> ``), and the name prefix.
pub(crate) fn split_token_path(
    token: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> (PathBuf, String, String) {
    let (dir_text, prefix) = match token.rfind('/') {
        Some(ix) => (&token[..=ix], &token[ix + 1..]),
        None => ("", token),
    };
    let dir = if dir_text.is_empty() {
        cwd.to_path_buf()
    } else {
        resolve_cd_target(
            Some(
                dir_text
                    .trim_end_matches('/')
                    .max(if dir_text == "/" { "/" } else { "" }),
            ),
            cwd,
            home,
        )
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| cwd.to_path_buf())
    };
    let dir = if dir_text == "/" {
        PathBuf::from("/")
    } else {
        dir
    };
    (dir, dir_text.to_string(), prefix.to_string())
}

/// One completion candidate: the name and whether it is a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
}

/// Completes `prefix` against `candidates` (already filtered to the
/// directory being completed). Returns the text to replace the token
/// with (if anything can be filled in) and the ambiguous matches to
/// show. A unique match is quoted and finished with `/` for a directory
/// or a space for a file; an ambiguous one extends to the common prefix.
pub(crate) fn complete_token(
    dir_text: &str,
    prefix: &str,
    candidates: &[Candidate],
) -> (Option<String>, Vec<Candidate>) {
    let mut matches: Vec<Candidate> = candidates
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .filter(|c| !c.name.starts_with('.') || prefix.starts_with('.'))
        .cloned()
        .collect();
    matches.sort_by(|a, b| a.name.cmp(&b.name));
    match matches.len() {
        0 => (None, Vec::new()),
        1 => {
            let m = &matches[0];
            let mut text = shell_quote(&format!("{dir_text}{}", m.name));
            text.push(if m.is_dir { '/' } else { ' ' });
            (Some(text), Vec::new())
        }
        _ => {
            let names: Vec<String> = matches.iter().map(|c| c.name.clone()).collect();
            let shared = common_prefix(&names);
            let value = (shared.chars().count() > prefix.chars().count())
                .then(|| format!("{dir_text}{shared}"));
            (value, matches)
        }
    }
}

/// The text a chosen candidate puts into the token position.
pub(crate) fn candidate_text(dir_text: &str, candidate: &Candidate) -> String {
    let mut text = shell_quote(&format!("{dir_text}{}", candidate.name));
    text.push(if candidate.is_dir { '/' } else { ' ' });
    text
}

/// The command history: newest last, no duplicates, capped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct History {
    entries: VecDeque<String>,
}

impl History {
    pub(crate) fn from_lines(text: &str) -> Self {
        let mut history = Self::default();
        for line in text.lines() {
            history.push(line);
        }
        history
    }

    /// Adds `line` as the newest entry, dropping an earlier identical one
    /// and the oldest entries beyond [`HISTORY_CAP`]. Blank lines are
    /// ignored.
    pub(crate) fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.entries.retain(|e| e != line);
        self.entries.push_back(line.to_string());
        while self.entries.len() > HISTORY_CAP {
            self.entries.pop_front();
        }
    }

    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(e);
            out.push('\n');
        }
        out
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(&self, ix: usize) -> Option<&str> {
        self.entries.get(ix).map(String::as_str)
    }

    pub(crate) fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::from_lines(&text))
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, path: &Path) -> duet_config::Result<()> {
        duet_config::atomic_write(path, &self.to_text())
    }
}

/// Up/Down recall cursor over a [`History`]: `None` = on the draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Recall {
    position: Option<usize>,
    draft: String,
}

impl Recall {
    /// Moves one entry back (`Up`). `current` is what the field holds now
    /// (saved as the draft on leaving it). Returns the text to show.
    pub(crate) fn prev<'a>(&mut self, history: &'a History, current: &str) -> Option<&'a str> {
        let next = match self.position {
            None => {
                if history.len() == 0 {
                    return None;
                }
                self.draft = current.to_string();
                history.len() - 1
            }
            Some(0) => return history.get(0),
            Some(ix) => ix - 1,
        };
        self.position = Some(next);
        history.get(next)
    }

    /// Moves one entry forward (`Down`); past the newest, the draft comes
    /// back. Returns the text to show, `None` when nothing changes.
    pub(crate) fn next(&mut self, history: &History) -> Option<String> {
        let ix = self.position?;
        if ix + 1 < history.len() {
            self.position = Some(ix + 1);
            history.get(ix + 1).map(str::to_string)
        } else {
            self.position = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    pub(crate) fn reset(&mut self) {
        self.position = None;
        self.draft.clear();
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> Option<usize> {
        self.position
    }
}

/// `terminal.shell` resolved to a program: `$VAR`/`${VAR}` expanded, and
/// `/bin/sh` when the result is empty or not an executable.
pub(crate) fn resolve_shell(setting: &str) -> String {
    let trimmed = setting.trim();
    let expanded = if let Some(var) = trimmed
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .or_else(|| trimmed.strip_prefix('$'))
    {
        std::env::var(var).unwrap_or_default()
    } else {
        trimmed.to_string()
    };
    if !expanded.is_empty() && duet_meta::desktop::program_exists(&expanded) {
        expanded
    } else {
        "/bin/sh".to_string()
    }
}

// ---------------------------------------------------------------------
// Running a command
// ---------------------------------------------------------------------

/// What the command's reader thread sends to the UI.
enum OutputEvent {
    Line(String),
    Exited {
        status: ExitStatus,
        elapsed: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitStatus {
    Code(i32),
    Signal(i32),
    Unknown,
}

impl ExitStatus {
    fn from_std(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(sig)) => Self::Signal(sig),
            _ => Self::Unknown,
        }
    }

    fn is_success(self) -> bool {
        self == Self::Code(0)
    }
}

/// Spawns `shell -c line` in `cwd` and streams its combined output;
/// blocking, so call on `spawn_blocking`. The receiver side is the UI.
fn run_captured(
    shell: &str,
    line: &str,
    cwd: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<OutputEvent>,
) -> Result<(), String> {
    use std::io::{BufRead as _, BufReader};
    let started = Instant::now();
    let mut child = std::process::Command::new(shell)
        .arg("-c")
        .arg(line)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{shell}: {e}"))?;
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.split(b'\n') {
                let Ok(bytes) = line else { break };
                let text = String::from_utf8_lossy(&bytes)
                    .trim_end_matches('\r')
                    .to_string();
                if tx.send(OutputEvent::Line(text)).is_err() {
                    break;
                }
            }
        }));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    for reader in readers {
        let _ = reader.join();
    }
    let _ = tx.send(OutputEvent::Exited {
        status: ExitStatus::from_std(status),
        elapsed: started.elapsed(),
    });
    Ok(())
}

// ---------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------

/// A finished or running command, for the output pane's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunInfo {
    pub(crate) line: String,
    pub(crate) cwd: PathBuf,
    pub(crate) status: Option<(ExitStatus, Duration)>,
}

pub(crate) struct CommandLineState {
    input: Entity<InputState>,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    shell: String,
    history_path: Option<PathBuf>,
    history: History,
    recall: Recall,
    /// Completion: the directory text kept in front of the candidate, the
    /// head of the line before the token, the candidates, the selection
    /// and the hint window -- see `path_bar.rs` for the same shape.
    completion: Option<CompletionSession>,
    /// Set when the bar writes its own input so the resulting `Change`
    /// doesn't clear the completion hint (see `path_bar.rs`).
    programmatic_value: Option<String>,
    output: VecDeque<String>,
    run: Option<RunInfo>,
    output_visible: bool,
    output_scroll: ScrollHandle,
    _subscription: Subscription,
}

struct CompletionSession {
    head: String,
    dir_text: String,
    candidates: Vec<Candidate>,
    selected: Option<usize>,
    window_start: usize,
}

impl CommandLineState {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        shell: String,
        history_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Command line  (Enter runs in the active panel's directory, Shift+Enter in a terminal)")
        });
        let subscription = cx.subscribe_in(&input, window, Self::on_input_event);
        let this = Self {
            input,
            workspace,
            tokio_handle,
            shell,
            history_path: history_path.clone(),
            history: History::default(),
            recall: Recall::default(),
            completion: None,
            programmatic_value: None,
            output: VecDeque::new(),
            run: None,
            output_visible: false,
            output_scroll: ScrollHandle::new(),
            _subscription: subscription,
        };
        // History is read off the UI thread and lands whenever it lands;
        // a recall before that just sees an empty history.
        if let Some(path) = history_path {
            let (tx, rx) = tokio::sync::oneshot::channel::<History>();
            this.tokio_handle.spawn_blocking(move || {
                let _ = tx.send(History::load(&path));
            });
            cx.spawn(async move |this, cx| {
                if let Ok(history) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        // Entries typed before the file arrived win.
                        let mut merged = history;
                        for ix in 0..this.history.len() {
                            if let Some(e) = this.history.get(ix) {
                                merged.push(e);
                            }
                        }
                        this.history = merged;
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        this
    }

    pub(crate) fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn output_lines(&self) -> impl Iterator<Item = &str> {
        self.output.iter().map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn run_info(&self) -> Option<&RunInfo> {
        self.run.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn history(&self) -> &History {
        &self.history
    }

    #[cfg(test)]
    pub(crate) fn completion_candidates(&self) -> Vec<String> {
        self.completion
            .as_ref()
            .map(|c| c.candidates.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Replaces the field's text and puts the cursor at the end. The
    /// widget's `set_cursor_position` focuses the input as a side effect;
    /// when the text came from the panel (Ctrl+Enter while browsing) the
    /// panel must keep the focus, so whatever had it before gets it back.
    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self.input.read(cx).focus_handle(cx).is_focused(window);
        let previous = window.focused(cx);
        self.programmatic_value = Some(text.clone());
        self.input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_cursor_position(Position::new(0, u32::MAX), window, cx);
        });
        if !had_focus && let Some(previous) = previous {
            window.focus(&previous);
        }
    }

    /// Appends `text` (already quoted) plus a separating space, keeping
    /// whatever was typed. Used by the insert-name/path keys.
    pub(crate) fn append_token(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut line = self.value(cx);
        if !line.is_empty() && !line.ends_with(char::is_whitespace) {
            line.push(' ');
        }
        line.push_str(text);
        line.push(' ');
        self.set_text(line, window, cx);
        self.completion = None;
        cx.notify();
    }

    /// Replaces the field's text (history recall, palette).
    pub(crate) fn replace_text(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_text(text, window, cx);
        self.completion = None;
        cx.notify();
    }

    fn on_input_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { secondary: false } => self.execute(false, window, cx),
            // Ctrl+Enter inside the field: TC's "insert the name under the
            // cursor" works while typing, not only from the panel.
            InputEvent::PressEnter { secondary: true } => {
                let _ = self.workspace.update(cx, |ws, cx| {
                    ws.insert_cursor_name_into_command_line(false, window, cx);
                });
            }
            InputEvent::Change => {
                let current = self.value(cx);
                if self
                    .programmatic_value
                    .take()
                    .is_some_and(|ours| ours == current)
                {
                    return;
                }
                if self.completion.take().is_some() {
                    cx.notify();
                }
                self.recall.reset();
            }
            _ => {}
        }
    }

    /// The active panel's directory and cursor entry, from the workspace.
    fn panel_context(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(PanelSide, PathBuf, Option<String>)> {
        self.workspace
            .update(cx, |ws, cx| ws.command_line_context(cx))
            .ok()
    }

    /// Enter (or Shift+Enter with `in_terminal`): see the module doc.
    pub(crate) fn execute(
        &mut self,
        in_terminal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = self.value(cx).trim().to_string();
        if line.is_empty() {
            return;
        }
        let Some((side, cwd, _)) = self.panel_context(cx) else {
            return;
        };
        self.history.push(&line);
        self.recall.reset();
        self.completion = None;
        self.persist_history();
        self.set_text(String::new(), window, cx);

        if let Some(arg) = parse_cd(&line) {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            match resolve_cd_target(arg.as_deref(), &cwd, home.as_deref()) {
                Some(target) => {
                    let _ = self.workspace.update(cx, |ws, cx| {
                        ws.navigate_panel_to_path(side, target, window, cx);
                    });
                }
                None => self.push_notice("cd: no home directory", cx),
            }
            cx.notify();
            return;
        }

        if in_terminal {
            let plan = duet_meta::LaunchPlan {
                argv: vec![self.shell.clone(), "-c".to_string(), line.clone()],
                cwd: Some(cwd.clone()),
                terminal: true,
                label: line.clone(),
            };
            let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
            self.tokio_handle.spawn_blocking(move || {
                let result = duet_meta::spawn(&plan)
                    .map(|mut child| {
                        // Reaped on its own thread, like every other launch.
                        let _ = std::thread::Builder::new()
                            .name("duet-reap".to_string())
                            .spawn(move || {
                                let _ = child.wait();
                            });
                    })
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Err(message)) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        this.push_notice(&format!("Couldn't open a terminal: {message}"), cx);
                    });
                }
            })
            .detach();
            cx.notify();
            return;
        }

        self.output.clear();
        self.output_visible = true;
        self.run = Some(RunInfo {
            line: line.clone(),
            cwd: cwd.clone(),
            status: None,
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<OutputEvent>();
        let shell = self.shell.clone();
        let spawn_error_tx = tx.clone();
        self.tokio_handle.spawn_blocking(move || {
            if let Err(message) = run_captured(&shell, &line, &cwd, tx) {
                let _ = spawn_error_tx.send(OutputEvent::Line(message));
                let _ = spawn_error_tx.send(OutputEvent::Exited {
                    status: ExitStatus::Unknown,
                    elapsed: Duration::ZERO,
                });
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let keep_going = this
                    .update(cx, |this, cx| {
                        match event {
                            OutputEvent::Line(text) => {
                                if this.output.len() >= OUTPUT_CAP {
                                    this.output.pop_front();
                                }
                                this.output.push_back(text);
                                this.output_scroll.scroll_to_bottom();
                            }
                            OutputEvent::Exited { status, elapsed } => {
                                if let Some(run) = this.run.as_mut() {
                                    run.status = Some((status, elapsed));
                                }
                                // Commands change directories: reread both
                                // panels, the way a finished job does.
                                let _ = this
                                    .workspace
                                    .update(cx, |ws, cx| ws.request_panel_refresh(cx));
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !keep_going {
                    return;
                }
                crate::workspace::yield_once().await;
            }
        })
        .detach();
        cx.notify();
    }

    fn persist_history(&self) {
        let Some(path) = self.history_path.clone() else {
            return;
        };
        let history = self.history.clone();
        self.tokio_handle.spawn_blocking(move || {
            if let Err(err) = history.save(&path) {
                tracing::warn!(target: "duet_ui::command_line", "failed to save command history: {err}");
            }
        });
    }

    fn push_notice(&self, message: &str, cx: &mut Context<Self>) {
        let message = message.to_string();
        let _ = self.workspace.update(cx, |ws, cx| {
            ws.push_pending_notice(crate::workspace::NoticeLevel::Warning, message, cx);
        });
    }

    /// Up: previous history entry (the draft is kept).
    pub(crate) fn history_prev(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.value(cx);
        if let Some(text) = self
            .recall
            .prev(&self.history, &current)
            .map(str::to_string)
        {
            self.set_text(text, window, cx);
            self.completion = None;
            cx.notify();
        }
    }

    /// Down: next history entry, or the draft past the newest.
    pub(crate) fn history_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.recall.next(&self.history) {
            self.set_text(text, window, cx);
            self.completion = None;
            cx.notify();
        }
    }

    /// Esc: one layer at a time -- hint, text, output pane, then focus
    /// back to the panel.
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
            return;
        }
        if !self.value(cx).is_empty() {
            self.set_text(String::new(), window, cx);
            self.recall.reset();
            cx.notify();
            return;
        }
        if self.output_visible {
            self.output_visible = false;
            cx.notify();
            return;
        }
        let _ = self
            .workspace
            .update(cx, |ws, cx| ws.focus_active_panel(window, cx));
    }

    /// Tab: complete the last token, or cycle an existing candidate list.
    pub(crate) fn complete(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion.is_some() {
            self.cycle(backwards, window, cx);
            return;
        }
        let line = self.value(cx);
        let Some((_, cwd, _)) = self.panel_context(cx) else {
            return;
        };
        let (head, token, is_first) = split_last_token(&line);
        let head = head.to_string();
        let token = shell_unquote(token);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let (dir, dir_text, prefix) = split_token_path(&token, &cwd, home.as_deref());
        let want_commands = is_first && !token.contains('/');
        let command_prefix = prefix.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<Candidate>>();
        self.tokio_handle.spawn_blocking(move || {
            let mut candidates: Vec<Candidate> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let Ok(name) = entry.file_name().into_string() else {
                        continue;
                    };
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        || std::fs::metadata(entry.path())
                            .map(|m| m.is_dir())
                            .unwrap_or(false);
                    candidates.push(Candidate { name, is_dir });
                }
            }
            if want_commands && let Some(paths) = std::env::var_os("PATH") {
                for path_dir in std::env::split_paths(&paths) {
                    let Ok(entries) = std::fs::read_dir(&path_dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let Ok(name) = entry.file_name().into_string() else {
                            continue;
                        };
                        if !name.starts_with(&command_prefix)
                            || candidates.iter().any(|c| c.name == name)
                        {
                            continue;
                        }
                        candidates.push(Candidate {
                            name,
                            is_dir: false,
                        });
                    }
                }
            }
            let _ = tx.send(candidates);
        });
        cx.spawn_in(window, async move |this, cx| {
            let candidates = rx.await.unwrap_or_default();
            let _ = this.update_in(cx, |this, window, cx| {
                let (filled, matches) = complete_token(&dir_text, &prefix, &candidates);
                if let Some(text) = filled {
                    this.set_text(format!("{head}{text}"), window, cx);
                }
                this.completion = if matches.is_empty() {
                    None
                } else {
                    Some(CompletionSession {
                        head,
                        dir_text,
                        candidates: matches,
                        selected: None,
                        window_start: 0,
                    })
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn cycle(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.completion.as_mut() else {
            return;
        };
        let ix = cycle_index(session.selected, session.candidates.len(), backwards);
        session.selected = Some(ix);
        session.window_start = scroll_hint_window(session.window_start, ix, HINT_VISIBLE);
        let text = format!(
            "{}{}",
            session.head,
            candidate_text(&session.dir_text, &session.candidates[ix])
        );
        self.set_text(text, window, cx);
        cx.notify();
    }

    fn toggle_output(&mut self, cx: &mut Context<Self>) {
        self.output_visible = !self.output_visible;
        cx.notify();
    }
}

impl Focusable for CommandLineState {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for CommandLineState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let hint = self.completion.as_ref().map(|session| {
            let start = session.window_start.min(session.candidates.len());
            let end = (start + HINT_VISIBLE).min(session.candidates.len());
            let before = start;
            let after = session.candidates.len() - end;
            let mut row = h_flex()
                .px_2()
                .gap_2()
                .text_size(px(10.))
                .text_color(tokens.color.statusbar_fg)
                .overflow_hidden();
            if before > 0 {
                row = row.child(SharedString::from(format!("+{before} …")));
            }
            for (ix, c) in session.candidates[start..end].iter().enumerate() {
                let label = if c.is_dir {
                    format!("{}/", c.name)
                } else {
                    c.name.clone()
                };
                let mut item = div().child(SharedString::from(label));
                if Some(start + ix) == session.selected {
                    item = item
                        .font_weight(FontWeight::BOLD)
                        .text_color(tokens.color.panel_fg_active);
                }
                row = row.child(item);
            }
            if after > 0 {
                row = row.child(SharedString::from(format!("… +{after}")));
            }
            row
        });

        let output = (self.output_visible && self.run.is_some()).then(|| {
            let run = self.run.as_ref().expect("checked");
            let (status_text, status_color) = match run.status {
                None => ("running…".to_string(), tokens.color.info),
                Some((status, elapsed)) => {
                    let text = match status {
                        ExitStatus::Code(code) => format!("exit {code}"),
                        ExitStatus::Signal(sig) => format!("killed by signal {sig}"),
                        ExitStatus::Unknown => "failed to start".to_string(),
                    };
                    (
                        format!("{text} · {:.1} s", elapsed.as_secs_f64()),
                        if status.is_success() {
                            tokens.color.success
                        } else {
                            tokens.color.error
                        },
                    )
                }
            };
            let header = h_flex()
                .w_full()
                .px_2()
                .py_0p5()
                .gap_2()
                .items_center()
                .bg(tokens.color.header_bg)
                .text_color(tokens.color.header_fg)
                .text_size(px(11.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!(
                            "$ {}   ({})",
                            run.line,
                            run.cwd.display()
                        ))),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(status_color)
                        .child(SharedString::from(status_text)),
                )
                .child(
                    div()
                        .id("cmdline-output-close")
                        .flex_none()
                        .px_1()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_output(cx)))
                        .child("×"),
                );
            let lines: Vec<SharedString> = self
                .output
                .iter()
                .map(|l| SharedString::from(l.clone()))
                .collect();
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(tokens.color.border_default)
                .child(header)
                .child(
                    div()
                        .id("cmdline-output")
                        .w_full()
                        .max_h(px(OUTPUT_PANE_HEIGHT))
                        .overflow_y_scroll()
                        .track_scroll(&self.output_scroll)
                        .px_2()
                        .py_1()
                        .font_family("monospace")
                        .text_size(px(12.))
                        .bg(tokens.color.panel_bg_inactive)
                        .children(
                            lines
                                .into_iter()
                                .map(|l| div().whitespace_nowrap().child(l)),
                        ),
                )
        });

        v_flex()
            .w_full()
            .key_context("CommandLine")
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.escape(window, cx)))
            .on_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.complete(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                this.complete(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryPrev, window, cx| {
                this.history_prev(window, cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryNext, window, cx| {
                this.history_next(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RunInTerminal, window, cx| {
                this.execute(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertPath, window, cx| {
                let _ = this.workspace.update(cx, |ws, cx| {
                    ws.insert_cursor_name_into_command_line(true, window, cx);
                });
            }))
            .when_some(output, |this, output| this.child(output))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .bg(tokens.color.panel_bg_active)
                    .border_t_1()
                    .border_color(tokens.color.border_default)
                    .items_center()
                    .child(
                        div()
                            .text_color(tokens.color.accent)
                            .font_weight(FontWeight::BOLD)
                            .child("$"),
                    )
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .when(self.run.is_some() && !self.output_visible, |this| {
                        this.child(
                            div()
                                .id("cmdline-output-show")
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(tokens.color.statusbar_fg)
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_output(cx)))
                                .child("output ▴"),
                        )
                    }),
            )
            .when_some(hint, |this, hint| this.child(hint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_words_and_quotes_the_rest() {
        assert_eq!(shell_quote("Makefile"), "Makefile");
        assert_eq!(shell_quote("~/src/a-b_c.txt"), "~/src/a-b_c.txt");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_unquote("'a b'"), "a b");
        assert_eq!(shell_unquote("\"a b\""), "a b");
        assert_eq!(shell_unquote("a\\ b"), "a b");
        assert_eq!(shell_unquote("'it'\\''s'"), "it's");
    }

    #[test]
    fn cd_lines_are_recognised_and_resolved() {
        assert_eq!(parse_cd("cd"), Some(None));
        assert_eq!(parse_cd("  cd  "), Some(None));
        assert_eq!(parse_cd("cd ~"), Some(None));
        assert_eq!(parse_cd("cd src"), Some(Some("src".into())));
        assert_eq!(parse_cd("cd 'a b'"), Some(Some("a b".into())));
        assert_eq!(parse_cd("cdx"), None);
        assert_eq!(parse_cd("ls"), None);
        let cwd = Path::new("/home/u/proj");
        let home = Path::new("/home/u");
        assert_eq!(
            resolve_cd_target(None, cwd, Some(home)),
            Some(home.to_path_buf())
        );
        assert_eq!(
            resolve_cd_target(Some("src"), cwd, Some(home)),
            Some("/home/u/proj/src".into())
        );
        assert_eq!(
            resolve_cd_target(Some(".."), cwd, Some(home)),
            Some("/home/u".into())
        );
        assert_eq!(
            resolve_cd_target(Some("~/x"), cwd, Some(home)),
            Some("/home/u/x".into())
        );
        assert_eq!(
            resolve_cd_target(Some("/etc/./x/../y"), cwd, None),
            Some("/etc/y".into())
        );
        assert_eq!(resolve_cd_target(None, cwd, None), None);
    }

    #[test]
    fn last_token_and_its_path_parts_are_split_like_a_shell() {
        assert_eq!(split_last_token("make"), ("", "make", true));
        assert_eq!(split_last_token("cat src/ma"), ("cat ", "src/ma", false));
        assert_eq!(split_last_token("ls "), ("ls ", "", false));
        assert_eq!(split_last_token(""), ("", "", true));
        let cwd = Path::new("/w");
        let home = Path::new("/h");
        assert_eq!(
            split_token_path("ma", cwd, Some(home)),
            (PathBuf::from("/w"), "".into(), "ma".into())
        );
        assert_eq!(
            split_token_path("src/ma", cwd, Some(home)),
            (PathBuf::from("/w/src"), "src/".into(), "ma".into())
        );
        assert_eq!(
            split_token_path("~/Doc", cwd, Some(home)),
            (PathBuf::from("/h"), "~/".into(), "Doc".into())
        );
        assert_eq!(
            split_token_path("/et", cwd, Some(home)),
            (PathBuf::from("/"), "/".into(), "et".into())
        );
        assert_eq!(
            split_token_path("/etc/ho", cwd, None),
            (PathBuf::from("/etc"), "/etc/".into(), "ho".into())
        );
    }

    #[test]
    fn complete_token_handles_unique_ambiguous_hidden_and_quoting() {
        let c = |name: &str, is_dir: bool| Candidate {
            name: name.into(),
            is_dir,
        };
        let names = vec![
            c("Makefile", false),
            c("main.rs", false),
            c("my dir", true),
            c(".hidden", false),
        ];
        assert_eq!(
            complete_token("", "Mak", &names),
            (Some("Makefile ".into()), vec![])
        );
        assert_eq!(
            complete_token("src/", "my", &names),
            (Some("'src/my dir'/".into()), vec![])
        );
        let (filled, matches) = complete_token("", "m", &names);
        assert_eq!(filled, None, "'main.rs' and 'my dir' share only 'm'");
        assert_eq!(matches.len(), 2);
        assert_eq!(
            complete_token("", "ma", &names),
            (Some("main.rs ".into()), vec![]),
            "only main.rs starts with 'ma'"
        );
        assert_eq!(
            complete_token("", "", &names).1.len(),
            3,
            "dotfiles hidden unless asked"
        );
        assert_eq!(complete_token("", ".", &names).0, Some(".hidden ".into()));
        assert_eq!(candidate_text("", &c("a b", true)), "'a b'/");
    }

    #[test]
    fn history_dedups_caps_and_round_trips_and_recall_keeps_the_draft() {
        let mut h = History::default();
        h.push("ls");
        h.push("make");
        h.push("  ");
        h.push("ls");
        assert_eq!(
            h.to_text(),
            "make\nls\n",
            "re-run moves to the end, blanks dropped"
        );
        for i in 0..(HISTORY_CAP + 10) {
            h.push(&format!("cmd{i}"));
        }
        assert_eq!(h.len(), HISTORY_CAP);
        assert_eq!(h.get(0), Some("cmd10"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history").join("cmdline");
        let mut small = History::default();
        small.push("echo one");
        small.push("echo two");
        small.save(&path).unwrap();
        assert_eq!(History::load(&path), small);
        assert_eq!(
            History::load(&dir.path().join("missing")),
            History::default()
        );

        let mut r = Recall::default();
        assert_eq!(r.prev(&small, "draft"), Some("echo two"));
        assert_eq!(r.prev(&small, "echo two"), Some("echo one"));
        assert_eq!(
            r.prev(&small, "echo one"),
            Some("echo one"),
            "stays at the oldest"
        );
        assert_eq!(r.next(&small), Some("echo two".into()));
        assert_eq!(r.next(&small), Some("draft".into()), "the draft comes back");
        assert_eq!(r.position(), None);
        assert_eq!(r.next(&small), None);
        assert_eq!(Recall::default().prev(&History::default(), "x"), None);
    }

    #[test]
    fn shell_setting_resolves_variables_and_falls_back() {
        assert_eq!(resolve_shell("/bin/sh"), "/bin/sh");
        assert_eq!(resolve_shell("  "), "/bin/sh");
        assert_eq!(resolve_shell("/definitely/not/a/shell"), "/bin/sh");
        let shell = resolve_shell("$SHELL");
        assert!(duet_meta::desktop::program_exists(&shell));
        assert_eq!(resolve_shell("${DUET_TEST_UNSET_VAR_XYZ}"), "/bin/sh");
    }
}
