use std::{
    borrow::Cow,
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        input::InputTime,
        renderer::{
            element::solid::SolidColorBuffer, sync::SyncPoint, utils::with_renderer_surface_state,
        },
    },
    desktop::{
        LayerSurface, PopupGrab, PopupManager, PopupUngrabStrategy, Space, Window,
        WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
        utils::{under_from_surface_tree, with_surfaces_surface_tree},
    },
    input::{
        Seat, SeatState,
        keyboard::{Keycode, LedState},
        pointer::{CursorImageStatus, MotionEvent},
    },
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
            generic::Generic,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{
            CompositorClientState, CompositorState, SurfaceData, get_parent, send_surface_state,
            with_states,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        drm_syncobj::DrmSyncobjState,
        fractional_scale::{FractionalScaleManagerState, with_fractional_scale},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        output::OutputManagerState,
        pointer_constraints::{PointerConstraintsState, with_pointer_constraint},
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState},
            xdg::{
                SurfaceCachedState, ToplevelSurface, XdgShellState, XdgToplevelSurfaceData,
                decoration::XdgDecorationState,
            },
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Surface, X11Wm, XWayland, xwm::WmWindowType},
};

use crate::{
    config::{Action, Config},
    cursor::Cursors,
    focus::KeyboardFocus,
    grabs::Drag,
    handlers::{ForeignToplevelState, ScreencopyState, SessionLock},
    ipc::{Ipc, Position},
    layout::Rect,
    render::{LayerEntry, LayerPart, LayerTiers},
    safety::Chord,
    stats::Stats,
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

/// The same for an X11 window, which has some more ways of saying so.
fn x11_wants_to_float(window: &X11Surface) -> bool {
    let fixed = match (window.min_size(), window.max_size()) {
        (Some(min), Some(max)) => min.w > 0 && min.h > 0 && min == max,
        _ => false,
    };
    let floating_type = matches!(
        window.window_type(),
        Some(
            WmWindowType::Dialog
                | WmWindowType::Menu
                | WmWindowType::Splash
                | WmWindowType::Toolbar
                | WmWindowType::Utility
        )
    );
    fixed || floating_type || window.is_transient_for().is_some() || window.is_modal()
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

/// What lies under a point of the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Lock,
    Layer(LayerSurface),
    Window(Window),
}

pub struct Wirbel {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub screencopy: ScreencopyState,
    pub foreign_toplevels: ForeignToplevelState,
    pub cursor_shape_state: CursorShapeManagerState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub session_lock_state: SessionLockManagerState,
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub idle_notifier_state: IdleNotifierState<Self>,
    pub activation_state: XdgActivationState,
    pub constraints_state: PointerConstraintsState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub presentation_state: PresentationState,
    /// Only where the GPU driver can do it, set up by `offer_syncobj`.
    pub syncobj_state: Option<DrmSyncobjState>,
    pub xwayland_shell_state: XWaylandShellState,
    /// The X11 window manager, once Xwayland is up.
    pub xwm: Option<X11Wm>,
    /// What keeps Xwayland going, see `handlers::xwayland`.
    pub xwayland: XwaylandState,
    pub clock: Clock<Monotonic>,

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
    /// X11 menus and tooltips: they place themselves, above all windows, and take no focus.
    pub overrides: Vec<Window>,
    pub focused: Option<Window>,
    /// Layer surfaces between their creation or unmap and the commit that maps them.
    pub unmapped_layers: Vec<(LayerSurface, Output)>,
    /// The layer surface that got the keyboard by a click. Goes before `focused`.
    pub layer_focus: Option<LayerSurface>,
    /// The popup grab in progress and the root surface of its popups.
    pub popup_grab: Option<(PopupGrab<Self>, WlSurface)>,
    pub lock: SessionLock,
    /// Surfaces that keep the session from going idle while they are visible.
    pub idle_inhibitors: Vec<WlSurface>,
    /// All visible windows, bottom to top, as of the last `retile`.
    pub frames: Vec<Frame>,
    pub borders: Vec<(Window, Vec<BorderEdge>)>,
    /// Pressed keys that triggered a bind. Their release must not reach a client either.
    pub swallowed_keys: Vec<Keycode>,
    /// The bind of a modifier key, on its way out of the keyboard filter.
    pub modifier_action: Option<Action>,
    /// The timer of a held bind.
    pub bind_repeat: Option<RegistrationToken>,
    pub last_axis_bind: Option<Instant>,
    /// The window drag in progress.
    pub drag: Option<Drag>,
    pub cursor_status: CursorImageStatus,
    /// The theme for named cursors. Stays empty where the host draws them.
    pub cursors: Option<Cursors>,
    /// What the client shows at the pointer during drag and drop.
    pub dnd_icon: Option<WlSurface>,
    pub ipc: Ipc,
    /// Asks the renderer whether it can use a dmabuf. Set by a backend that offers dmabufs.
    pub import_dmabuf: Box<dyn FnMut(&Dmabuf) -> bool>,

    pub needs_redraw: bool,
    pub stats: Stats,
    /// Asks the backend for a new frame. Set by the backend.
    pub redraw: Box<dyn Fn()>,
    /// Switches to a virtual terminal. Set by a backend that owns one.
    pub change_vt: Option<Box<dyn FnMut(i32)>>,
    /// Tells the GPUs about a newly committed buffer. Set by a backend with more than one.
    pub early_import: Box<dyn FnMut(&WlSurface)>,
    /// Sets the keyboard LEDs. Set by a backend that has real keyboards.
    pub led_update: Box<dyn FnMut(LedState)>,
    /// Sees the same keys as the watcher thread, which is not running in every setup.
    pub chord: Chord,
    /// What the emergency chord does. The binary ends the process here.
    pub emergency: Box<dyn Fn()>,
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
        let clock = Clock::<Monotonic>::new();
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);

        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            loop_handle: event_loop.handle(),
            loop_signal: event_loop.get_signal(),

            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            layer_shell_state: WlrLayerShellState::new::<Self>(&dh),
            decoration_state: XdgDecorationState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            dmabuf_state: DmabufState::new(),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&dh),
            seat_state,
            seat,
            data_device_state: DataDeviceState::new::<Self>(&dh),
            data_control_state: DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            ),
            primary_selection_state,
            screencopy: ScreencopyState::new(&dh),
            foreign_toplevels: ForeignToplevelState::new(&dh),
            cursor_shape_state: CursorShapeManagerState::new::<Self>(&dh),
            viewporter_state: ViewporterState::new::<Self>(&dh),
            fractional_scale_state: FractionalScaleManagerState::new::<Self>(&dh),
            session_lock_state: SessionLockManagerState::new::<Self, _>(&dh, |_| true),
            idle_inhibit_state: IdleInhibitManagerState::new::<Self>(&dh),
            idle_notifier_state: IdleNotifierState::new(&dh, event_loop.handle()),
            activation_state: XdgActivationState::new::<Self>(&dh),
            constraints_state: PointerConstraintsState::new::<Self>(&dh),
            relative_pointer_state: RelativePointerManagerState::new::<Self>(&dh),
            presentation_state: PresentationState::new::<Self>(&dh, clock.id() as u32),
            syncobj_state: None,
            xwayland_shell_state: XWaylandShellState::new::<Self>(&dh),
            xwm: None,
            xwayland: XwaylandState::default(),
            clock,

            config,
            config_path: None,

            space: Space::default(),
            popups: PopupManager::default(),
            monitors: Vec::new(),
            active_monitor: 0,
            unconfigured: Vec::new(),
            awaiting_buffer: Vec::new(),
            overrides: Vec::new(),
            focused: None,
            unmapped_layers: Vec::new(),
            layer_focus: None,
            popup_grab: None,
            lock: SessionLock::default(),
            idle_inhibitors: Vec::new(),
            frames: Vec::new(),
            borders: Vec::new(),
            swallowed_keys: Vec::new(),
            modifier_action: None,
            bind_repeat: None,
            last_axis_bind: None,
            drag: None,
            cursor_status: CursorImageStatus::default_named(),
            cursors: None,
            dnd_icon: None,
            ipc,
            import_dmabuf: Box::new(|_| false),

            needs_redraw: true,
            stats: Stats::default(),
            redraw: Box::new(|| {}),
            change_vt: None,
            early_import: Box::new(|_| {}),
            led_update: Box::new(|_| {}),
            chord: Chord::default(),
            emergency: Box::new(|| tracing::error!("emergency chord pressed")),
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
        let start = self.stats.start();
        self.space.refresh();
        self.popups.cleanup();
        self.reap_popup_grab();
        self.settle_x11_windows();
        self.lock_refresh();
        self.update_idle_inhibit();
        self.refresh_foreign_toplevels();
        self.publish_workspaces();
        if std::mem::take(&mut self.needs_redraw) {
            (self.redraw)();
        }
        if let Err(err) = self.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
        self.stats.tidied(start);
    }

    /// With `--stats`: tells the stats when the GPU is through with a frame.
    pub fn time_gpu(&self, fence: &SyncPoint) {
        let Some(start) = self.stats.start() else {
            return;
        };
        let Some(fd) = fence.export() else {
            return;
        };
        let source = Generic::new(fd, Interest::READ, Mode::OneShot);
        let inserted = self.loop_handle.insert_source(source, move |_, _, state| {
            state.stats.gpu_done(start);
            Ok(PostAction::Remove)
        });
        if let Err(err) = inserted {
            tracing::debug!(%err, "cannot wait for this fence");
        }
    }

    /// Runs a shell command as a client of this compositor.
    pub fn spawn(&self, command: &str) {
        let mut child = Command::new("sh");
        child
            .args(["-c", command])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .stdin(Stdio::null());
        // Without Xwayland, a client must not end up on the host's X server.
        match self.xwayland.display {
            Some(display) => child.env("DISPLAY", format!(":{display}")),
            None => child.env_remove("DISPLAY"),
        };
        let child = child.spawn();
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
        self.close_layers(output);
        self.lock_remove_output(output);
        self.screencopy.output_removed(output);
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

    /// To be called by the backend after the size or the scale of `output` changed.
    pub fn output_resized(&mut self, output: &Output) {
        self.screencopy.output_changed(output);
        layer_map_for_output(output).arrange();
        self.lock_configure_output(output);
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

    /// The part of a connected monitor that panels leave free.
    pub fn work_area(&self, index: usize) -> Option<Rect> {
        let area = self.monitor_area(index)?;
        let zone = layer_map_for_output(&self.monitors[index].output).non_exclusive_zone();
        if zone.size.w <= 0 || zone.size.h <= 0 {
            return Some(area);
        }
        Some(Rect::new(
            area.x + zone.loc.x,
            area.y + zone.loc.y,
            zone.size.w,
            zone.size.h,
        ))
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
            .find(|window| window.wl_surface().as_deref() == Some(surface))
    }

    /// The window of an X11 surface, managed or not.
    pub fn find_x11(&self, surface: &X11Surface) -> Option<Window> {
        self.monitors
            .iter()
            .flat_map(|monitor| &monitor.workspaces)
            .flat_map(Workspace::windows)
            .chain(self.overrides.iter().cloned())
            .find(|window| window.x11_surface() == Some(surface))
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
        let placement = if wants_to_float(&toplevel) {
            Placement::Floating(Rect::new(0, 0, 0, 0))
        } else {
            Placement::Tiled
        };
        self.place(window, placement, requested)
    }

    /// `place_new_window` for a window of the X server. Its size is known from the start, and one
    /// that names a place for itself gets it.
    pub fn place_x11_window(&mut self, window: Window) -> bool {
        let Some(surface) = window.x11_surface().cloned() else {
            return true;
        };
        let requested = if surface.is_fullscreen() {
            Some(FullKind::Fullscreen)
        } else if surface.is_maximized() {
            Some(FullKind::Maximized)
        } else {
            None
        };
        let placement = if x11_wants_to_float(&surface) {
            let wanted = surface.last_configure();
            let placed = wanted.loc != Point::default() && wanted.size.w > 0 && wanted.size.h > 0;
            Placement::Floating(if placed {
                Rect::new(wanted.loc.x, wanted.loc.y, wanted.size.w, wanted.size.h)
            } else {
                Rect::new(0, 0, 0, 0)
            })
        } else {
            Placement::Tiled
        };
        self.place(window, placement, requested)
    }

    fn place(&mut self, window: Window, placement: Placement, requested: Option<FullKind>) -> bool {
        let Some(monitor) = self.monitors.get_mut(self.active_monitor) else {
            return false;
        };
        let workspace = monitor.workspace_mut();
        workspace.put(window.clone(), placement);
        // Whoever covers the workspace would hide the newcomer.
        workspace.full = requested.map(|kind| (window.clone(), kind));
        self.awaiting_buffer.push(window);
        self.retile();
        true
    }

    /// Follows the size a floating window gave itself, and places it once it has one.
    pub fn floating_commit(&mut self, window: &Window) {
        let (Some((m, w)), geometry) = (self.locate(window), window.geometry().size) else {
            return;
        };
        let area = self.work_area(m);
        let settled = match window.x11_surface() {
            // X has no way of saying that a commit answers a configure, only the size tells.
            Some(surface) => surface.bbox().size == surface.last_configure().size,
            None => window.toplevel().is_some_and(answered_last_size),
        };
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
            let work = self.work_area(index);
            self.monitors[index].work_area = work;
            if let (Some(area), Some(work)) = (self.monitor_area(index), work) {
                let workspace = self.monitors[index].workspace();
                frames.extend(workspace.frames(area, work, &self.config));
            }
        }

        let hidden: Vec<Window> = self
            .space
            .elements()
            .filter(|window| !frames.iter().any(|frame| frame.window == **window))
            .filter(|window| !self.overrides.contains(window))
            .cloned()
            .collect();
        for window in &hidden {
            self.space.unmap_elem(window);
            self.dismiss_window_popups(window);
            if let Some(surface) = window.x11_surface()
                && let Err(err) = surface.set_hidden(true)
            {
                tracing::debug!(%err, "failed to tell an X11 window that it is hidden");
            }
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
            if let Some(surface) = frame.window.x11_surface() {
                configure_x11(surface, frame);
            }
            // Mapping again also moves the window to the top, which yields the frame order.
            self.space
                .map_element(frame.window.clone(), (frame.inner.x, frame.inner.y), false);
        }
        for window in &self.overrides {
            if let Some(surface) = window.x11_surface() {
                self.space
                    .map_element(window.clone(), surface.last_configure().loc, false);
            }
        }

        if let Some(xwm) = self.xwm.as_mut() {
            let stack = frames.iter().rev();
            let order = stack.filter_map(|frame| frame.window.x11_surface());
            if let Err(err) = xwm.update_stacking_order_upwards(order) {
                tracing::warn!(%err, "failed to update the X11 stacking order");
            }
        }

        self.frames = frames;
        self.update_borders();
        self.update_surface_scales();
        self.refresh_pointer_focus();
    }

    /// Tells visible surfaces the scale and transform of their output, as far as they changed.
    pub(crate) fn update_surface_scales(&self) {
        let mut roots: Vec<(WlSurface, Output)> = Vec::new();
        for frame in &self.frames {
            if let (Some(root), Some((m, _))) =
                (frame.window.wl_surface(), self.locate(&frame.window))
            {
                roots.push((root.into_owned(), self.monitors[m].output.clone()));
            }
        }
        for monitor in self.monitors.iter().filter(|monitor| monitor.connected) {
            let map = layer_map_for_output(&monitor.output);
            let layers = map.layers().map(LayerSurface::wl_surface);
            roots.extend(layers.map(|layer| (layer.clone(), monitor.output.clone())));
        }
        roots.extend(self.lock.surfaces());

        for (root, output) in roots {
            let popups = PopupManager::popups_for_surface(&root);
            let popups = popups.map(|(popup, _)| popup.wl_surface().clone());
            for tree in popups.chain([root.clone()]) {
                with_surfaces_surface_tree(&tree, |surface, states| {
                    apply_output_scale(surface, states, &output);
                });
            }
        }
    }

    /// The output a surface is shown on, or else the one it is likely to appear on.
    pub fn output_of_surface(&self, surface: &WlSurface) -> Option<Output> {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(popup) = self.popups.find_popup(&root)
            && let Ok(parent) = find_popup_root_surface(&popup)
        {
            root = parent;
        }
        let window = self.find_window(&root);
        window
            .and_then(|window| self.locate(&window))
            .map(|(m, _)| self.monitors[m].output.clone())
            .or_else(|| self.find_layer(&root).map(|(_, output)| output))
            .or_else(|| self.lock.output_of(&root))
            .or_else(|| {
                let monitor = self.monitors.get(self.active_monitor)?;
                Some(monitor.output.clone())
            })
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
        if window.is_some() {
            self.layer_focus = None;
        }
        self.focus_window(window, true);
        self.update_keyboard_focus();
    }

    /// Who should have the keyboard: the lock screen rules everything, then a layer surface that
    /// claims the keyboard, then one that was clicked, then the focused window.
    fn keyboard_target(&self) -> Option<KeyboardFocus> {
        if self.lock.is_active() {
            let surface = self
                .lock
                .surface_for_input(self.monitors.get(self.active_monitor));
            return surface.map(KeyboardFocus::Surface);
        }
        let active = self.monitors.get(self.active_monitor).into_iter();
        let monitors = active.chain(&self.monitors);
        for monitor in monitors {
            let map = layer_map_for_output(&monitor.output);
            for layer in [Layer::Overlay, Layer::Top] {
                let exclusive = map.layers_on(layer).rev().find(|surface| {
                    surface.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::Exclusive
                        && has_buffer(surface.wl_surface())
                });
                if let Some(surface) = exclusive {
                    return Some(surface.wl_surface().clone().into());
                }
            }
        }
        let layer = self.layer_focus.as_ref().filter(|layer| {
            self.find_layer(layer.wl_surface()).is_some()
                && layer.can_receive_keyboard_focus()
                && has_buffer(layer.wl_surface())
        });
        let layer = layer.map(|layer| layer.wl_surface().clone().into());
        layer.or_else(|| {
            let window = self.focused.as_ref()?;
            match window.x11_surface() {
                Some(surface) => Some(KeyboardFocus::X11(Box::new(surface.clone()))),
                None => Some(window.toplevel()?.wl_surface().clone().into()),
            }
        })
    }

    /// Brings the keyboard in line with `keyboard_target`. Whatever changes the answer calls this.
    pub fn update_keyboard_focus(&mut self) {
        let target = self.keyboard_target();
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard.current_focus() != target
        {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
        self.update_pointer_constraint();
    }

    /// Withdraws the popups of a window or layer surface the user can no longer see.
    pub fn dismiss_popups(&mut self, surface: &WlSurface) {
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|(_, root)| root == surface)
        {
            self.end_popup_grab();
        }
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            // Fails only for a surface that is gone along with its popups.
            let _ = PopupManager::dismiss_popup(surface, &popup);
        }
    }

    fn dismiss_window_popups(&mut self, window: &Window) {
        if let Some(toplevel) = window.toplevel() {
            self.dismiss_popups(toplevel.wl_surface());
        }
    }

    /// Ends the popup grab and withdraws its popups.
    pub fn end_popup_grab(&mut self) {
        let Some((mut grab, _)) = self.popup_grab.take() else {
            return;
        };
        grab.ungrab(PopupUngrabStrategy::All);
        let serials = [Some(grab.serial()), grab.previous_serial()];
        let serials = serials.into_iter().flatten();
        if let Some(pointer) = self.seat.get_pointer()
            && serials.clone().any(|serial| pointer.has_grab(serial))
        {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.unset_grab(self, serial, InputTime::now());
        }
        if let Some(keyboard) = self.seat.get_keyboard()
            && serials.clone().any(|serial| keyboard.has_grab(serial))
        {
            keyboard.unset_grab(self);
        }
        self.update_keyboard_focus();
        self.update_pointer_focus();
    }

    /// Notices a grab that came to an end without us, because its popups went away.
    fn reap_popup_grab(&mut self) {
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|(grab, _)| grab.has_ended())
        {
            self.end_popup_grab();
        }
    }

    /// Only a surface the user can see holds off idling.
    fn update_idle_inhibit(&mut self) {
        self.idle_inhibitors.retain(Resource::is_alive);
        let inhibited = self.idle_inhibitors.iter().any(|surface| {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            self.is_shown(&root)
        });
        self.idle_notifier_state.set_is_inhibited(inhibited);
    }

    /// Whether the root surface `root` is on screen right now.
    fn is_shown(&self, root: &WlSurface) -> bool {
        if self.lock.is_active() {
            return self.lock.output_of(root).is_some();
        }
        self.find_layer(root).is_some()
            || self
                .find_window(root)
                .is_some_and(|window| self.frames.iter().any(|frame| frame.window == window))
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
            if raise
                && let (Some(xwm), Some(surface)) = (self.xwm.as_mut(), window.x11_surface())
                && let Err(err) = xwm.raise_window(surface)
            {
                tracing::warn!(%err, "failed to raise an X11 window");
            }
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
        self.focused = window;
        self.end_popup_grab();
        self.update_keyboard_focus();
        if restack {
            self.retile();
        } else {
            self.update_borders();
        }
    }

    /// Forgets a window that is gone, or unmapped itself, and moves focus to its neighbour.
    pub fn remove_window(&mut self, window: &Window) {
        self.dismiss_window_popups(window);
        self.awaiting_buffer.retain(|other| other != window);
        let next = self.workspace_of_mut(window).and_then(|workspace| {
            workspace.take(window);
            workspace.focus_candidate()
        });
        self.space.unmap_elem(window);
        if self.focused.as_ref() == Some(window) {
            self.focus_window(next, true);
            self.update_keyboard_focus();
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
        let area = self.work_area(m);
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
        match self.under(position)?.0 {
            Target::Window(window) => Some(window),
            _ => None,
        }
    }

    pub fn surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.under(position)
            .map(|(_, surface, location)| (surface, location))
    }

    /// The topmost thing at `position`, with its surface and where that surface begins. A locked
    /// session has nothing but lock surfaces.
    pub fn under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(Target, WlSurface, Point<f64, Logical>)> {
        let monitor = self.monitor_at(position);
        if self.lock.is_active() {
            let area = self.monitor_area(monitor?)?;
            let root = self.lock.surface_on(&self.monitors[monitor?].output)?;
            let origin = Point::<i32, Logical>::from((area.x, area.y));
            let relative = position - origin.to_f64();
            let (surface, offset) =
                under_from_surface_tree(&root, relative, (0, 0), WindowSurfaceType::ALL)?;
            return Some((Target::Lock, surface, (offset + origin).to_f64()));
        }

        let LayerTiers { above, below } =
            monitor.map_or_else(LayerTiers::default, |index| self.layer_tiers(index));
        let on_layer = |entries: &[LayerEntry]| {
            entries.iter().find_map(|entry| {
                let relative = position - entry.location.to_f64();
                let kind = match entry.part {
                    LayerPart::Whole => WindowSurfaceType::ALL,
                    LayerPart::Surface => {
                        WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE
                    }
                    LayerPart::Popups => WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                };
                let (surface, offset) = entry.layer.surface_under(relative, kind)?;
                let location = (offset + entry.location).to_f64();
                Some((Target::Layer(entry.layer.clone()), surface, location))
            })
        };
        on_layer(&above)
            .or_else(|| {
                let (window, location) = self.space.element_under(position)?;
                let relative = position - location.to_f64();
                let (surface, offset) = window.surface_under(relative, WindowSurfaceType::ALL)?;
                let location = (offset + location).to_f64();
                Some((Target::Window(window.clone()), surface, location))
            })
            .or_else(|| on_layer(&below))
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
        self.update_pointer_constraint();
        location
    }

    /// A constraint holds while the pointer is inside its region and its window has the keyboard:
    /// a window the user is not working with does not get to catch the pointer.
    pub fn update_pointer_constraint(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(surface) = pointer.current_focus() else {
            return;
        };
        let root_of = |mut surface: WlSurface| {
            while let Some(parent) = get_parent(&surface) {
                surface = parent;
            }
            surface
        };
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|focus| focus.wl_surface().map(Cow::into_owned))
            .map(root_of);
        let wanted = focused == Some(root_of(surface.clone()));
        let location = pointer.current_location();
        let origin = self.surface_under(location).map(|under| under.1);
        with_pointer_constraint(&surface, &pointer, |constraint| {
            let Some(constraint) = constraint else {
                return;
            };
            let inside = origin.is_some_and(|origin| {
                let local = (location - origin).to_i32_floor();
                constraint
                    .region()
                    .is_none_or(|region| region.contains(local))
            });
            if wanted && inside {
                constraint.activate();
            } else if constraint.is_active() && !wanted {
                constraint.deactivate();
            }
        });
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

