//! Reads back whatever is currently on the Wayland clipboard, across all MIME
//! types offered by the current selection owner. Used as the verification
//! tool for both directions of the S-2 round trip: reading what our own
//! `write-copy`/`write-cut` offered, and (manually) reading what Nautilus/
//! Dolphin offer after a real copy in those apps.
//!
//! Per the wl_data_device protocol spec (wayland.xml):
//! "The selection event is sent to a client immediately before receiving
//! keyboard focus and when a new selection is set while the client has
//! keyboard focus." That means a window-less client (no surface, never
//! focused) will *never* receive `wl_data_device.selection`, even if a
//! selection already exists -- this is not documented anywhere obvious and
//! was discovered empirically while building this spike (a first cut of
//! `read` with no surface waited forever). So, exactly like `write_mode`,
//! `read` has to create a tiny surface purely to receive keyboard focus,
//! which is what triggers the compositor to actually deliver the current
//! selection offer.

use std::io::Read as _;
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::DataOfferHandler;
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
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

/// MIME types we specifically look for and print the content of, mirroring
/// design.md §9.10 plus a plain-text fallback so the tool is also useful for
/// eyeballing "what did Nautilus/Dolphin actually put on the clipboard".
const INTERESTING_MIME_TYPES: &[&str] = &[
    "text/uri-list",
    "x-special/gnome-copied-files",
    "application/x-kde-cutselection",
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
];

pub fn run() {
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

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let data_device_manager_state =
        DataDeviceManagerState::bind(&globals, &qh).expect("wl_data_device_manager not available");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("duet-s2-clipboard-read");
    window.set_app_id("net.duet.s2-clipboard-spike");
    window.set_min_size(Some((16, 16)));
    window.commit();

    let pool = SlotPool::new(16 * 16 * 4, &shm).expect("failed to create SlotPool");

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        data_device_manager_state,
        window,
        pool,
        buffer: None,
        first_configure: true,
        width: 16,
        height: 16,
        keyboard: None,
        seat_objects: Vec::new(),
        got_selection: false,
        found_any_mime: Vec::new(),
        results: Vec::new(),
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !state.got_selection && std::time::Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(50), &mut state)
            .expect("event loop dispatch failed");
    }

    if !state.got_selection {
        println!(
            "read: no clipboard selection offer observed within 10s (clipboard may be empty, \
             we never got keyboard focus, or no seat/data-device-manager present)."
        );
        return;
    }

    println!("read: mime types offered: {:?}", state.found_any_mime);
    for (mime, content) in &state.results {
        match std::str::from_utf8(content) {
            Ok(text) => println!("read: [{mime}] ({} bytes) = {text:?}", content.len()),
            Err(_) => println!("read: [{mime}] ({} bytes, not valid UTF-8) = {content:?}", content.len()),
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
    data_device_manager_state: DataDeviceManagerState,
    window: Window,
    pool: SlotPool,
    buffer: Option<Buffer>,
    first_configure: bool,
    width: u32,
    height: u32,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat_objects: Vec<SeatObject>,
    got_selection: bool,
    found_any_mime: Vec<String>,
    results: Vec<(String, Vec<u8>)>,
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
            canvas.chunks_exact_mut(4).for_each(|px| px.copy_from_slice(&[200, 80, 80, 255]));
        }
        self.window.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(self.window.wl_surface()).expect("buffer attach failed");
        self.window.wl_surface().commit();
        let _ = qh;
    }

    fn drain_selection(
        &mut self,
        conn: &Connection,
        offer: &smithay_client_toolkit::data_device_manager::data_offer::SelectionOffer,
    ) {
        let mimes = offer.with_mime_types(|mimes| mimes.to_vec());
        self.found_any_mime = mimes.clone();

        // IMPORTANT: SCTK's `receive()` docs warn that reading the returned
        // pipe right away, without flushing the connection first, can
        // deadlock -- the `wl_data_offer.receive` request has to actually
        // reach the compositor (and be relayed to the source client as a
        // `wl_data_source.send` event) before the peer will ever write
        // anything into our end of the pipe. We hit this deadlock during
        // development: `receive()` then an immediate blocking
        // `read_to_end()` hung forever. Fix: queue up all `receive()` calls,
        // flush once, *then* do the blocking reads.
        let mut pipes = Vec::new();
        for mime in &mimes {
            if !INTERESTING_MIME_TYPES.contains(&mime.as_str()) {
                continue;
            }
            match offer.receive(mime.clone()) {
                Ok(read_pipe) => pipes.push((mime.clone(), read_pipe)),
                Err(e) => eprintln!("read: error requesting mime {mime:?}: {e:?}"),
            }
        }

        conn.flush().expect("failed to flush wayland connection");

        for (mime, mut read_pipe) in pipes {
            // Blocking read is fine here now that we've flushed: this is a
            // short-lived CLI verification tool, not the real app's UI thread.
            let mut buf = Vec::new();
            match read_pipe.read_to_end(&mut buf) {
                Ok(_) => self.results.push((mime.clone(), buf)),
                Err(e) => eprintln!("read: error reading mime {mime:?}: {e}"),
            }
        }
        self.got_selection = true;
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
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {}

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
        let idx = if let Some(idx) = self.seat_objects.iter().position(|s| s.seat == seat) {
            idx
        } else {
            let data_device = self.data_device_manager_state.get_data_device(qh, &seat);
            self.seat_objects.push(SeatObject { seat: seat.clone(), keyboard: None, data_device });
            self.seat_objects.len() - 1
        };

        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None).expect("failed to create keyboard");
            self.keyboard = Some(keyboard.clone());
            self.seat_objects[idx].keyboard = Some(keyboard);
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
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        if self.window.wl_surface() == surface {
            eprintln!("read: got keyboard focus (serial={serial}) -- selection offer should have just arrived");
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: smithay_client_toolkit::seat::keyboard::KeyEvent) {}
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

    fn selection(&mut self, conn: &Connection, _qh: &QueueHandle<Self>, wl_data_device: &wayland_client::protocol::wl_data_device::WlDataDevice) {
        let Some(seat_object) = self.seat_objects.iter().find(|s| s.data_device.inner() == wl_data_device) else {
            return;
        };
        match seat_object.data_device.data().selection_offer() {
            Some(offer) => self.drain_selection(conn, &offer),
            None => {
                // Selection was cleared.
                self.found_any_mime.clear();
                self.results.clear();
                self.got_selection = true;
            }
        }
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_client::protocol::wl_data_device::WlDataDevice) {}
}

impl DataOfferHandler for AppState {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer, _: DndAction) {}
}

// Required by the DataSource dispatch bound even though `read` never creates
// a source; SCTK's delegate_dispatch2! wires this generically.
impl DataSourceHandler for AppState {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: String, _: WritePipe) {}
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
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
