//! Deterministic, headless verification for the S-6 spike.
//!
//! Unlike `src/main.rs` (which drives a *real* platform window over the
//! real Wayland session, including the real `wl_data_device` clipboard),
//! these tests run against `gpui`'s own `TestAppContext`: a fake platform
//! with an in-process, synchronous clipboard (`Mutex<Option<ClipboardItem>>`,
//! see `gpui-0.2.2/src/platform/test/platform.rs`). That sidesteps a real
//! finding from the manual run of `src/main.rs`: reading back this
//! process's own just-written clipboard content over real Wayland is
//! asynchronous and, on a real desktop session, can race with whatever the
//! *user's actual desktop clipboard* currently holds (we observed exactly
//! that: a readback that returned unrelated pre-existing clipboard content
//! instead of what we had just written). That makes the real-window path
//! non-deterministic as a pass/fail oracle, even though it is what a real
//! user's paste goes through end to end. These tests are the reliable,
//! reproducible half of the picture; `src/main.rs` is the "does this work
//! against the real platform at all" half.
//!
//! Still zero screen automation, zero human input: every action here is a
//! direct, synchronous GPUI API call (`TestAppContext::write_to_clipboard`,
//! `TestAppContext::dispatch_action`, `Window::draw`).

use std::ops::Range;

use gpui::{
    AppContext as _, Bounds, ClipboardItem, Context, Entity, EntityInputHandler as _,
    IntoElement, ParentElement as _, Render, TestAppContext, VisualTestContext, Window, div,
};
use gpui_component::{
    Root,
    input::{Input, InputState, Paste},
};
use unicode_segmentation::UnicodeSegmentation;

struct Harness {
    input: Entity<InputState>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(Input::new(&self.input))
    }
}

fn build_long_path(target_len: usize) -> String {
    let mut s = String::from("/home/meir/projects/duet2/spikes/s6-text-input/");
    let segment = "another-nested-directory-segment-with-numbers-1234567890-and-dashes/";
    while s.len() < target_len {
        s.push_str(segment);
    }
    s.truncate(target_len);
    s
}

const EMOJI_STR: &str = "📁🗂️👨‍👩‍👧‍👦👍🏽🇺🇸";
const ARABIC_PATH: &str = "/home/meir/مستندات/ملف_مهم_جدا.txt";
const HEBREW_PATH: &str = "/home/meir/מסמכים/מסמך-חשוב-מאוד.pdf";

fn byte_range_to_utf16(s: &str, range: Range<usize>) -> Range<usize> {
    let start = s[..range.start].encode_utf16().count();
    let end = s[..range.end].encode_utf16().count();
    start..end
}

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

/// Build a window, focus its `Input`, clipboard-paste `text` into it via a
/// real dispatched `Paste` action, and assert the field's value comes back
/// byte-identical. Also exercises `bounds_for_range` (the API GPUI uses to
/// position IME candidate windows) over the whole string and over the first
/// and last extended grapheme clusters, to check layout doesn't panic or
/// collapse to zero size, and to see whether the logically-first grapheme is
/// painted to the left of the logically-last one (LTR layout) or not.
fn paste_roundtrip(cx: &mut TestAppContext, text: &str) {
    cx.update(|cx| gpui_component::init(cx));
    let window = cx.add_window(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Enter a path\u{2026}"));
        Root::new(cx.new(|_| Harness { input }), window, cx)
    });

    // From here on, drive everything through a `VisualTestContext`, which
    // reaches `&mut Window` via the App-level `update_window` (a cheap
    // `AnyView` clone of the root, no entity borrow) rather than
    // `WindowHandle<Root>::update` (which calls `Entity<Root>::update` and
    // holds an exclusive borrow on `Root` for the whole closure). `Input`'s
    // own focus-change handling calls back into `Root::update` synchronously
    // (see `gpui-component-0.5.1/src/input/state.rs:1721`, tracking
    // `Root.focused_input`) — going through `WindowHandle::update` here
    // deadlocks/panics on gpui's re-entrant-borrow guard
    // ("cannot update gpui_component::root::Root while it is already being
    // updated"), which is itself a real, source-confirmed gotcha for any
    // caller scripting a `gpui-component` `Root`-rooted window headlessly.
    let mut vcx = VisualTestContext::from_window(window.into(), cx);

    // First paint, so focus tracking / dispatch-tree registration exists.
    let _ = vcx.update(|window, cx| window.draw(cx));

    let input = window
        .update(cx, |root, _, cx| {
            root.view()
                .clone()
                .downcast::<Harness>()
                .unwrap()
                .read(cx)
                .input
                .clone()
        })
        .unwrap();

    vcx.update(|window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
    let _ = vcx.update(|window, cx| window.draw(cx));

    vcx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    assert_eq!(
        vcx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
        Some(text),
        "sanity check: TestAppContext's fake clipboard should read back exactly what was written, synchronously"
    );

    vcx.dispatch_action(Paste);
    let _ = vcx.update(|window, cx| window.draw(cx));

    let got = vcx.update(|_, cx| input.read(cx).value().to_string());
    assert_eq!(got, text, "pasted value must round-trip byte-identically");

    // No leftover IME preedit/marked-text state after a plain paste.
    let marked_clean = vcx.update(|window, cx| {
        input.update(cx, |state, cx| state.marked_text_range(window, cx).is_none())
    });
    assert!(marked_clean, "paste must not leave a stray marked_text_range()");

    let expected_len_utf16 = text.encode_utf16().count();
    let (first_g, last_g) = first_last_grapheme_byte_ranges(text);
    let first_g_utf16 = byte_range_to_utf16(text, first_g);
    let last_g_utf16 = byte_range_to_utf16(text, last_g);

    let (full, first, last) = vcx.update(|window, cx| {
        input.update(cx, |state, cx| {
            (
                state.bounds_for_range(0..expected_len_utf16, Bounds::default(), window, cx),
                state.bounds_for_range(first_g_utf16, Bounds::default(), window, cx),
                state.bounds_for_range(last_g_utf16, Bounds::default(), window, cx),
            )
        })
    });

    let full = full.expect("bounds_for_range(whole string) must resolve after a paint");
    let first = first.expect("bounds_for_range(first grapheme) must resolve after a paint");
    let last = last.expect("bounds_for_range(last grapheme) must resolve after a paint");

    assert!(
        f32::from(full.size.width).is_finite() && f32::from(full.size.width) > 0.0,
        "layout width must be finite and positive for {} bytes of text, got {:?}",
        text.len(),
        full.size.width
    );
    assert!(
        f32::from(full.size.height).is_finite() && f32::from(full.size.height) > 0.0
    );

    eprintln!(
        "[{} bytes / {} graphemes] full={:.1}x{:.1}  first-grapheme.x={:.1}  last-grapheme.x={:.1}  ({})",
        text.len(),
        text.graphemes(true).count(),
        f32::from(full.size.width),
        f32::from(full.size.height),
        f32::from(first.origin.x),
        f32::from(last.origin.x),
        if f32::from(first.origin.x) < f32::from(last.origin.x) {
            "logical-first grapheme drawn left-of logical-last => LTR visual order, no BiDi reordering"
        } else {
            "logical-first grapheme drawn right-of logical-last => visually reordered"
        }
    );
}

