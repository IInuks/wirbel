//! Dragging windows with the pointer.
//!
//! The grab only keeps clients out of the way. The drag itself is plain state in `Wirbel`, moved
//! along by `on_pointer_motion` and `on_pointer_button`: code running inside a grab must not touch
//! the pointer, and nearly everything a drag causes ends up doing so through `retile`.

use smithay::{
    desktop::Window,
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Serial},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

use crate::{
    Wirbel, config::MouseAction, layout::Rect, state::MIN_FLOATING_SIZE, workspace::Placement,
};

/// The edges a resize moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl From<xdg_toplevel::ResizeEdge> for Edges {
    fn from(edge: xdg_toplevel::ResizeEdge) -> Self {
        use xdg_toplevel::ResizeEdge as Edge;
        Self {
            left: matches!(edge, Edge::Left | Edge::TopLeft | Edge::BottomLeft),
            right: matches!(edge, Edge::Right | Edge::TopRight | Edge::BottomRight),
            top: matches!(edge, Edge::Top | Edge::TopLeft | Edge::TopRight),
            bottom: matches!(edge, Edge::Bottom | Edge::BottomLeft | Edge::BottomRight),
        }
    }
}

impl Edges {
    /// The corner of `rect` nearest to the given point.
    fn nearest_corner(rect: Rect, location: Point<f64, Logical>) -> Self {
        let (x, y) = rect.center();
        let right = location.x >= f64::from(x);
        let bottom = location.y >= f64::from(y);
        Self {
            left: !right,
            right,
            top: !bottom,
            bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize(Edges),
}

/// A window drag in progress.
#[derive(Debug, Clone)]
pub struct Drag {
    pub window: Window,
    pub kind: DragKind,
    /// The pointer when the drag started.
    start: Point<f64, Logical>,
    /// The pointer at the last motion.
    last: Point<f64, Logical>,
    /// The floating window when the drag started.
    rect: Rect,
}

fn min_size(window: &Window) -> (i32, i32) {
    let min = window.toplevel().map(|toplevel| {
        with_states(toplevel.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            cached.current().min_size
        })
    });
    let min = min.unwrap_or_default();
    (min.w.max(MIN_FLOATING_SIZE), min.h.max(MIN_FLOATING_SIZE))
}

impl Wirbel {
    /// Starts dragging on behalf of a mouse bind. The press that triggered it goes to the grab.
    pub fn start_mouse_drag(
        &mut self,
        window: Window,
        action: MouseAction,
        button: u32,
        serial: Serial,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = pointer.current_location();
        let kind = match action {
            MouseAction::Move => DragKind::Move,
            MouseAction::Resize => {
                let content = self
                    .workspace_of_mut(&window)
                    .and_then(|workspace| workspace.floating_entry(&window))
                    .map(|entry| entry.content());
                DragKind::Resize(content.map_or(Edges::default(), |content| {
                    Edges::nearest_corner(content, location)
                }))
            }
        };
        let start = GrabStartData {
            focus: None,
            button,
            location,
        };
        self.start_drag(window, kind, start, serial);
    }

    /// Takes over a drag a client asked for, if `serial` belongs to a press on that client.
    pub fn start_client_drag(
        &mut self,
        surface: &WlSurface,
        seat: &Seat<Self>,
        serial: Serial,
        kind: DragKind,
    ) {
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        let Some(window) = self.find_window(surface) else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start) = pointer.grab_start_data() else {
            return;
        };
        let same_client = start.focus.as_ref().is_some_and(|(focus, _)| {
            use smithay::reexports::wayland_server::Resource;
            focus.id().same_client_as(&surface.id())
        });
        if same_client {
            self.start_drag(window, kind, start, serial);
        }
    }

