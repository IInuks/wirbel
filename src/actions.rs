//! What binds do.

use smithay::desktop::Window;

use crate::{
    Wirbel,
    config::{Action, WORKSPACES, WorkspaceTarget},
    layout::{Direction, Rect, find_in_direction},
    workspace::{FullKind, Workspace},
};

impl Wirbel {
    pub fn run_action(&mut self, action: Action) {
        match action {
            Action::Spawn(command) => self.spawn(&command),
            Action::Close => {
                if let Some(toplevel) = self.focused.as_ref().and_then(Window::toplevel) {
                    toplevel.send_close();
                }
            }
            Action::Quit => self.loop_signal.stop(),
            Action::Reload => self.reload_config(),
            Action::Focus(direction) => self.focus_direction(direction),
            Action::FocusNext => self.focus_cycle(1),
            Action::FocusPrevious => self.focus_cycle(-1),
            Action::Swap(direction) => self.swap_direction(direction),
            Action::SplitRatio(delta) => {
                if let Some(window) = self.focused.clone()
                    && let Some(workspace) = self.workspace_of_mut(&window)
                    && workspace.tiled.grow(&window, delta)
                {
                    self.retile();
                }
            }
            Action::Float => {
                if let Some(window) = self.focused.clone() {
                    self.toggle_floating(&window);
                }
            }
            Action::Fullscreen => self.toggle_full(FullKind::Fullscreen),
            Action::Maximize => self.toggle_full(FullKind::Maximized),
            Action::Workspace(target) => {
                if let Some(index) = self.workspace_index(target) {
                    self.show_workspace(index);
                }
            }
            Action::MoveToWorkspace(index) => self.move_to_workspace(index),
            Action::FocusOutput(direction) => self.focus_output(direction),
            Action::MoveToOutput(direction) => self.move_to_output(direction),
        }
    }

    fn toggle_full(&mut self, kind: FullKind) {
        if let Some(window) = self.focused.clone() {
            let new = (self.full_kind(&window) != Some(kind)).then_some(kind);
            self.set_full(&window, new);
        }
    }

    /// Moves the pointer into the focused window, unless it already is inside.
    fn warp_to_focus(&mut self) {
        if !self.config.warp_cursor {
            return;
        }
        let frame = self
            .frames
            .iter()
            .find(|frame| Some(&frame.window) == self.focused.as_ref());
        let Some(rect) = frame.map(|frame| frame.outer) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = pointer.current_location();
        if !rect.contains(location.x, location.y) {
            let (x, y) = rect.center();
            self.warp_pointer((f64::from(x), f64::from(y)).into());
        }
    }

    /// The visible windows of the active workspace, apart from the focused one.
    fn other_frames(&self) -> Vec<(&Window, Rect, bool)> {
        let Some(workspace) = self.active_workspace() else {
            return Vec::new();
        };
        self.frames
            .iter()
            .filter(|frame| workspace.contains(&frame.window))
            .filter(|frame| Some(&frame.window) != self.focused.as_ref())
            .map(|frame| (&frame.window, frame.outer, frame.tiled))
            .collect()
    }

    fn focus_direction(&mut self, direction: Direction) {
        let Some(from) = self.focused_frame() else {
            // Nothing to start from, so anything on the workspace will do.
            let candidate = self.active_workspace().and_then(Workspace::focus_candidate);
            self.set_focus(candidate);
            return;
        };
        let candidates = self.other_frames();
        let candidates = candidates
            .into_iter()
            .map(|(window, rect, _)| (window, rect));
        let target = find_in_direction(from, candidates, direction).cloned();
        if target.is_some() {
            self.set_focus(target);
            self.warp_to_focus();
        }
    }

    fn focused_frame(&self) -> Option<Rect> {
        let window = self.focused.as_ref()?;
        let frames = self.frames.iter();
        frames
            .into_iter()
            .find(|frame| frame.window == *window)
            .map(|frame| frame.outer)
    }

    fn focus_cycle(&mut self, step: isize) {
        let Some(windows) = self.active_workspace().map(Workspace::windows) else {
            return;
        };
        if windows.is_empty() {
            return;
        }
        let current = self
            .focused
            .as_ref()
            .and_then(|focused| windows.iter().position(|window| window == focused));
        let next = match current {
            Some(index) => (index as isize + step).rem_euclid(windows.len() as isize) as usize,
            None => 0,
        };
        self.set_focus(Some(windows[next].clone()));
        self.warp_to_focus();
    }

