// SPDX-License-Identifier: MIT
//! T-5.3.6: the internal viewer (F3, FR-TOOL-01, design.md §9.6) -- a
//! full-window overlay the workspace owns like its dialogs, showing the
//! cursor file as text, binary, hex or an image.
//!
//! Layout of the module:
//! - `source.rs`, `text.rs`, `index.rs`, `search.rs`, `hex.rs`: the pure
//!   engine (no gpui, no threads), unit-tested over in-memory bytes.
//! - `worker.rs`: the one blocking task per open file that runs the
//!   engine; the only code that reads the file.
//! - this file: `ViewerState`, the gpui entity -- key handling, the
//!   request/reply dance with the worker, and rendering (`render.rs`).
//!
//! The viewer never waits for the whole file: a page is a byte-offset
//! anchor plus as many rows as fit (`text::Pager`), so a 5 GB log and
//! a 200 MB single-line file open and jump to the end the same way a
//! small one does. The line index that makes "line N of M" and
//! `Ctrl+G <line>` possible is built in the background and is optional.
//!
//! T-5.3.8's quick view (Ctrl+Q, FR-TOOL-02) is the same entity in an
//! embedded flavour ([`ViewerState::new_embedded`]): it sits in place of
//! the inactive panel, follows the other panel's cursor through
//! [`ViewerState::show`], builds no line index, and Esc or Tab hand the
//! keyboard back to the file panel.
//!
//! Keys (design.md Appendix A, viewer context): Esc/F3 close, Space /
//! PgDn and Backspace / PgUp page, arrows line and column, Home/End,
//! Ctrl+F (and F7) find, Ctrl+G goto, Ctrl+PgDn/PgUp next/previous file,
//! W wrap, Ctrl+1/2/3/4 text/hex/binary/image. Duet additions: Ctrl+E
//! cycles the encoding override; inside the find bar Enter finds the next
//! hit and Shift+Enter the previous.

pub(crate) mod hex;
pub(crate) mod index;
mod render;
pub(crate) mod search;
pub(crate) mod source;
pub(crate) mod text;
pub(crate) mod worker;

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use duet_widgets::input::{InputEvent, InputState};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, KeyBinding, Pixels, RenderImage,
    Size, Subscription, WeakEntity, Window, actions, px,
};

use crate::workspace::{PanelSide, Workspace, yield_once};
use hex::{BINARY_WIDTH, HEX_WIDTHS, HexLayout, HexRow};
use text::{Detected, Page, Position, TextEncoding};
use worker::{Anchor, Reply, Request, WorkerHandle};

actions!(
    duet_viewer,
    [
        ViewerClose,
        ViewerPageDown,
        ViewerPageUp,
        ViewerLineDown,
        ViewerLineUp,
        ViewerHome,
        ViewerEnd,
        ViewerLeft,
        ViewerRight,
        ViewerFind,
        ViewerFindPrev,
        ViewerGoto,
        ViewerWrap,
        ViewerModeText,
        ViewerModeHex,
        ViewerModeBinary,
        ViewerModeImage,
        ViewerEncoding,
        ViewerNextFile,
        ViewerPrevFile,
    ]
);

pub(crate) fn bind_viewer_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", ViewerClose, Some("Viewer")),
        KeyBinding::new("f3", ViewerClose, Some("Viewer")),
        KeyBinding::new("space", ViewerPageDown, Some("Viewer")),
        KeyBinding::new("pagedown", ViewerPageDown, Some("Viewer")),
        KeyBinding::new("backspace", ViewerPageUp, Some("Viewer")),
        KeyBinding::new("pageup", ViewerPageUp, Some("Viewer")),
        KeyBinding::new("down", ViewerLineDown, Some("Viewer")),
        KeyBinding::new("up", ViewerLineUp, Some("Viewer")),
        KeyBinding::new("home", ViewerHome, Some("Viewer")),
        KeyBinding::new("ctrl-home", ViewerHome, Some("Viewer")),
        KeyBinding::new("end", ViewerEnd, Some("Viewer")),
        KeyBinding::new("ctrl-end", ViewerEnd, Some("Viewer")),
        KeyBinding::new("left", ViewerLeft, Some("Viewer")),
        KeyBinding::new("right", ViewerRight, Some("Viewer")),
        KeyBinding::new("ctrl-f", ViewerFind, Some("Viewer")),
        KeyBinding::new("f7", ViewerFind, Some("Viewer")),
        KeyBinding::new("shift-enter", ViewerFindPrev, Some("Viewer > Input")),
        KeyBinding::new("ctrl-g", ViewerGoto, Some("Viewer")),
        KeyBinding::new("w", ViewerWrap, Some("Viewer")),
        KeyBinding::new("ctrl-1", ViewerModeText, Some("Viewer")),
        KeyBinding::new("ctrl-2", ViewerModeHex, Some("Viewer")),
        KeyBinding::new("ctrl-3", ViewerModeBinary, Some("Viewer")),
        KeyBinding::new("ctrl-4", ViewerModeImage, Some("Viewer")),
        KeyBinding::new("ctrl-e", ViewerEncoding, Some("Viewer")),
        KeyBinding::new("ctrl-pagedown", ViewerNextFile, Some("Viewer")),
        KeyBinding::new("ctrl-pageup", ViewerPrevFile, Some("Viewer")),
    ]);
}

