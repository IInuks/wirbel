//! Window hook: something built into wirbel shows windows its own way instead of the layout. It
//! hears when windows map, unmap and commit, may keep windows that open meanwhile out of the
//! workspaces, and names the windows it shows, so that they get their frame callbacks although
//! the layout does not show them. The core does not know how they are shown.
//!
//! A window kept out of the workspaces sits in `Wirbel::held_windows`: it is known to the core
//! (commits, the taskbar, closing) but has no place in the layout and never takes the keyboard.
//! When the hook goes, the held windows join the active workspace as if they had just opened
//! there, without the keyboard.

use std::rc::Rc;

use smithay::{
    desktop::{PopupManager, Window, space::SpaceElement, utils::with_surfaces_surface_tree},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Rectangle, Size},
    wayland::seat::WaylandFocus,
};

use crate::{Wirbel, state::apply_output_scale, workspace::Placement};

/// What happened to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    /// It has its first buffer: it can be shown now.
    Mapped,
    /// It is gone or took its buffer away. Unmapped windows may map again later.
    Unmapped,
    /// Its surface tree changed: a new buffer, size, subsurface.
    Committed,
}

/// See the module documentation. Every method gets the compositor; the core holds no borrow of
/// the hook while it runs.
pub trait WindowHook {
    /// A window opens now. `Some(size)` keeps it out of the workspaces and asks it to take that
    /// size; `None` lets it join the layout as usual.
    fn hold_new_window(&self, wirbel: &Wirbel, window: &Window) -> Option<Size<i32, Logical>>;

    fn event(&self, wirbel: &mut Wirbel, window: &Window, event: WindowEvent);

    /// The windows the hook shows on `output`. They get frame callbacks after each frame of that
    /// output, like the windows the layout shows there.
    fn shown_windows(&self, wirbel: &Wirbel, output: &Output) -> Vec<Window>;
}

impl Wirbel {
    /// Sets the hook, or takes it away with `None`. Without a hook the held windows join the
    /// active workspace.
    pub fn set_window_hook(&mut self, hook: Option<Rc<dyn WindowHook>>) {
        let gone = hook.is_none();
        self.window_hook = hook;
        if gone {
            self.release_held_windows();
        }
    }

    /// Tells the hook, if there is one.
    pub fn window_event(&mut self, window: &Window, event: WindowEvent) {
        if let Some(hook) = self.window_hook.clone() {
            hook.event(self, window, event);
        }
    }

    /// Whether `window` is kept out of the workspaces.
    pub fn is_held(&self, window: &Window) -> bool {
        self.held_windows.contains(window)
    }

