//! Scene assembly, independent of the backend.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Kind,
            memory::MemoryRenderBufferRenderElement,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    desktop::{LayerSurface, PopupManager, layer_map_for_output, utils::send_frames_surface_tree},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Physical, Point, Scale},
    wayland::{compositor::with_states, shell::wlr_layer::Layer},
};
use std::{sync::Mutex, time::Duration};

use crate::{Wirbel, workspace::FullKind};

smithay::backend::renderer::element::render_elements! {
    pub SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<R>,
}

/// Which part of a layer surface a place in the stacking order is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerPart {
    Whole,
    Surface,
    Popups,
}

/// One entry of the stacking order. `location` is global.
#[derive(Debug, Clone)]
pub struct LayerEntry {
    pub layer: LayerSurface,
    pub location: Point<i32, Logical>,
    pub part: LayerPart,
}

/// The layer surfaces of a monitor around its windows, each list front to back.
#[derive(Debug, Default)]
pub struct LayerTiers {
    pub above: Vec<LayerEntry>,
    pub below: Vec<LayerEntry>,
}

impl Wirbel {
    /// How the layers of a monitor stack up with its windows. A fullscreen window covers the Top
    /// layer. Popups belong to something the user is working with, so whatever the layer of their
    /// parent, they stay above the windows.
    pub fn layer_tiers(&self, index: usize) -> LayerTiers {
        let mut tiers = LayerTiers::default();
        let Some(area) = self.monitor_area(index) else {
            return tiers;
        };
        let monitor = &self.monitors[index];
        let origin = Point::<i32, Logical>::from((area.x, area.y));
        let covered = monitor
            .workspace()
            .full
            .as_ref()
            .is_some_and(|(_, kind)| *kind == FullKind::Fullscreen);

        let map = layer_map_for_output(&monitor.output);
        let on = |layer: Layer, part: LayerPart| {
            let map = &map;
            map.layers_on(layer).rev().map(move |surface| LayerEntry {
                layer: surface.clone(),
                location: origin + map.layer_geometry(surface).unwrap_or_default().loc,
                part,
            })
        };
        tiers.above.extend(on(Layer::Overlay, LayerPart::Whole));
        if covered {
            tiers.above.extend(on(Layer::Top, LayerPart::Popups));
            tiers.below.extend(on(Layer::Top, LayerPart::Surface));
        } else {
            tiers.above.extend(on(Layer::Top, LayerPart::Whole));
        }
        for layer in [Layer::Bottom, Layer::Background] {
            tiers.above.extend(on(layer, LayerPart::Popups));
            tiers.below.extend(on(layer, LayerPart::Surface));
        }
        tiers
    }

    fn layer_elements<R>(
        &self,
        renderer: &mut R,
        entries: &[LayerEntry],
        origin: Point<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + 'static,
    {
        let mut elements = Vec::new();
        for entry in entries {
            let surface = entry.layer.wl_surface();
            let location: Point<i32, Physical> =
                (entry.location - origin).to_physical_precise_round(scale);
            if entry.part == LayerPart::Whole {
                elements.extend(entry.layer.render_elements(renderer, location, scale, 1.0));
                continue;
            }
            if entry.part == LayerPart::Popups {
                for (popup, offset) in PopupManager::popups_for_surface(surface) {
                    let offset = offset - popup.geometry().loc;
                    let location =
                        (entry.location + offset - origin).to_physical_precise_round(scale);
                    elements.extend(render_elements_from_surface_tree(
                        renderer,
                        popup.wl_surface(),
                        location,
                        scale,
                        1.0,
                        Kind::Unspecified,
                    ));
                }
                continue;
            }
            elements.extend(render_elements_from_surface_tree(
                renderer,
                surface,
                location,
                scale,
                1.0,
                Kind::Unspecified,
            ));
        }
        elements
    }

    /// Everything visible on `output`, front to back: the cursor with a dragged icon, the layers
    /// above the windows, each window on top of its own border, so that a floating window also
    /// covers the borders of the tiles below, then the layers below the windows. A locked session
    /// shows its lock surface and nothing else.
    pub fn scene_elements<R>(&self, renderer: &mut R, output: &Output) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + Send + 'static,
    {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let origin = self
            .space
            .output_geometry(output)
            .map(|geometry| geometry.loc)
            .unwrap_or_default();

        let mut elements = self.cursor_elements(renderer, origin, scale);

        if self.lock.is_active() {
            if let Some(surface) = self.lock.surface_on(output) {
                elements.extend(render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                ));
            }
            return elements;
        }

        let tiers = self
            .monitor_of_output(output)
            .map_or_else(LayerTiers::default, |index| self.layer_tiers(index));
        elements.extend(self.layer_elements(renderer, &tiers.above, origin, scale));

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
        elements.extend(self.layer_elements(renderer, &tiers.below, origin, scale));
        elements
    }

