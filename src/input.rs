//! Input handling. Backends translate their events into the `on_*` methods, which is also where
//! tests inject input.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, InputTime, KeyState,
        KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
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
                let Some(output) = self.space.outputs().next() else { return };
                let Some(geometry) = self.space.output_geometry(output) else { return };
                let position = event.position_transformed(geometry.size) + geometry.loc.to_f64();
                self.on_pointer_motion(position, event.time());
            }
            InputEvent::PointerButton { event } => {
                self.on_pointer_button(event.button_code(), event.state(), event.time());
            }
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<I>(&event),
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: Keycode, state: KeyState, time: InputTime) {
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.input::<(), _>(self, key, state, serial, time, |_, _, _| FilterResult::Forward);
    }

    pub fn on_pointer_motion(&mut self, location: Point<f64, Logical>, time: InputTime) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let under = self.surface_under(location);
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(self, under, &MotionEvent { location, serial, time });
        pointer.frame(self);
        self.needs_redraw = true;
    }

    pub fn on_pointer_button(&mut self, button: u32, state: ButtonState, time: InputTime) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        if state == ButtonState::Pressed
            && !pointer.is_grabbed()
            && let Some(window) = self.window_at(pointer.current_location())
            && self.focused.as_ref() != Some(&window)
        {
            self.set_focus(Some(window));
        }
        let serial = SERIAL_COUNTER.next_serial();
        pointer.button(self, &ButtonEvent { button, state, serial, time });
        pointer.frame(self);
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: &I::PointerAxisEvent) {
        let source = event.source();
        let mut frame = AxisFrame::new(event.time()).source(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let discrete = event.amount_v120(axis);
            let amount = event.amount(axis).or(discrete.map(|v120| v120 * 15.0 / 120.0)).unwrap_or(0.0);
            if amount != 0.0 {
                frame = frame.value(axis, amount);
                if let Some(discrete) = discrete {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }
        let Some(pointer) = self.seat.get_pointer() else { return };
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}
