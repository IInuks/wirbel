//! Windows asking for the focus. A window gets it when it holds a token from an input event of the
//! client that had the keyboard then: the user clicked something and it opens another window. A
//! window that only wants attention, like kitty on a bell, has no such token and is left alone.

use std::time::Duration;

use smithay::{
    input::Seat,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

use crate::Wirbel;

/// What a token is good for. Whoever waits this long is no longer what the user is doing.
const TOKEN_LIFETIME: Duration = Duration::from_secs(10);

impl XdgActivationHandler for Wirbel {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    /// Every client gets a token, it only is worth nothing without a fresh input event.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = data.serial else {
            return false;
        };
        let keyboard = self.seat.get_keyboard();
        let entered = keyboard.and_then(|keyboard| keyboard.last_enter());
        Seat::from_resource(&seat).as_ref() == Some(&self.seat)
            && entered.is_some_and(|entered| serial.is_no_older_than(&entered))
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if data.timestamp.elapsed() > TOKEN_LIFETIME {
            return;
        }
        if let Some(window) = self.find_window(&surface) {
            self.activate_window(&window);
        }
    }
}
