//! Clipboard, primary selection and drag and drop between clients.

use smithay::{
    input::{
        Seat,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        pointer::Focus,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Serial},
    wayland::selection::{
        SelectionHandler,
        data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler},
        primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
    },
};

use crate::Wirbel;

impl SelectionHandler for Wirbel {
    type SelectionUserData = ();
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
