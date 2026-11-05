//! Clipboard, primary selection and drag and drop between clients.

use std::os::fd::OwnedFd;

use smithay::{
    input::{
        Seat,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        pointer::Focus,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Serial},
    wayland::selection::{
        SelectionHandler, SelectionSource, SelectionTarget,
        data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler},
        primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
        wlr_data_control::{DataControlHandler, DataControlState},
    },
};

use crate::Wirbel;

impl SelectionHandler for Wirbel {
    type SelectionUserData = ();

    /// X11 clients get to see what Wayland clients copied.
    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.new_selection(target, source.map(|source| source.mime_types()))
        {
            tracing::warn!(?err, ?target, "failed to offer the selection to X11");
        }
    }

    /// A Wayland client pastes what an X11 client copied.
    fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(target, mime_type, fd)
        {
            tracing::warn!(?err, ?target, "failed to fetch the selection from X11");
        }
    }
}

impl DataDeviceHandler for Wirbel {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PrimarySelectionHandler for Wirbel {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

// Clipboard managers. Reads and writes the same selection as the two above.
impl DataControlHandler for Wirbel {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl WaylandDndGrabHandler for Wirbel {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        _kind: GrabType,
    ) {
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        let Some(start) = pointer.grab_start_data() else {
            return;
        };
        self.dnd_icon = icon;
        let grab = DnDGrab::new_pointer(&self.display_handle, start, source, seat);
        pointer.set_grab(self, grab, serial, Focus::Keep);
    }
}

impl DndGrabHandler for Wirbel {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_icon = None;
    }
}
