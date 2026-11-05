//! X11 programs: the window manager side of Xwayland. X11 windows go through the same workspaces as
//! all others, `retile` tells them where they are. Menus and tooltips (override redirect) place
//! themselves and stay out of the tree, in `overrides`.
//!
//! Xwayland starts with the compositor. Its display number is known before that, the socket exists
//! and clients wait on it, so `-c` commands do not have to wait for anything.

use std::{
    borrow::Cow,
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::Stdio,
    time::{Duration, Instant},
};

use smithay::{
    desktop::Window,
    reexports::wayland_server::{Client, protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle},
    wayland::{
        seat::WaylandFocus,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};

use crate::{
    Wirbel,
    config::Xwayland,
    cursor::Cursors,
    focus::KeyboardFocus,
    grabs::DragKind,
    layout::Rect,
    state::has_buffer,
    workspace::FullKind,
    xwayland::{self, Binary},
};

/// A server that dies this often in this time is not coming back.
const MAX_STARTS: usize = 3;
const START_WINDOW: Duration = Duration::from_secs(60);

impl XWaylandShellHandler for Wirbel {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl Wirbel {
    /// Starts the X server, if there is one and the configuration wants it.
    pub fn start_xwayland(&mut self) {
        let configured = match &self.config.xwayland {
            Xwayland::Off => return,
            Xwayland::Find => None,
            Xwayland::At(path) => Some(path.clone()),
        };
        let now = Instant::now();
        self.xwayland
            .starts
            .retain(|start| now.duration_since(*start) < START_WINDOW);
        if self.xwayland.starts.len() >= MAX_STARTS {
            tracing::error!("Xwayland keeps dying, giving up: no X11 programs from now on");
            return;
        }
        self.xwayland.starts.push(now);

        let Some(binary) = xwayland::locate(configured.as_deref()) else {
            tracing::warn!("no working Xwayland found: no X11 programs");
            return;
        };
        tracing::info!(command = ?binary.0, "starting Xwayland");
        if let Err(err) = self.spawn_xwayland(&binary) {
            tracing::error!(%err, "failed to start Xwayland");
        }
    }

    fn spawn_xwayland(&mut self, binary: &Binary) -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!("wirbel-{}", std::process::id()));
        let path = binary.shim(&directory)?;
        let envs = path.iter().map(|path| ("PATH", path.0.clone()));
        let spawned = XWayland::spawn(
            &self.display_handle,
            None,
            envs,
            std::iter::empty::<&str>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| {},
        );
        if path.is_some() {
            std::fs::remove_dir_all(&directory)?;
        }
        let (server, client) = spawned?;
        self.xwayland.display = Some(server.display_number());
        let source = self
            .loop_handle
            .insert_source(server, move |event, _, state| match event {
                XWaylandEvent::Ready { x11_socket, .. } => {
                    state.xwayland_ready(x11_socket, client.clone());
                }
                XWaylandEvent::Error => {
                    tracing::error!("Xwayland died while starting");
                    state.xwayland_gone();
                }
            })
            .map_err(|err| err.error)?;
        self.xwayland.source = Some(source);
        Ok(())
    }

    fn xwayland_ready(&mut self, socket: UnixStream, client: Client) {
        let wm = X11Wm::start_wm(
            self.loop_handle.clone(),
            &self.display_handle,
            socket,
            client,
        );
        let mut wm = match wm {
            Ok(wm) => wm,
            Err(err) => {
                tracing::error!(%err, "failed to start the X11 window manager");
                return;
            }
        };
        let cursors = Cursors::load(self.config.cursor_theme.as_deref(), self.config.cursor_size);
        let (pixels, size, hotspot) = cursors.default_pixels();
        if let Err(err) = wm.set_cursor(&pixels, size.into(), hotspot.into()) {
            tracing::warn!(%err, "failed to set the X11 cursor");
        }
        tracing::info!(display = ?self.xwayland.display, "Xwayland is ready");
        self.xwm = Some(wm);
    }

    /// The server is gone, and with it all X11 windows. Their programs die with it, or have to
    /// reconnect on their own.
    fn xwayland_gone(&mut self) {
        self.xwm = None;
        self.xwayland.display = None;
        if let Some(source) = self.xwayland.source.take() {
            self.loop_handle.remove(source);
        }
        let windows: Vec<Window> = self
            .monitors
            .iter()
            .flat_map(|monitor| &monitor.workspaces)
            .flat_map(|workspace| workspace.windows())
            .filter(|window| window.x11_surface().is_some())
            .collect();
        for window in &windows {
            self.remove_window(window);
        }
        for window in std::mem::take(&mut self.overrides) {
            self.space.unmap_elem(&window);
        }
        self.start_xwayland();
    }

    /// X and Wayland are separate connections, so the first buffer of a window may come in before
    /// the X server says which window that surface belongs to. No commit tells about that.
    pub(crate) fn settle_x11_windows(&mut self) {
        let surfaces: Vec<WlSurface> = self
            .awaiting_buffer
            .iter()
            .filter(|window| window.x11_surface().is_some())
            .filter_map(|window| window.wl_surface().map(Cow::into_owned))
            .filter(has_buffer)
            .collect();
        for surface in surfaces {
            self.toplevel_commit(&surface);
        }
    }

    /// The X server may hand over what it knows only for the window with the keyboard.
    fn x11_has_keyboard(&self, xwm: XwmId) -> bool {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        matches!(focus, Some(KeyboardFocus::X11(surface)) if surface.xwm_id() == Some(xwm))
    }

    fn x11_full_request(&mut self, surface: &X11Surface, kind: Option<FullKind>) {
        match self.find_x11(surface) {
            Some(window) => self.set_full(&window, kind),
            // Not mapped yet: `place_x11_window` picks it up from here.
            None => {
                let result = surface
                    .set_fullscreen(kind == Some(FullKind::Fullscreen))
                    .and_then(|()| surface.set_maximized(kind == Some(FullKind::Maximized)));
                if let Err(err) = result {
                    tracing::debug!(%err, "failed to update an X11 window");
                }
            }
        }
    }
}