#[gpui::test]
fn paste_4000_char_path(cx: &mut TestAppContext) {
    let path = build_long_path(4000);
    assert_eq!(path.len(), 4000);
    paste_roundtrip(cx, &path);
}

#[gpui::test]
fn paste_emoji_zwj_skintone_flag(cx: &mut TestAppContext) {
    // Sanity: this is really 5 extended grapheme clusters even though it is
    // many more Unicode scalar values (folder, card-index-dividers, a
    // 7-codepoint ZWJ family, a skin-toned thumbs-up, and a 2-codepoint
    // regional-indicator flag).
    assert_eq!(EMOJI_STR.graphemes(true).count(), 5);
    paste_roundtrip(cx, EMOJI_STR);
}

#[gpui::test]
fn paste_arabic_rtl_path(cx: &mut TestAppContext) {
    paste_roundtrip(cx, ARABIC_PATH);
}

#[gpui::test]
fn paste_hebrew_rtl_path(cx: &mut TestAppContext) {
    paste_roundtrip(cx, HEBREW_PATH);
}

/// Smoke test that `InputState` really does implement the marked-text
/// (IME preedit) side of `EntityInputHandler`, independent of paste: drive
/// `replace_and_mark_text_in_range` and `unmark_text` directly (this is
/// exactly what a real IME's "preedit update" / "commit" events cause GPUI
/// to call on Linux, via `ElementInputHandler` -> `Window::handle_input`),
/// and check the marked range is tracked and then cleared correctly. This
/// does not exercise a real IME engine (see the S-6 doc for why that is out
/// of reach here) but it does prove the plumbing GPUI would drive is live
/// and functional for this widget, not just present in the trait
/// declaration.
#[gpui::test]
fn ime_marked_text_plumbing_is_live(cx: &mut TestAppContext) {
    cx.update(|cx| gpui_component::init(cx));
    let window = cx.add_window(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx));
        Root::new(cx.new(|_| Harness { input }), window, cx)
    });

    // See the comment in `paste_roundtrip` for why this goes through
    // `VisualTestContext` rather than `WindowHandle<Root>::update`.
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let _ = vcx.update(|window, cx| window.draw(cx));

    let input = window
        .update(cx, |root, _, cx| {
            root.view()
                .clone()
                .downcast::<Harness>()
                .unwrap()
                .read(cx)
                .input
                .clone()
        })
        .unwrap();

    vcx.update(|window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
    let _ = vcx.update(|window, cx| window.draw(cx));

    // Simulate what a CJK IME's preedit-update event drives: composing "n"
    // as marked (uncommitted) text.
    vcx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.replace_and_mark_text_in_range(None, "n", None, window, cx);
        });
    });

    let marked = vcx.update(|window, cx| {
        input.update(cx, |state, cx| state.marked_text_range(window, cx))
    });
    assert!(marked.is_some(), "after replace_and_mark_text_in_range, marked_text_range() must be Some");

    // Simulate the IME committing the composed character (e.g. "你" chosen
    // from a candidate window), replacing the marked range.
    vcx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.replace_text_in_range(None, "你", window, cx);
        });
    });

    let (value, marked_after_commit) = vcx.update(|window, cx| {
        input.update(cx, |state, cx| {
            (state.value().to_string(), state.marked_text_range(window, cx))
        })
    });

    assert_eq!(value, "你");
    assert!(
        marked_after_commit.is_none(),
        "after a commit, marked_text_range() must be cleared"
    );
}
