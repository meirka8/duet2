// SPDX-License-Identifier: MIT
//! A minimal, first-party window titlebar: drag-to-move, minimize, close.
//! Deliberately *not* `gpui_component::TitleBar` reused wholesale (see
//! `duet_widgets::layout`'s own doc comment for how that one was wired in
//! originally) -- this exists specifically to omit maximize/restore.
//!
//! # Why maximize is missing, not just hidden
//!
//! Live UAT found a real, reproduced defect in the vendored `gpui`/
//! `blade-graphics` rendering engine on Linux/Wayland: `BladeRenderer::
//! draw()` (`gpui-0.2.2/src/platform/blade/blade_renderer.rs`) never
//! checks whether the frame it just acquired is actually valid.
//! `blade_graphics::vulkan::Surface::acquire_frame` returns a frame
//! flagged `image_index: None` -- a stale placeholder, not real image
//! data -- whenever the Vulkan swapchain reports itself out of date
//! (`VK_ERROR_OUT_OF_DATE_KHR`), which happens on essentially any resize,
//! maximize included. `draw()` renders into that stale frame anyway. Two
//! reproductions on this project's own machine hit the same downstream
//! symptom from there (repeated `Acquire failed because the surface is
//! out of date` warnings, GPU resources torn down, then `window not
//! found` errors from anything still holding a handle to the window) --
//! one via `TitleBar`'s own maximize control, the other independently.
//!
//! A real fix lives inside that vendored engine, not in application code
//! -- it would mean forking `gpui` (and possibly `blade-graphics`),
//! patching the acquire/reconfigure path, and permanently maintaining
//! that fork against every future upstream update. That is a materially
//! bigger, longer-lived commitment than this titlebar itself, so instead:
//! drag-to-move (`window.start_window_move()`), minimize
//! (`window.minimize_window()`), and close (`window.remove_window()`)
//! are kept -- none of them touch the resize/reconfigure path that
//! triggers the bug -- and maximize/restore, plus the double-click-
//! anywhere-on-the-titlebar shortcut to the same, are omitted entirely
//! rather than merely hidden behind a disabled-looking button (a visibly
//! present but non-functional control reads as broken, not as a
//! deliberate scope decision).
//!
//! Revisit this once either upstream `gpui` fixes the underlying
//! acquire/reconfigure gap, or this project decides the fork-and-
//! maintain cost is worth paying.
//!
//! # Structural template
//!
//! Mirrors `gpui_component::TitleBar`'s own drag/minimize/close halves
//! closely (same `TITLE_BAR_HEIGHT`, same hover/theme colors, same
//! `window.use_state`-backed `should_move` drag-detection state machine)
//! -- this module only subtracts the maximize control and its double-
//! click shortcut, it does not reinvent the rest.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement, Pixels, Render, RenderOnce, StatefulInteractiveElement as _, StyleRefinement,
    Styled, TitlebarOptions, Window, WindowControlArea, div, point, px,
};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable as _, StyledExt as _, h_flex};

pub const TITLE_BAR_HEIGHT: Pixels = px(34.);
const TITLE_BAR_LEFT_PADDING: Pixels = px(12.);

type CloseWindowHandler = Rc<Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>>;

/// Duet's own titlebar element -- see the module doc comment for why this
/// exists instead of `gpui_component::TitleBar`.
#[derive(IntoElement)]
pub struct DuetTitleBar {
    style: StyleRefinement,
    children: Vec<AnyElement>,
    on_close_window: Option<CloseWindowHandler>,
}

impl Default for DuetTitleBar {
    fn default() -> Self {
        Self::new()
    }
}

impl DuetTitleBar {
    pub fn new() -> Self {
        Self {
            style: StyleRefinement::default(),
            children: Vec::new(),
            on_close_window: None,
        }
    }

    /// The window's own `WindowOptions::titlebar` metadata to pair with
    /// this element -- `appears_transparent`/`traffic_light_position`
    /// only matter on macOS (native traffic lights floating over this
    /// bar); `title: None` because this element renders its own title
    /// text as a child instead. This project is Linux-only in practice,
    /// but the same values `gpui_component::TitleBar::title_bar_options`
    /// already used are kept for parity in case that changes.
    pub fn title_bar_options() -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), px(9.0))),
        }
    }

    /// Custom close-window handling (Linux only, mirroring
    /// `gpui_component::TitleBar`'s own convention) -- default is
    /// `window.remove_window()`.
    pub fn on_close_window(
        mut self,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        if cfg!(target_os = "linux") {
            self.on_close_window = Some(Rc::new(Box::new(f)));
        }
        self
    }
}