    fn start_drag(
        &mut self,
        window: Window,
        kind: DragKind,
        start: GrabStartData<Self>,
        serial: Serial,
    ) {
        if self.drag.is_some() || self.full_kind(&window).is_some() {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = start.location;
        let rect = self
            .workspace_of_mut(&window)
            .and_then(|workspace| workspace.floating_entry_mut(&window))
            .map(|entry| {
                let rect = entry.content();
                entry.rect = rect;
                entry.anchor = (None, None);
                rect
            });
        if matches!(kind, DragKind::Resize(_)) {
            set_resizing(&window, true);
        }
        self.drag = Some(Drag {
            window,
            kind,
            start: location,
            last: location,
            rect: rect.unwrap_or(Rect::new(0, 0, 0, 0)),
        });
        pointer.set_grab(self, WindowGrab { start }, serial, Focus::Clear);
    }

    pub(crate) fn drag_motion(&mut self, location: Point<f64, Logical>) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let last = std::mem::replace(&mut drag.last, location);
        let drag = drag.clone();
        let Some((m, w)) = self.locate(&drag.window) else {
            return;
        };
        let (dx, dy) = (location.x - drag.start.x, location.y - drag.start.y);
        let (dx, dy) = (dx.round() as i32, dy.round() as i32);
        let area = self.work_area(m);
        let gaps = self.config.gaps;
        let workspace = &mut self.monitors[m].workspaces[w];

        if let Some(entry) = workspace.floating_entry_mut(&drag.window) {
            let start = drag.rect;
            match drag.kind {
                DragKind::Move => {
                    entry.rect.x = start.x + dx;
                    entry.rect.y = start.y + dy;
                }
                DragKind::Resize(edges) => {
                    let (min_w, min_h) = min_size(&drag.window);
                    let grow = |start: i32, near: bool, far: bool, delta: i32, min: i32| {
                        (start + i32::from(far) * delta - i32::from(near) * delta).max(min)
                    };
                    entry.rect.w = grow(start.w, edges.left, edges.right, dx, min_w);
                    entry.rect.h = grow(start.h, edges.top, edges.bottom, dy, min_h);
                    entry.anchor = (
                        edges.left.then_some(start.x + start.w),
                        edges.top.then_some(start.y + start.h),
                    );
                }
            }
        } else if matches!(drag.kind, DragKind::Resize(_))
            && let Some(area) = area
        {
            let delta = location - last;
            workspace
                .tiled
                .drag(&drag.window, area, gaps, delta.x, delta.y);
        } else {
            return;
        }
        self.retile();
    }

    pub(crate) fn finish_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        match drag.kind {
            DragKind::Resize(_) => set_resizing(&drag.window, false),
            DragKind::Move => self.drop_window(&drag.window, drag.last),
        }
    }

    /// A dragged window lands: on the output under the pointer, and a tile also takes the place
    /// of the tile it is dropped on.
    fn drop_window(&mut self, window: &Window, location: Point<f64, Logical>) {
        let Some((m, w)) = self.locate(window) else {
            return;
        };
        let workspace = &self.monitors[m].workspaces[w];
        let tiled = workspace.placement(window) == Some(Placement::Tiled);
        let target_monitor = if tiled {
            self.monitor_at(location)
        } else {
            workspace
                .floating_entry(window)
                .map(|entry| entry.content().center())
                .and_then(|(x, y)| self.monitor_at((f64::from(x), f64::from(y)).into()))
        };
        let target = tiled
            .then(|| {
                let frames = self.frames.iter().filter(|frame| frame.tiled);
                let mut frames = frames.filter(|frame| frame.window != *window);
                frames.find(|frame| frame.outer.contains(location.x, location.y))
            })
            .flatten()
            .map(|frame| frame.window.clone());

        let Some(target_monitor) = target_monitor else {
            return;
        };
        if target_monitor != m {
            self.send_to_monitor(window, target_monitor, target);
            self.set_focus(Some(window.clone()));
        } else if let Some(target) = target {
            self.monitors[m].workspaces[w].tiled.swap(window, &target);
            self.retile();
        }
    }
}

fn set_resizing(window: &Window, resizing: bool) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|state| {
            if resizing {
                state.states.set(xdg_toplevel::State::Resizing);
            } else {
                state.states.unset(xdg_toplevel::State::Resizing);
            }
        });
        toplevel.send_pending_configure();
    }
}

/// Holds the pointer away from clients until the last button goes up.
struct WindowGrab {
    start: GrabStartData<Wirbel>,
}

type Handle<'a, 'b> = &'a mut PointerInnerHandle<'b, Wirbel>;

impl PointerGrab<Wirbel> for WindowGrab {
    fn motion(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
    }

    fn relative_motion(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(&mut self, data: &mut Wirbel, handle: Handle<'_, '_>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut Wirbel, handle: Handle<'_, '_>, details: AxisFrame) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut Wirbel, handle: Handle<'_, '_>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Wirbel,
        handle: Handle<'_, '_>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<Wirbel> {
        &self.start
    }

    fn unset(&mut self, _data: &mut Wirbel) {}
}
