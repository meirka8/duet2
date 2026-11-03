// SPDX-License-Identifier: MIT
//! `ViewerState`'s rendering: a title bar, the page body (text, hex or
//! image), the find/goto bar when open, and a status line. Rows are
//! plain monospace divs -- the entity decides which rows exist, so no
//! scroll container and no whole-file height are involved (see the
//! module doc comment in `mod.rs`).

use duet_widgets::input::{Escape, Input};
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollWheelEvent, SharedString, Styled as _, StyledImage as _, TextRun, Window, div, px,
};

use super::{Bar, FONT_SIZE, LINE_HEIGHT, Mode, ViewerState, human_size, thousands};

impl ViewerState {
    fn measure_glyph(&mut self, window: &mut Window) {
        if self.ch_width.is_some() {
            return;
        }
        let run = TextRun {
            len: 1,
            font: gpui::font("monospace"),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line("0".into(), FONT_SIZE, &[run], None);
        if line.width > px(0.) {
            self.ch_width = Some(line.width);
        }
    }

    fn position_text(&self) -> String {
        if self.len == 0 {
            return "empty".to_string();
        }
        match self.mode {
            Some(Mode::Text) => {
                let offset = self.top.row_start;
                let pct = (offset as f64 / self.len as f64 * 100.0).floor() as u64;
                let pct = if self.text.as_ref().is_some_and(|t| t.page.at_end) {
                    100
                } else {
                    pct
                };
                match self.text.as_ref() {
                    Some(t) => match (t.line, t.total_lines) {
                        (Some(line), Some(total)) => format!(
                            "Line {} of {} · {pct}%",
                            thousands(line + 1),
                            thousands(total)
                        ),
                        (Some(line), None) => format!(
                            "Line {} · {pct}% · indexing {:.0}%",
                            thousands(line + 1),
                            self.index_progress.unwrap_or(0.0) * 100.0
                        ),
                        _ => match self.index_progress {
                            Some(p) => format!("{pct}% · indexing {:.0}%", p * 100.0),
                            None => format!("offset {} · {pct}%", thousands(offset)),
                        },
                    },
                    None => "…".to_string(),
                }
            }
            Some(Mode::Hex) | Some(Mode::Binary) => {
                let offset = self.current_offset();
                let pct = match self.hex.as_ref() {
                    Some(h) if h.row_count > 0 => {
                        let shown_to = (h.first_row + h.rows.len() as u64).min(h.row_count);
                        (shown_to as f64 / h.row_count as f64 * 100.0).floor() as u64
                    }
                    _ => (offset as f64 / self.len as f64 * 100.0).floor() as u64,
                };
                if self.hex_layout.decimal {
                    format!("offset {} · {pct}%", thousands(offset))
                } else {
                    format!("offset 0x{offset:X} · {pct}%")
                }
            }
            Some(Mode::Image) => match self.image {
                Some((_, w, h)) => format!("{w} × {h}"),
                None => "decoding…".to_string(),
            },
            None => "…".to_string(),
        }
    }

    fn mode_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(mode) = self.mode {
            parts.push(mode.label().to_string());
        }
        match self.mode {
            Some(Mode::Text) => {
                let shown = self.text.as_ref().map(|t| t.encoding).or(self.encoding());
                if let Some(enc) = shown {
                    let mark = if self.encoding_override.is_some() {
                        " (manual)"
                    } else {
                        ""
                    };
                    parts.push(format!("{}{mark}", enc.name()));
                }
                if self.wrap {
                    parts.push("wrap".to_string());
                }
            }
            Some(Mode::Hex) => {
                parts.push(format!("{} bytes/row", self.hex_layout.width));
                parts.push(
                    if self.hex_layout.decimal {
                        "dec"
                    } else {
                        "hex"
                    }
                    .to_string(),
                );
            }
            _ => {}
        }
        parts.join(" · ")
    }

