//! S-6 spike: "text input reality check" for a path-bar / inline-rename field.
//!
//! This is a real GPUI app (real `Application`, real platform window, real
//! `cosmic-text` shaping) that self-drives a sequence of programmatic input
//! tests against a single-line `gpui_component::input::Input` styled like a
//! path bar, then prints a pass/fail report to stdout and exits.
//!
//! No screen automation is used anywhere here: every "keystroke" is a
//! synthetic clipboard-set + `Paste` action dispatch driven directly through
//! GPUI's own APIs, exactly the way a headless integration test would drive
//! it. See `documentation/spikes/S-6.md` for the write-up and for what this
//! approach cannot cover (real CJK IME composition needs a live ibus/fcitx
//! engine intercepting real keystrokes before they reach the app — see the
//! doc for why that is out of reach of any programmatic driver, not just this
//! one).

use std::ops::Range;
use std::time::Instant;

use gpui::{
    App, AppContext as _, Application, Bounds, ClipboardItem, Context, Entity,
    EntityInputHandler as _, IntoElement, ParentElement as _, Pixels, Render, Styled as _, Window,
    WindowBounds, WindowOptions, div, px, rgb, size,
};
use gpui_component::input::{Input, InputState, Paste};
use unicode_segmentation::UnicodeSegmentation;

/// Build a 4000-character valid-looking absolute path by repeating a
/// plausible nested-directory segment until the target length is hit, then
/// truncating exactly to it (ASCII-only, so truncation is always on a char
/// boundary).
fn build_long_path(target_len: usize) -> String {
    let mut s = String::from("/home/meir/projects/duet2/spikes/s6-text-input/");
    let segment = "another-nested-directory-segment-with-numbers-1234567890-and-dashes/";
    while s.len() < target_len {
        s.push_str(segment);
    }
    s.truncate(target_len);
    s
}

/// Folder + card-index-dividers + a 7-codepoint ZWJ family sequence + a
/// skin-tone-modified thumbs-up + a flag built from regional-indicator pairs.
/// Deliberately stresses several different grapheme-clustering mechanisms at
/// once (ZWJ joins, emoji variation selectors, skin-tone modifiers, RI
/// pairs).
const EMOJI_STR: &str = "📁🗂️👨‍👩‍👧‍👦👍🏽🇺🇸";

const ARABIC_PATH: &str = "/home/meir/مستندات/ملف_مهم_جدا.txt";
const HEBREW_PATH: &str = "/home/meir/מסמכים/מסמך-חשוב-מאוד.pdf";

struct TestCase {
    name: &'static str,
    text: String,
}

#[derive(Debug)]
struct CheckResult {
    name: &'static str,
    input_len_bytes: usize,
    input_graphemes: usize,
    roundtrip_ok: bool,
    layout_ok: bool,
    layout_detail: String,
    marked_range_clean: bool,
}

enum Phase {
    Warmup,
    Clear(usize),
    Paste(usize),
    Verify(usize),
    Done,
}

struct RootView {
    input: Entity<InputState>,
    cases: Vec<TestCase>,
    phase: Phase,
    results: Vec<CheckResult>,
    started: Instant,
    clipboard_probe_frame: u32,
}

fn byte_range_to_utf16(s: &str, range: Range<usize>) -> Range<usize> {
    let start = s[..range.start].encode_utf16().count();
    let end = s[..range.end].encode_utf16().count();
    start..end
}

/// Returns the byte ranges of the first and last extended grapheme cluster
/// in `s`.
fn first_last_grapheme_byte_ranges(s: &str) -> (Range<usize>, Range<usize>) {
    let indices: Vec<(usize, &str)> = s.grapheme_indices(true).collect();
    let first = indices
        .first()
        .map(|&(i, g)| i..(i + g.len()))
        .unwrap_or(0..0);
    let last = indices
        .last()
        .map(|&(i, g)| i..(i + g.len()))
        .unwrap_or(0..0);
    (first, last)
}

fn finite_positive(p: Pixels) -> bool {
    let v = f32::from(p);
    v.is_finite() && v > 0.0
}