    fn swap_direction(&mut self, direction: Direction) {
        let Some(window) = self.focused.clone() else {
            return;
        };
        let Some(from) = self.focused_frame() else {
            return;
        };
        let candidates = self.other_frames();
        let tiles = candidates.into_iter().filter(|(_, _, tiled)| *tiled);
        let tiles = tiles.map(|(window, rect, _)| (window, rect));
        let Some(target) = find_in_direction(from, tiles, direction).cloned() else {
            return;
        };
        if let Some(workspace) = self.workspace_of_mut(&window)
            && workspace.tiled.swap(&window, &target)
        {
            self.retile();
            self.warp_to_focus();
        }
    }

    fn workspace_index(&self, target: WorkspaceTarget) -> Option<usize> {
        let active = self.monitors.get(self.active_monitor)?.active;
        Some(match target {
            WorkspaceTarget::Index(index) => index,
            WorkspaceTarget::Previous => (active + WORKSPACES - 1) % WORKSPACES,
            WorkspaceTarget::Next => (active + 1) % WORKSPACES,
        })
    }

    pub fn show_workspace(&mut self, index: usize) {
        let Some(monitor) = self.monitors.get_mut(self.active_monitor) else {
            return;
        };
        if monitor.active == index {
            return;
        }
        monitor.active = index;
        let candidate = monitor.workspace().focus_candidate();
        self.retile();
        self.set_focus(candidate);
    }

    fn move_to_workspace(&mut self, index: usize) {
        let Some(window) = self.focused.clone() else {
            return;
        };
        let Some((m, w)) = self.locate(&window) else {
            return;
        };
        if w == index {
            return;
        }
        let monitor = &mut self.monitors[m];
        let Some(placement) = monitor.workspaces[w].take(&window) else {
            return;
        };
        monitor.workspaces[index].put(window.clone(), placement);
        monitor.workspaces[index].focused = Some(window);
        self.show_workspace(index);
    }

    /// The monitor in `direction` of the active one.
    fn monitor_in_direction(&self, direction: Direction) -> Option<usize> {
        let from = self.monitor_area(self.active_monitor)?;
        let others = (0..self.monitors.len())
            .filter(|index| *index != self.active_monitor)
            .filter_map(|index| Some((index, self.monitor_area(index)?)));
        find_in_direction(from, others, direction)
    }

    fn focus_output(&mut self, direction: Direction) {
        let Some(target) = self.monitor_in_direction(direction) else {
            return;
        };
        self.active_monitor = target;
        let candidate = self.monitors[target].workspace().focus_candidate();
        let window = candidate.is_some();
        self.set_focus(candidate);
        if window {
            self.warp_to_focus();
        } else if let Some(area) = self.monitor_area(target) {
            // An empty output has nothing to warp to, but work happens where the pointer is.
            let (x, y) = area.center();
            self.warp_pointer((f64::from(x), f64::from(y)).into());
        }
    }

    fn move_to_output(&mut self, direction: Direction) {
        let Some(window) = self.focused.clone() else {
            return;
        };
        let Some(target) = self.monitor_in_direction(direction) else {
            return;
        };
        // A floating window keeps its offset to the output.
        if let (Some(from), Some(to)) = (
            self.monitor_area(self.active_monitor),
            self.monitor_area(target),
        ) && let Some(entry) = self
            .workspace_of_mut(&window)
            .and_then(|workspace| workspace.floating_entry_mut(&window))
        {
            let moved = Rect::new(
                entry.rect.x - from.x + to.x,
                entry.rect.y - from.y + to.y,
                entry.rect.w,
                entry.rect.h,
            );
            entry.rect = moved.clamped_into(to);
        }
        self.send_to_monitor(&window, target, None);
        self.set_focus(Some(window));
        self.warp_to_focus();
    }

    /// Moves `window` to the shown workspace of another monitor, at the same global position if
    /// it floats. A tile lands next to `beside`, or else next to what is focused there.
    pub(crate) fn send_to_monitor(
        &mut self,
        window: &Window,
        target: usize,
        beside: Option<Window>,
    ) {
        let Some((m, w)) = self.locate(window) else {
            return;
        };
        let Some(placement) = self.monitors[m].workspaces[w].take(window) else {
            return;
        };
        let workspace = self.monitors[target].workspace_mut();
        if beside.is_some() {
            workspace.focused = beside;
        }
        workspace.put(window.clone(), placement);
        self.active_monitor = target;
        self.retile();
    }
}