    /// Part of placing a new window: keeps it out of the workspaces if the hook wants that, and
    /// asks it for the size the hook names. Returns whether it was kept.
    pub(crate) fn hold_new_window(&mut self, window: &Window) -> bool {
        let Some(hook) = self.window_hook.clone() else {
            return false;
        };
        let Some(size) = hook.hold_new_window(self, window) else {
            return false;
        };
        if let Some(toplevel) = window.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            toplevel.with_pending_state(|state| {
                state.size = Some(size);
                state.fullscreen_output = None;
                state.states.unset(State::Fullscreen);
                state.states.unset(State::Maximized);
            });
        }
        if let Some(surface) = window.x11_surface() {
            let loc = surface.last_configure().loc;
            let result = surface
                .set_fullscreen(false)
                .and_then(|()| surface.set_maximized(false));
            if let Err(err) = result {
                tracing::debug!(%err, "failed to update a held X11 window");
            }
            if let Err(err) = surface.configure(Rectangle::new(loc, size)) {
                tracing::debug!(%err, "failed to configure a held X11 window");
            }
        }
        if !self.held_windows.contains(window) {
            self.held_windows.push(window.clone());
        }
        if !self.awaiting_buffer.contains(window) {
            self.awaiting_buffer.push(window.clone());
        }
        true
    }

    /// The windows that get frame callbacks on `output`: those of the layout there and those the
    /// hook shows there.
    pub(crate) fn frame_windows(&self, output: &Output) -> Vec<Window> {
        let mut windows: Vec<Window> = self.space.elements_for_output(output).cloned().collect();
        if let Some(hook) = &self.window_hook {
            for window in hook.shown_windows(self, output) {
                if !windows.contains(&window) {
                    windows.push(window);
                }
            }
        }
        windows
    }

    /// The root surfaces of what the hook shows on `output`: its windows and their popups.
    pub(crate) fn hook_surfaces(&self, output: &Output) -> Vec<WlSurface> {
        let Some(hook) = &self.window_hook else {
            return Vec::new();
        };
        let mut roots = Vec::new();
        for window in hook.shown_windows(self, output) {
            let Some(root) = window.wl_surface() else {
                continue;
            };
            let popups = PopupManager::popups_for_surface(&root);
            roots.extend(popups.map(|(popup, _)| popup.wl_surface().clone()));
            roots.push(root.into_owned());
        }
        roots
    }

    /// Does for the windows the hook shows what `Space::refresh` does for the layout's: every
    /// surface of their trees and popups, also one that came since, enters the output it is
    /// shown on and learns its scale; a window shown there no longer leaves it. Clients go by
    /// these events (scale, whether they are visible at all).
    pub(crate) fn refresh_hook_outputs(&mut self) {
        let mut shown: Vec<(Window, Output)> = Vec::new();
        if let Some(hook) = self.window_hook.clone() {
            for monitor in self.monitors.iter().filter(|monitor| monitor.connected) {
                for window in hook.shown_windows(self, &monitor.output) {
                    shown.push((window, monitor.output.clone()));
                }
            }
        }
        for (window, output) in &self.hook_outputs {
            let still = shown.iter().any(|(w, o)| w == window && o == output);
            // Back in the layout on that output, the space keeps its outputs.
            let in_space = self.space.outputs_for_element(window).contains(output);
            if !still && !in_space && window.alive() {
                window.output_leave(output);
            }
        }
        // The hook does not say where on the output a window is: all of it is on it.
        let everywhere = Rectangle::new(
            (i32::MIN / 2, i32::MIN / 2).into(),
            (i32::MAX, i32::MAX).into(),
        );
        for (window, output) in &shown {
            window.output_enter(output, everywhere);
            let Some(root) = window.wl_surface() else {
                continue;
            };
            let popups = PopupManager::popups_for_surface(&root);
            let popups = popups.map(|(popup, _)| popup.wl_surface().clone());
            for tree in popups.chain([root.into_owned()]) {
                with_surfaces_surface_tree(&tree, |surface, states| {
                    apply_output_scale(surface, states, output);
                });
            }
        }
        self.hook_outputs = shown;
    }

    /// Whether the hook shows `window` on some output.
    pub(crate) fn hook_shows(&self, window: &Window) -> bool {
        self.window_hook.as_ref().is_some_and(|hook| {
            self.monitors
                .iter()
                .any(|monitor| hook.shown_windows(self, &monitor.output).contains(window))
        })
    }

    /// Puts the held windows on the active workspace, tiled or floating as they would be when
    /// opened there. They do not take the keyboard: the user did not ask for them in 2D.
    fn release_held_windows(&mut self) {
        let held = std::mem::take(&mut self.held_windows);
        if held.is_empty() {
            return;
        }
        let Some(monitor) = self.monitors.get_mut(self.active_monitor) else {
            // No output to put them on: they stay held until there is one and a hook comes and
            // goes again. They still are known to the core.
            self.held_windows = held;
            return;
        };
        let workspace = monitor.workspace_mut();
        for window in &held {
            let placement = if crate::state::wants_to_float_window(window) {
                Placement::Floating(crate::layout::Rect::new(0, 0, 0, 0))
            } else {
                Placement::Tiled
            };
            workspace.put(window.clone(), placement);
        }
        self.retile();
        // A floating one gets its place in the middle from its size, as on a first commit.
        for window in &held {
            if window.wl_surface().is_some() {
                self.floating_commit(window);
            }
        }
    }
}
