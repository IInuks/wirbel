//! Who holds the keyboard. X11 windows are more than their `wl_surface`: they have to be told so on
//! the X side, and may not have a surface yet when the focus reaches them.

use std::borrow::Cow;

use smithay::{
    backend::input::{InputTime, KeyState},
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::Wirbel;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Surface(WlSurface),
    X11(Box<X11Surface>),
}

impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

impl From<PopupKind> for KeyboardFocus {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.wl_surface().clone())
    }
}

/// Smithay's popup grabs ask for this. What they convert comes out of `From<PopupKind>` and
/// `From<WlSurface>`, so it always is a Wayland surface: popups belong to Wayland clients.
impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Surface(surface) => surface,
            KeyboardFocus::X11(surface) => surface
                .wl_surface()
                .expect("popup grab on a bare X11 window"),
        }
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(surface) => surface.alive(),
            Self::X11(surface) => surface.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(surface) => Some(Cow::Borrowed(surface)),
            Self::X11(surface) => surface.wl_surface().map(Cow::Owned),
        }
    }
}

macro_rules! forward {
    ($self:ident, $target:ident => $call:expr) => {
        match $self {
            Self::Surface($target) => $call,
            Self::X11($target) => {
                let $target = &**$target;
                $call
            }
        }
    };
}

impl KeyboardTarget<Wirbel> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<Wirbel>,
        data: &mut Wirbel,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        forward!(self, target => KeyboardTarget::enter(target, seat, data, keys, serial));
    }

    fn leave(&self, seat: &Seat<Wirbel>, data: &mut Wirbel, serial: Serial) {
        forward!(self, target => KeyboardTarget::leave(target, seat, data, serial));
    }

    fn key(
        &self,
        seat: &Seat<Wirbel>,
        data: &mut Wirbel,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: InputTime,
    ) {
        forward!(self, target => KeyboardTarget::key(target, seat, data, key, state, serial, time));
    }

    fn modifiers(
        &self,
        seat: &Seat<Wirbel>,
        data: &mut Wirbel,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        forward!(self, target => KeyboardTarget::modifiers(target, seat, data, modifiers, serial));
    }
}
