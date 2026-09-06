// SPDX-License-Identifier: MIT
//! T-4.3.4 (FR-NAV-09): the panel header as a breadcrumb path bar that is
//! also an editable path input.
//!
//! Two faces, one header slot (`workspace::panel_chrome`'s first child):
//!
//! - **Breadcrumb** (the default): the panel's current directory as one
//!   clickable segment per component -- `/`, `home`, `meirk`, ... -- each
//!   navigating to that ancestor. Clicking anywhere else on the header, or
//!   `Ctrl+L` (`nav.goto_path`, `docs/commands.md`), switches to editing.
//!   Long paths keep their *tail* visible: leading segments are elided
//!   behind a single `…` segment ([`elide_segments`]), since the deepest
//!   directories are what the user is working in.
//! - **Editing**: a [`duet_widgets::input::Input`] pre-filled with the
//!   path, cursor at the end. `Enter` navigates, `Esc` cancels, `Tab`
//!   completes ([`PathBarState::try_complete`]); focus returns to whatever
//!   had it before (the panel's table, normally) either way.
//!
//! # Completion
//!
//! Real filesystem completion, unlike the copy dialog's narrower "only
//! against an already-loaded panel" form (`copy_move_dialog`'s own
//! "Tab-completion" section explains that cut): the parent directory of
//! what has been typed is listed **off the UI thread** (the core Tokio
//! runtime, same shape as every other background query in this crate --
//! ADR-002/§8.2's "the main thread does no I/O", now enforced by
//! T-3.1.6's armed guard) and the directory names starting with the typed
//! prefix come back through a `oneshot`. Exactly one match completes the
//! field with a trailing `/`; several extend the field to their longest
//! common prefix and are listed under the input as a hint so the next
//! keystroke can disambiguate; none is a silent no-op. Only directories
//! are offered -- a path bar navigates, it never opens files.
//!
//! # What "navigates" means
//!
//! `~` and `~/...` expand to `$HOME`; a relative path is taken relative to
//! the panel's current directory; `.`/`..` are folded lexically
//! ([`expand_path`]). The result is checked to be a directory off the UI
//! thread before the panel is told to navigate; anything else keeps the
//! bar open with a notice, because `FileTable`'s own directory load only
//! logs a failed listing (it has to tolerate a directory vanishing under a
//! live panel) and would otherwise leave the user staring at an unchanged
//! panel with no explanation.
//!
//! # Acceptance criteria, as reworded after S-6
//!
//! The task's original AC ("IME input works") is carried with the two
//! qualifications the S-6 spike established (`documentation/spikes/
//! S-6.md`, `task.md`'s T-4.3.4 deviation note):
//! - **RTL/BiDi**: `gpui-component` 0.5.1's `Input` lays out Arabic/Hebrew
//!   in logical order with no visual reordering. Paths round-trip
//!   byte-exact (S-6 verified) and navigation works; only the *rendering*
//!   of an RTL segment is wrong. Documented 1.0 limitation, not fixable
//!   from this crate (R-G7: the widget is vendored).
//! - **CJK composition** cannot be exercised headlessly (the test platform
//!   has no text-input protocol); it is a named manual check in the UAT
//!   pass, not something this module's tests can claim.
//!
//! The 4000-character paste case is covered: S-6 proved the widget copes,
//! and the header's own layout truncates rather than wraps.

use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use duet_widgets::input::{Escape, IndentInline, Input, InputEvent, InputState, Position};
use duet_widgets::layout::{h_flex, v_flex};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window, div, px,
};

use crate::workspace::{PanelSide, Workspace};

/// Breadcrumb click handler: called with the clicked segment's target.
pub(crate) type SegmentHandler = Rc<dyn Fn(&Path, &mut Window, &mut App)>;
/// "Start editing" handler: the header's empty area or its pencil.
pub(crate) type EditHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One breadcrumb segment: what it shows and where clicking it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub label: String,
    pub target: PathBuf,
}