    /// One text or hex row: the text split around the highlight so the
    /// hit gets a background, with `h_offset` columns skipped in no-wrap
    /// text mode.
    fn row_element(
        &self,
        ix: usize,
        text: &str,
        highlight: Option<std::ops::Range<usize>>,
        fg: Hsla,
        hit_bg: Hsla,
        hit_fg: Hsla,
    ) -> impl IntoElement {
        let skip = if self.mode == Some(Mode::Text) && !self.wrap {
            self.h_offset
        } else {
            0
        };
        let skip_bytes = if skip == 0 {
            0
        } else {
            text.char_indices().nth(skip).map_or(text.len(), |(i, _)| i)
        };
        let shown = &text[skip_bytes..];
        let row = div()
            .id(("viewer-row", ix))
            .h(LINE_HEIGHT)
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(fg);
        match highlight {
            Some(hl) if hl.end > skip_bytes => {
                let a = hl.start.max(skip_bytes) - skip_bytes;
                let b = hl.end - skip_bytes;
                let b = b.min(shown.len());
                row.child(SharedString::from(shown[..a].to_string()))
                    .child(
                        div()
                            .bg(hit_bg)
                            .text_color(hit_fg)
                            .child(SharedString::from(shown[a..b].to_string())),
                    )
                    .child(SharedString::from(shown[b..].to_string()))
            }
            _ => row.child(SharedString::from(shown.to_string())),
        }
    }
}

