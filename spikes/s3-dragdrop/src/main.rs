//! S-3 spike: cross-application drag & drop feasibility (design.md §9.10 / §7.4 R-G3).
//!
//! Investigation summary (see documentation/spikes/S-3.md for the full write-up):
//!
//! - GPUI 0.2.2's `Interactivity::on_drop<T>()` / `FileDropEvent` machinery IS wired to the
//!   real platform DnD target protocol on both backends:
//!     * Wayland: `platform/linux/wayland/client.rs` implements `wl_data_device` Enter/Motion/
//!       Leave/Drop, reads the offered `text/uri-list` mime type over a pipe, and turns it into
//!       `PlatformInput::FileDrop(FileDropEvent::Entered { paths, .. })`.
//!     * X11: `platform/linux/x11/client.rs` implements a real XDND target (`XdndEnter` /
//!       `XdndPosition` / `XdndDrop`, `XdndStatus`/`XdndFinished` replies).
//!   `Window::handle_input` (see `src/window.rs` around `PlatformInput::FileDrop`) translates
//!   those OS events into the *same* internal `active_drag` / `ExternalPaths` value used by
//!   the ordinary in-app `on_drag`/`on_drop` element API. Concretely: any `div().on_drop::<
//!   ExternalPaths>(...)` handler receives both same-window drags of that type AND real
//!   drops dragged in from Nautilus/Firefox/etc. This is genuine, native, working
//!   **inbound** cross-app DnD on both Wayland and X11 - nothing needed to be bolted on.
//!
//! - There is NO outbound drag-source implementation anywhere in gpui-0.2.2's Linux backends:
//!   grepping the crate for `start_drag`/`WlDataSource`/XDND-source logic turns up only the
//!   *clipboard* data-source path (`set_selection`), never `wl_data_device.start_drag(...)`
//!   nor an XDND `XdndEnter`/`XdndPosition` sent as a *source*. GPUI's `on_drag`/`on_drag_move`
//!   API is a purely synthetic, in-process mechanism (tracked via `App::active_drag`, driven
//!   by ordinary mouse-move dispatch) - it never touches the compositor's real DnD protocol,
//!   so nothing dragged out of a GPUI window can land in another application's window.
//!
//! - `gpui::Window` does implement `raw_window_handle`'s `HasWindowHandle`/`HasDisplayHandle`,
//!   which *does* expose the live `wl_surface*`/`wl_display*` GPUI itself is using. In theory
//!   a second, independent `wayland-client`/`wayland-backend` `Connection` could be attached
//!   to that same raw `wl_display*`, bind its own `wl_seat`/`wl_pointer`/`wl_data_device`, and
//!   call `start_drag` with a serial captured from that second pointer object. We did not
//!   attempt this live: wayland-backend's `Connection`/event-queue own the read/dispatch
//!   cursor of the underlying fd, and driving two independent Rust-side connections against
//!   the same fd from two different unsynchronized owners is exactly the kind of unsupported,
//!   undefined-behaviour-risking arrangement that could destabilize GPUI's own event loop -
//!   unacceptable for a timeboxed spike whose explicit AC includes "confirm the binary
//!   launches without crashing". This is flagged below as the concrete P1 recommendation.

use std::fs;
use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Application, Bounds, Context, ExternalPaths, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, WindowBounds, WindowOptions, div, px, size, white,
};

/// Path to the real file backing the draggable "drag-me.txt" row.
const DRAG_FILE_PATH: &str = "/tmp/duet-s3-test/drag-me.txt";

/// Payload used for the *intra-app only* synthetic drag (GPUI's own `on_drag`/`on_drop`
/// mechanism). This is NOT the same as `gpui::ExternalPaths`, which is what real OS-level
/// drops from other applications arrive as - `ExternalPaths`'s inner field is private to
/// the gpui crate, so application code cannot construct one to fake an external drop.
#[derive(Clone)]
struct InAppDragPayload {
    path: PathBuf,
}

