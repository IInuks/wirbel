//! Wayland protocol handlers.

mod compositor;
mod layer_shell;
mod selection;
mod session_lock;
mod xdg_shell;

use smithay::{
    backend::{allocator::Format, allocator::dmabuf::Dmabuf},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus, tablet::TabletSeatHandler},
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::with_states,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{data_device::set_data_device_focus, primary_selection::set_primary_focus},
        shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData, decoration::XdgDecorationHandler},
    },
};

use crate::{Wirbel, state::apply_output_scale};

pub use session_lock::SessionLock;

impl SeatHandler for Wirbel {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// The clipboard belongs to the client with the keyboard.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(Resource::client);
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.needs_redraw = true;
    }
}

// The cursor-shape global needs this. Tablets themselves are not supported.
impl TabletSeatHandler for Wirbel {
    type ToolFocus = WlSurface;
}

impl OutputHandler for Wirbel {}

// `WlSurface` is only a valid pointer target with this. The protocol itself is not offered yet.
impl PointerConstraintsHandler for Wirbel {}

/// Windows have no decoration except what we draw around them, so nobody draws their own.
impl XdgDecorationHandler for Wirbel {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        server_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        server_side(&toplevel);
    }
}

fn server_side(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
    // The protocol wants an answer even if nothing changed, which Smithay would not send.
    with_states(toplevel.wl_surface(), |states| {
        if let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() {
            data.lock().unwrap().initial_decoration_configure_sent = false;
        }
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_configure();
    }
}

impl DmabufHandler for Wirbel {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if (self.import_dmabuf)(&dmabuf) {
            if let Err(err) = notifier.successful::<Self>() {
                tracing::warn!(%err, "client gone before its dmabuf was ready");
            }
        } else {
            notifier.failed();
        }
    }
}

impl Wirbel {
    /// Offers dmabufs of the given device to clients. The backend calls this once it knows what
    /// its renderer can import, along with setting `import_dmabuf`.
    pub fn offer_dmabufs(&mut self, device: u64, formats: impl IntoIterator<Item = Format>) {
        match DmabufFeedbackBuilder::new(device, formats).build() {
            Ok(feedback) => {
                self.dmabuf_state
                    .create_global_with_default_feedback::<Self>(&self.display_handle, &feedback);
            }
            Err(err) => tracing::warn!(%err, "failed to prepare the dmabuf feedback"),
        }
    }
}

impl FractionalScaleHandler for Wirbel {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        if let Some(output) = self.output_of_surface(&surface) {
            with_states(&surface, |states| {
                apply_output_scale(&surface, states, &output);
            });
        }
    }
}

impl IdleInhibitHandler for Wirbel {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.push(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(index) = self.idle_inhibitors.iter().position(|s| *s == surface) {
            self.idle_inhibitors.swap_remove(index);
        }
    }
}

impl IdleNotifierHandler for Wirbel {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

smithay::delegate_dispatch2!(Wirbel);
