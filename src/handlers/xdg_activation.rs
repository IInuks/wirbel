//! Windows asking for the focus. A window gets it when it holds a token from an input event of the
//! client that had the keyboard then: the user clicked something and it opens another window. A
//! window that only wants attention, like kitty on a bell, has no such token: it does not get the
//! focus, the bar shows the workspace as urgent.

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

/// A mark on a token that came from an input event.
struct Fresh;

impl XdgActivationHandler for Wirbel {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    /// Every client gets a token. Without a fresh input event it only is good for attention.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let entered = self.seat.get_keyboard().and_then(|k| k.last_enter());
        let fresh = data.serial.as_ref().is_some_and(|(serial, seat)| {
            Seat::from_resource(seat).as_ref() == Some(&self.seat)
                && entered.is_some_and(|entered| serial.is_no_older_than(&entered))
        });
        if fresh {
            data.user_data.insert_if_missing(|| Fresh);
        }
        // Tokens of no use pile up otherwise: a bell makes one each time.
        self.activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() <= TOKEN_LIFETIME);
        true
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let Some(window) = self.find_window(&surface) else {
            return;
        };
        if data.user_data.get::<Fresh>().is_some() && data.timestamp.elapsed() <= TOKEN_LIFETIME {
            self.activate_window(&window);
        } else {
            self.mark_urgent(&window);
        }
    }
}
