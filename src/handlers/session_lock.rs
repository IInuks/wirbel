//! ext-session-lock. From the moment a client asks for the lock, the screen shows only lock
//! surfaces and input reaches only them; the client hears that the session is locked once every
//! output has presented such a frame. If the locker dies, the session stays locked until another
//! locker takes over. `wirbel pos` and `wirbel move` keep working: the pointer moves, but
//! `Wirbel::under` finds nothing except the lock surfaces.

use smithay::{
    backend::input::InputTime,
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{
            Resource,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        },
    },
    utils::SERIAL_COUNTER,
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};

use crate::{Wirbel, workspace::Monitor};

#[derive(Debug, Default)]
enum Phase {
    #[default]
    Unlocked,
    /// Waiting for these outputs to present a locked frame.
    Locking {
        locker: SessionLocker,
        waiting: Vec<Output>,
    },
    Locked(ExtSessionLockV1),
}

#[derive(Debug, Default)]
pub struct SessionLock {
    phase: Phase,
    surfaces: Vec<(Output, LockSurface)>,
}

impl SessionLock {
    /// Whether the session counts as locked, which starts before the client hears about it.
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Unlocked)
    }

    /// Whether the client was told that the session is locked.
    pub fn is_confirmed(&self) -> bool {
        matches!(self.phase, Phase::Locked(_))
    }

    pub fn surface_on(&self, output: &Output) -> Option<WlSurface> {
        let (_, surface) = self.surfaces.iter().find(|(other, _)| other == output)?;
        Some(surface.wl_surface().clone())
    }

    pub fn output_of(&self, surface: &WlSurface) -> Option<Output> {
        let (output, _) = self
            .surfaces
            .iter()
            .find(|(_, other)| other.wl_surface() == surface)?;
        Some(output.clone())
    }

    /// The lock surface of `monitor`, or else any.
    pub fn surface_for_input(&self, monitor: Option<&Monitor>) -> Option<WlSurface> {
        monitor
            .and_then(|monitor| self.surface_on(&monitor.output))
            .or_else(|| {
                let (_, surface) = self.surfaces.first()?;
                Some(surface.wl_surface().clone())
            })
    }

    pub fn surfaces(&self) -> impl Iterator<Item = (WlSurface, Output)> + '_ {
        let surfaces = self.surfaces.iter();
        surfaces.map(|(output, surface)| (surface.wl_surface().clone(), output.clone()))
    }
}

impl SessionLockHandler for Wirbel {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        match &self.lock.phase {
            Phase::Unlocked => {}
            Phase::Locked(lock) if !lock.is_alive() => {
                // The next locker takes over from one that died. The screen is locked already.
                let lock = confirmation.ext_session_lock().clone();
                confirmation.lock();
                self.lock.phase = Phase::Locked(lock);
                return;
            }
            // Dropping the confirmation refuses the lock.
            _ => return,
        }

        let connected = self.monitors.iter().filter(|monitor| monitor.connected);
        let waiting: Vec<Output> = connected.map(|monitor| monitor.output.clone()).collect();
        self.lock.phase = if waiting.is_empty() {
            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            Phase::Locked(lock)
        } else {
            Phase::Locking {
                locker: confirmation,
                waiting,
            }
        };
        self.release_input();
        self.needs_redraw = true;
    }

    fn unlock(&mut self) {
        self.lock.phase = Phase::Unlocked;
        self.lock.surfaces.clear();
        self.update_keyboard_focus();
        self.refresh_pointer_focus();
        self.needs_redraw = true;
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        self.configure_lock_surface(&surface, &output);
        output.enter(surface.wl_surface());
        // A later locker on the same output replaces one that died.
        self.lock.surfaces.retain(|(other, _)| *other != output);
        self.lock.surfaces.push((output, surface));
        self.update_surface_scales();
        self.update_keyboard_focus();
        self.needs_redraw = true;
    }
}

impl Wirbel {
    /// Takes the seat away from everybody the moment the session locks: popups, drags and even a
    /// button held down over a client.
    fn release_input(&mut self) {
        self.end_session(false);
        self.end_popup_grab();
        self.finish_drag();
        self.dnd_icon = None;
        self.cursor_status = CursorImageStatus::default_named();
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.unset_grab(self);
        }
        self.update_keyboard_focus();
        self.refresh_pointer_focus();
    }

    fn configure_lock_surface(&self, surface: &LockSurface, output: &Output) {
        let Some(area) = self.space.output_geometry(output) else {
            return;
        };
        surface.with_pending_state(|state| {
            state.size = Some((area.size.w as u32, area.size.h as u32).into());
        });
    }

    pub(crate) fn lock_configure_output(&self, output: &Output) {
        for (_, surface) in self
            .lock
            .surfaces
            .iter()
            .filter(|(other, _)| other == output)
        {
            self.configure_lock_surface(surface, output);
            surface.send_configure();
        }
    }

    /// Notes that `output` presented a frame. Only one drawn after the lock began counts, which
    /// holds for all of them: nothing else is drawn from that moment on.
    pub fn lock_frame_presented(&mut self, output: &Output) {
        if let Phase::Locking { waiting, .. } = &mut self.lock.phase {
            waiting.retain(|other| other != output);
        }
        self.confirm_lock();
    }

    pub(crate) fn lock_remove_output(&mut self, output: &Output) {
        self.lock.surfaces.retain(|(other, _)| other != output);
        self.lock_frame_presented(output);
        self.update_keyboard_focus();
    }

    fn confirm_lock(&mut self) {
        if !matches!(&self.lock.phase, Phase::Locking { waiting, .. } if waiting.is_empty()) {
            return;
        }
        let phase = std::mem::take(&mut self.lock.phase);
        if let Phase::Locking { locker, .. } = phase {
            let lock = locker.ext_session_lock().clone();
            locker.lock();
            self.lock.phase = Phase::Locked(lock);
        }
    }

    /// Lets go of lock surfaces that are gone. The session stays locked regardless.
    pub(crate) fn lock_refresh(&mut self) {
        let before = self.lock.surfaces.len();
        self.lock.surfaces.retain(|(_, surface)| surface.alive());
        if self.lock.surfaces.len() != before {
            self.update_keyboard_focus();
            self.refresh_pointer_focus();
            self.needs_redraw = true;
        }
    }
}
