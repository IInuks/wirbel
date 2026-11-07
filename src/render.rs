//! Scene assembly, independent of the backend.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Kind, RenderElementStates, default_primary_scanout_output_compare,
            memory::MemoryRenderBufferRenderElement,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            utils::{CropRenderElement, select_dmabuf_feedback},
        },
    },
    desktop::{
        LayerSurface, PopupManager, Window, layer_map_for_output,
        utils::{
            OutputPresentationFeedback, send_dmabuf_feedback_surface_tree,
            send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, take_presentation_feedback_surface_tree,
            update_surface_primary_scanout_output, with_surfaces_surface_tree,
        },
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Physical, Point, Rectangle, Scale},
    wayland::{
        compositor::with_states, dmabuf::DmabufFeedback, seat::WaylandFocus,
        shell::wlr_layer::Layer,
    },
};
use std::{borrow::Cow, sync::Mutex, time::Duration};

use crate::{Wirbel, workspace::FullKind};

smithay::backend::renderer::element::render_elements! {
    pub SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<R>,
    Cropped=CropRenderElement<WaylandSurfaceRenderElement<R>>,
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
        let lines = self.line_elements(renderer, origin, scale);
        elements.extend(lines.into_iter().map(SceneElement::Cursor));

        let tiers = self
            .monitor_of_output(output)
            .map_or_else(LayerTiers::default, |index| self.layer_tiers(index));
        elements.extend(self.layer_elements(renderer, &tiers.above, origin, scale));

        for window in self.space.elements_for_output(output).rev() {
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            let location = location - window.geometry().loc;
            // What plugins did to it moves the picture, window and border alike, and nothing else.
            let effect = self.plugin_state.effect(window).unwrap_or_default();
            let shift = Point::<i32, Logical>::from((
                effect.offset.0.round() as i32,
                effect.offset.1.round() as i32,
            ));
            let alpha = effect.opacity;
            match self.overflow_clip(window, location) {
                None => {
                    let location = (location + shift - origin).to_physical_precise_round(scale);
                    elements.extend(window.render_elements(renderer, location, scale, alpha));
                }
                Some(clip) => {
                    let clip = Rectangle::new(clip.loc + shift - origin, clip.size);
                    let at = location + shift - origin;
                    elements.extend(self.clipped_window(renderer, window, at, clip, scale, alpha));
                }
            }

            let border = self.borders.iter().find(|(owner, _)| owner == window);
            let edges = border.into_iter().flat_map(|(_, edges)| edges);
            elements.extend(edges.map(|edge| {
                let location = (edge.location + shift - origin).to_physical_precise_round(scale);
                SceneElement::Solid(SolidColorRenderElement::from_buffer(
                    &edge.buffer,
                    location,
                    scale,
                    alpha,
                    Kind::Unspecified,
                ))
            }));
        }
        elements.extend(self.layer_elements(renderer, &tiers.below, origin, scale));
        elements
    }

    /// The rectangle a window has to stay in, if it does not. A tile or a window that fills its
    /// workspace has its place, and a client that draws more than it was told must not cover the
    /// neighbours. Floating windows and menus of X11 clients go where they like.
    pub fn overflow_clip(
        &self,
        window: &Window,
        location: Point<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        let clip = self.clip_of(window)?;
        let mut bbox = window.bbox();
        bbox.loc += location;
        (!clip.contains_rect(bbox)).then_some(clip)
    }

    /// The place of a window, see `overflow_clip`.
    pub fn clip_of(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let frame = self.frames.iter().find(|frame| frame.window == *window)?;
        let bound = frame.tiled || frame.request.full.is_some();
        let inner = frame.inner;
        bound.then(|| Rectangle::new((inner.x, inner.y).into(), (inner.w, inner.h).into()))
    }

    /// A window cut off at `clip`. Its popups are not, they are supposed to leave the window.
    fn clipped_window<R>(
        &self,
        renderer: &mut R,
        window: &Window,
        location: Point<i32, Logical>,
        clip: Rectangle<i32, Logical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<SceneElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Texture + 'static,
    {
        let mut elements = Vec::new();
        let Some(surface) = window.wl_surface() else {
            return elements;
        };
        for (popup, offset) in PopupManager::popups_for_surface(&surface) {
            let offset = window.geometry().loc + offset - popup.geometry().loc;
            elements.extend(render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                (location + offset).to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            ));
        }
        let clip = clip.to_physical_precise_round(scale);
        let tree: Vec<WaylandSurfaceRenderElement<R>> = render_elements_from_surface_tree(
            renderer,
            &surface,
            location.to_physical_precise_round::<_, i32>(scale),
            scale,
            alpha,
            Kind::Unspecified,
        );
        elements.extend(tree.into_iter().filter_map(|element| {
            let cropped = CropRenderElement::from_element(element, scale, clip)?;
            Some(SceneElement::Cropped(cropped))
        }));
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
            let location = (at + self.dnd_offset.to_f64())
                .to_physical(scale)
                .to_i32_round();
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

    /// What is on `output` right now: every surface of the scene, windows and layers first.
    fn shown_surfaces(&self, output: &Output) -> Vec<WlSurface> {
        if self.lock.is_active() {
            return self.lock.surface_on(output).into_iter().collect();
        }
        let windows = self.space.elements_for_output(output);
        let mut roots: Vec<WlSurface> = windows
            .filter_map(|window| window.wl_surface().map(Cow::into_owned))
            .collect();
        let map = layer_map_for_output(output);
        roots.extend(map.layers().map(|layer| layer.wl_surface().clone()));
        let popups = roots.iter().flat_map(PopupManager::popups_for_surface);
        let popups: Vec<WlSurface> = popups
            .map(|(popup, _)| popup.wl_surface().clone())
            .collect();
        roots.extend(popups);
        roots
    }

    /// Notes which output each surface was last seen on, from what a render found out. Everything
    /// that goes by the output of a surface comes after this.
    pub fn update_scanout_outputs(&self, output: &Output, states: &RenderElementStates) {
        let mut roots = self.shown_surfaces(output);
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            roots.push(surface.clone());
        }
        roots.extend(self.dnd_icon.clone());
        for root in roots.iter().filter(|root| root.is_alive()) {
            with_surfaces_surface_tree(root, |surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
    }

    /// Takes what clients want to know about the frame that `states` belong to, to answer it when
    /// the frame is on the screen.
    pub fn presentation_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        for root in self.shown_surfaces(output) {
            take_presentation_feedback_surface_tree(
                &root,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        feedback
    }

    /// Sends each surface the dmabuf feedback for how it was drawn last: `scanout` for one that
    /// made it onto a plane of its own, or could, `render` for the rest.
    pub fn send_dmabuf_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
        render: &DmabufFeedback,
        scanout: &DmabufFeedback,
    ) {
        for root in self.shown_surfaces(output) {
            send_dmabuf_feedback_surface_tree(
                &root,
                output,
                surface_primary_scanout_output,
                |surface, _| select_dmabuf_feedback(surface, states, render, scanout),
            );
        }
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
