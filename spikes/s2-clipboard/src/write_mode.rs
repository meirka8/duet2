//! Writes files onto the Wayland clipboard (`wl_data_device`) with custom MIME
//! types, bypassing GPUI's clipboard API entirely (it cannot express these
//! MIME types at all -- see documentation/spikes/S-2.md).
//!
//! This mirrors what `wl-copy` does under the hood: create a tiny (offscreen)
//! xdg_toplevel surface purely to receive keyboard focus, grab the serial from
//! the `wl_keyboard.enter` event, and use that serial to set the selection.
//! The process then blocks on the event loop indefinitely, serving `send`
//! requests for as long as it holds the selection -- exactly the lifetime
//! contract regular Wayland clipboard owners (browsers, file managers, `wl-copy`)
//! have to honor, since there is no clipboard manager compositing this data in
//! the background.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::DataOfferHandler;
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::keyboard::{KeyboardHandler, Keysym};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_data_device_manager::DndAction, wl_data_source::WlDataSource, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

use crate::fixtures::ensure_fixtures;
use crate::mime::{self, Mode};

pub fn run(mode: Mode) {
    let uris = ensure_fixtures();
    eprintln!(
        "[s2-clipboard write-{}] fixtures: {:?}",
        mode.verb(),
        uris
    );

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let (globals, event_queue) = registry_queue_init(&conn).expect("failed to init registry");
    let qh = event_queue.handle();
    let mut event_loop: smithay_client_toolkit::reexports::calloop::EventLoop<AppState> =
        smithay_client_toolkit::reexports::calloop::EventLoop::try_new()
            .expect("failed to create event loop");
    let loop_handle = event_loop.handle();
    smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .expect("failed to insert wayland source into event loop");

    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let data_device_manager_state =
        DataDeviceManagerState::bind(&globals, &qh).expect("wl_data_device_manager not available");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(format!("duet-s2-clipboard-write-{}", mode.verb()));
    window.set_app_id("net.duet.s2-clipboard-spike");
    window.set_min_size(Some((16, 16)));
    window.commit();

    let pool = SlotPool::new(16 * 16 * 4, &shm).expect("failed to create SlotPool");

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        data_device_manager_state,
        window,
        pool,
        buffer: None,
        first_configure: true,
        width: 16,
        height: 16,
        exit: false,
        mode,
        uris,
        keyboard: None,
        seat_objects: Vec::new(),
        copy_paste_source: None,
        selection_set: false,
    };

    eprintln!("[s2-clipboard write-{}] waiting for keyboard focus to obtain a serial...", mode.verb());

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !state.selection_set && !state.exit {
        if std::time::Instant::now() > deadline {
            eprintln!(
                "[s2-clipboard write-{}] TIMED OUT waiting for keyboard focus/serial after 15s. \
                 The compositor never gave our surface keyboard focus, so we never obtained a \
                 valid serial for wl_data_device.set_selection. This is the concrete failure \
                 mode the Wayland-fallback prototype has to work around in real integration.",
                mode.verb()
            );
            std::process::exit(1);
        }
        event_loop
            .dispatch(Duration::from_millis(50), &mut state)
            .expect("event loop dispatch failed");
    }

    eprintln!(
        "[s2-clipboard write-{}] selection set. Serving clipboard requests (this process must \
         stay alive -- kill it when done verifying).",
        mode.verb()
    );

    loop {
        event_loop
            .dispatch(Duration::from_millis(200), &mut state)
            .expect("event loop dispatch failed");
        if state.exit {
            eprintln!("[s2-clipboard write-{}] exiting (selection cancelled/taken over).", mode.verb());
            break;
        }
    }
}

struct SeatObject {
    seat: wl_seat::WlSeat,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    data_device: DataDevice,
}

struct AppState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    // Kept alive for the lifetime of the surface it created; never read again.
    #[allow(dead_code)]
    compositor: CompositorState,
    data_device_manager_state: DataDeviceManagerState,
    window: Window,
    pool: SlotPool,
    buffer: Option<Buffer>,
    first_configure: bool,
    width: u32,
    height: u32,
    exit: bool,
    mode: Mode,
    uris: Vec<String>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat_objects: Vec<SeatObject>,
    copy_paste_source: Option<CopyPasteSource>,
    selection_set: bool,
}