    /// The cursor and a dragged icon. Named cursors come from `cursors`, nested the host has them.
    fn cursor_elements<R>(
        &self,
        renderer: &mut R,
        origin: Point<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + Send + 'static,
    {
        let mut elements = Vec::new();
        let Some(pointer) = self.seat.get_pointer() else {
            return elements;
        };
        let at = pointer.current_location() - origin.to_f64();
        if let CursorImageStatus::Named(icon) = &self.cursor_status
            && let Some(cursors) = &self.cursors
        {
            let image = cursors.image(icon.name(), icon.alt_names(), scale.x);
            let location = at.to_physical(scale).to_i32_round() - image.hotspot;
            let element = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location.to_f64(),
                &image.buffer,
                None,
                None,
                None,
                Kind::Cursor,
            );
            match element {
                Ok(element) => elements.push(SceneElement::Cursor(element)),
                Err(err) => tracing::warn!(?err, "failed to import the cursor"),
            }
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status
            && surface.is_alive()
        {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .and_then(|attributes| {
                        attributes.lock().ok().map(|attributes| attributes.hotspot)
                    })
                    .unwrap_or_default()
            });
            let location = (at - hotspot.to_f64()).to_physical(scale).to_i32_round();
            elements.extend(render_elements_from_surface_tree(
                renderer,
                surface,
                location,
                scale,
                1.0,
                Kind::Cursor,
            ));
        }
        if let Some(icon) = self.dnd_icon.as_ref().filter(|icon| icon.is_alive()) {
            let location = at.to_physical(scale).to_i32_round();
            elements.extend(render_elements_from_surface_tree(
                renderer,
                icon,
                location,
                scale,
                1.0,
                Kind::Unspecified,
            ));
        }
        elements
    }

    /// Tells clients that a frame went out, so that they may draw the next one, and counts that
    /// frame for the lock. A backend with frames in flight has to tell those apart: it calls
    /// `lock_frame_presented` only for frames drawn after the lock began, and `send_frames`.
    pub fn send_frame_callbacks(&mut self, output: &Output) {
        self.lock_frame_presented(output);
        self.send_frames(output);
    }

    /// The frame callbacks of `send_frame_callbacks` alone.
    pub fn send_frames(&mut self, output: &Output) {
        let time = self.time();
        let throttle = Some(Duration::ZERO);
        let scanout = |_: &_, _: &_| Some(output.clone());
        if self.lock.is_active() {
            if let Some(surface) = self.lock.surface_on(output) {
                send_frames_surface_tree(&surface, output, time, throttle, scanout);
            }
        } else {
            for window in self.space.elements_for_output(output) {
                window.send_frame(output, time, throttle, scanout);
            }
            for layer in layer_map_for_output(output).layers() {
                layer.send_frame(output, time, throttle, scanout);
            }
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status
            && surface.is_alive()
        {
            send_frames_surface_tree(surface, output, time, throttle, scanout);
        }
        if let Some(icon) = self.dnd_icon.as_ref().filter(|icon| icon.is_alive()) {
            send_frames_surface_tree(icon, output, time, throttle, scanout);
        }
    }
}
