use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        shm::{ShmHandler, ShmState},
    },
    xwayland::XWaylandClientData,
};

use crate::{ClientState, Wirbel};

impl CompositorHandler for Wirbel {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("client without ClientState")
            .compositor_state
    }

    /// A dmabuf may still be in the works on the GPU. Its commit waits until it can be read: for
    /// the acquire point of a client with explicit sync, or else for the fence of the buffer.
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let (dmabuf, acquire) = with_states(surface, |states| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let dmabuf = match cached.pending().buffer.as_ref() {
                    Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
                    _ => None,
                };
                let mut syncobj = states.cached_state.get::<DrmSyncobjCachedState>();
                (dmabuf, syncobj.pending().acquire_point.clone())
            });
            let (Some(dmabuf), Some(client)) = (dmabuf, surface.client()) else {
                return;
            };
            let unblock = move |state: &mut Wirbel| {
                let display = state.display_handle.clone();
                state
                    .client_compositor_state(&client)
                    .blocker_cleared(state, &display);
            };
            if let Some(acquire) = acquire {
                match acquire.generate_blocker() {
                    Ok((blocker, source)) => {
                        let unblock = unblock.clone();
                        let cleared =
                            state.loop_handle.insert_source(source, move |_, _, state| {
                                unblock(state);
                                Ok(())
                            });
                        if cleared.is_ok() {
                            add_blocker(surface, blocker);
                            return;
                        }
                    }
                    Err(err) => tracing::warn!(?err, "failed to wait for an acquire point"),
                }
            }
            let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
                return;
            };
            let cleared = state.loop_handle.insert_source(source, move |_, _, state| {
                unblock(state);
                Ok(())
            });
            if cleared.is_ok() {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        (self.early_import)(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.find_window(&root) {
                window.on_commit();
            }
        }

        self.toplevel_commit(surface);
        self.layer_commit(surface);
        self.popup_commit(surface);
        // A window that got its first buffer or a new size may now cover the pointer, or no
        // longer do so.
        self.update_pointer_focus();
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
