//! Nested backend: runs inside a window of another Wayland compositor or X server.

use std::{cell::RefCell, rc::Rc};

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        winit::{cursor::Cursor, dpi::LogicalSize, window::WindowAttributes},
    },
    utils::Transform,
};

use crate::Wirbel;

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
    let attributes = WindowAttributes::default()
        .with_title("wirbel")
        .with_surface_size(LogicalSize::new(1280.0, 800.0));
    let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)?;
    offer_dmabufs(&mut backend, state);

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
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
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
    state.import_dmabuf = {
        let nested = nested.clone();
        Box::new(move |dmabuf| {
            let mut nested = nested.borrow_mut();
            let result = nested.backend.renderer().import_dmabuf(dmabuf, None);
            if let Err(err) = &result {
                tracing::warn!(%err, "failed to import a dmabuf");
            }
            result.is_ok()
        })
    };

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                let mut nested = nested.borrow_mut();
                let mode = Mode {
                    size,
                    refresh: REFRESH_MHZ,
                };
                nested
                    .output
                    .change_current_state(Some(mode), None, None, None);
                nested.damage_tracker = OutputDamageTracker::from_output(&nested.output);
                let output = nested.output.clone();
                drop(nested);
                state.output_resized(&output);
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => nested.borrow_mut().render(state),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })?;

    Ok(())
}

/// Lets clients render on the GPU of the host window, if we can tell which one it is.
fn offer_dmabufs(backend: &mut WinitGraphicsBackend<GlesRenderer>, state: &mut Wirbel) {
    let device = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    let node = match device {
        Ok(Some(node)) => node,
        Ok(None) => {
            tracing::warn!("the EGL device has no DRM node, not offering dmabufs");
            return;
        }
        Err(err) => {
            tracing::warn!(%err, "failed to find the EGL device, not offering dmabufs");
            return;
        }
    };
    tracing::info!(%node, "offering dmabufs");
    let formats = backend.renderer().dmabuf_formats();
    state.offer_dmabufs(node.dev_id(), formats);
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
            state.config.background_color,
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