/// Monospace metrics; fixed, so a page's row count is viewport height
/// over [`LINE_HEIGHT`].
pub(crate) const FONT_SIZE: Pixels = px(13.);
pub(crate) const LINE_HEIGHT: Pixels = px(18.);
/// Columns a Left/Right press moves in no-wrap mode.
const H_STEP: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Text,
    Binary,
    Hex,
    Image,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Mode::Text => "Text",
            Mode::Binary => "Binary",
            Mode::Hex => "Hex",
            Mode::Image => "Image",
        }
    }
}

/// Which bar sits above the status line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bar {
    None,
    Find,
    Goto,
}

/// The last text page received and what it was requested for.
pub(crate) struct TextView {
    pub(crate) page: Page,
    /// The encoding the page was decoded with (the override, or the
    /// detected one).
    pub(crate) encoding: TextEncoding,
    pub(crate) line: Option<u64>,
    pub(crate) total_lines: Option<u64>,
}

pub(crate) struct HexView {
    pub(crate) rows: Vec<HexRow>,
    pub(crate) first_row: u64,
    pub(crate) row_count: u64,
}

/// What the quick view should show for the cursor entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum QuickTarget {
    File(PathBuf),
    Directory(PathBuf),
    Nothing,
}

pub(crate) struct ViewerState {
    /// The quick view flavour -- see the module doc comment.
    embedded: bool,
    /// Shown instead of a page: a directory under the cursor, or none.
    placeholder: Option<String>,
    dir: PathBuf,
    /// Regular files of the panel in display order; Ctrl+PgDn/PgUp walk
    /// them.
    files: Vec<String>,
    file_ix: usize,
    side: PanelSide,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,

    worker: Option<WorkerHandle>,
    len: u64,
    detected: Option<Detected>,
    mime: Option<&'static str>,
    /// `None` until the file's head has been sniffed.
    mode: Option<Mode>,
    /// The user picked a mode; auto-detection no longer overrides it on
    /// the next file.
    mode_forced: bool,
    encoding_override: Option<TextEncoding>,
    wrap: bool,
    h_offset: usize,
    hex_layout: HexLayout,

    top: Position,
    text: Option<TextView>,
    hex_first_row: u64,
    hex: Option<HexView>,
    image: Option<(Arc<RenderImage>, u32, u32)>,
    index_progress: Option<f32>,
    status: Option<String>,

    viewport: Size<Pixels>,
    rows_visible: usize,
    cols_visible: usize,
    ch_width: Option<Pixels>,

    bar: Bar,
    find_input: Entity<InputState>,
    goto_input: Entity<InputState>,
    fold_case: bool,
    highlight: Option<Range<u64>>,
    search_progress: Option<f32>,
    _subscriptions: Vec<Subscription>,
}

impl ViewerState {
    /// The F3 viewer on `files[file_ix]` in `dir`.
    pub(crate) fn new(
        dir: PathBuf,
        files: Vec<String>,
        file_ix: usize,
        side: PanelSide,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::build(dir, files, file_ix, side, workspace, false, window, cx);
        this.open_current(cx);
        this
    }

    /// The quick view, showing nothing until [`Self::show`]. `side` is
    /// the panel it replaces.
    pub(crate) fn new_embedded(
        side: PanelSide,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::build(
            PathBuf::new(),
            Vec::new(),
            0,
            side,
            workspace,
            true,
            window,
            cx,
        );
        this.placeholder = Some("No file under the cursor".to_string());
        this
    }

