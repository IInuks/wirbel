//! Scene assembly, independent of the backend.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Kind,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    desktop::utils::send_frames_surface_tree,
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    utils::{Logical, Point, Scale},
    wayland::compositor::with_states,
};
use std::{sync::Mutex, time::Duration};

use crate::Wirbel;

smithay::backend::renderer::element::render_elements! {
    pub SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
}

impl Wirbel {
    /// Everything visible on `output`, front to back: the cursor, then each window on top of its
    /// own border, so that a floating window also covers the borders of the tiles below.
    pub fn scene_elements<R>(&self, renderer: &mut R, output: &Output) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + 'static,
    {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let origin = self
            .space
            .output_geometry(output)
            .map(|geometry| geometry.loc)
            .unwrap_or_default();

        let mut elements = self.cursor_elements(renderer, origin, scale);

        for window in self.space.elements_for_output(output).rev() {
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            let location =
                (location - window.geometry().loc - origin).to_physical_precise_round(scale);
            elements.extend(window.render_elements(renderer, location, scale, 1.0));

            let border = self.borders.iter().find(|(owner, _)| owner == window);
            let edges = border.into_iter().flat_map(|(_, edges)| edges);
            elements.extend(edges.map(|edge| {
                let location = (edge.location - origin).to_physical_precise_round(scale);
                SceneElement::Solid(SolidColorRenderElement::from_buffer(
                    &edge.buffer,
                    location,
                    scale,
                    1.0,
                    Kind::Unspecified,
                ))
            }));
        }
        elements
    }

    /// The client's cursor image. Named cursors are the backend's job.
    fn cursor_elements<R>(
        &self,
        renderer: &mut R,
        origin: Point<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + 'static,
    {
        let CursorImageStatus::Surface(surface) = &self.cursor_status else {
            return Vec::new();
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return Vec::new();
        };
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<Mutex<CursorImageAttributes>>()
                .and_then(|attributes| attributes.lock().ok().map(|attributes| attributes.hotspot))
                .unwrap_or_default()
        });
        let location = (pointer.current_location() - origin.to_f64() - hotspot.to_f64())
            .to_physical(scale)
            .to_i32_round();
        render_elements_from_surface_tree(renderer, surface, location, scale, 1.0, Kind::Cursor)
    }

    /// Tells clients that a frame went out, so that they may draw the next one.
    pub fn send_frame_callbacks(&self, output: &Output) {
        let time = self.time();
        let throttle = Some(Duration::ZERO);
        for window in self.space.elements_for_output(output) {
            window.send_frame(output, time, throttle, |_, _| Some(output.clone()));
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            send_frames_surface_tree(surface, output, time, throttle, |_, _| Some(output.clone()));
        }
    }
}