impl XwmHandler for Wirbel {
    /// Smithay only gets here with events of a connection that is alive, and `xwm` lives as long.
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("X11 event without a window manager")
    }

    fn new_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(err) = surface.set_mapped(true) {
            tracing::warn!(?err, "failed to map an X11 window");
            return;
        }
        let window = Window::new_x11_window(surface);
        if self.place_x11_window(window.clone()) {
            self.awaiting_buffer.push(window);
            self.retile();
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let location = surface.last_configure().loc;
        let window = Window::new_x11_window(surface);
        self.space.map_element(window.clone(), location, false);
        self.overrides.push(window);
        self.needs_redraw = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.find_x11(&surface) {
            self.overrides.retain(|other| *other != window);
            self.space.unmap_elem(&window);
            self.remove_window(&window);
        }
        if !surface.is_override_redirect()
            && let Err(err) = surface.set_mapped(false)
        {
            tracing::debug!(?err, "failed to unmap an X11 window");
        }
        self.update_pointer_focus();
        self.needs_redraw = true;
    }

    fn destroyed_window(&mut self, xwm: XwmId, surface: X11Surface) {
        self.unmapped_window(xwm, surface);
    }

    /// Floating windows go where they want, the others get told that they stay.
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let Some(window) = self.find_x11(&surface) else {
            let mut wanted = surface.last_configure();
            wanted.loc.x = x.unwrap_or(wanted.loc.x);
            wanted.loc.y = y.unwrap_or(wanted.loc.y);
            wanted.size.w = w.map_or(wanted.size.w, |w| w as i32);
            wanted.size.h = h.map_or(wanted.size.h, |h| h as i32);
            if let Err(err) = surface.configure(wanted) {
                tracing::debug!(?err, "failed to configure an X11 window");
            }
            return;
        };
        let full = self.full_kind(&window).is_some();
        let entry = self
            .workspace_of_mut(&window)
            .and_then(|workspace| workspace.floating_entry_mut(&window));
        match entry {
            Some(entry) if !full => {
                let Rect {
                    x: old_x,
                    y: old_y,
                    w: old_w,
                    h: old_h,
                } = entry.content();
                entry.rect = Rect::new(
                    x.unwrap_or(old_x),
                    y.unwrap_or(old_y),
                    w.map_or(old_w, |w| w as i32),
                    h.map_or(old_h, |h| h as i32),
                );
                entry.anchor = (None, None);
                self.retile();
            }
            _ => {
                if let Err(err) = surface.configure(None) {
                    tracing::debug!(?err, "failed to answer an X11 window");
                }
            }
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if surface.is_override_redirect()
            && let Some(window) = self.find_x11(&surface)
        {
            self.space.map_element(window, geometry.loc, false);
            self.needs_redraw = true;
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_full_request(&surface, Some(FullKind::Maximized));
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_full_request(&surface, None);
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_full_request(&surface, Some(FullKind::Fullscreen));
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_full_request(&surface, None);
    }

    fn resize_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32, edge: ResizeEdge) {
        self.start_x11_drag(&surface, DragKind::Resize(edge.into()));
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        self.start_x11_drag(&surface, DragKind::Move);
    }

    /// `_NET_ACTIVE_WINDOW`. Like xdg-activation: only out of what the user is working with.
    fn active_window_request(
        &mut self,
        xwm: XwmId,
        surface: X11Surface,
        _timestamp: u32,
        _currently_active: Option<X11Surface>,
    ) {
        if self.x11_has_keyboard(xwm)
            && let Some(window) = self.find_x11(&surface)
        {
            self.activate_window(&window);
        }
    }

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        self.x11_has_keyboard(xwm)
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let sent = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd).map_err(|_| ())
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime_type, fd).map_err(|_| ())
            }
        };
        if sent.is_err() {
            tracing::warn!(?selection, "failed to pass the selection on to X11");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ());
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    fn disconnected(&mut self, xwm: XwmId) {
        // News of an old server may arrive once the next one is up.
        if self.xwm.as_ref().is_some_and(|current| current.id() == xwm) {
            tracing::warn!("Xwayland went away");
            self.xwayland_gone();
        }
    }
}
