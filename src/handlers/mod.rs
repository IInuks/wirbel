//! Wayland protocol handlers.

mod compositor;
mod xdg_shell;

use smithay::{
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{output::OutputHandler, pointer_constraints::PointerConstraintsHandler},
};

use crate::Wirbel;

impl SeatHandler for Wirbel {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.needs_redraw = true;
    }
}

impl OutputHandler for Wirbel {}

// `WlSurface` is only a valid pointer target with this. The protocol itself is not offered yet.
impl PointerConstraintsHandler for Wirbel {}

smithay::delegate_dispatch2!(Wirbel);
