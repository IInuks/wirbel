//! wlr-layer-shell: panels, wallpapers, launchers. Each surface sits on one output. It stays in
//! `unmapped_layers` until its first commit, which arranges it and gets its first configure. A
//! commit that drops the buffer sends it back there.

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::{
    desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    wayland::shell::{
        wlr_layer::{
            Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
        },
        xdg::PopupSurface,
    },
};

use crate::{Wirbel, state::has_buffer};

/// Set on a layer surface once it has shown a buffer.
#[derive(Default)]
struct Shown(AtomicBool);

impl WlrLayerShellHandler for Wirbel {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output.as_ref().and_then(Output::from_resource).or_else(|| {
            let monitor = self.monitors.get(self.active_monitor)?;
            Some(monitor.output.clone())
        });
        let connected = |output: &Output| {
            self.monitor_of_output(output)
                .is_some_and(|index| self.monitors[index].connected)
        };
        match output.filter(connected) {
            Some(output) => {
                let layer = LayerSurface::new(surface, namespace);
                self.unmapped_layers.push((layer, output));
            }
            None => surface.send_close(),
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        self.unmapped_layers
            .retain(|(layer, _)| *layer.layer_surface() != surface);
        if let Some((layer, output)) = self.find_layer(surface.wl_surface()) {
            layer_map_for_output(&output).unmap_layer(&layer);
            self.layers_changed();
        }
    }
}

impl Wirbel {
    /// The mapped layer surface `surface` is the root of, with its output.
    pub fn find_layer(&self, surface: &WlSurface) -> Option<(LayerSurface, Output)> {
        self.monitors.iter().find_map(|monitor| {
            let map = layer_map_for_output(&monitor.output);
            let layer = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)?;
            Some((layer.clone(), monitor.output.clone()))
        })
    }

    pub(super) fn layer_commit(&mut self, surface: &WlSurface) {
        let unmapped = self
            .unmapped_layers
            .iter()
            .position(|(layer, _)| layer.wl_surface() == surface);
        if let Some(index) = unmapped {
            let (layer, output) = self.unmapped_layers.swap_remove(index);
            if let Err(err) = layer_map_for_output(&output).map_layer(&layer) {
                tracing::warn!(%err, "failed to map a layer surface");
                return;
            }
            layer.layer_surface().send_configure();
            self.update_surface_scales();
            self.layers_changed();
            return;
        }

        let Some((layer, output)) = self.find_layer(surface) else {
            return;
        };
        let shown = layer.user_data().get_or_insert(Shown::default);
        if has_buffer(surface) {
            shown.0.store(true, Ordering::Relaxed);
        } else if shown.0.swap(false, Ordering::Relaxed) {
            layer_map_for_output(&output).unmap_layer(&layer);
            self.unmapped_layers.push((layer, output));
            self.layers_changed();
            return;
        }
        layer_map_for_output(&output).arrange();
        self.layers_changed();
    }

    /// After a layer surface changed: the room for windows and who wants the keyboard may be
    /// different now.
    fn layers_changed(&mut self) {
        let outdated = (0..self.monitors.len())
            .any(|index| self.work_area(index) != self.monitors[index].work_area);
        if outdated {
            self.retile();
        }
        self.update_keyboard_focus();
        self.needs_redraw = true;
    }

    /// Tells the layer surfaces of a lost output that they are gone.
    pub(crate) fn close_layers(&mut self, output: &Output) {
        let mut map = layer_map_for_output(output);
        let layers: Vec<_> = map.layers().cloned().collect();
        for layer in layers {
            layer.layer_surface().send_close();
            map.unmap_layer(&layer);
        }
        drop(map);
        self.unmapped_layers.retain(|(layer, other)| {
            if other == output {
                layer.layer_surface().send_close();
            }
            other != output
        });
        self.update_keyboard_focus();
    }
}
