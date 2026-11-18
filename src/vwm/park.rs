//! Parked windows: windows that were already open on the desktop and are taken out of sight.
//!
//! A program cannot be moved into a shadow once it runs, so a parked window stays a window of
//! the visible compositor: off every workspace, out of the taskbar, still drawing. It can be
//! photographed, closed and put back. It has no keyboard or pointer of its own: input belongs
//! to the one seat of the desktop, which it can borrow for a moment if the configuration allows
//! it (see below).
//!
//! Requests of the visible compositor:
//!
//! - `vwm adopt WINDOW|focused [NAME]`: parks a window, by its id from `windows`. Answers
//!   `ok NAME`.
//! - `vwm release NAME`: puts it on the active workspace again, as if it had just opened.
//! - `vwm shot` TAB NAME TAB PATH TAB X,Y,W,H|-: a picture of it, see `super::shot`.
//! - `vwm close NAME` asks it to close, `vwm list` shows it with the kind `parked`.
//! - `vwm input NAME REQUEST`: an `inject` request (without the word `inject`) for the parked
//!   window, if the configuration says `vwm_adopt_input = borrow`.
//!
//! Input reaches a parked window by borrowing: for the request the window gets the keyboard
//! and the pointer of the seat, the keys go straight to it past binds and filters, pointer
//! events land on its surface at the place asked for. It keeps them while requests follow each
//! other ([`BORROW_TIME`] after the last one) so that a press and its release reach the same
//! window, and gives them back at once when the user touches a key, the mouse or the touchpad.
//! Borrowing is refused while the user holds a key or a button, while the pointer is grabbed and
//! while the session is locked. What the window still holds when it gives them back is let go.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    desktop::Window,
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::calloop::{
        RegistrationToken,
        timer::{TimeoutAction, Timer},
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER},
};

use super::{shot::Shot, valid_name};
use crate::{
    Wirbel, focus::KeyboardFocus, handlers::window_names, inject::Inject, layout::Rect,
    plugin_api::window_id, workspace::Placement,
};

/// How long a parked window keeps the keyboard and the pointer after the last request for it.
pub const BORROW_TIME: Duration = Duration::from_millis(300);
/// What one wheel notch scrolls, as `inject wheel` counts it.
const NOTCH: f64 = 15.0;

pub struct Parked {
    pub name: String,
    pub window: Window,
    pub since: Instant,
    /// Where the pointer of the requests is on the window, in its own pixels.
    pub pointer: (f64, f64),
}

/// A parked window that has the keyboard and the pointer for a moment.
pub struct Borrow {
    window: Window,
    /// Where the pointer of the user was, to put it back there.
    pointer_before: Point<f64, Logical>,
    /// Keys and buttons pressed in the window and not yet let go.
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    timer: Option<RegistrationToken>,
}

impl Wirbel {
    /// Whether `window` is parked.
    pub fn is_parked(&self, window: &Window) -> bool {
        self.vwm
            .parked
            .iter()
            .any(|parked| parked.window == *window)
    }

    /// The parked windows that still exist.
    pub(crate) fn parked_windows(&self) -> Vec<Window> {
        let alive = self
            .vwm
            .parked
            .iter()
            .filter(|parked| parked.window.alive());
        alive.map(|parked| parked.window.clone()).collect()
    }

    fn free_parked_name(&self, base: &str) -> String {
        let base: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(24)
            .collect();
        let base = if base.is_empty() { "window" } else { &base };
        let taken = |name: &str| {
            self.vwm.find(name).is_some()
                || self.vwm.parked.iter().any(|parked| parked.name == name)
        };
        (1..)
            .map(|n| format!("{base}{n}"))
            .find(|name| !taken(name))
            .unwrap_or_default()
    }

