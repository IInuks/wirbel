//! `wlr-foreign-toplevel-management`: taskbars and window lists.
//!
//! Nothing else in the compositor has to know about it. `refresh_foreign_toplevels` compares the
//! windows with what the clients were told last, after every loop iteration, and sends the rest.

use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, State, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::ClientId, protocol::wl_output::WlOutput,
        },
    },
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{Wirbel, workspace::FullKind};

const VERSION: u32 = 3;

#[derive(Default)]
pub struct ForeignToplevelState {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    entries: Vec<Entry>,
}

impl ForeignToplevelState {
    pub fn new(display: &DisplayHandle) -> Self {
        display.create_global::<Wirbel, ZwlrForeignToplevelManagerV1, ()>(VERSION, ());
        Self::default()
    }
}

/// What clients know about a window.
#[derive(Clone, PartialEq, Default)]
struct Info {
    title: String,
    app_id: String,
    activated: bool,
    full: Option<FullKind>,
    output: Option<Output>,
    parent: Option<Window>,
}

impl Info {
    fn states(&self) -> Vec<u8> {
        let activated = self.activated.then_some(State::Activated);
        let full = self.full.map(|kind| match kind {
            FullKind::Fullscreen => State::Fullscreen,
            FullKind::Maximized => State::Maximized,
        });
        let states = activated.into_iter().chain(full);
        states
            .flat_map(|state| (state as u32).to_ne_bytes())
            .collect()
    }
}

struct Entry {
    window: Window,
    info: Info,
    handles: Vec<ZwlrForeignToplevelHandleV1>,
}

fn wl_outputs(output: &Output, client: &Client) -> impl Iterator<Item = WlOutput> {
    output.client_outputs(client)
}

/// Sends what differs from `old`, which is `None` for a new handle, and closes the list.
fn announce(
    handle: &ZwlrForeignToplevelHandleV1,
    old: Option<&Info>,
    new: &Info,
    display: &DisplayHandle,
) {
    if old.is_none_or(|old| old.title != new.title) {
        handle.title(new.title.clone());
    }
    if old.is_none_or(|old| old.app_id != new.app_id) {
        handle.app_id(new.app_id.clone());
    }
    if old.is_none_or(|old| (old.activated, old.full) != (new.activated, new.full)) {
        handle.state(new.states());
    }
    let old_output = old.and_then(|old| old.output.as_ref());
    if old_output != new.output.as_ref()
        && let Ok(client) = display.get_client(handle.id())
    {
        for wl_output in old_output.iter().flat_map(|o| wl_outputs(o, &client)) {
            handle.output_leave(&wl_output);
        }
        for wl_output in new.output.iter().flat_map(|o| wl_outputs(o, &client)) {
            handle.output_enter(&wl_output);
        }
    }
}

/// The title and app id of a window, empty where it has none. X11 windows give their class.
pub(crate) fn window_names(window: &Window) -> (String, String) {
    if let Some(toplevel) = window.toplevel() {
        return with_states(toplevel.wl_surface(), |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return Default::default();
            };
            let data = data.lock().unwrap();
            let title = data.title.clone().unwrap_or_default();
            (title, data.app_id.clone().unwrap_or_default())
        });
    }
    match window.x11_surface() {
        Some(surface) => (surface.title(), surface.class()),
        None => Default::default(),
    }
}

impl Wirbel {
    fn foreign_info(&self, window: &Window) -> Info {
        let mut info = Info {
            activated: self.focused.as_ref() == Some(window),
            full: self.full_kind(window),
            output: self
                .locate(window)
                .map(|(m, _)| &self.monitors[m])
                .filter(|monitor| monitor.connected)
                .map(|monitor| monitor.output.clone()),
            ..Info::default()
        };
        (info.title, info.app_id) = window_names(window);
        if let Some(toplevel) = window.toplevel() {
            info.parent = toplevel
                .parent()
                .and_then(|parent| self.find_window(&parent));
        }
        if let Some(surface) = window.x11_surface() {
            info.parent = surface.is_transient_for().and_then(|parent| {
                let mut windows = self.listed_windows().into_iter();
                windows.find(|window| {
                    window
                        .x11_surface()
                        .is_some_and(|surface| surface.window_id() == parent)
                })
            });
        }
        info
    }

    /// Windows with something on the screen, those on hidden workspaces too.
    pub(crate) fn listed_windows(&self) -> Vec<Window> {
        let workspaces = self.monitors.iter().flat_map(|monitor| &monitor.workspaces);
        workspaces
            .flat_map(|workspace| workspace.windows())
            .filter(|window| !self.awaiting_buffer.contains(window))
            .collect()
    }

    fn make_handle(
        &self,
        manager: &ZwlrForeignToplevelManagerV1,
    ) -> Option<ZwlrForeignToplevelHandleV1> {
        let client = manager.client()?;
        let handle = client
            .create_resource::<ZwlrForeignToplevelHandleV1, _, Self>(
                &self.display_handle,
                manager.version(),
                (),
            )
            .ok()?;
        manager.toplevel(&handle);
        Some(handle)
    }

