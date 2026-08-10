//! S-8 packaging spike: the smallest possible GPUI app.
//!
//! Purpose: a body to package as a Flatpak and an AppImage and confirm both
//! artifacts actually launch a real window (or at least map a real window)
//! with no GTK/Qt/KDE runtime present. See documentation/spikes/S-8.md.

use gpui::{
    App, AppContext as _, Application, Bounds, Context, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};

struct HelloView {
    text: SharedString,
}

impl Render for HelloView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xffffff))
            .child(self.text.clone())
    }
}

fn main() {
    // Log a heartbeat to stdout/stderr so a headless launch check (process
    // stayed alive, no panic) has something concrete to grep for, since we
    // cannot rely on screenshotting the window in this environment.
    eprintln!("hello_gpui: starting Application::new()");

    Application::new().run(|cx: &mut App| {
        eprintln!("hello_gpui: inside App closure, opening window");

        let bounds = Bounds::centered(None, size(px(480.0), px(240.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|_cx| HelloView {
                    text: "Hello, Duet (S-8 packaging spike)".into(),
                })
            },
        )
        .expect("failed to open window");

        eprintln!("hello_gpui: window opened successfully, entering event loop");
    });

    eprintln!("hello_gpui: event loop exited cleanly");
}