    /// Takes a window of the layout out of sight. Returns its name.
    pub fn vwm_adopt(&mut self, window: &Window, name: Option<&str>) -> Result<String, String> {
        if self.is_parked(window) {
            return Err("that window is parked already".into());
        }
        if self.locate(window).is_none() {
            return Err("only a window on a workspace can be parked".into());
        }
        let name = match name {
            Some(name) if !valid_name(name) => {
                return Err(format!(
                    "`{name}` cannot be a name: letters, digits, - and _"
                ));
            }
            Some(name)
                if self.vwm.find(name).is_some()
                    || self.vwm.parked.iter().any(|p| p.name == name) =>
            {
                return Err(format!("there is an invisible window `{name}` already"));
            }
            Some(name) => name.to_string(),
            None => self.free_parked_name(&window_names(window).1),
        };
        // Out of the layout as if it had closed, but it stays known as a held window.
        let next = self.workspace_of_mut(window).and_then(|workspace| {
            workspace.take(window);
            workspace.focus_candidate()
        });
        self.space.unmap_elem(window);
        self.urgent.retain(|other| other != window);
        if self.focused.as_ref() == Some(window) {
            self.focus_window(next, true);
            self.update_keyboard_focus();
        }
        if !self.held_windows.contains(window) {
            self.held_windows.push(window.clone());
        }
        self.vwm.parked.push(Parked {
            name: name.clone(),
            window: window.clone(),
            since: Instant::now(),
            pointer: {
                let size = window.geometry().size;
                (size.w as f64 / 2.0, size.h as f64 / 2.0)
            },
        });
        self.retile();
        self.needs_redraw = true;
        Ok(name)
    }

    /// Puts a parked window on the active workspace again.
    pub fn vwm_release(&mut self, name: &str) -> Result<(), String> {
        self.vwm_end_borrow();
        let index = self
            .vwm
            .parked
            .iter()
            .position(|parked| parked.name == name);
        let index = index.ok_or_else(|| format!("no parked window `{name}`"))?;
        let window = self.vwm.parked.remove(index).window;
        self.held_windows.retain(|other| *other != window);
        if !window.alive() {
            return Ok(());
        }
        let monitor = self
            .monitors
            .get_mut(self.active_monitor)
            .ok_or("no output")?;
        let placement = match crate::state::wants_to_float_window(&window) {
            true => Placement::Floating(Rect::new(0, 0, 0, 0)),
            false => Placement::Tiled,
        };
        monitor.workspace_mut().put(window.clone(), placement);
        self.retile();
        if window.wl_surface().is_some() {
            self.floating_commit(&window);
        }
        // Whoever brings a window back wants to use it.
        self.focus_window(Some(window), true);
        self.update_keyboard_focus();
        self.needs_redraw = true;
        Ok(())
    }

    /// Forgets parked windows that are gone.
    pub(crate) fn reap_parked(&mut self) {
        if self
            .vwm
            .borrow
            .as_ref()
            .is_some_and(|borrow| !borrow.window.alive())
        {
            self.vwm_end_borrow();
        }
        self.vwm.parked.retain(|parked| parked.window.alive());
    }

    /// The lines of `vwm list` for the parked windows.
    pub(crate) fn parked_list(&self) -> String {
        let mut answer = String::new();
        for parked in self
            .vwm
            .parked
            .iter()
            .filter(|parked| parked.window.alive())
        {
            let size = parked.window.geometry().size;
            let (title, _) = window_names(&parked.window);
            answer += &format!(
                "entry\t{}\tparked\tready\t{}\t-\t{}x{}\t{}\t{}\n",
                parked.name,
                window_id(&parked.window),
                size.w,
                size.h,
                parked.since.elapsed().as_secs(),
                title.replace(['\t', '\n'], " "),
            );
        }
        answer
    }