    #[allow(clippy::too_many_arguments)] // every argument is a distinct piece of panel context
    fn build(
        dir: PathBuf,
        files: Vec<String>,
        file_ix: usize,
        side: PanelSide,
        workspace: WeakEntity<Workspace>,
        embedded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let find_input = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let goto_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Line, 50%, or 0x1F offset"));
        let _subscriptions = vec![
            cx.subscribe_in(&find_input, window, Self::on_find_event),
            cx.subscribe_in(&goto_input, window, Self::on_goto_event),
        ];
        Self {
            embedded,
            placeholder: None,
            dir,
            files,
            file_ix,
            side,
            workspace,
            focus_handle: cx.focus_handle(),
            worker: None,
            len: 0,
            detected: None,
            mime: None,
            mode: None,
            mode_forced: false,
            encoding_override: None,
            wrap: false,
            h_offset: 0,
            hex_layout: HexLayout {
                width: 16,
                decimal: false,
                chars_only: false,
            },
            top: Position {
                row_start: 0,
                piece: 0,
            },
            text: None,
            hex_first_row: 0,
            hex: None,
            image: None,
            index_progress: None,
            status: None,
            viewport: Size::default(),
            rows_visible: 40,
            cols_visible: 120,
            ch_width: None,
            bar: Bar::None,
            find_input,
            goto_input,
            fold_case: true,
            highlight: None,
            search_progress: None,
            _subscriptions,
        }
    }

    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.files.get(self.file_ix).map(|name| self.dir.join(name))
    }

    pub(crate) fn file_name(&self) -> &str {
        self.files.get(self.file_ix).map_or("", String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn embedded(&self) -> bool {
        self.embedded
    }

    /// Quick view: show `target`, replacing whatever is shown. A new
    /// file reopens the worker; a directory or nothing shows a label.
    pub(crate) fn show(&mut self, target: QuickTarget, cx: &mut Context<Self>) {
        match target {
            QuickTarget::File(path) => {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return;
                };
                self.dir = parent.to_path_buf();
                self.files = vec![name.to_string_lossy().into_owned()];
                self.file_ix = 0;
                self.placeholder = None;
                self.open_current(cx);
            }
            QuickTarget::Directory(path) => {
                self.clear_file(cx);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.placeholder = Some(format!("{name}/\ndirectory"));
                cx.notify();
            }
            QuickTarget::Nothing => {
                self.clear_file(cx);
                self.placeholder = Some("No file under the cursor".to_string());
                cx.notify();
            }
        }
    }

    fn clear_file(&mut self, _cx: &mut Context<Self>) {
        self.worker = None;
        self.files.clear();
        self.file_ix = 0;
        self.len = 0;
        self.detected = None;
        self.mime = None;
        if !self.mode_forced {
            self.mode = None;
        }
        self.text = None;
        self.hex = None;
        self.image = None;
        self.highlight = None;
        self.status = None;
        self.search_progress = None;
        self.index_progress = None;
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> Option<Mode> {
        self.mode
    }

    pub(crate) fn encoding(&self) -> Option<TextEncoding> {
        self.encoding_override
            .or_else(|| self.detected.map(|d| d.encoding))
    }

    #[cfg(test)]
    pub(crate) fn wrap(&self) -> bool {
        self.wrap
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn highlight(&self) -> Option<Range<u64>> {
        self.highlight.clone()
    }

    #[cfg(test)]
    pub(crate) fn find_bar_open(&self) -> bool {
        self.bar == Bar::Find
    }

    #[cfg(test)]
    pub(crate) fn find_input(&self) -> &Entity<InputState> {
        &self.find_input
    }

    #[cfg(test)]
    pub(crate) fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    // -- opening -----------------------------------------------------------

    /// (Re)opens the current file: a fresh worker, state reset, the
    /// first page requested once the head has been sniffed.
    fn open_current(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path() else {
            return;
        };
        self.worker = None;
        self.len = 0;
        self.detected = None;
        self.mime = None;
        if !self.mode_forced {
            self.mode = None;
        }
        self.top = Position {
            row_start: 0,
            piece: 0,
        };
        self.h_offset = 0;
        self.text = None;
        self.hex_first_row = 0;
        self.hex = None;
        self.image = None;
        self.index_progress = None;
        self.status = None;
        self.highlight = None;
        self.search_progress = None;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Reply>();
        let handle = worker::spawn(path, tx, !self.embedded);
        self.worker = Some(handle);
        cx.spawn(async move |this, cx| {
            while let Some(reply) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_reply(reply, cx))
                    .is_err()
                {
                    return;
                }
                yield_once().await;
            }
        })
        .detach();
        cx.notify();
    }

    fn on_reply(&mut self, reply: Reply, cx: &mut Context<Self>) {
        match reply {
            Reply::Opened {
                len,
                detected,
                mime,
            } => {
                self.len = len;
                self.detected = Some(detected);
                self.mime = mime;
                if self.mode.is_none() {
                    self.mode = Some(self.auto_mode());
                }
                if detected.bom > 0 {
                    self.top = Position {
                        row_start: detected.bom,
                        piece: 0,
                    };
                }
                self.request_current(cx);
            }
            Reply::Text {
                page,
                encoding,
                line,
                total_lines,
            } => {
                self.top = page.top;
                self.text = Some(TextView {
                    page,
                    encoding,
                    line,
                    total_lines,
                });
            }
            Reply::Hex {
                rows,
                first_row,
                row_count,
            } => {
                self.hex_first_row = first_row;
                self.hex = Some(HexView {
                    rows,
                    first_row,
                    row_count,
                });
            }
            Reply::Image {
                image,
                width,
                height,
            } => {
                self.image = Some((image, width, height));
            }
            Reply::SearchHit(range) => {
                self.search_progress = None;
                self.status = None;
                self.highlight = Some(range.clone());
                self.show_offset(range.start, cx);
            }
            Reply::SearchMiss => {
                self.search_progress = None;
                self.status = Some("Not found".to_string());
            }
            Reply::SearchProgress(fraction) => {
                self.search_progress = Some(fraction);
            }
            Reply::IndexProgress { fraction, complete } => {
                self.index_progress = (!complete).then_some(fraction);
                if complete && self.mode == Some(Mode::Text) {
                    // Line numbers are final now.
                    self.request_text(Anchor::At(self.top), cx);
                    return;
                }
            }
            Reply::Error(message) => {
                self.search_progress = None;
                self.status = Some(message);
            }
        }
        cx.notify();
    }

    /// Image for `image/*`, hex for binary, text otherwise.
    fn auto_mode(&self) -> Mode {
        if self.mime.is_some_and(|m| m.starts_with("image/")) {
            Mode::Image
        } else if self.detected.is_some_and(|d| d.binary) {
            Mode::Hex
        } else {
            Mode::Text
        }
    }

    // -- requests ----------------------------------------------------------

    fn request_current(&mut self, cx: &mut Context<Self>) {
        match self.mode {
            Some(Mode::Text) => self.request_text(Anchor::At(self.top), cx),
            Some(Mode::Hex) | Some(Mode::Binary) => self.request_hex(self.hex_first_row, cx),
            Some(Mode::Image) => {
                if self.image.is_none()
                    && let Some(worker) = &self.worker
                {
                    worker.send(Request::Image);
                }
            }
            None => {}
        }
        cx.notify();
    }

    fn request_text(&mut self, anchor: Anchor, _cx: &mut Context<Self>) {
        let Some(worker) = &self.worker else {
            return;
        };
        worker.send(Request::TextPage {
            anchor,
            rows: self.rows_visible.max(1),
            cols: self.wrap.then_some(self.cols_visible.max(1)),
            encoding: self.encoding_override,
            highlight: self.highlight.clone(),
        });
    }

    fn request_hex(&mut self, first_row: u64, _cx: &mut Context<Self>) {
        let Some(worker) = &self.worker else {
            return;
        };
        worker.send(Request::HexPage {
            first_row,
            rows: self.rows_visible.max(1),
            layout: self.current_hex_layout(),
            highlight: self.highlight.clone(),
        });
    }

    fn current_hex_layout(&self) -> HexLayout {
        if self.mode == Some(Mode::Binary) {
            HexLayout {
                width: BINARY_WIDTH,
                decimal: self.hex_layout.decimal,
                chars_only: true,
            }
        } else {
            HexLayout {
                chars_only: false,
                ..self.hex_layout
            }
        }
    }

    /// Brings the byte at `offset` on screen: in text mode the page
    /// anchored on its row (unless it is already visible), in hex mode
    /// its row.
    fn show_offset(&mut self, offset: u64, cx: &mut Context<Self>) {
        match self.mode {
            Some(Mode::Text) => {
                let visible = self.text.as_ref().is_some_and(|t| {
                    let rows = &t.page.rows;
                    match (rows.first(), rows.last()) {
                        (Some(first), Some(last)) => {
                            offset >= first.row.start && offset < last.row.end
                        }
                        _ => false,
                    }
                });
                let anchor = if visible {
                    Anchor::At(self.top)
                } else {
                    Anchor::Offset(offset)
                };
                self.request_text(anchor, cx);
            }
            Some(Mode::Hex) | Some(Mode::Binary) => {
                let width = self.current_hex_layout().width as u64;
                let row = offset / width;
                let visible = self
                    .hex
                    .as_ref()
                    .is_some_and(|h| row >= h.first_row && row < h.first_row + h.rows.len() as u64);
                let first = if visible { self.hex_first_row } else { row };
                self.request_hex(first, cx);
            }
            _ => {}
        }
    }

    /// Called from the render-time canvas with the body's size.
    fn record_viewport(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        if (self.viewport.width - size.width).abs() < px(1.0)
            && (self.viewport.height - size.height).abs() < px(1.0)
        {
            return;
        }
        self.viewport = size;
        let rows = (f32::from(size.height) / f32::from(LINE_HEIGHT)).floor() as usize;
        let ch = self.ch_width.map_or(8.0, f32::from).max(1.0);
        let cols = ((f32::from(size.width) - 16.0) / ch).floor() as usize;
        let changed = rows != self.rows_visible || cols != self.cols_visible;
        self.rows_visible = rows.max(1);
        self.cols_visible = cols.max(1);
        if changed {
            self.request_current(cx);
        }
    }

    // -- navigation ---------------------------------------------------------

    fn scroll_rows(&mut self, delta: i64, cx: &mut Context<Self>) {
        match self.mode {
            Some(Mode::Text) => self.request_text(
                Anchor::Scroll {
                    from: self.top,
                    delta,
                },
                cx,
            ),
            Some(Mode::Hex) | Some(Mode::Binary) => {
                let first = self.hex_first_row.saturating_add_signed(delta);
                self.request_hex(first, cx);
            }
            _ => {}
        }
    }

    fn page_down(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows_visible.saturating_sub(1).max(1) as i64;
        self.scroll_rows(rows, cx);
    }

    fn page_up(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows_visible.saturating_sub(1).max(1) as i64;
        self.scroll_rows(-rows, cx);
    }

    fn go_home(&mut self, cx: &mut Context<Self>) {
        self.h_offset = 0;
        match self.mode {
            Some(Mode::Text) => self.request_text(Anchor::Start, cx),
            Some(Mode::Hex) | Some(Mode::Binary) => self.request_hex(0, cx),
            _ => {}
        }
    }

    fn go_end(&mut self, cx: &mut Context<Self>) {
        match self.mode {
            Some(Mode::Text) => self.request_text(Anchor::End, cx),
            Some(Mode::Hex) | Some(Mode::Binary) => self.request_hex(u64::MAX, cx),
            _ => {}
        }
    }

    fn scroll_left(&mut self, cx: &mut Context<Self>) {
        if self.h_offset > 0 {
            self.h_offset = self.h_offset.saturating_sub(H_STEP);
            cx.notify();
        }
    }

    fn scroll_right(&mut self, cx: &mut Context<Self>) {
        if self.mode == Some(Mode::Text) && !self.wrap {
            self.h_offset += H_STEP;
            cx.notify();
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode_forced = true;
        if self.mode == Some(mode) {
            return;
        }
        // Keep the place: carry the byte offset across modes.
        let offset = self.current_offset();
        self.mode = Some(mode);
        self.h_offset = 0;
        match mode {
            Mode::Text => {
                self.top = Position {
                    row_start: offset,
                    piece: 0,
                };
                self.request_text(Anchor::Offset(offset), cx);
            }
            Mode::Hex | Mode::Binary => {
                let width = self.current_hex_layout().width as u64;
                self.hex_first_row = offset / width;
                self.request_hex(self.hex_first_row, cx);
            }
            Mode::Image => self.request_current(cx),
        }
        cx.notify();
    }

    /// Auto mode again: re-detect for this file.
    fn set_mode_auto(&mut self, cx: &mut Context<Self>) {
        self.mode_forced = false;
        let mode = self.auto_mode();
        self.mode_forced = false;
        if self.mode != Some(mode) {
            self.set_mode(mode, cx);
            self.mode_forced = false;
        }
    }

    /// The byte offset of the top of the screen.
    pub(crate) fn current_offset(&self) -> u64 {
        match self.mode {
            Some(Mode::Text) | None => self.top.row_start,
            Some(Mode::Hex) | Some(Mode::Binary) => {
                self.hex_first_row * self.current_hex_layout().width as u64
            }
            Some(Mode::Image) => 0,
        }
    }

    fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        self.h_offset = 0;
        if self.mode == Some(Mode::Text) {
            // Re-anchor on the row; the piece index changes meaning.
            let offset = self.top.row_start;
            self.request_text(Anchor::Offset(offset), cx);
        }
        cx.notify();
    }

    fn cycle_encoding(&mut self, cx: &mut Context<Self>) {
        let current = self.encoding();
        let Some(current) = current else {
            return;
        };
        self.encoding_override = Some(current.next());
        if self.mode == Some(Mode::Text) {
            let offset = self.top.row_start;
            self.request_text(Anchor::Offset(offset), cx);
        }
        cx.notify();
    }

    fn cycle_hex_width(&mut self, cx: &mut Context<Self>) {
        let pos = HEX_WIDTHS
            .iter()
            .position(|w| *w == self.hex_layout.width)
            .unwrap_or(0);
        let offset = self.current_offset();
        self.hex_layout.width = HEX_WIDTHS[(pos + 1) % HEX_WIDTHS.len()];
        if self.mode == Some(Mode::Hex) {
            self.hex_first_row = offset / self.hex_layout.width as u64;
            self.request_hex(self.hex_first_row, cx);
        }
        cx.notify();
    }

    fn toggle_offset_base(&mut self, cx: &mut Context<Self>) {
        self.hex_layout.decimal = !self.hex_layout.decimal;
        if matches!(self.mode, Some(Mode::Hex) | Some(Mode::Binary)) {
            self.request_hex(self.hex_first_row, cx);
        }
        cx.notify();
    }

    fn step_file(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.file_ix.checked_add_signed(delta) else {
            return;
        };
        if next >= self.files.len() {
            return;
        }
        self.file_ix = next;
        self.close_bar(window, cx);
        self.open_current(cx);
        let name = self.files[next].clone();
        let side = self.side;
        let _ = self.workspace.update(cx, |workspace, cx| {
            workspace.viewer_moved_to(side, &name, cx);
        });
    }

    // -- find / goto bars --------------------------------------------------

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bar = Bar::Find;
        self.status = None;
        self.find_input
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn open_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bar = Bar::Goto;
        self.status = None;
        self.goto_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn close_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bar == Bar::None {
            return;
        }
        self.bar = Bar::None;
        if let Some(worker) = &self.worker {
            worker
                .search_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.search_progress = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn on_find_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.find(false, cx);
        }
    }

    fn on_goto_event(
        &mut self,
        _emitter: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            let text = self.goto_input.read(cx).value().to_string();
            self.goto(text.trim(), cx);
            self.close_bar(window, cx);
        }
    }

    /// Searches from just after the current hit (or the top of the
    /// screen) forward, or from the current hit backward.
    pub(crate) fn find(&mut self, backward: bool, cx: &mut Context<Self>) {
        let query = self.find_input.read(cx).value().to_string();
        if query.is_empty() {
            return;
        }
        let Some(worker) = &self.worker else {
            return;
        };
        let from = match (&self.highlight, backward) {
            (Some(hit), false) => hit.start + 1,
            (Some(hit), true) => hit.start,
            (None, false) => self.current_offset(),
            (None, true) => self.len,
        };
        self.status = None;
        self.search_progress = Some(0.0);
        worker.send(Request::Search {
            query,
            from,
            backward,
            fold_case: self.fold_case,
            encoding: self.encoding_override,
        });
        cx.notify();
    }

    /// `123` is a line (hex mode: a byte offset), `45%` a position,
    /// `0x1F` a byte offset.
    pub(crate) fn goto(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(target) = parse_goto(text, matches!(self.mode, Some(Mode::Hex | Mode::Binary)))
        else {
            self.status = Some(format!("Can't go to \u{201c}{text}\u{201d}"));
            cx.notify();
            return;
        };
        match self.mode {
            Some(Mode::Text) => self.request_text(target, cx),
            Some(Mode::Hex) | Some(Mode::Binary) => {
                let width = self.current_hex_layout().width as u64;
                let offset = match target {
                    Anchor::Offset(o) | Anchor::Line(o) => o,
                    Anchor::Percent(p) => (self.len as f64 * p / 100.0) as u64,
                    Anchor::End => self.len,
                    _ => 0,
                };
                self.request_hex(offset / width, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    // -- workspace integration ---------------------------------------------

    pub(crate) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let embedded = self.embedded;
        let focused = self.focus_handle.is_focused(window);
        let _ = self.workspace.update(cx, |workspace, cx| {
            if embedded {
                workspace.close_quick_view(focused, window, cx);
            } else {
                workspace.close_viewer(window, cx);
            }
        });
    }

    /// The palette's `view.*` commands (`docs/commands.md`); `view.open`
    /// is the workspace's. `false` for an id this doesn't know.
    pub(crate) fn run_command(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match id {
            "view.close" => self.close(window, cx),
            "view.mode_text" => self.set_mode(Mode::Text, cx),
            "view.mode_hex" => self.set_mode(Mode::Hex, cx),
            "view.mode_binary" => self.set_mode(Mode::Binary, cx),
            "view.mode_image" => self.set_mode(Mode::Image, cx),
            "view.mode_auto" => self.set_mode_auto(cx),
            "view.set_encoding" => self.cycle_encoding(cx),
            "view.search" => self.open_find(window, cx),
            "view.search_next" => self.find(false, cx),
            "view.search_prev" => self.find(true, cx),
            "view.hex_set_width" => self.cycle_hex_width(cx),
            "view.hex_toggle_offset_base" => self.toggle_offset_base(cx),
            "view.next_file" => self.step_file(1, window, cx),
            "view.prev_file" => self.step_file(-1, window, cx),
            "view.wrap_toggle" => self.toggle_wrap(cx),
            _ => return false,
        }
        true
    }

    // -- test seams ----------------------------------------------------------

    #[cfg(test)]
    pub(crate) fn visible_rows(&self) -> Vec<String> {
        match self.mode {
            Some(Mode::Text) => self
                .text
                .as_ref()
                .map(|t| t.page.rows.iter().map(|r| r.text.clone()).collect())
                .unwrap_or_default(),
            Some(Mode::Hex) | Some(Mode::Binary) => self
                .hex
                .as_ref()
                .map(|h| h.rows.iter().map(|r| r.text.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// See [`ViewerState::goto`].
fn parse_goto(text: &str, byte_mode: bool) -> Option<Anchor> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(pct) = text.strip_suffix('%') {
        let pct: f64 = pct.trim().parse().ok()?;
        return Some(Anchor::Percent(pct));
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        let offset = u64::from_str_radix(hex, 16).ok()?;
        return Some(Anchor::Offset(offset));
    }
    let number: u64 = text.replace([',', '_', ' '], "").parse().ok()?;
    if byte_mode {
        Some(Anchor::Offset(number))
    } else {
        Some(Anchor::Line(number.saturating_sub(1)))
    }
}

impl Focusable for ViewerState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Regular files (anything but directories) of a listing, for the
/// viewer's sibling walk; the workspace builds it from the panel.
pub(crate) fn file_index(files: &[String], name: &str) -> Option<usize> {
    files.iter().position(|f| f == name)
}

/// Human-readable byte count for the status line.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `1,234,567`.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goto_parses_lines_percentages_and_offsets() {
        assert_eq!(parse_goto("12", false), Some(Anchor::Line(11)));
        assert_eq!(parse_goto("1,200", false), Some(Anchor::Line(1199)));
        assert_eq!(parse_goto("0", false), Some(Anchor::Line(0)));
        assert_eq!(parse_goto("12", true), Some(Anchor::Offset(12)));
        assert_eq!(parse_goto(" 50 % ", false), Some(Anchor::Percent(50.0)));
        assert_eq!(parse_goto("0x1F", false), Some(Anchor::Offset(31)));
        assert_eq!(parse_goto("abc", false), None);
        assert_eq!(parse_goto("", false), None);
    }

    #[test]
    fn status_formatting_helpers() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(file_index(&["a".into(), "b".into()], "b"), Some(1));
    }
}