/// A trivial view used purely to render the drag preview/ghost that follows the cursor
/// during a same-window GPUI drag.
struct DragGhost {
    label: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(gpui::rgb(0x2d5be3))
            .text_color(white())
            .rounded_md()
            .child(self.label.clone())
    }
}

struct RootView {
    /// Human-readable log of everything the drop zone has received, newest first.
    received: Vec<String>,
}

impl RootView {
    fn record(&mut self, line: String) {
        println!("[drop-zone] {line}");
        self.received.insert(0, line);
        if self.received.len() > 20 {
            self.received.truncate(20);
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let received_lines = self.received.clone();

        div()
            .id("root")
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x1e1e1e))
            .text_color(white())
            .p_4()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(18.0))
                            .child("Duet S-3 spike: cross-application drag & drop"),
                    )
                    .child(div().text_size(px(12.0)).text_color(gpui::rgb(0xaaaaaa)).child(
                        format!(
                            "Backing file for the draggable row: {DRAG_FILE_PATH}. \
                             See documentation/spikes/S-3.md for the manual test script."
                        ),
                    )),
            )
            .child(
                // Row 1: the draggable "file" (source).
                div()
                    .id("drag-source-row")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(gpui::rgb(0x2a2a2a))
                    .border_1()
                    .border_color(gpui::rgb(0x444444))
                    .cursor_grab()
                    .on_drag(
                        InAppDragPayload {
                            path: PathBuf::from(DRAG_FILE_PATH),
                        },
                        |payload, _offset, _window, cx| {
                            cx.new(|_| DragGhost {
                                label: payload
                                    .path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default()
                                    .into(),
                            })
                        },
                    )
                    .child("\u{1F4C4}  drag-me.txt")
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(gpui::rgb(0x888888))
                            .child("(drag this row - onto the box below to test in-app DnD, or out to Nautilus/a terminal to test cross-app DnD)"),
                    ),
            )
            .child(
                // Row 2: the drop target (sink), reacts to BOTH real external drops
                // (ExternalPaths, native OS DnD) and the in-app-only demo drag above.
                div()
                    .id("drop-zone")
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_2()
                    .border_color(gpui::rgb(0x555555))
                    .bg(gpui::rgb(0x151515))
                    .drag_over::<ExternalPaths>(|style, _paths, _window, _cx| {
                        style.bg(gpui::rgb(0x27401f)).border_color(gpui::rgb(0x4caf50))
                    })
                    .drag_over::<InAppDragPayload>(|style, _payload, _window, _cx| {
                        style.bg(gpui::rgb(0x27401f)).border_color(gpui::rgb(0x4caf50))
                    })
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                        let list = paths
                            .paths()
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        this.record(format!("OS drop received (text/uri-list): {list}"));
                        cx.notify();
                    }))
                    .on_drop(cx.listener(|this, payload: &InAppDragPayload, _window, cx| {
                        this.record(format!(
                            "in-app drag received: {}",
                            payload.path.display()
                        ));
                        cx.notify();
                    }))
                    .child(div().text_size(px(13.0)).child("Drop target (accepts real OS drops and the demo row above)"))
                    .children(received_lines.into_iter().map(|line| {
                        div()
                            .text_size(px(12.0))
                            .text_color(gpui::rgb(0xcccccc))
                            .child(line)
                    })),
            )
    }
}

fn ensure_test_file() {
    let dir = "/tmp/duet-s3-test";
    fs::create_dir_all(dir).expect("failed to create /tmp/duet-s3-test");
    fs::write(
        DRAG_FILE_PATH,
        b"Duet S-3 drag-and-drop spike fixture.\n\
          If you can read this after dropping the row onto a Nautilus window,\n\
          outbound cross-app DnD worked (it is not expected to, per gpui source \
          inspection - see documentation/spikes/S-3.md).\n",
    )
    .expect("failed to write drag-me.txt");
}

fn main() {
    ensure_test_file();

    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(760.0), px(480.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| RootView { received: Vec::new() }),
        )
        .unwrap();
    });
}