/// Tells an X11 window about its frame, as far as anything changed. A floating window has the last
/// word on its size, like all clients, so what it is told may differ from what it shows so far.
fn configure_x11(surface: &X11Surface, frame: &Frame) {
    let (w, h) = frame.request.size.unwrap_or((frame.inner.w, frame.inner.h));
    let extents = surface.frame_extents();
    let rect = Rectangle::new(
        (frame.inner.x - extents.left, frame.inner.y - extents.top).into(),
        (
            w + extents.left + extents.right,
            h + extents.top + extents.bottom,
        )
            .into(),
    );
    let full = frame.request.full == Some(FullKind::Fullscreen);
    let maximized = frame.request.full == Some(FullKind::Maximized);
    let result = (|| {
        if surface.is_hidden() {
            surface.set_hidden(false)?;
        }
        if surface.is_fullscreen() != full {
            surface.set_fullscreen(full)?;
        }
        if surface.is_maximized() != maximized {
            surface.set_maximized(maximized)?;
        }
        Ok::<_, smithay::reexports::x11rb::errors::ConnectionError>(())
    })();
    if let Err(err) = result {
        tracing::debug!(%err, "failed to update an X11 window");
    }
    if surface.last_configure() != rect
        && let Err(err) = surface.configure(rect)
    {
        tracing::debug!(?err, "failed to configure an X11 window");
    }
}

/// Whether a surface has content to show.
pub(crate) fn has_buffer(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Sends the scale and transform of `output` to `surface`, as far as they changed.
pub(crate) fn apply_output_scale(surface: &WlSurface, states: &SurfaceData, output: &Output) {
    let scale = output.current_scale();
    send_surface_state(
        surface,
        states,
        scale.integer_scale(),
        output.current_transform(),
    );
    with_fractional_scale(states, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
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

/// What it takes to keep an X server running.
#[derive(Debug, Default)]
pub struct XwaylandState {
    /// The display number. Known from the start: clients may connect before the server is up, and
    /// wait for it.
    pub display: Option<u32>,
    /// Dropping this ends the server.
    pub server: Option<XWayland>,
    /// When it was started, to give up on a server that keeps dying.
    pub starts: Vec<Instant>,
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