/// Splits `dir` into breadcrumb segments: a leading `/` segment for the
/// root, then one per component, each carrying the ancestor path it stands
/// for. A relative path (which a panel should never hold, but `PathBuf`
/// allows) simply has no root segment.
pub(crate) fn breadcrumb_segments(dir: &Path) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut so_far = PathBuf::new();
    for component in dir.components() {
        match component {
            Component::RootDir => {
                so_far.push("/");
                segments.push(Segment {
                    label: "/".to_string(),
                    target: so_far.clone(),
                });
            }
            Component::Normal(name) => {
                so_far.push(name);
                segments.push(Segment {
                    label: name.to_string_lossy().into_owned(),
                    target: so_far.clone(),
                });
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    segments
}

/// Keeps the *tail* of `segments` that fits a rough character budget
/// (labels plus a separator each), dropping leading segments behind one
/// `…` placeholder. The last segment always survives, however long: the
/// header truncates it visually rather than hiding where the user is. The
/// budget is in characters, not pixels, because the header's width isn't
/// known at build time; it only needs to be roughly right, and the
/// `truncate()` on the header handles the rest.
pub(crate) fn elide_segments(segments: &[Segment], char_budget: usize) -> Vec<Segment> {
    let Some(last) = segments.last() else {
        return Vec::new();
    };
    let mut used = last.label.chars().count() + 3;
    let mut keep_from = segments.len() - 1;
    while keep_from > 0 {
        let cost = segments[keep_from - 1].label.chars().count() + 3;
        if used + cost > char_budget {
            break;
        }
        used += cost;
        keep_from -= 1;
    }
    if keep_from == 0 {
        return segments.to_vec();
    }
    let mut out = Vec::with_capacity(segments.len() - keep_from + 1);
    out.push(Segment {
        label: "…".to_string(),
        target: segments[keep_from - 1].target.clone(),
    });
    out.extend_from_slice(&segments[keep_from..]);
    out
}

/// Turns what the user typed into the directory to navigate to: `~` /
/// `~/...` against `home`, a relative path against `current`, and `.` /
/// `..` folded lexically (no filesystem access -- symlinks are resolved by
/// the kernel when the panel lists the result, exactly as for any other
/// navigation). Returns `None` for an empty entry.
pub(crate) fn expand_path(typed: &str, current: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    let raw: PathBuf = if typed == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = typed.strip_prefix("~/") {
        home?.join(rest)
    } else if typed.starts_with('/') {
        PathBuf::from(typed)
    } else {
        current.join(typed)
    };
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    Some(out)
}

/// The longest prefix shared by every name in `names` (the whole name when
/// there is exactly one), on `char` boundaries.
pub(crate) fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in &names[1..] {
        let shared = prefix
            .iter()
            .zip(name.chars())
            .take_while(|(a, b)| *a == b)
            .count();
        prefix.truncate(shared);
    }
    prefix.into_iter().collect()
}

/// What a Tab found: the completed field value to set (if anything) and
/// the candidate names to show (empty unless the match was ambiguous).
/// Pure so it can be unit-tested against a plain name list.
pub(crate) fn completion_for(
    parent: &Path,
    prefix: &str,
    dir_names: &[String],
) -> (Option<String>, Vec<String>) {
    let mut matches: Vec<String> = dir_names
        .iter()
        .filter(|name| name.starts_with(prefix))
        .cloned()
        .collect();
    matches.sort();
    match matches.len() {
        0 => (None, Vec::new()),
        1 => {
            let mut value = parent.join(&matches[0]).to_string_lossy().into_owned();
            if !value.ends_with('/') {
                value.push('/');
            }
            (Some(value), Vec::new())
        }
        _ => {
            let shared = common_prefix(&matches);
            let value = if shared.chars().count() > prefix.chars().count() {
                Some(parent.join(&shared).to_string_lossy().into_owned())
            } else {
                None
            };
            (value, matches)
        }
    }
}

/// Splits the field's current text into a candidate parent directory and
/// the partial last segment to complete. A trailing `/` means the user
/// has started the next segment but typed nothing of it yet.
pub(crate) fn split_parent_prefix(text: &str) -> Option<(PathBuf, String)> {
    let path = Path::new(text);
    if text.ends_with('/') {
        return Some((path.to_path_buf(), String::new()));
    }
    let parent = path.parent()?;
    let prefix = path.file_name()?.to_str()?;
    Some((parent.to_path_buf(), prefix.to_string()))
}

/// A path bar in its *editing* face, owned by `Workspace` while open
/// (`Workspace::path_bar`), one at a time -- opening it on the other panel
/// closes this one first.
pub(crate) struct PathBarState {
    side: PanelSide,
    /// The directory the bar opened on: what relative entries resolve
    /// against, and what `Esc` leaves untouched.
    current: PathBuf,
    input: Entity<InputState>,
    /// Ambiguous-Tab hint: the directory names the last completion attempt
    /// found. Cleared on every edit.
    candidates: Vec<String>,
    /// Why the last `Enter` was refused ("Not a directory: ..."), shown
    /// under the input until the next edit. Inline rather than a toast:
    /// the user is looking at the field they need to fix.
    error: Option<String>,
    /// The value this bar last wrote into its own input (a completion).
    /// `InputState::set_value` emits `Change` like a keystroke would, and
    /// that event arrives *after* the completion has stored its candidate
    /// hint -- so a `Change` matching this value is ours and must not
    /// clear the hint the way a real edit does.
    programmatic_value: Option<String>,
    workspace: WeakEntity<Workspace>,
    tokio_handle: tokio::runtime::Handle,
    _subscription: Subscription,
}

impl PathBarState {
    pub(crate) fn new(
        side: PanelSide,
        current: PathBuf,
        workspace: WeakEntity<Workspace>,
        tokio_handle: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = current.to_string_lossy().into_owned();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Type a directory path");
            state.set_value(initial.clone(), window, cx);
            state.set_cursor_position(Position::new(0, u32::MAX), window, cx);
            state
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let subscription = cx.subscribe_in(&input, window, Self::on_input_event);
        Self {
            side,
            current,
            input,
            candidates: Vec::new(),
            error: None,
            programmatic_value: None,
            workspace,
            tokio_handle,
            _subscription: subscription,
        }
    }

    pub(crate) fn side(&self) -> PanelSide {
        self.side
    }

    pub(crate) fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn candidates(&self) -> &[String] {
        &self.candidates
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether keyboard focus is currently inside the bar's own input.
    #[cfg(test)]
    pub(crate) fn is_focused(&self, window: &Window, cx: &App) -> bool {
        use gpui::Focusable as _;
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Test seam: replaces the field's text as if typed.
    #[cfg(test)]
    pub(crate) fn set_value_for_test(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
            state.set_cursor_position(Position::new(0, u32::MAX), window, cx);
        });
    }

    fn on_input_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.confirm(window, cx),
            InputEvent::Change => {
                let current = self.value(cx);
                if self
                    .programmatic_value
                    .take()
                    .is_some_and(|ours| ours == current)
                {
                    return;
                }
                if !self.candidates.is_empty() || self.error.is_some() {
                    self.candidates.clear();
                    self.error = None;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| workspace.close_path_bar(window, cx));
    }

    /// `Enter`: resolve what was typed ([`expand_path`]), check off the UI
    /// thread that it is a directory, then navigate the panel and close --
    /// or keep the bar open with a notice saying why not.
    pub(crate) fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.value(cx);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let Some(target) = expand_path(&typed, &self.current, home.as_deref()) else {
            self.cancel(_window, cx);
            return;
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let probe = target.clone();
        self.tokio_handle.spawn(async move {
            let is_dir = std::fs::metadata(&probe)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            let _ = tx.send(is_dir);
        });
        let side = self.side;
        let workspace = self.workspace.clone();
        let this = cx.entity().downgrade();
        cx.spawn_in(_window, async move |_, cx| {
            let is_dir = rx.await.unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| {
                if is_dir {
                    let _ = workspace.update(cx, |workspace, cx| {
                        workspace.navigate_panel_to_path(side, target.clone(), window, cx);
                        workspace.close_path_bar(window, cx);
                    });
                } else {
                    this.error = Some(format!("Not a directory: {}", target.display()));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// `Tab`: see the module doc comment's "Completion" section.
    pub(crate) fn try_complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.value(cx);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        // Complete against the expanded form, so `~/Doc<Tab>` works, but
        // keep `~` in what the user sees only if nothing was completed.
        let Some(expanded) = expand_path(&typed, &self.current, home.as_deref()) else {
            return;
        };
        let expanded_text = if typed.ends_with('/') && expanded.as_os_str() != "/" {
            format!("{}/", expanded.display())
        } else {
            expanded.display().to_string()
        };
        let Some((parent, prefix)) = split_parent_prefix(&expanded_text) else {
            return;
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
        let list_dir = parent.clone();
        self.tokio_handle.spawn(async move {
            let mut names = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&list_dir) {
                for entry in entries.flatten() {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        || std::fs::metadata(entry.path())
                            .map(|m| m.is_dir())
                            .unwrap_or(false);
                    if is_dir && let Ok(name) = entry.file_name().into_string() {
                        names.push(name);
                    }
                }
            }
            let _ = tx.send(names);
        });
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let names = rx.await.unwrap_or_default();
            let _ = this.update_in(cx, |this, window, cx| {
                let (completed, candidates) = completion_for(&parent, &prefix, &names);
                if let Some(value) = completed {
                    this.programmatic_value = Some(value.clone());
                    this.input.update(cx, |state, cx| {
                        state.set_value(value.clone(), window, cx);
                        state.set_cursor_position(Position::new(0, u32::MAX), window, cx);
                    });
                }
                this.candidates = candidates;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for PathBarState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = TokenPalette::current(cx);
        let error: Option<SharedString> = self.error.clone().map(SharedString::from);
        let hint: Option<SharedString> = (!self.candidates.is_empty()).then(|| {
            let shown: Vec<&str> = self.candidates.iter().take(8).map(String::as_str).collect();
            let more = self.candidates.len().saturating_sub(shown.len());
            let mut text = shown.join("  ");
            if more > 0 {
                text.push_str(&format!("  … +{more}"));
            }
            text.into()
        });
        v_flex()
            .w_full()
            .key_context("PathBar")
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.try_complete(window, cx);
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Input::new(&self.input)),
            )
            .when_some(hint, |this, hint| {
                this.child(
                    div()
                        .px_1()
                        .text_size(px(10.))
                        .text_color(tokens.color.statusbar_fg)
                        .truncate()
                        .child(hint),
                )
            })
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .px_1()
                        .text_size(px(10.))
                        .text_color(tokens.color.error)
                        .truncate()
                        .child(error),
                )
            })
    }
}

/// Rough number of characters a panel header can show before the leading
/// segments get elided -- see [`elide_segments`].
const BREADCRUMB_CHAR_BUDGET: usize = 72;

/// The breadcrumb face of the header. `on_segment` is invoked with the
/// clicked segment's target; `on_edit` when the header is clicked anywhere
/// else (or the pencil at its end).
pub(crate) fn breadcrumb_header(
    side: PanelSide,
    dir: &Path,
    tokens: &TokenPalette,
    on_segment: SegmentHandler,
    on_edit: EditHandler,
) -> impl IntoElement {
    let side_tag = match side {
        PanelSide::Left => "left",
        PanelSide::Right => "right",
    };
    let segments = elide_segments(&breadcrumb_segments(dir), BREADCRUMB_CHAR_BUDGET);
    let accent = tokens.color.accent;
    let muted = tokens.color.statusbar_fg;
    let hover_bg = tokens.color.tab_active_bg;
    let count = segments.len();
    let on_edit_for_bar = on_edit.clone();
    h_flex()
        .id(SharedString::from(format!("path-bar-{side_tag}")))
        .w_full()
        .items_center()
        .gap_0()
        .overflow_hidden()
        .cursor_text()
        .on_click(move |_, window, cx| on_edit_for_bar(window, cx))
        .children(segments.into_iter().enumerate().map(|(i, segment)| {
            let target = segment.target.clone();
            let on_segment = on_segment.clone();
            let is_last = i + 1 == count;
            h_flex()
                .items_center()
                .child(
                    div()
                        .id(SharedString::from(format!("crumb-{side_tag}-{i}")))
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(is_last, |this| this.text_color(accent))
                        .hover(move |style| style.bg(hover_bg))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            on_segment(&target, window, cx);
                        })
                        .child(SharedString::from(segment.label)),
                )
                .when(!is_last && segment.target.as_os_str() != "/", |this| {
                    this.child(div().text_color(muted).child("/"))
                })
        }))
        .child(div().flex_1())
        .child(
            div()
                .id(SharedString::from(format!("path-bar-edit-{side_tag}")))
                .px_1()
                .text_color(muted)
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_edit(window, cx);
                })
                .child("✎"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(label: &str, target: &str) -> Segment {
        Segment {
            label: label.to_string(),
            target: PathBuf::from(target),
        }
    }

    #[test]
    fn breadcrumb_segments_walk_every_component_with_cumulative_targets() {
        assert_eq!(
            breadcrumb_segments(Path::new("/home/meirk/projects")),
            vec![
                seg("/", "/"),
                seg("home", "/home"),
                seg("meirk", "/home/meirk"),
                seg("projects", "/home/meirk/projects"),
            ]
        );
        assert_eq!(breadcrumb_segments(Path::new("/")), vec![seg("/", "/")]);
    }

    #[test]
    fn elide_keeps_the_tail_and_always_the_last_segment() {
        let segments = breadcrumb_segments(Path::new("/a/bb/ccc/dddd/eeeee"));
        let elided = elide_segments(&segments, 16);
        assert_eq!(elided[0].label, "…");
        assert_eq!(
            elided[0].target,
            PathBuf::from("/a/bb/ccc"),
            "the ellipsis navigates to the last hidden ancestor"
        );
        assert_eq!(
            elided.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(),
            vec!["…", "dddd", "eeeee"]
        );
        let tiny = elide_segments(&segments, 1);
        assert_eq!(tiny.last().unwrap().label, "eeeee");
        assert_eq!(elide_segments(&segments, 1000), segments);
        assert!(elide_segments(&[], 10).is_empty());
    }

    #[test]
    fn expand_path_handles_home_relative_and_dot_segments() {
        let home = Path::new("/home/meirk");
        let cur = Path::new("/home/meirk/projects");
        assert_eq!(expand_path("~", cur, Some(home)), Some(home.to_path_buf()));
        assert_eq!(
            expand_path("~/docs", cur, Some(home)),
            Some(PathBuf::from("/home/meirk/docs"))
        );
        assert_eq!(
            expand_path("../", cur, Some(home)),
            Some(home.to_path_buf())
        );
        assert_eq!(
            expand_path("./duet2/./crates", cur, Some(home)),
            Some(PathBuf::from("/home/meirk/projects/duet2/crates"))
        );
        assert_eq!(
            expand_path("/../..", cur, Some(home)),
            Some(PathBuf::from("/"))
        );
        assert_eq!(expand_path("   ", cur, Some(home)), None);
        assert_eq!(expand_path("~", cur, None), None, "no $HOME, no expansion");
    }

    #[test]
    fn completion_for_covers_unique_ambiguous_and_none() {
        let names: Vec<String> = ["alpha", "alphabet", "beta"]
            .into_iter()
            .map(String::from)
            .collect();
        let parent = Path::new("/tmp/x");
        assert_eq!(
            completion_for(parent, "b", &names),
            (Some("/tmp/x/beta/".to_string()), Vec::new())
        );
        let (value, candidates) = completion_for(parent, "al", &names);
        assert_eq!(value, Some("/tmp/x/alpha".to_string()));
        assert_eq!(
            candidates,
            vec!["alpha".to_string(), "alphabet".to_string()]
        );
        let (value, candidates) = completion_for(parent, "alpha", &names);
        assert_eq!(value, None, "already at the common prefix: nothing to add");
        assert_eq!(candidates.len(), 2);
        assert_eq!(completion_for(parent, "zzz", &names), (None, Vec::new()));
        let (value, _) = completion_for(parent, "", &names);
        assert_eq!(value, None, "three names share no prefix");
    }

    #[test]
    fn split_parent_prefix_matches_the_copy_dialog_convention() {
        assert_eq!(
            split_parent_prefix("/usr/sh"),
            Some((PathBuf::from("/usr"), "sh".to_string()))
        );
        assert_eq!(
            split_parent_prefix("/usr/share/"),
            Some((PathBuf::from("/usr/share/"), String::new()))
        );
        assert_eq!(
            split_parent_prefix("/"),
            Some((PathBuf::from("/"), String::new()))
        );
    }
}
