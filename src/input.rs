//! Input handling. Backends translate their events into the `on_*` methods, which is also where
//! tests inject input.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        InputTime, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::Wirbel;

impl Wirbel {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                self.on_key(event.key_code(), event.state(), event.time());
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

    /// Binds take the key from clients, press and release alike.
    pub fn on_key(&mut self, key: Keycode, state: KeyState, time: InputTime) {
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
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = self.move_pointer(location, time);
        if let Some(monitor) = self.monitor_at(location) {
            self.active_monitor = monitor;
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

    /// A press focuses and raises the window under the pointer. If a mouse bind applies, it
    /// starts a drag instead of reaching the client.
    pub fn on_pointer_button(&mut self, button: u32, state: ButtonState, time: InputTime) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Pressed
            && !pointer.is_grabbed()
            && let Some(window) = self.window_at(pointer.current_location())
        {
            self.set_focus(Some(window.clone()));
            let modifiers = self
                .seat
                .get_keyboard()
                .map(|keyboard| (&keyboard.modifier_state()).into());
            let action =
                modifiers.and_then(|modifiers| self.config.action_for_button(modifiers, button));
            if let Some(action) = action {
                self.start_mouse_drag(window, action, button, serial);
            }
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
        }
    }

    pub fn on_pointer_axis(&mut self, scroll: Scroll) {
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