impl RootView {
    fn on_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.phase, Phase::Done) {
            Phase::Warmup => {
                self.input.update(cx, |state, cx| {
                    state.focus(window, cx);
                });
                eprintln!(
                    "[debug] after focus(): window.focused = {:?}, window.is_window_active = {}",
                    window.focused(cx).is_some(),
                    window.is_window_active()
                );
                self.phase = Phase::Clear(0);
            }
            Phase::Clear(i) => {
                if i >= self.cases.len() {
                    self.print_report();
                    cx.quit();
                    return;
                }
                self.input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
                self.phase = Phase::Paste(i);
            }
            Phase::Paste(i) => {
                let text = self.cases[i].text.clone();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                let readback = cx.read_from_clipboard().and_then(|item| item.text());
                eprintln!(
                    "[debug] frame={} before dispatch Paste: window.focused = {:?}, clipboard_readback_len = {:?}",
                    self.clipboard_probe_frame,
                    window.focused(cx).is_some(),
                    readback.as_ref().map(|s| s.len())
                );
                if readback.is_none() && self.clipboard_probe_frame < 40 {
                    // The Wayland selection-offer round trip (self-offer ->
                    // compositor -> data_offer event back to us) may need
                    // several event-loop turns; keep polling for a while
                    // before concluding it genuinely never completes.
                    self.clipboard_probe_frame += 1;
                    self.phase = Phase::Paste(i);
                    self.schedule_next_frame(window, cx);
                    return;
                }
                self.clipboard_probe_frame = 0;
                window.dispatch_action(Box::new(Paste), cx);
                self.phase = Phase::Verify(i);
            }
            Phase::Verify(i) => {
                let expected = self.cases[i].text.clone();
                let name = self.cases[i].name;

                let got = self.input.read(cx).value().to_string();
                let roundtrip_ok = got == expected;
                eprintln!(
                    "[debug] case={:?} expected_len={} got_len={} got_prefix={:?}",
                    name,
                    expected.len(),
                    got.len(),
                    &got.chars().take(40).collect::<String>()
                );

                let expected_len_utf16 = expected.encode_utf16().count();
                let (first_g, last_g) = first_last_grapheme_byte_ranges(&expected);
                let first_g_utf16 = byte_range_to_utf16(&expected, first_g);
                let last_g_utf16 = byte_range_to_utf16(&expected, last_g);

                let mut layout_ok = false;
                let mut layout_detail = String::new();
                let mut marked_range_clean = false;

                self.input.update(cx, |state, cx| {
                    marked_range_clean = state.marked_text_range(window, cx).is_none();

                    let full_bounds =
                        state.bounds_for_range(0..expected_len_utf16, Bounds::default(), window, cx);
                    let first_bounds =
                        state.bounds_for_range(first_g_utf16, Bounds::default(), window, cx);
                    let last_bounds =
                        state.bounds_for_range(last_g_utf16, Bounds::default(), window, cx);

                    match (full_bounds, first_bounds, last_bounds) {
                        (Some(full), Some(first), Some(last)) => {
                            layout_ok = finite_positive(full.size.width)
                                && finite_positive(full.size.height);
                            layout_detail = format!(
                                "full=({:.1}x{:.1} @ {:.1},{:.1})  first-grapheme.origin.x={:.1}  last-grapheme.origin.x={:.1}  (first<last means logical-first grapheme is drawn left-of logical-last => LTR visual order; first>last => RTL-reordered visual order)",
                                f32::from(full.size.width),
                                f32::from(full.size.height),
                                f32::from(full.origin.x),
                                f32::from(full.origin.y),
                                f32::from(first.origin.x),
                                f32::from(last.origin.x),
                            );
                        }
                        _ => {
                            layout_detail = "bounds_for_range returned None (no layout yet)".into();
                        }
                    }
                });

                self.results.push(CheckResult {
                    name,
                    input_len_bytes: expected.len(),
                    input_graphemes: expected.graphemes(true).count(),
                    roundtrip_ok,
                    layout_ok,
                    layout_detail,
                    marked_range_clean,
                });

                self.phase = Phase::Clear(i + 1);
            }
            Phase::Done => return,
        }

        self.schedule_next_frame(window, cx);
    }

    fn schedule_next_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        window.on_next_frame(move |window, cx| {
            entity.update(cx, |this, cx| this.on_frame(window, cx));
        });
    }

    fn print_report(&self) {
        println!("\n===== S-6 spike report (elapsed {:?}) =====", self.started.elapsed());
        let mut all_pass = true;
        for r in &self.results {
            let status = if r.roundtrip_ok && r.layout_ok {
                "PASS"
            } else {
                all_pass = false;
                "FAIL"
            };
            println!("\n[{status}] {}", r.name);
            println!(
                "  input: {} bytes, {} extended grapheme clusters",
                r.input_len_bytes, r.input_graphemes
            );
            println!(
                "  roundtrip (paste -> InputState::value() byte-identical): {}",
                r.roundtrip_ok
            );
            println!("  layout produced finite, positive bounds: {}", r.layout_ok);
            println!("  layout detail: {}", r.layout_detail);
            println!(
                "  marked_text_range() is None after paste (no stray IME preedit state): {}",
                r.marked_range_clean
            );
        }
        println!(
            "\n===== overall: {} =====\n",
            if all_pass { "ALL PROGRAMMATIC CHECKS PASSED" } else { "AT LEAST ONE CHECK FAILED" }
        );
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .flex()
            .flex_col()
            .p_4()
            .child(
                div()
                    .text_color(rgb(0xcccccc))
                    .mb_2()
                    .child("Duet S-6 text input spike \u{2014} path bar"),
            )
            .child(
                div()
                    .w_full()
                    .rounded_md()
                    .bg(rgb(0x2d2d2d))
                    .border_1()
                    .border_color(rgb(0x454545))
                    .px_2()
                    .py_1()
                    .child(Input::new(&self.input)),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(900.0), px(200.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder("Enter a path\u{2026}")
                });

                let cases = vec![
                    TestCase {
                        name: "4000-char path paste",
                        text: build_long_path(4000),
                    },
                    TestCase {
                        name: "emoji / ZWJ / skin-tone / flag paste",
                        text: EMOJI_STR.to_string(),
                    },
                    TestCase {
                        name: "Arabic RTL path paste",
                        text: ARABIC_PATH.to_string(),
                    },
                    TestCase {
                        name: "Hebrew RTL path paste",
                        text: HEBREW_PATH.to_string(),
                    },
                ];

                let root = cx.new(|_cx| RootView {
                    input,
                    cases,
                    phase: Phase::Warmup,
                    results: Vec::new(),
                    started: Instant::now(),
                    clipboard_probe_frame: 0,
                });

                root.update(cx, |this, cx| {
                    this.schedule_next_frame(window, cx);
                });

                // `Input`'s element paint path (focus tracking, mouse context
                // menu) calls into `gpui_component::Root::read`/`Root::update`
                // and expects the window's root view to be a `Root`, so the
                // real render root is `Root` wrapping our `RootView`.
                cx.new(|cx| gpui_component::Root::new(root, window, cx))
            },
        )
        .unwrap();
    });
}
