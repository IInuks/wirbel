//! Nested backend: runs inside a window of another Wayland compositor or X server.

use std::{cell::RefCell, rc::Rc};

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, winit::cursor::Cursor},
    utils::Transform,
};

use crate::{Wirbel, state::BACKGROUND_COLOR};

const REFRESH_MHZ: i32 = 60_000;

struct Nested {
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
    output: Output,
    /// Whether a frame went out yet. The surface has no buffer age before that.
    presented: bool,
}

pub fn init(
    event_loop: &mut EventLoop<'static, Wirbel>,
    state: &mut Wirbel,
) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wirbel".into(),
            model: "nested".into(),
            serial_number: "0".into(),
        },
    );
    let _global = output.create_global::<Wirbel>(&state.display_handle);
    // GLES framebuffers are upside down relative to the window.
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.add_output(&output);

    let nested = Rc::new(RefCell::new(Nested {
        damage_tracker: OutputDamageTracker::from_output(&output),
        backend,
        output,
        presented: false,
    }));

    state.redraw = {
        let nested = nested.clone();
        Box::new(move || nested.borrow().backend.window().request_redraw())
    };

    event_loop.handle().insert_source(winit, move |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            let mut nested = nested.borrow_mut();
            let mode = Mode {
                size,
                refresh: REFRESH_MHZ,
            };
            nested.output.change_current_state(Some(mode), None, None, None);
            nested.damage_tracker = OutputDamageTracker::from_output(&nested.output);
            drop(nested);
            state.retile();
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => nested.borrow_mut().render(state),
        WinitEvent::CloseRequested => state.loop_signal.stop(),
        _ => {}
    })?;

    Ok(())
}

impl Nested {
    fn render(&mut self, state: &mut Wirbel) {
        match &state.cursor_status {
            CursorImageStatus::Named(icon) => {
                self.backend.window().set_cursor(Cursor::Icon(*icon));
                self.backend.window().set_cursor_visible(true);
            }
            CursorImageStatus::Surface(_) | CursorImageStatus::Hidden => {
                self.backend.window().set_cursor_visible(false);
            }
        }

        let age = if self.presented {
            self.backend.buffer_age().unwrap_or(0)
        } else {
            0
        };
        let (renderer, mut framebuffer) = match self.backend.bind() {
            Ok(bound) => bound,
            Err(err) => {
                tracing::error!(%err, "failed to bind the window surface");
                return;
            }
        };
        let elements = state.scene_elements(renderer, &self.output);
        let damage = match self.damage_tracker.render_output(
            renderer,
            &mut framebuffer,
            age,
            &elements,
            BACKGROUND_COLOR,
        ) {
            Ok(result) => result.damage.cloned(),
            Err(err) => {
                tracing::error!(?err, "failed to render");
                return;
            }
        };
        drop(framebuffer);

        if let Err(err) = self.backend.submit(damage.as_deref()) {
            tracing::error!(%err, "failed to present");
            return;
        }
        self.presented = true;
        state.send_frame_callbacks(&self.output);
    }
}