impl AppState {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;
        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                .expect("failed to create buffer")
                .0
        });
        if let Some(canvas) = self.pool.canvas(buffer) {
            canvas.chunks_exact_mut(4).for_each(|px| px.copy_from_slice(&[80, 80, 200, 255]));
        }
        self.window.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(self.window.wl_surface()).expect("buffer attach failed");
        self.window.wl_surface().commit();
        let _ = qh;
    }

    fn try_set_selection(&mut self, qh: &QueueHandle<Self>, serial: u32, keyboard: &wl_keyboard::WlKeyboard) {
        if self.selection_set {
            return;
        }
        let Some(seat_object) = self.seat_objects.iter().find(|s| s.keyboard.as_ref() == Some(keyboard))
        else {
            return;
        };
        let mime_types = self.mode.offered_mime_types();
        eprintln!(
            "[s2-clipboard write-{}] creating data source, offering mime types: {:?} (serial={})",
            self.mode.verb(),
            mime_types,
            serial
        );
        let source = self.data_device_manager_state.create_copy_paste_source(qh, mime_types);
        source.set_selection(&seat_object.data_device, serial);
        self.copy_paste_source = Some(source);
        self.selection_set = true;
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for AppState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _window: &Window, configure: WindowConfigure, _serial: u32) {
        self.width = configure.new_size.0.map(|w| w.get()).unwrap_or(self.width);
        self.height = configure.new_size.1.map(|h| h.get()).unwrap_or(self.height);
        self.buffer = None;
        if self.first_configure {
            self.first_configure = false;
            self.draw(qh);
        }
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        let seat_object = if let Some(idx) = self.seat_objects.iter().position(|s| s.seat == seat) {
            idx
        } else {
            let data_device = self.data_device_manager_state.get_data_device(qh, &seat);
            self.seat_objects.push(SeatObject { seat: seat.clone(), keyboard: None, data_device });
            self.seat_objects.len() - 1
        };

        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None).expect("failed to create keyboard");
            self.keyboard = Some(keyboard.clone());
            self.seat_objects[seat_object].keyboard = Some(keyboard);
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(kbd) = self.keyboard.take() {
                kbd.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        if self.window.wl_surface() == surface {
            eprintln!("[s2-clipboard write-{}] got keyboard focus (serial={})", self.mode.verb(), serial);
            self.try_set_selection(qh, serial, keyboard);
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        _event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        // Fallback: if we somehow processed a keypress before `enter` gave us
        // a serial (shouldn't happen, but cheap insurance), use this serial.
        self.try_set_selection(qh, serial, keyboard);
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: smithay_client_toolkit::seat::keyboard::KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: smithay_client_toolkit::seat::keyboard::KeyEvent) {}
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: smithay_client_toolkit::seat::keyboard::Modifiers,
        _: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _: u32,
    ) {
    }
}

impl DataDeviceHandler for AppState {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice) {
        // We're a write-only client in this mode; we don't act on incoming
        // selection offers here (that's `read` mode's job).
    }
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice) {}
}

impl DataOfferHandler for AppState {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer, _: DndAction) {}
}

impl DataSourceHandler for AppState {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, mime: Option<String>) {
        eprintln!("[s2-clipboard write-{}] peer accepted mime type: {mime:?}", self.mode.verb());
    }

    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource, mime: String, write_pipe: WritePipe) {
        let Some(ours) = self.copy_paste_source.as_ref() else { return };
        if ours.inner() != source {
            return;
        }
        let payload: Option<Vec<u8>> = match mime.as_str() {
            mime::URI_LIST => Some(mime::uri_list_payload(&self.uris)),
            mime::GNOME_COPIED_FILES => Some(mime::gnome_copied_files_payload(self.mode, &self.uris)),
            mime::KDE_CUT_SELECTION if self.mode == Mode::Cut => Some(mime::kde_cut_selection_payload()),
            other => {
                eprintln!("[s2-clipboard write-{}] ignoring send request for unsupported mime {other:?}", self.mode.verb());
                None
            }
        };
        if let Some(bytes) = payload {
            eprintln!("[s2-clipboard write-{}] serving {} bytes for mime {mime:?}", self.mode.verb(), bytes.len());
            let fd: OwnedFd = write_pipe.into();
            let mut f = File::from(fd);
            if let Err(e) = f.write_all(&bytes) {
                eprintln!("[s2-clipboard write-{}] error writing clipboard payload: {e}", self.mode.verb());
            }
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self.copy_paste_source.as_ref().map(|s| s.inner()) == Some(source) {
            eprintln!("[s2-clipboard write-{}] selection cancelled (another client took ownership)", self.mode.verb());
            self.exit = true;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(AppState);
smithay_client_toolkit::delegate_registry!(AppState);
