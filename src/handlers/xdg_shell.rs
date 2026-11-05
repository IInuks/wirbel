use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::{PopupKind, Window, find_popup_root_surface, get_popup_toplevel_coords},
    reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface},
    utils::Serial,
    wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
};

use crate::Wirbel;

fn is_window_of(window: &Window, surface: &WlSurface) -> bool {
    window.toplevel().is_some_and(|toplevel| toplevel.wl_surface() == surface)
}

impl XdgShellHandler for Wirbel {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.unconfigured.push(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let surface = surface.wl_surface();
        self.unconfigured.retain(|window| !is_window_of(window, surface));
        let window = self.space.elements().find(|window| is_window_of(window, surface)).cloned();
        if let Some(window) = window {
            self.remove_window(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(%err, "failed to track popup");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
}

impl Wirbel {
    /// Drives a toplevel through its lifecycle. The first commit tiles it, the first buffer
    /// focuses it, and a commit that drops the buffer unmaps it until it starts over.
    pub(super) fn toplevel_commit(&mut self, surface: &WlSurface) {
        if let Some(index) = self.unconfigured.iter().position(|window| is_window_of(window, surface)) {
            let window = self.unconfigured.remove(index);
            let Some(toplevel) = window.toplevel().cloned() else { return };

            self.layout.insert(self.focused.as_ref(), window.clone());
            self.awaiting_buffer.push(window.clone());
            self.space.map_element(window, (0, 0), false);
            self.retile();
            toplevel.send_configure();
            return;
        }

        let Some(window) = self.space.elements().find(|window| is_window_of(window, surface)).cloned()
        else {
            return;
        };
        let has_buffer = with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);
        let index = self.awaiting_buffer.iter().position(|other| *other == window);
        match (index, has_buffer) {
            (Some(index), true) => {
                self.awaiting_buffer.remove(index);
                self.set_focus(Some(window));
            }
            (None, false) => {
                self.remove_window(&window);
                self.unconfigured.push(window);
            }
            _ => {}
        }
    }

    pub(super) fn popup_commit(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(err) = popup.send_configure()
        {
            tracing::warn!(?err, "failed to configure popup");
        }
    }

    /// Keeps a popup inside the output.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        let Some(window) = self.space.elements().find(|window| is_window_of(window, &root)) else {
            return;
        };
        let output_geometry = self.space.outputs().next().and_then(|output| self.space.output_geometry(output));
        let (Some(mut target), Some(window_location)) = (output_geometry, self.space.element_location(window))
        else {
            return;
        };

        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= window_location;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
