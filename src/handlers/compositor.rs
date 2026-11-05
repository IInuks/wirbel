use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface},
        shm::{ShmHandler, ShmState},
    },
};

use crate::{ClientState, Wirbel};

impl CompositorHandler for Wirbel {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().expect("client without ClientState").compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self.space.elements().find(|window| {
                window.toplevel().is_some_and(|toplevel| toplevel.wl_surface() == &root)
            });
            if let Some(window) = window {
                window.on_commit();
            }
        }

        self.toplevel_commit(surface);
        self.popup_commit(surface);
        self.needs_redraw = true;
    }
}

impl BufferHandler for Wirbel {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Wirbel {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
