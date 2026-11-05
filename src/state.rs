use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{input::InputTime, renderer::element::solid::SolidColorBuffer},
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{
        Seat, SeatState,
        keyboard::Keycode,
        pointer::{CursorImageStatus, MotionEvent},
    },
    output::Output,
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        output::OutputManagerState,
        shell::xdg::{SurfaceCachedState, ToplevelSurface, XdgShellState, XdgToplevelSurfaceData},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::{
    config::Config,
    grabs::Drag,
    ipc::{Ipc, Position},
    layout::Rect,
    workspace::{Frame, FullKind, Monitor, Placement, Workspace},
};

/// The smallest size a user can drag a floating window to.
pub const MIN_FLOATING_SIZE: i32 = 32;
/// Largest share of the output a window takes up when it starts floating.
const FLOATING_SHARE: f64 = 0.75;

/// One edge of a window border.
pub struct BorderEdge {
    pub location: Point<i32, Logical>,
    pub buffer: SolidColorBuffer,
}

/// Whether a window asks to float by its nature: dialogs and windows of a fixed size.
fn wants_to_float(toplevel: &ToplevelSurface) -> bool {
    let (min, max) = with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        (current.min_size, current.max_size)
    });
    toplevel.parent().is_some() || (min.w > 0 && min.h > 0 && min == max)
}

/// Whether the client has caught up with the size last asked of it. Until then, the size of what
/// it commits says nothing about what it wants.
fn answered_last_size(toplevel: &ToplevelSurface) -> bool {
    let requested = with_states(toplevel.wl_surface(), |states| {
        let data = states.data_map.get::<XdgToplevelSurfaceData>()?;
        data.lock().unwrap().current_server_state().size
    });
    requested == toplevel.with_committed_state(|state| state.and_then(|state| state.size))
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

    pub config: Config,
    /// Where `reload` reads the configuration from.
    pub config_path: Option<PathBuf>,

    /// Mapped windows with their on-screen position. Holds the visible windows only, `monitors`
    /// holds all of them.
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub monitors: Vec<Monitor>,
    /// Index into `monitors`: where new windows open and workspace keys act.
    pub active_monitor: usize,
    /// Toplevels that have not made their initial commit yet, so they are not placed.
    pub unconfigured: Vec<Window>,
    /// Placed windows waiting for their first buffer. They get focus when it arrives.
    pub awaiting_buffer: Vec<Window>,
    pub focused: Option<Window>,
    /// All visible windows, bottom to top, as of the last `retile`.
    pub frames: Vec<Frame>,
    pub borders: Vec<(Window, Vec<BorderEdge>)>,
    /// Pressed keys that triggered a bind. Their release must not reach a client either.
    pub swallowed_keys: Vec<Keycode>,
    /// The window drag in progress.
    pub drag: Option<Drag>,
    pub cursor_status: CursorImageStatus,
    pub ipc: Ipc,

    pub needs_redraw: bool,
    /// Asks the backend for a new frame. Set by the backend.
    pub redraw: Box<dyn Fn()>,
}

impl Wirbel {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        let (delay, rate) = (config.repeat_delay, config.repeat_rate);
        if let Err(err) = seat.add_keyboard(config.xkb(), delay, rate) {
            tracing::error!(%err, "keymap failed to load, falling back to the xkb default");
            seat.add_keyboard(Default::default(), delay, rate)?;
        }
        seat.add_pointer();

