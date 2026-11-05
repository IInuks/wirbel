use std::{
    ffi::OsString,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{input::InputTime, renderer::element::solid::SolidColorBuffer},
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, pointer::CursorImageStatus, pointer::MotionEvent},
    output::Output,
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::layout::{Dwindle, Gaps, Rect};

// Values taken from the user's mango config. The config file arrives in stage 2.
pub const GAPS: Gaps = Gaps { outer: 10, inner: 5 };
pub const BORDER_WIDTH: i32 = 4;
pub const BORDER_COLOR: [f32; 4] = rgba(0x44, 0x44, 0x44);
pub const FOCUS_COLOR: [f32; 4] = rgba(0xc9, 0xb8, 0x90);
pub const BACKGROUND_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];

const fn rgba(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// One edge of a window border.
pub struct BorderEdge {
    pub location: Point<i32, Logical>,
    pub buffer: SolidColorBuffer,
}

pub struct Wirbel {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,

    /// Mapped windows with their on-screen position.
    pub space: Space<Window>,
    pub popups: PopupManager,
    /// Tiling order of all windows in `space`.
    pub layout: Dwindle<Window>,
    /// Toplevels that have not made their initial commit yet, so they are not tiled.
    pub unconfigured: Vec<Window>,
    /// Tiled windows waiting for their first buffer. They get focus when it arrives.
    pub awaiting_buffer: Vec<Window>,
    pub focused: Option<Window>,
    pub borders: Vec<BorderEdge>,
    pub cursor_status: CursorImageStatus,

    pub needs_redraw: bool,
    /// Asks the backend for a new frame. Set by the backend.
    pub redraw: Box<dyn Fn()>,
}

impl Wirbel {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        Ok(Self {
            start_time: Instant::now(),
            socket_name: Self::listen(display, event_loop)?,
            loop_signal: event_loop.get_signal(),

            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&dh),
            seat_state,
            seat,

            space: Space::default(),
            popups: PopupManager::default(),
            layout: Dwindle::new(),
            unconfigured: Vec::new(),
            awaiting_buffer: Vec::new(),
            focused: None,
            borders: Vec::new(),
            cursor_status: CursorImageStatus::default_named(),

            needs_redraw: true,
            redraw: Box::new(|| {}),
            display_handle: dh,
        })
    }

    /// Opens the Wayland socket and hooks client traffic into the event loop.
    fn listen(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        let socket = ListeningSocketSource::new_auto()?;
        let name = socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle.insert_source(socket, |stream, _, state| {
            let client = Arc::new(ClientState::default());
            if let Err(err) = state.display_handle.insert_client(stream, client) {
                tracing::warn!(%err, "failed to add client");
            }
        })?;
        handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: the display is only dropped together with this event source.
                if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
                    tracing::error!(%err, "failed to dispatch clients");
                    state.loop_signal.stop();
                }
                Ok(PostAction::Continue)
            },
        )?;
        Ok(name)
    }

    /// Housekeeping after every event loop iteration.
    pub fn after_dispatch(&mut self) {
        self.space.refresh();
        self.popups.cleanup();
        if std::mem::take(&mut self.needs_redraw) {
            (self.redraw)();
        }
        if let Err(err) = self.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
    }

    /// Runs a shell command as a client of this compositor.
    pub fn spawn(&self, command: &str) {
        let child = Command::new("sh")
            .args(["-c", command])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            // A client without Wayland support must not end up on the host's X server.
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .spawn();
        match child {
            Ok(mut child) => {
                // Reap it when it exits, the compositor has no use for the status.
                std::thread::spawn(move || child.wait());
            }
            Err(err) => tracing::warn!(%err, command, "failed to spawn"),
        }
    }

    pub fn add_output(&mut self, output: &Output) {
        self.space.map_output(output, (0, 0));
        self.retile();
    }

    /// Moves windows to the tiles the layout assigns them and redraws their borders.
    pub fn retile(&mut self) {
        let area = self.space.outputs().next().and_then(|output| self.space.output_geometry(output));
        let Some(area) = area else { return };
        let area = Rect::new(area.loc.x, area.loc.y, area.size.w, area.size.h);

        let tiles: Vec<(Window, Rect)> = self
            .layout
            .arrange(area, GAPS)
            .into_iter()
            .map(|(window, tile)| (window.clone(), tile))
            .collect();

        for (window, tile) in &tiles {
            let inner = tile.shrink(BORDER_WIDTH);
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some((inner.w, inner.h).into());
                });
                toplevel.send_pending_configure();
            }
            self.space.map_element(window.clone(), (inner.x, inner.y), false);
        }

        self.borders.clear();
        for (window, tile) in &tiles {
            let color = if self.focused.as_ref() == Some(window) {
                FOCUS_COLOR
            } else {
                BORDER_COLOR
            };
            self.borders.extend(border_edges(*tile, color));
        }

        self.needs_redraw = true;
        self.refresh_pointer_focus();
    }

    /// Gives `window` the keyboard. `None` clears the focus.
    pub fn set_focus(&mut self, window: Option<Window>) {
        if let Some(old) = self.focused.take() {
            old.set_activated(false);
            if let Some(toplevel) = old.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        if let Some(window) = &window {
            window.set_activated(true);
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
            self.space.raise_element(window, false);
        }
        let surface = window
            .as_ref()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        self.focused = window;

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        }
        self.retile();
    }

    /// Forgets a window that is gone, or unmapped itself, and moves focus to its neighbour.
    pub fn remove_window(&mut self, window: &Window) {
        let neighbour = self.layout.neighbour(window).cloned();
        self.layout.remove(window);
        self.awaiting_buffer.retain(|other| other != window);
        self.space.unmap_elem(window);
        if self.focused.as_ref() == Some(window) {
            self.focused = None;
            self.set_focus(neighbour);
        } else {
            self.retile();
        }
    }

    pub fn window_at(&self, position: Point<f64, Logical>) -> Option<Window> {
        self.space.element_under(position).map(|(window, _)| window.clone())
    }

    pub fn surface_under(&self, position: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, location) = self.space.element_under(position)?;
        window
            .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
            .map(|(surface, offset)| (surface, (offset + location).to_f64()))
    }

    /// Re-evaluates what is under the pointer, for when windows moved instead of the pointer.
    pub fn refresh_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let location = pointer.current_location();
        let under = self.surface_under(location);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
    }

    pub fn time(&self) -> Duration {
        self.start_time.elapsed()
    }
}

fn border_edges(tile: Rect, color: [f32; 4]) -> impl Iterator<Item = BorderEdge> {
    let Rect { x, y, w, h } = tile;
    let side = BORDER_WIDTH.min(w).min(h);
    [
        (x, y, w, side),
        (x, y + h - side, w, side),
        (x, y + side, side, h - 2 * side),
        (x + w - side, y + side, side, h - 2 * side),
    ]
    .into_iter()
    .map(move |(x, y, w, h)| BorderEdge {
        location: (x, y).into(),
        buffer: SolidColorBuffer::new((w, h.max(0)), color),
    })
}

/// Per-client data.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