    /// The handle of `parent` that belongs to the same client as `handle`.
    fn parent_handle(
        &self,
        handle: &ZwlrForeignToplevelHandleV1,
        parent: Option<&Window>,
    ) -> Option<ZwlrForeignToplevelHandleV1> {
        let parent = parent?;
        let client = handle.client()?;
        let entries = self.foreign_toplevels.entries.iter();
        let entry = entries.into_iter().find(|entry| entry.window == *parent)?;
        let handles = entry.handles.iter();
        handles
            .into_iter()
            .find(|other| other.client().as_ref() == Some(&client))
            .cloned()
    }

    /// Brings the clients up to date.
    pub fn refresh_foreign_toplevels(&mut self) {
        let state = &mut self.foreign_toplevels;
        if state.managers.is_empty() {
            state.entries.clear();
            return;
        }
        let windows = self.listed_windows();

        let mut entries = std::mem::take(&mut self.foreign_toplevels.entries);
        entries.retain(|entry| {
            let alive = windows.contains(&entry.window);
            if !alive {
                entry.handles.iter().for_each(|handle| handle.closed());
            }
            alive
        });

        let mut changed = Vec::new();
        for window in windows {
            let info = self.foreign_info(&window);
            let index = entries.iter().position(|entry| entry.window == window);
            let index = index.unwrap_or_else(|| {
                let managers = self.foreign_toplevels.managers.iter();
                let handles = managers.filter_map(|manager| self.make_handle(manager));
                entries.push(Entry {
                    handles: handles.collect(),
                    window,
                    info: Info::default(),
                });
                changed.push((entries.len() - 1, None));
                entries.len() - 1
            });
            let entry = &mut entries[index];
            let first = changed.iter().any(|(i, _)| *i == index);
            if first {
                entry.info = info;
            } else if entry.info != info {
                let old = std::mem::replace(&mut entry.info, info);
                changed.push((index, Some(old)));
            }
        }
        self.foreign_toplevels.entries = entries;

        // Only now all handles exist that a parent could refer to.
        let entries = &self.foreign_toplevels.entries;
        for (index, old) in &changed {
            let entry = &entries[*index];
            for handle in &entry.handles {
                announce(handle, old.as_ref(), &entry.info, &self.display_handle);
                let parent_changed = old.as_ref().map_or(entry.info.parent.is_some(), |old| {
                    old.parent != entry.info.parent
                });
                if parent_changed && handle.version() >= 3 {
                    handle.parent(
                        self.parent_handle(handle, entry.info.parent.as_ref())
                            .as_ref(),
                    );
                }
                handle.done();
            }
        }
    }

    /// A client that binds an output late still learns which windows are on it.
    pub(super) fn foreign_output_bound(&self, output: &Output, wl_output: &WlOutput) {
        let Some(client) = wl_output.client() else {
            return;
        };
        for entry in &self.foreign_toplevels.entries {
            if entry.info.output.as_ref() != Some(output) {
                continue;
            }
            let handles = entry.handles.iter();
            for handle in handles.filter(|handle| handle.client().as_ref() == Some(&client)) {
                handle.output_enter(wl_output);
                handle.done();
            }
        }
    }

    pub fn close_window(&self, window: &Window) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        }
        if let Some(surface) = window.x11_surface()
            && let Err(err) = surface.close()
        {
            tracing::warn!(%err, "failed to close an X11 window");
        }
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for Wirbel {
    fn bind(
        state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrForeignToplevelManagerV1>,
        _data: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let manager = init.init(manager, ());
        state.foreign_toplevels.managers.push(manager.clone());
        for index in 0..state.foreign_toplevels.entries.len() {
            let Some(handle) = state.make_handle(&manager) else {
                continue;
            };
            let entry = &mut state.foreign_toplevels.entries[index];
            entry.handles.push(handle.clone());
            let info = entry.info.clone();
            announce(&handle, None, &info, &state.display_handle);
            if handle.version() >= 3 && info.parent.is_some() {
                let parent = state.parent_handle(&handle, info.parent.as_ref());
                handle.parent(parent.as_ref());
            }
            handle.done();
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Wirbel {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            state
                .foreign_toplevels
                .managers
                .retain(|other| other != manager);
            manager.finished();
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        manager: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state
            .foreign_toplevels
            .managers
            .retain(|other| other != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Wirbel {
    fn request(
        state: &mut Self,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let entries = state.foreign_toplevels.entries.iter();
        let entry = entries
            .into_iter()
            .find(|entry| entry.handles.contains(handle));
        // A window that is gone leaves its handle behind until the client destroys it.
        let Some(window) = entry.map(|entry| entry.window.clone()) else {
            return;
        };
        match request {
            Request::Activate { .. } => state.activate_window(&window),
            Request::Close => state.close_window(&window),
            Request::SetMaximized => state.set_full(&window, Some(FullKind::Maximized)),
            Request::SetFullscreen { .. } => state.set_full(&window, Some(FullKind::Fullscreen)),
            Request::UnsetMaximized | Request::UnsetFullscreen => state.set_full(&window, None),
            // No such thing as minimizing here, and the rectangle is only a hint for that.
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
        _data: &(),
    ) {
        for entry in &mut state.foreign_toplevels.entries {
            entry.handles.retain(|other| other != handle);
        }
    }
}