impl Render for ViewerState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.measure_glyph(window);
        let tokens = TokenPalette::current(cx);
        let colors = tokens.color;
        let this = cx.entity().clone();
        let title_name: SharedString = format!(
            "{}  ({} of {})",
            self.file_name(),
            self.file_ix + 1,
            self.files.len()
        )
        .into();
        let size_text: SharedString = human_size(self.len).into();
        let mode_text: SharedString = self.mode_text().into();
        let position: SharedString = self.position_text().into();
        let status: Option<SharedString> = match (&self.status, self.search_progress) {
            (Some(s), _) => Some(s.clone().into()),
            (None, Some(p)) => Some(format!("Searching… {:.0}%", p * 100.0).into()),
            _ => None,
        };

        let body = match self.mode {
            Some(Mode::Text) => {
                let rows: Vec<_> = self
                    .text
                    .as_ref()
                    .map(|t| {
                        t.page
                            .rows
                            .iter()
                            .enumerate()
                            .map(|(ix, r)| {
                                self.row_element(
                                    ix,
                                    &r.text,
                                    r.highlight.clone(),
                                    colors.panel_fg_active,
                                    colors.selection_bg,
                                    colors.selection_fg,
                                )
                                .into_any_element()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                div().flex().flex_col().children(rows).into_any_element()
            }
            Some(Mode::Hex) | Some(Mode::Binary) => {
                let rows: Vec<_> = self
                    .hex
                    .as_ref()
                    .map(|h| {
                        h.rows
                            .iter()
                            .enumerate()
                            .map(|(ix, r)| {
                                self.row_element(
                                    ix,
                                    &r.text,
                                    r.highlight.clone(),
                                    colors.panel_fg_active,
                                    colors.selection_bg,
                                    colors.selection_fg,
                                )
                                .into_any_element()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                div().flex().flex_col().children(rows).into_any_element()
            }
            Some(Mode::Image) => match &self.image {
                Some((image, _, _)) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::img(gpui::ImageSource::Render(image.clone()))
                            .object_fit(gpui::ObjectFit::ScaleDown)
                            .max_w_full()
                            .max_h_full(),
                    )
                    .into_any_element(),
                None => div().into_any_element(),
            },
            None => div().into_any_element(),
        };

        let bar = match self.bar {
            Bar::None => None,
            Bar::Find => Some(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .bg(colors.header_bg)
                    .text_color(colors.header_fg)
                    .child(div().text_size(px(12.)).child("Find:"))
                    .child(div().flex_1().child(Input::new(&self.find_input)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(colors.statusbar_fg)
                            .child("Enter next · Shift+Enter previous · Esc close"),
                    ),
            ),
            Bar::Goto => Some(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .bg(colors.header_bg)
                    .text_color(colors.header_fg)
                    .child(div().text_size(px(12.)).child("Go to:"))
                    .child(div().flex_1().child(Input::new(&self.goto_input)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(colors.statusbar_fg)
                            .child("line · 50% · 0x1F · Esc close"),
                    ),
            ),
        };

        let hints: SharedString = match self.mode {
            Some(Mode::Image) => "Esc close · Ctrl+1 text · Ctrl+2 hex · Ctrl+PgDn/PgUp next/prev file".into(),
            Some(Mode::Hex) | Some(Mode::Binary) => {
                "Esc close · Ctrl+F find · Ctrl+G offset · Ctrl+1 text · Ctrl+4 image · Ctrl+PgDn/PgUp next/prev file".into()
            }
            _ => "Esc close · Ctrl+F find · Ctrl+G line · W wrap · Ctrl+E encoding · Ctrl+2 hex · Ctrl+PgDn/PgUp next/prev file".into(),
        };

        div()
            .id("viewer")
            .key_context("Viewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.panel_bg_active)
            .text_color(colors.panel_fg_active)
            .on_action(cx.listener(|this, _: &super::ViewerClose, window, cx| {
                if this.bar != Bar::None {
                    this.close_bar(window, cx);
                } else {
                    this.close(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.bar != Bar::None {
                    this.close_bar(window, cx);
                } else {
                    this.close(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &super::ViewerPageDown, _w, cx| this.page_down(cx)))
            .on_action(cx.listener(|this, _: &super::ViewerPageUp, _w, cx| this.page_up(cx)))
            .on_action(
                cx.listener(|this, _: &super::ViewerLineDown, _w, cx| this.scroll_rows(1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &super::ViewerLineUp, _w, cx| this.scroll_rows(-1, cx)),
            )
            .on_action(cx.listener(|this, _: &super::ViewerHome, _w, cx| this.go_home(cx)))
            .on_action(cx.listener(|this, _: &super::ViewerEnd, _w, cx| this.go_end(cx)))
            .on_action(cx.listener(|this, _: &super::ViewerLeft, _w, cx| this.scroll_left(cx)))
            .on_action(cx.listener(|this, _: &super::ViewerRight, _w, cx| this.scroll_right(cx)))
            .on_action(
                cx.listener(|this, _: &super::ViewerFind, window, cx| this.open_find(window, cx)),
            )
            .on_action(cx.listener(|this, _: &super::ViewerFindPrev, _w, cx| this.find(true, cx)))
            .on_action(
                cx.listener(|this, _: &super::ViewerGoto, window, cx| this.open_goto(window, cx)),
            )
            .on_action(cx.listener(|this, _: &super::ViewerWrap, _w, cx| this.toggle_wrap(cx)))
            .on_action(
                cx.listener(|this, _: &super::ViewerModeText, _w, cx| {
                    this.set_mode(Mode::Text, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &super::ViewerModeHex, _w, cx| this.set_mode(Mode::Hex, cx)),
            )
            .on_action(cx.listener(|this, _: &super::ViewerModeBinary, _w, cx| {
                this.set_mode(Mode::Binary, cx)
            }))
            .on_action(cx.listener(|this, _: &super::ViewerModeImage, _w, cx| {
                this.set_mode(Mode::Image, cx)
            }))
            .on_action(
                cx.listener(|this, _: &super::ViewerEncoding, _w, cx| this.cycle_encoding(cx)),
            )
            .on_action(cx.listener(|this, _: &super::ViewerNextFile, window, cx| {
                this.step_file(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &super::ViewerPrevFile, window, cx| {
                this.step_file(-1, window, cx)
            }))
            // Title bar.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .bg(colors.header_bg)
                    .text_color(colors.header_fg)
                    .text_size(px(12.))
                    .child(div().font_weight(gpui::FontWeight::BOLD).child(title_name))
                    .child(div().child(mode_text))
                    .child(div().child(size_text)),
            )
            // Body: the rows, over a canvas that measures the area.
            .child(
                div()
                    .id("viewer-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px_2()
                    .font_family("monospace")
                    .text_size(FONT_SIZE)
                    .line_height(LINE_HEIGHT)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        let delta = event.delta.pixel_delta(LINE_HEIGHT);
                        let rows = (f32::from(delta.y) / f32::from(LINE_HEIGHT)).round() as i64;
                        if rows != 0 {
                            this.scroll_rows(-rows * 3 / 3, cx);
                        }
                    }))
                    .child(body)
                    .child(
                        gpui::canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |this, cx| this.record_viewport(bounds.size, cx));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .when_some(bar, |root, bar| root.child(bar))
            // Status line.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .bg(colors.statusbar_bg)
                    .text_color(colors.statusbar_fg)
                    .text_size(px(11.))
                    .child(div().child(position))
                    .child(match status {
                        Some(s) => div().text_color(colors.warning).child(s),
                        None => div().child(hints),
                    }),
            )
    }
}