        let socket_name = Self::listen(display, event_loop)?;
        let ipc = Ipc::new(event_loop.handle(), &socket_name);

        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            loop_signal: event_loop.get_signal(),

            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&dh),
            seat_state,
            seat,

            config,
            config_path: None,

            space: Space::default(),
            popups: PopupManager::default(),
            monitors: Vec::new(),
            active_monitor: 0,
            unconfigured: Vec::new(),
            awaiting_buffer: Vec::new(),
            focused: None,
            frames: Vec::new(),
            borders: Vec::new(),
            swallowed_keys: Vec::new(),
            drag: None,
            cursor_status: CursorImageStatus::default_named(),
            ipc,

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

    /// Reads the configuration again. Anything wrong with it keeps the running configuration.
    pub fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else {
            tracing::warn!("no configuration file to reload");
            return;
        };
        let config = match Config::load(&path) {
            Ok((config, errors)) if errors.is_empty() => config,
            Ok((_, errors)) => {
                for error in errors {
                    tracing::error!(path = %path.display(), %error, "configuration not reloaded");
                }
                return;
            }
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "configuration not reloaded");
                return;
            }
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if !config.same_keymap(&self.config)
            && let Err(err) = keyboard.set_xkb_config(self, config.xkb())
        {
            tracing::error!(%err, "configuration not reloaded: bad keymap");
            return;
        }
        keyboard.change_repeat_info(config.repeat_rate, config.repeat_delay);
        self.config = config;
        self.retile();
        tracing::info!("configuration reloaded");
    }

    /// Adds an output at the position the backend gave it. The first output after the last one
    /// went away takes over its windows.
    pub fn add_output(&mut self, output: &Output) {
        self.space.map_output(output, output.current_location());
        match self.monitors.iter_mut().find(|monitor| !monitor.connected) {
            Some(monitor) => {
                monitor.output = output.clone();
                monitor.connected = true;
            }
            None => self.monitors.push(Monitor::new(output.clone())),
        }
        self.retile();
    }

    /// Removes an output. Its workspaces merge into those of the same number on another output.
    pub fn remove_output(&mut self, output: &Output) {
        let Some(index) = self.monitor_of_output(output) else {
            return;
        };
        let location = self
            .seat
            .get_pointer()
            .map(|pointer| pointer.current_location());
        let pointer_lost =
            location.is_some_and(|location| self.monitor_at(location) == Some(index));
        self.space.unmap_output(output);
        let target = self
            .monitors
            .iter()
            .position(|monitor| monitor.connected && monitor.output != *output);
        let Some(target) = target else {
            self.monitors[index].connected = false;
            self.retile();
            return;
        };

        let removed = self.monitors.remove(index);
        let target = target - usize::from(target > index);
        self.active_monitor = if self.active_monitor == index {
            target
        } else {
            self.active_monitor - usize::from(self.active_monitor > index)
        };
        let area = self.monitor_area(target);
        for (number, mut workspace) in removed.workspaces.into_iter().enumerate() {
            for window in workspace.windows() {
                let Some(placement) = workspace.take(&window) else {
                    continue;
                };
                let placement = match (placement, area) {
                    (Placement::Floating(rect), Some(area)) => {
                        Placement::Floating(rect.clamped_into(area))
                    }
                    (placement, _) => placement,
                };
                self.monitors[target].workspaces[number].put(window, placement);
            }
        }
        self.retile();

        let visible = self
            .focused
            .as_ref()
            .is_some_and(|window| self.is_visible(window));
        if !visible {
            let candidate = self.active_workspace().and_then(Workspace::focus_candidate);
            self.set_focus(candidate);
        }
        if pointer_lost && let Some(area) = self.monitor_area(self.active_monitor) {
            let (x, y) = area.center();
            self.move_pointer((f64::from(x), f64::from(y)).into(), InputTime::now());
        }
    }

    /// To be called by the backend after the size of `output` changed.
    pub fn output_resized(&mut self, output: &Output) {
        if let Some(index) = self.monitor_of_output(output)
            && let Some(area) = self.monitor_area(index)
        {
            for workspace in &mut self.monitors[index].workspaces {
                for entry in &mut workspace.floating {
                    entry.rect = entry.rect.clamped_into(area);
                    entry.anchor = (None, None);
                }
            }
        }
        self.retile();
    }

    pub fn monitor_of_output(&self, output: &Output) -> Option<usize> {
        self.monitors
            .iter()
            .position(|monitor| monitor.output == *output)
    }

    /// The rectangle of a connected monitor.
    pub fn monitor_area(&self, index: usize) -> Option<Rect> {
        let monitor = self
            .monitors
            .get(index)
            .filter(|monitor| monitor.connected)?;
        let area = self.space.output_geometry(&monitor.output)?;
        Some(Rect::new(area.loc.x, area.loc.y, area.size.w, area.size.h))
    }

    pub fn monitor_at(&self, location: Point<f64, Logical>) -> Option<usize> {
        (0..self.monitors.len()).find(|index| {
            self.monitor_area(*index)
                .is_some_and(|area| area.contains(location.x, location.y))
        })
    }

    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.monitors
            .get(self.active_monitor)
            .map(Monitor::workspace)
    }

    /// The monitor and workspace of `window`.
    pub fn locate(&self, window: &Window) -> Option<(usize, usize)> {
        self.monitors.iter().enumerate().find_map(|(m, monitor)| {
            let workspaces = monitor.workspaces.iter();
            let w = workspaces.into_iter().position(|ws| ws.contains(window))?;
            Some((m, w))
        })
    }

    pub fn workspace_of_mut(&mut self, window: &Window) -> Option<&mut Workspace> {
        let (m, w) = self.locate(window)?;
        Some(&mut self.monitors[m].workspaces[w])
    }

    pub fn find_window(&self, surface: &WlSurface) -> Option<Window> {
        self.monitors
            .iter()
            .flat_map(|monitor| &monitor.workspaces)
            .flat_map(Workspace::windows)
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == surface)
            })
    }

    /// Whether `window` is on the shown workspace of a connected monitor. A window hidden behind
    /// a fullscreen window counts, it still may get the focus.
    pub fn is_visible(&self, window: &Window) -> bool {
        self.locate(window).is_some_and(|(m, w)| {
            let monitor = &self.monitors[m];
            monitor.connected && monitor.active == w
        })
    }

    /// Places a window that made its initial commit. Returns false if there is no output yet.
    pub fn place_new_window(&mut self, window: Window) -> bool {
        let Some(toplevel) = window.toplevel().cloned() else {
            return true;
        };
        let requested = toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                Some(FullKind::Fullscreen)
            } else if state.states.contains(xdg_toplevel::State::Maximized) {
                Some(FullKind::Maximized)
            } else {
                None
            }
        });
        let floats = wants_to_float(&toplevel);
        let Some(monitor) = self.monitors.get_mut(self.active_monitor) else {
            return false;
        };
        let workspace = monitor.workspace_mut();
        let placement = if floats {
            Placement::Floating(Rect::new(0, 0, 0, 0))
        } else {
            Placement::Tiled
        };
        workspace.put(window.clone(), placement);
        // Whoever covers the workspace would hide the newcomer.
        workspace.full = requested.map(|kind| (window.clone(), kind));
        self.awaiting_buffer.push(window);
        self.retile();
        true
    }

    /// Follows the size a floating window gave itself, and places it once it has one.
    pub fn floating_commit(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        let (Some((m, w)), geometry) = (self.locate(window), window.geometry().size) else {
            return;
        };
        let area = self.monitor_area(m);
        let settled = answered_last_size(&toplevel);
        let dragged = self
            .drag
            .as_ref()
            .is_some_and(|drag| drag.window == *window);
        let Some(entry) = self.monitors[m].workspaces[w].floating_entry_mut(window) else {
            return;
        };
        if geometry.w > 0 && geometry.h > 0 && (settled || entry.rect.w == 0) {
            if entry.rect.w == 0
                && let Some(area) = area
            {
                let (x, y) = area.center();
                entry.rect = Rect::new(x - geometry.w / 2, y - geometry.h / 2, 0, 0);
            }
            let content = entry.content();
            entry.rect = content;
            if !dragged {
                entry.anchor = (None, None);
            }
        }
        let content = entry.content();
        let moved = self
            .frames
            .iter()
            .find(|frame| frame.window == *window)
            .is_some_and(|frame| frame.inner != content);
        if moved {
            self.retile();
        }
    }

    /// Brings the screen in line with the workspaces: shows and hides windows, moves them to
    /// their place in stacking order, and tells clients about their size and state.
    pub fn retile(&mut self) {
        let mut frames = Vec::new();
        for index in 0..self.monitors.len() {
            if let Some(area) = self.monitor_area(index) {
                let workspace = self.monitors[index].workspace();
                frames.extend(workspace.frames(area, &self.config));
            }
        }

        let hidden: Vec<Window> = self
            .space
            .elements()
            .filter(|window| !frames.iter().any(|frame| frame.window == **window))
            .cloned()
            .collect();
        for window in &hidden {
            self.space.unmap_elem(window);
        }

        for frame in &frames {
            if let Some(toplevel) = frame.window.toplevel() {
                let full = frame.request.full;
                let output = self
                    .locate(&frame.window)
                    .map(|(m, _)| &self.monitors[m].output);
                let wl_output =
                    output
                        .zip(toplevel.wl_surface().client())
                        .and_then(|(output, client)| {
                            (full == Some(FullKind::Fullscreen))
                                .then(|| output.client_outputs(&client).next())
                                .flatten()
                        });
                toplevel.with_pending_state(|state| {
                    state.size = frame.request.size.map(Into::into);
                    state.fullscreen_output = wl_output;
                    match full {
                        Some(FullKind::Fullscreen) => {
                            state.states.set(xdg_toplevel::State::Fullscreen);
                            state.states.unset(xdg_toplevel::State::Maximized);
                        }
                        Some(FullKind::Maximized) => {
                            state.states.set(xdg_toplevel::State::Maximized);
                            state.states.unset(xdg_toplevel::State::Fullscreen);
                        }
                        None => {
                            state.states.unset(xdg_toplevel::State::Fullscreen);
                            state.states.unset(xdg_toplevel::State::Maximized);
                        }
                    }
                });
                toplevel.send_pending_configure();
            }
            // Mapping again also moves the window to the top, which yields the frame order.
            self.space
                .map_element(frame.window.clone(), (frame.inner.x, frame.inner.y), false);
        }

        self.frames = frames;
        self.update_borders();
        self.refresh_pointer_focus();
    }

    fn update_borders(&mut self) {
        self.borders.clear();
        for frame in self.frames.iter().filter(|frame| frame.has_border) {
            let color = if self.focused.as_ref() == Some(&frame.window) {
                self.config.focus_color
            } else {
                self.config.border_color
            };
            let edges = border_edges(frame.outer, self.config.border_width, color);
            self.borders.push((frame.window.clone(), edges.collect()));
        }
        self.needs_redraw = true;
    }

    /// Gives `window` the keyboard and raises it. `None` clears the focus.
    pub fn set_focus(&mut self, window: Option<Window>) {
        self.focus_window(window, true);
    }

    /// Like `set_focus`, with stacking left alone if `raise` is false. A window on a hidden
    /// workspace only becomes the one to focus there. Focusing another window ends the fullscreen
    /// of its workspace, because it would stay hidden otherwise.
    pub fn focus_window(&mut self, window: Option<Window>, raise: bool) {
        let mut restack = false;
        if let Some(window) = &window {
            let Some((m, w)) = self.locate(window) else {
                return;
            };
            let visible = self.is_visible(window);
            let workspace = &mut self.monitors[m].workspaces[w];
            workspace.focused = Some(window.clone());
            if !visible {
                return;
            }
            if workspace
                .full
                .as_ref()
                .is_some_and(|(full, _)| full != window)
            {
                workspace.full = None;
                restack = true;
            }
            restack |= raise && workspace.raise(window);
            self.active_monitor = m;
        }

        if self.focused == window {
            if restack {
                self.retile();
            }
            return;
        }
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
        }
        let surface = window
            .as_ref()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        self.focused = window;

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        }
        if restack {
            self.retile();
        } else {
            self.update_borders();
        }
    }

    /// Forgets a window that is gone, or unmapped itself, and moves focus to its neighbour.
    pub fn remove_window(&mut self, window: &Window) {
        self.awaiting_buffer.retain(|other| other != window);
        let next = self.workspace_of_mut(window).and_then(|workspace| {
            workspace.take(window);
            workspace.focus_candidate()
        });
        self.space.unmap_elem(window);
        if self.focused.as_ref() == Some(window) {
            self.set_focus(next);
        }
        self.retile();
    }

    /// Enters or leaves floating. A window that starts floating sits in the middle of its output
    /// at the size it had, but no larger than most of the output.
    pub fn toggle_floating(&mut self, window: &Window) {
        let Some((m, w)) = self.locate(window) else {
            return;
        };
        let size = window.geometry().size;
        let area = self.monitor_area(m);
        let workspace = &mut self.monitors[m].workspaces[w];
        match workspace.take(window) {
            Some(Placement::Floating(_)) => workspace.put(window.clone(), Placement::Tiled),
            Some(Placement::Tiled) => {
                let rect = area.map_or(Rect::new(0, 0, size.w, size.h), |area| {
                    let limit = |length: i32| (f64::from(length) * FLOATING_SHARE) as i32;
                    let w = size
                        .w
                        .clamp(MIN_FLOATING_SIZE, limit(area.w).max(MIN_FLOATING_SIZE));
                    let h = size
                        .h
                        .clamp(MIN_FLOATING_SIZE, limit(area.h).max(MIN_FLOATING_SIZE));
                    let (x, y) = area.center();
                    Rect::new(x - w / 2, y - h / 2, w, h)
                });
                workspace.put(window.clone(), Placement::Floating(rect));
            }
            None => return,
        }
        workspace.focused = Some(window.clone());
        self.retile();
    }

    /// Makes `window` cover its workspace, or ends that if `kind` is `None`.
    pub fn set_full(&mut self, window: &Window, kind: Option<FullKind>) {
        let Some(workspace) = self.workspace_of_mut(window) else {
            return;
        };
        match kind {
            Some(kind) => workspace.full = Some((window.clone(), kind)),
            None if workspace
                .full
                .as_ref()
                .is_some_and(|(full, _)| full == window) =>
            {
                workspace.full = None;
            }
            None => return,
        }
        self.retile();
        if kind.is_some() {
            self.set_focus(Some(window.clone()));
        }
    }

    pub fn full_kind(&self, window: &Window) -> Option<FullKind> {
        let (m, w) = self.locate(window)?;
        let workspace = &self.monitors[m].workspaces[w];
        workspace
            .full
            .as_ref()
            .filter(|(full, _)| full == window)
            .map(|(_, kind)| *kind)
    }

    pub fn window_at(&self, position: Point<f64, Logical>) -> Option<Window> {
        self.space
            .element_under(position)
            .map(|(window, _)| window.clone())
    }

    pub fn surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, location) = self.space.element_under(position)?;
        window
            .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
            .map(|(surface, offset)| (surface, (offset + location).to_f64()))
    }

    /// The nearest point to `location` on any output. Without outputs, `location` itself.
    pub fn clamp_to_outputs(&self, location: Point<f64, Logical>) -> Point<f64, Logical> {
        let areas = (0..self.monitors.len()).filter_map(|index| self.monitor_area(index));
        if areas
            .clone()
            .any(|area| area.contains(location.x, location.y))
        {
            return location;
        }
        let distance = |(x, y): &(f64, f64)| (x - location.x).powi(2) + (y - location.y).powi(2);
        areas
            .map(|area| area.closest_point(location.x, location.y))
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .map_or(location, Into::into)
    }

    /// The one place that moves the pointer, whatever the reason. Anything else that changes its
    /// position has to come through here, or `wirbel pos` misses it. Returns where it ended up.
    pub fn move_pointer(
        &mut self,
        location: Point<f64, Logical>,
        time: InputTime,
    ) -> Point<f64, Logical> {
        let location = self.clamp_to_outputs(location);
        let Some(pointer) = self.seat.get_pointer() else {
            return location;
        };
        let under = self.surface_under(location);
        let serial = SERIAL_COUNTER.next_serial();
        let event = MotionEvent {
            location,
            serial,
            time,
        };
        pointer.motion(self, under, &event);
        pointer.frame(self);
        self.needs_redraw = true;
        let position = self.pointer_position();
        self.ipc.publish(&position);
        location
    }

    /// Where the pointer is, as `wirbel pos` shows it.
    pub fn pointer_position(&self) -> Position {
        let location = self
            .seat
            .get_pointer()
            .map_or_else(Point::default, |pointer| pointer.current_location());
        Position {
            x: location.x,
            y: location.y,
            output: self
                .monitor_at(location)
                .map(|index| self.monitors[index].output.name()),
        }
    }

    /// Moves the pointer on the compositor's own behalf. A grab keeps it where it is.
    pub fn warp_pointer(&mut self, location: Point<f64, Logical>) {
        let grabbed = self
            .seat
            .get_pointer()
            .is_none_or(|pointer| pointer.is_grabbed());
        if !grabbed {
            self.move_pointer(location, InputTime::now());
        }
    }

    /// Re-evaluates what is under the pointer, for when windows or outputs changed around it.
    pub fn refresh_pointer_focus(&mut self) {
        if let Some(pointer) = self.seat.get_pointer() {
            self.move_pointer(pointer.current_location(), InputTime::now());
        }
    }

    /// Like `refresh_pointer_focus`, but stays silent if the pointer is still on the same surface.
    pub fn update_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let under = self.surface_under(pointer.current_location());
        if under.map(|(surface, _)| surface) != pointer.current_focus() {
            self.refresh_pointer_focus();
        }
    }

    pub fn time(&self) -> Duration {
        self.start_time.elapsed()
    }
}

fn border_edges(tile: Rect, width: i32, color: [f32; 4]) -> impl Iterator<Item = BorderEdge> {
    let Rect { x, y, w, h } = tile;
    let side = width.min(w).min(h);
    [
        (x, y, w, side),
        (x, y + h - side, w, side),
        (x, y + side, side, h - 2 * side),
        (x + w - side, y + side, side, h - 2 * side),
    ]
    .into_iter()
    .filter(|(_, _, w, h)| *w > 0 && *h > 0)
    .map(move |(x, y, w, h)| BorderEdge {
        location: (x, y).into(),
        buffer: SolidColorBuffer::new((w, h), color),
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