    /// Why the window cannot borrow the keyboard and the pointer now, if it cannot.
    fn borrow_refused(&self) -> Option<String> {
        if !self.config.vwm_adopt_input {
            return Some(
                "typing and clicking into parked windows is off; `vwm_adopt_input = borrow` in \
                 the configuration allows it"
                    .into(),
            );
        }
        if self.lock.is_active() {
            return Some("the session is locked".into());
        }
        if self.vwm.borrow.is_some() {
            return None;
        }
        let keyboard = self.seat.get_keyboard();
        if keyboard.is_some_and(|keyboard| !keyboard.pressed_keys().is_empty()) {
            return Some("a key is held down on the desktop, try again when it is let go".into());
        }
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return Some(
                "a mouse button is held or the pointer is grabbed, try again in a moment".into(),
            );
        }
        None
    }

    /// Gives the keyboard and the pointer to `window` until [`BORROW_TIME`] after now.
    fn borrow_for(&mut self, window: &Window) {
        if self
            .vwm
            .borrow
            .as_ref()
            .is_some_and(|borrow| borrow.window != *window)
        {
            self.vwm_end_borrow();
        }
        if self.vwm.borrow.is_none() {
            let pointer_before = self
                .seat
                .get_pointer()
                .map(|pointer| pointer.current_location())
                .unwrap_or_default();
            self.vwm.borrow = Some(Borrow {
                window: window.clone(),
                pointer_before,
                keys: BTreeSet::new(),
                buttons: BTreeSet::new(),
                timer: None,
            });
        }
        let timer = self
            .loop_handle
            .insert_source(Timer::from_duration(BORROW_TIME), |_, _, state| {
                if let Some(borrow) = state.vwm.borrow.as_mut() {
                    borrow.timer = None;
                }
                state.vwm_end_borrow();
                TimeoutAction::Drop
            })
            .ok();
        let old = self
            .vwm
            .borrow
            .as_mut()
            .and_then(|borrow| std::mem::replace(&mut borrow.timer, timer));
        if let Some(old) = old {
            self.loop_handle.remove(old);
        }
    }

    /// A key for the window that borrows the keyboard, past binds and filters. The keyboard
    /// focus is checked each time: something else may have moved it meanwhile.
    pub(crate) fn borrowed_key(&mut self, code: Keycode, state: KeyState) {
        let Some(borrow) = self.vwm.borrow.as_mut() else {
            return;
        };
        let target = KeyboardFocus::of_window(&borrow.window);
        match state {
            KeyState::Pressed => borrow.keys.insert(code.raw()),
            KeyState::Released => borrow.keys.remove(&code.raw()),
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if target.is_some() && keyboard.current_focus() != target {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.input::<(), _>(self, code, state, serial, InputTime::now(), |_, _, _| {
            FilterResult::Forward
        });
    }

    /// Puts the pointer of the seat on the borrowing window at `at`, in its own pixels.
    fn borrowed_pointer_to(&mut self, at: (f64, f64)) {
        let Some(borrow) = self.vwm.borrow.as_ref() else {
            return;
        };
        let window = borrow.window.clone();
        let Some(surface) = window.wl_surface().map(|surface| surface.into_owned()) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // The window's own pixels start at the corner of its geometry, which may lie inside
        // its surface (a shadow drawn by the program).
        let corner = window.geometry().loc.to_f64();
        let location: Point<f64, Logical> = at.into();
        let origin = Point::<f64, Logical>::from((0.0, 0.0)) - corner;
        let event = MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        };
        pointer.motion(self, Some((surface, origin)), &event);
        pointer.frame(self);
    }

    /// Gives the keyboard and the pointer back, letting go what the window still holds.
    pub fn vwm_end_borrow(&mut self) {
        let Some(borrow) = self.vwm.borrow.take() else {
            return;
        };
        if let Some(timer) = borrow.timer {
            self.loop_handle.remove(timer);
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let target = KeyboardFocus::of_window(&borrow.window);
            if borrow.window.alive() && keyboard.current_focus() == target {
                for code in &borrow.keys {
                    let serial = SERIAL_COUNTER.next_serial();
                    let code = Keycode::new(*code);
                    keyboard.input::<(), _>(
                        self,
                        code,
                        KeyState::Released,
                        serial,
                        InputTime::now(),
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
        }
        if let Some(pointer) = self.seat.get_pointer() {
            for button in &borrow.buttons {
                let event = ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: InputTime::now(),
                    button: *button,
                    state: ButtonState::Released,
                };
                pointer.button(self, &event);
            }
            if !borrow.buttons.is_empty() {
                pointer.frame(self);
            }
        }
        self.move_pointer(borrow.pointer_before, InputTime::now());
        // Whatever should have the keyboard gets it again.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let target = self.keyboard_target();
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
        self.update_pointer_constraint();
    }

    /// `vwm input NAME REQUEST`: one `inject` request for a parked window.
    fn vwm_input(&mut self, name: &str, request: &str) -> Result<(), String> {
        let parked = self
            .parked(name)
            .ok_or_else(|| format!("no parked window `{name}`"))?;
        let window = parked.window.clone();
        let at = parked.pointer;
        if let Some(why) = self.borrow_refused() {
            return Err(why);
        }
        let request = Inject::parse(&format!("inject {request}"))
            .unwrap_or_else(|| Err("not an input request".into()))?;
        self.borrow_for(&window);
        let set_pointer = |state: &mut Self, at: (f64, f64)| {
            if let Some(parked) = state.vwm.parked.iter_mut().find(|p| p.name == name) {
                parked.pointer = at;
            }
        };
        let result = match request {
            Inject::At { x, y } => {
                set_pointer(self, (x, y));
                self.borrowed_pointer_to((x, y));
                Ok(())
            }
            Inject::Motion { dx, dy } => {
                let to = (at.0 + dx, at.1 + dy);
                set_pointer(self, to);
                self.borrowed_pointer_to(to);
                Ok(())
            }
            Inject::Button { button, down } => {
                self.borrowed_pointer_to(at);
                if let Some(borrow) = self.vwm.borrow.as_mut() {
                    match down {
                        true => borrow.buttons.insert(button),
                        false => borrow.buttons.remove(&button),
                    };
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    let event = ButtonEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::now(),
                        button,
                        state: match down {
                            true => ButtonState::Pressed,
                            false => ButtonState::Released,
                        },
                    };
                    pointer.button(self, &event);
                    pointer.frame(self);
                }
                Ok(())
            }
            Inject::Wheel(steps) => {
                self.borrowed_pointer_to(at);
                if let Some(pointer) = self.seat.get_pointer() {
                    let frame = AxisFrame::new(InputTime::now())
                        .source(AxisSource::Wheel)
                        .value(Axis::Vertical, steps * NOTCH)
                        .v120(Axis::Vertical, (steps * 120.0).round() as i32);
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
                Ok(())
            }
            keys => self.inject_keys_or_pointer(keys),
        };
        self.needs_redraw = true;
        result
    }

    fn parked(&self, name: &str) -> Option<&Parked> {
        let found = self.vwm.parked.iter().find(|parked| parked.name == name);
        found.filter(|parked| parked.window.alive())
    }
}

use smithay::wayland::seat::WaylandFocus;

/// The `vwm` requests about parked windows. `None` if `rest` is none of them, or names no
/// parked window where a shadow may be meant.
pub(crate) fn request(
    state: &mut Wirbel,
    peer: u64,
    rest: &str,
) -> Option<Result<Option<String>, String>> {
    state.reap_parked();
    if let Some(input) = rest.strip_prefix("input ") {
        let (name, request) = input.split_once(' ')?;
        return Some(state.vwm_input(name, request).map(|()| Some("ok".into())));
    }
    if let Some(shot) = rest.strip_prefix("shot\t") {
        let (name, shot) = shot.split_once('\t')?;
        let Some(parked) = state.parked(name) else {
            return Some(Err(format!("no parked window `{name}`")));
        };
        let window = parked.window.clone();
        return Some(Shot::parse(peer, shot).map(|shot| {
            state.vwm.parked_shots.push((shot, window));
            state.needs_redraw = true;
            (state.redraw)();
            None
        }));
    }
    let mut words = rest.split_ascii_whitespace();
    let answer = match (words.next()?, words.next(), words.next(), words.next()) {
        ("adopt", Some(target), name, None) => {
            let window = match target {
                "focused" => state
                    .focused
                    .clone()
                    .ok_or("no window has the focus".to_string()),
                id => {
                    let id: u64 = match id.parse() {
                        Ok(id) => id,
                        Err(_) => return Some(Err("expected a window id or `focused`".into())),
                    };
                    let mut windows = state.listed_windows().into_iter();
                    windows
                        .find(|window| window_id(window) == id)
                        .ok_or(format!("no window {id}"))
                }
            };
            window
                .and_then(|window| state.vwm_adopt(&window, name))
                .map(|name| format!("ok {name}"))
        }
        ("release", Some(name), None, None) => state.vwm_release(name).map(|()| "ok".into()),
        ("close", Some(name), _, None) if state.parked(name).is_some() => {
            let window = state.parked(name).map(|parked| parked.window.clone());
            if let Some(window) = window {
                state.close_window(&window);
            }
            Ok("ok".into())
        }
        ("where", Some(name), None, None) if state.parked(name).is_some() => Err(format!(
            "`{name}` is a parked window: it takes shot, release and close, typing and clicking need a window started with `wirbel vwm open` (or `vwm_adopt_input = borrow`)"
        )),
        _ => return None,
    };
    Some(answer.map(Some))
}
