//! Input handling. Backends translate their events into the `on_*` methods, which is also where
//! tests inject input.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        InputTime, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym, KeysymHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
};

use crate::{Wirbel, config::Action, state::Target};

/// xkb codes are evdev codes shifted by this.
const XKB_OFFSET: u32 = 8;
const SWITCH_VT_1: u32 = Keysym::XF86_Switch_VT_1.raw();
const SWITCH_VT_12: u32 = Keysym::XF86_Switch_VT_12.raw();
const F1: u32 = Keysym::F1.raw();
const F12: u32 = Keysym::F12.raw();

/// The VT a key asks for: `XF86Switch_VT_n`, or Ctrl+Alt+Fn for keymaps without those symbols.
fn vt_of_key(modifiers: &ModifiersState, handle: &KeysymHandle<'_>) -> Option<i32> {
    let number = |sym: u32, first: u32| (sym - first + 1) as i32;
    let modified = handle.modified_sym().raw();
    if (SWITCH_VT_1..=SWITCH_VT_12).contains(&modified) {
        return Some(number(modified, SWITCH_VT_1));
    }
    let function = handle
        .raw_syms()
        .iter()
        .map(|sym| sym.raw())
        .find(|sym| (F1..=F12).contains(sym))?;
    (modifiers.ctrl && modifiers.alt).then(|| number(function, F1))
}

