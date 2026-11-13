//! Input filter: something built into wirbel sees keys, pointer motion, buttons and the wheel
//! before binds and clients do, and may keep them. While it says it holds the input, no client
//! has the keyboard or the pointer and the cursor is not drawn. The core does not know what the
//! filter does with the input.
//!
//! A filter that does not hold the input may route it instead (`InputFilter::routes_input`):
//! then it, not the layout, says which surface is under the pointer and who has the keyboard, and
//! the core leaves out what only makes sense on the 2D layout (focus on click or hover, mouse
//! binds on windows). Events still reach clients through the core's pointer and keyboard. Layer
//! surfaces that a render override keeps on top (`Wirbel::override_layers`) come before the
//! filter's answer: the pointer over them is theirs, and one that claims the keyboard, or was
//! clicked, has it.
//!
//! A filter that holds the input may still aim the pointer at one surface
//! (`InputFilter::held_pointer`), for example through a pointer of its own: that surface gets
//! pointer focus and motion as usual, nobody gets the keyboard, and the filter hands buttons and
//! the wheel to the pointer itself (they come to it first anyway).
//!
//! A locked session has no filter: the lock screen gets everything, as without one. The
//! emergency chord and the VT keys come first anyway.

use std::rc::Rc;

use smithay::{
    input::keyboard::KeysymHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

use crate::{
    Wirbel,
    config::{Action, Modifiers},
    focus::KeyboardFocus,
    input::Scroll,
};

/// What becomes of a key.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyVerdict {
    /// Goes on to the binds and then to the client with the keyboard, as without a filter.
    Pass,
    /// Nobody else sees it, nor its release.
    Consume,
    /// Nobody else sees it; the core runs this action, as for a bind.
    Run(Action),
}

/// What becomes of a pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Consume,
}

/// Gets the input first, see the module documentation. Every method gets the compositor, but
/// `key` runs while the keyboard is busy with that key: it must not touch the keyboard, and
/// anything bigger belongs in the action it returns.
pub trait InputFilter {
    /// Whether clients are cut off from the keyboard and the pointer right now. The owner calls
    /// `Wirbel::refresh_input_focus` when the answer changes.
    fn holds_input(&self) -> bool;

    /// A key went down or up. A pressed key the filter keeps is also kept when it goes up,
    /// whatever the filter says about the release. `Run` only counts for a press. Modifier keys
    /// reach the client with the keyboard (if any) even when kept, or it would miss them going up.
    fn key(
        &self,
        wirbel: &mut Wirbel,
        modifiers: Modifiers,
        handle: &KeysymHandle<'_>,
        pressed: bool,
    ) -> KeyVerdict;

    /// A mouse moved by that much, which says nothing about where the pointer is.
    fn pointer_delta(
        &self,
        wirbel: &mut Wirbel,
        delta: Point<f64, Logical>,
        delta_unaccel: Point<f64, Logical>,
    ) -> Verdict;

    /// The pointer is asked to go to `location`, for example by an absolute device.
    fn pointer_motion(&self, wirbel: &mut Wirbel, location: Point<f64, Logical>) -> Verdict;

    fn pointer_button(&self, wirbel: &mut Wirbel, button: u32, pressed: bool) -> Verdict;

    fn pointer_axis(&self, wirbel: &mut Wirbel, scroll: &Scroll) -> Verdict;

    /// Whether the filter says where the input goes while it does not hold it (see the module
    /// documentation). The owner calls `Wirbel::refresh_input_focus` when the answer changes.
    fn routes_input(&self) -> bool {
        false
    }

    /// While routing: the surface under the pointer at `location` and where that surface would
    /// begin, so that `location - origin` is the point on the surface.
    fn route_pointer(
        &self,
        _wirbel: &Wirbel,
        _location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        None
    }

    /// While routing: who has the keyboard.
    fn route_keyboard(&self, _wirbel: &Wirbel) -> Option<KeyboardFocus> {
        None
    }

    /// While holding the input: the surface that still has the pointer at `location`, if any,
    /// and where it would begin (as `route_pointer`). The owner calls
    /// `Wirbel::refresh_pointer_focus` when the answer changes.
    fn held_pointer(
        &self,
        _wirbel: &Wirbel,
        _location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        None
    }
}

/// A filter as kept in `Wirbel::input_filter`.
pub struct InputFilterSlot {
    filter: Rc<dyn InputFilter>,
    /// Where the pointer was when the filter came, and goes back to when it leaves.
    pointer_before: Point<f64, Logical>,
}

impl Wirbel {
    /// The filter that gets the input now: none while the session is locked.
    pub fn input_filter(&self) -> Option<Rc<dyn InputFilter>> {
        if self.lock.is_active() {
            return None;
        }
        Some(self.input_filter.as_ref()?.filter.clone())
    }

    /// Whether a filter keeps clients from the keyboard and the pointer right now.
    pub fn input_held(&self) -> bool {
        self.input_filter()
            .is_some_and(|filter| filter.holds_input())
    }

    /// The filter, if it routes the input right now (and does not hold it).
    pub fn input_router(&self) -> Option<Rc<dyn InputFilter>> {
        self.input_filter()
            .filter(|filter| !filter.holds_input() && filter.routes_input())
    }

    /// Sets the filter, or takes it away with `None`. Setting one ends what clients were in the
    /// middle of (popups, drags, a held button) and remembers where the pointer is. Taking it away
    /// puts the pointer back there, so clients get the keyboard and the pointer as before.
    pub fn set_input_filter(&mut self, filter: Option<Rc<dyn InputFilter>>) {
        match filter {
            Some(filter) => {
                let pointer_before = match self.input_filter.take() {
                    Some(slot) => slot.pointer_before,
                    None => self.pointer_location(),
                };
                self.input_filter = Some(InputFilterSlot {
                    filter,
                    pointer_before,
                });
                self.release_input();
            }
            None => {
                let Some(slot) = self.input_filter.take() else {
                    return;
                };
                self.update_keyboard_focus();
                self.warp_pointer(slot.pointer_before);
            }
        }
        self.needs_redraw = true;
    }

    /// Brings keyboard and pointer focus in line after `InputFilter::holds_input` changed.
    pub fn refresh_input_focus(&mut self) {
        self.update_keyboard_focus();
        self.refresh_pointer_focus();
        self.needs_redraw = true;
    }

    /// The surface the pointer is over at `location` as far as clients are concerned, and where
    /// that surface begins: while a filter holds the input only what it aims the pointer at, the
    /// filter's answer while it routes the input.
    pub fn pointer_target(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if let Some(filter) = self.input_filter() {
            if filter.holds_input() {
                return filter.held_pointer(self, location);
            }
            if filter.routes_input() {
                // Layer surfaces drawn over a render override are on top of the filter's picture.
                if let Some((_, surface, origin)) = self.override_layer_under(location) {
                    return Some((surface, origin));
                }
                return filter.route_pointer(self, location);
            }
        }
        self.surface_under(location)
    }
}