impl Styled for DuetTitleBar {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for DuetTitleBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

/// The drag-detection state machine: a plain mouse-down/mouse-move on the
/// titlebar bar means "start a window move," the same convention
/// `gpui_component::TitleBar` already establishes (see that module's own
/// `should_move` field for the identical shape this mirrors).
struct DuetTitleBarState {
    should_move: bool,
}

impl Render for DuetTitleBarState {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl RenderOnce for DuetTitleBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_linux = cfg!(target_os = "linux");
        let is_macos = cfg!(target_os = "macos");
        let state = window.use_state(cx, |_, _| DuetTitleBarState { should_move: false });

        div().flex_shrink_0().child(
            div()
                .id("duet-title-bar")
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(TITLE_BAR_HEIGHT)
                .pl(TITLE_BAR_LEFT_PADDING)
                .border_b_1()
                .border_color(cx.theme().title_bar_border)
                .bg(cx.theme().title_bar)
                .refine_style(&self.style)
                // Deliberately no `.on_double_click` -> `zoom_window()`
                // here -- see the module doc comment.
                .on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
                    state.should_move = false;
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    window.listener_for(&state, |state, _, _, _| {
                        state.should_move = true;
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    window.listener_for(&state, |state, _, _, _| {
                        state.should_move = false;
                    }),
                )
                .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
                    if state.should_move {
                        state.should_move = false;
                        window.start_window_move();
                    }
                }))
                .child(
                    h_flex()
                        .id("duet-title-bar-content")
                        .window_control_area(WindowControlArea::Drag)
                        .when(window.is_fullscreen(), |this| this.pl_3())
                        .h_full()
                        .justify_between()
                        .flex_shrink_0()
                        .flex_1()
                        .children(self.children),
                )
                .child(duet_window_controls(
                    self.on_close_window,
                    is_linux,
                    is_macos,
                    cx,
                )),
        )
    }
}

/// Minimize + close only -- see the module doc comment for why maximize
/// is missing entirely rather than merely disabled-looking.
fn duet_window_controls(
    on_close_window: Option<CloseWindowHandler>,
    is_linux: bool,
    is_macos: bool,
    cx: &App,
) -> impl IntoElement {
    // macOS uses native traffic lights (floating over this bar via
    // `title_bar_options`'s own `appears_transparent`); no custom control
    // row there. This project is Linux-only in practice, but the same
    // degrade-gracefully convention `gpui_component::TitleBar` already
    // uses is kept for parity.
    if is_macos {
        return div().id("duet-window-controls").into_any_element();
    }

    h_flex()
        .id("duet-window-controls")
        .items_center()
        .flex_shrink_0()
        .h_full()
        .child(control_icon(
            "minimize",
            IconName::WindowMinimize,
            false,
            is_linux,
            |window, _cx| window.minimize_window(),
            cx,
        ))
        .child(control_icon(
            "close",
            IconName::WindowClose,
            true,
            is_linux,
            move |window, cx| {
                if let Some(f) = on_close_window.clone() {
                    f(&ClickEvent::default(), window, cx);
                } else {
                    window.remove_window();
                }
            },
            cx,
        ))
        .into_any_element()
}

/// Mirrors `gpui_component::title_bar::ControlIcon::render` -- theme
/// colors are resolved from `cx` once, up front, and captured by value,
/// since the `.hover`/`.active` style closures below don't get `cx`.
fn control_icon(
    id: &'static str,
    icon: IconName,
    is_close: bool,
    is_linux: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let (hover_fg, hover_bg, active_bg) = if is_close {
        (
            cx.theme().danger_foreground,
            cx.theme().danger,
            cx.theme().danger_active,
        )
    } else {
        (
            cx.theme().secondary_foreground,
            cx.theme().secondary_hover,
            cx.theme().secondary_active,
        )
    };
    let foreground: Hsla = cx.theme().foreground;

    div()
        .id(id)
        .flex()
        .w(TITLE_BAR_HEIGHT)
        .h_full()
        .flex_shrink_0()
        .justify_center()
        .content_center()
        .items_center()
        .text_color(foreground)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(hover_fg))
        .when(is_linux, |this| {
            this.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_click(window, cx);
            })
        })
        .child(Icon::new(icon).small())
}