impl Wirbel {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                self.on_key(event.key_code(), event.state(), event.time());
            }
            InputEvent::PointerMotion { event } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let location = pointer.current_location() + event.delta();
                self.on_pointer_motion(location, event.time());
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // Absolute devices only exist in the nested backend, which has one output.
                let Some(geometry) = self
                    .monitors
                    .first()
                    .and_then(|monitor| self.space.output_geometry(&monitor.output))
                else {
                    return;
                };
                let position = event.position_transformed(geometry.size) + geometry.loc.to_f64();
                self.on_pointer_motion(position, event.time());
            }
            InputEvent::PointerButton { event } => {
                self.on_pointer_button(event.button_code(), event.state(), event.time());
            }
            InputEvent::PointerAxis { event } => {
                let mut scroll = Scroll::new(event.source(), event.time());
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    scroll.set(axis, event.amount(axis), event.amount_v120(axis));
                }
                self.on_pointer_axis(scroll);
            }
            _ => {}
        }
    }

    /// The user did something, so the session is not idle.
    fn note_activity(&mut self) {
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);
    }

    /// Binds take the key from clients, press and release alike. A locked session has no binds:
    /// every key belongs to the lock screen. The two ways out work regardless, the session stays
    /// locked behind them: the emergency chord and switching the VT.
    pub fn on_key(&mut self, key: Keycode, state: KeyState, time: InputTime) {
        let evdev = key.raw().saturating_sub(XKB_OFFSET) as u16;
        if self.chord.feed(evdev, state == KeyState::Pressed) {
            (self.emergency)();
        }
        self.note_activity();
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let action = keyboard.input(
            self,
            key,
            state,
            serial,
            time,
            |wirbel, modifiers, handle| match state {
                KeyState::Pressed
                    if let Some(vt) = vt_of_key(modifiers, &handle)
                        && wirbel.change_vt.is_some() =>
                {
                    wirbel.swallowed_keys.push(key);
                    FilterResult::Intercept(Some(Action::ChangeVt(vt)))
                }
                KeyState::Pressed if wirbel.lock.is_active() => FilterResult::Forward,
                KeyState::Pressed => {
                    let action = wirbel.config.action_for_key(modifiers.into(), &handle);
                    match action.cloned() {
                        Some(action) => {
                            wirbel.swallowed_keys.push(key);
                            FilterResult::Intercept(Some(action))
                        }
                        None => FilterResult::Forward,
                    }
                }
                KeyState::Released => {
                    let swallowed = wirbel.swallowed_keys.iter().position(|other| *other == key);
                    match swallowed {
                        Some(index) => {
                            wirbel.swallowed_keys.swap_remove(index);
                            FilterResult::Intercept(None)
                        }
                        None => FilterResult::Forward,
                    }
                }
            },
        );
        if let Some(Some(action)) = action {
            self.run_action(action);
        }
    }

    pub fn on_pointer_motion(&mut self, location: Point<f64, Logical>, time: InputTime) {
        self.note_activity();
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = self.move_pointer(location, time);
        if let Some(monitor) = self.monitor_at(location)
            && monitor != self.active_monitor
        {
            self.active_monitor = monitor;
            // The keyboard of a locked session follows to the lock surface of this monitor.
            self.update_keyboard_focus();
        }

        if self.drag.is_some() {
            self.drag_motion(location);
        } else if self.config.sloppy_focus
            && !pointer.is_grabbed()
            && let Some(window) = self.window_at(location)
            && self.focused.as_ref() != Some(&window)
        {
            self.focus_window(Some(window), false);
        }
    }

    /// A press focuses and raises the window under the pointer, or gives a layer surface that
    /// wants it the keyboard. If a mouse bind applies, it starts a drag instead of reaching the
    /// client. A press that ends a popup grab does all of that too, once the grab is over.
    pub fn on_pointer_button(&mut self, button: u32, state: ButtonState, time: InputTime) {
        self.note_activity();
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let pressed = state == ButtonState::Pressed;
        let popup_grab = self.popup_grab.is_some();
        if pressed && !pointer.is_grabbed() {
            self.press(Some(button), serial);
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time,
            },
        );
        pointer.frame(self);
        if !pointer.is_grabbed() {
            self.finish_drag();
            // A click grab leaves the focus on its client, wherever the pointer went meanwhile.
            self.update_pointer_focus();
            if pressed && popup_grab {
                // The client has this press already, so no bind can claim it.
                self.end_popup_grab();
                self.press(None, serial);
            }
        }
    }

    /// What a press does apart from reaching the client. `button` is the one binds may claim.
    fn press(&mut self, button: Option<u32>, serial: Serial) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        match self.under(pointer.current_location()).map(|under| under.0) {
            Some(Target::Window(window)) => {
                self.set_focus(Some(window.clone()));
                let modifiers = self
                    .seat
                    .get_keyboard()
                    .map(|keyboard| (&keyboard.modifier_state()).into());
                if let (Some(button), Some(modifiers)) = (button, modifiers)
                    && let Some(action) = self.config.action_for_button(modifiers, button)
                {
                    self.start_mouse_drag(window, action, button, serial);
                }
            }
            Some(Target::Layer(layer)) if layer.can_receive_keyboard_focus() => {
                self.layer_focus = Some(layer);
                self.update_keyboard_focus();
            }
            Some(Target::Lock) => {}
            _ => {
                self.layer_focus = None;
                self.update_keyboard_focus();
            }
        }
    }

    pub fn on_pointer_axis(&mut self, scroll: Scroll) {
        self.note_activity();
        let mut frame = AxisFrame::new(scroll.time).source(scroll.source);
        for (axis, amount, discrete) in [
            (Axis::Horizontal, scroll.horizontal.0, scroll.horizontal.1),
            (Axis::Vertical, scroll.vertical.0, scroll.vertical.1),
        ] {
            let value = amount
                .or(discrete.map(|v120| v120 * 15.0 / 120.0))
                .unwrap_or(0.0);
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(discrete) = discrete {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if scroll.source == AxisSource::Finger && amount == Some(0.0) {
                frame = frame.stop(axis);
            }
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

/// One scroll event. Each axis has a continuous amount and steps of 1/120 of a wheel notch.
#[derive(Debug, Clone, Copy)]
pub struct Scroll {
    pub source: AxisSource,
    pub time: InputTime,
    pub horizontal: (Option<f64>, Option<f64>),
    pub vertical: (Option<f64>, Option<f64>),
}

impl Scroll {
    pub fn new(source: AxisSource, time: InputTime) -> Self {
        Self {
            source,
            time,
            horizontal: (None, None),
            vertical: (None, None),
        }
    }

    pub fn set(&mut self, axis: Axis, amount: Option<f64>, v120: Option<f64>) {
        match axis {
            Axis::Horizontal => self.horizontal = (amount, v120),
            Axis::Vertical => self.vertical = (amount, v120),
        }
    }
}
