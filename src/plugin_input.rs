//! What plugins need for mouse gestures and the like: a button they can take for themselves, lines
//! on the screen, and a way to act on the window at a place. What a gesture means is up to the
//! plugin, wirbel only passes the mouse on and draws.
//!
//! - `grab MODIFIERS BUTTON`, `ungrab MODIFIERS BUTTON`: a press of BUTTON with exactly MODIFIERS
//!   held, like `ctrl+alt btn_left`, no longer reaches a client. It starts a session that lasts
//!   until one of the modifiers goes up. During it the button may go up and down as often as it
//!   likes, every press and release stays with wirbel, and the grabbing connection gets
//!   `event grab press X Y`, `event grab motion X Y`, `event grab release X Y` and at the end
//!   `event grab end X Y`, or `event grab cancel` if Escape, a lock or leaving the VT ended it.
//!   Escape during a session does not reach a client either. Positions are global.
//! - `line X1 Y1 X2 Y2 WIDTH COLOR` draws a line in front of everything but the cursor.
//!   `clear [MS [COLOR]]` takes back all lines of this connection, fading out over MS
//!   milliseconds, in COLOR if given. Lines last as long as the connection.
//! - `window_at X Y` answers the id of the window there, or `none`. `focus ID` gives it the
//!   focus, `close ID|focused` asks it to close.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform},
};

use crate::{Wirbel, config::Modifiers};

/// A button a connection took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grab {
    pub peer: u64,
    pub modifiers: Modifiers,
    pub button: u32,
}

/// A grab that is in use right now.
#[derive(Debug, Clone, Copy)]
struct Session {
    grab: Grab,
    button_down: bool,
}

/// Edge of a tile in logical pixels.
const TILE: i32 = 256;

/// The lines of one connection, painted into tiles that are created where they are needed. One
/// render element per tile and only the newly painted part uploaded, however long the drawing gets.
#[derive(Debug, Default)]
struct Drawing {
    tiles: HashMap<(i32, i32), MemoryRenderBuffer>,
    /// Fading out since then, over that long.
    fading: Option<(Instant, Duration)>,
}

#[derive(Debug, Default)]
pub struct PluginInput {
    grabs: Vec<Grab>,
    session: Option<Session>,
    /// A button that went down in a session that is over, so its release is not a client's.
    swallow_release: Option<u32>,
    drawings: HashMap<u64, Drawing>,
    fade_timer: bool,
}

impl PluginInput {
    pub fn in_session(&self) -> bool {
        self.session.is_some()
    }

    /// Whether any connection holds a grab or has lines on the screen.
    pub fn in_use(&self) -> bool {
        !self.grabs.is_empty() || !self.drawings.is_empty()
    }
}

fn held_all(wanted: Modifiers, held: Modifiers) -> bool {
    (!wanted.ctrl || held.ctrl)
        && (!wanted.alt || held.alt)
        && (!wanted.shift || held.shift)
        && (!wanted.logo || held.logo)
        && (!wanted.altgr || held.altgr)
}

fn at(location: Point<f64, Logical>) -> String {
    format!("{:.1} {:.1}", location.x, location.y)
}

impl Wirbel {
    fn held_modifiers(&self) -> Option<Modifiers> {
        let keyboard = self.seat.get_keyboard()?;
        Some((&keyboard.modifier_state()).into())
    }

    fn pointer_location(&self) -> Point<f64, Logical> {
        let pointer = self.seat.get_pointer();
        pointer.map(|p| p.current_location()).unwrap_or_default()
    }

    pub(crate) fn add_grab(&mut self, grab: Grab) -> Result<(), String> {
        let same = |other: &Grab| other.modifiers == grab.modifiers && other.button == grab.button;
        match self.plugin_input.grabs.iter().find(|other| same(other)) {
            Some(other) if other.peer != grab.peer => Err("another plugin has that grab".into()),
            Some(_) => Ok(()),
            None => {
                self.plugin_input.grabs.push(grab);
                Ok(())
            }
        }
    }

    pub(crate) fn remove_grab(&mut self, grab: Grab) -> Result<(), String> {
        let before = self.plugin_input.grabs.len();
        self.plugin_input.grabs.retain(|other| *other != grab);
        if self.plugin_input.grabs.len() == before {
            return Err("no such grab".into());
        }
        if self.plugin_input.session.is_some_and(|s| s.grab == grab) {
            self.end_session(false);
        }
        Ok(())
    }

    /// Called for every button event before anything else sees it. Whether a grab took it.
    pub(crate) fn grab_button(&mut self, button: u32, pressed: bool) -> bool {
        let location = self.pointer_location();
        if let Some(session) = &mut self.plugin_input.session
            && session.grab.button == button
        {
            if session.button_down == pressed {
                return true;
            }
            session.button_down = pressed;
            let (peer, what) = (session.grab.peer, if pressed { "press" } else { "release" });
            self.ipc
                .reply(peer, &format!("event grab {what} {}", at(location)));
            return true;
        }
        if !pressed {
            if self.plugin_input.swallow_release == Some(button) {
                self.plugin_input.swallow_release = None;
                return true;
            }
            return false;
        }
        if self.plugin_input.session.is_some() || self.lock.is_active() || self.drag.is_some() {
            return false;
        }
        if self.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
            return false;
        }
        let Some(held) = self.held_modifiers() else {
            return false;
        };
        let grabs = self.plugin_input.grabs.iter();
        let Some(grab) = grabs
            .into_iter()
            .find(|grab| grab.button == button && grab.modifiers == held)
            .copied()
        else {
            return false;
        };
        self.plugin_input.session = Some(Session {
            grab,
            button_down: true,
        });
        let line = format!("event grab press {}", at(location));
        self.ipc.reply(grab.peer, &line);
        true
    }

    /// The pointer went to `location`. Whether a session is going on, which keeps the focus where
    /// it is.
    pub(crate) fn grab_motion(&mut self, location: Point<f64, Logical>) -> bool {
        let Some(session) = self.plugin_input.session else {
            return false;
        };
        let line = format!("event grab motion {}", at(location));
        self.ipc.reply(session.grab.peer, &line);
        true
    }

    /// After every key: once a modifier of the session goes up, the session is over.
    pub(crate) fn grab_check_modifiers(&mut self) {
        let Some(session) = self.plugin_input.session else {
            return;
        };
        let held = self.held_modifiers().unwrap_or_default();
        if !held_all(session.grab.modifiers, held) {
            self.end_session(true);
        }
    }

    /// Ends the session, with `event grab end` if it ended as it should or else `cancel`.
    pub fn end_session(&mut self, complete: bool) {
        let Some(session) = self.plugin_input.session.take() else {
            return;
        };
        if session.button_down {
            self.plugin_input.swallow_release = Some(session.grab.button);
        }
        let line = if complete {
            format!("event grab end {}", at(self.pointer_location()))
        } else {
            "event grab cancel".into()
        };
        self.ipc.reply(session.grab.peer, &line);
    }

    pub(crate) fn add_line(
        &mut self,
        peer: u64,
        from: Point<f64, Logical>,
        to: Point<f64, Logical>,
        width: i32,
        color: [f32; 4],
    ) {
        let drawing = self.plugin_input.drawings.entry(peer).or_default();
        if drawing.fading.is_some() {
            // New lines start a new drawing.
            *drawing = Drawing::default();
        }
        drawing.paint_line(from, to, f64::from(width.max(1)), color);
        self.needs_redraw = true;
    }

    pub(crate) fn clear_lines(&mut self, peer: u64, fade: Duration, color: Option<[f32; 4]>) {
        if fade.is_zero() {
            if self.plugin_input.drawings.remove(&peer).is_some() {
                self.needs_redraw = true;
            }
            return;
        }
        let Some(drawing) = self.plugin_input.drawings.get_mut(&peer) else {
            return;
        };
        if let Some(color) = color {
            drawing.recolor(color);
        }
        drawing.fading = Some((Instant::now(), fade));
        self.needs_redraw = true;
        if std::mem::replace(&mut self.plugin_input.fade_timer, true) {
            return; // runs already
        }
        let tick = Duration::from_millis(16);
        let inserted =
            self.loop_handle
                .insert_source(Timer::from_duration(tick), move |_, _, state| {
                    state.needs_redraw = true;
                    let drawings = &mut state.plugin_input.drawings;
                    drawings.retain(|_, drawing| {
                        drawing
                            .fading
                            .is_none_or(|(since, fade)| since.elapsed() < fade)
                    });
                    if drawings.values().any(|drawing| drawing.fading.is_some()) {
                        TimeoutAction::ToDuration(tick)
                    } else {
                        state.plugin_input.fade_timer = false;
                        TimeoutAction::Drop
                    }
                });
        if inserted.is_err() {
            self.plugin_input.fade_timer = false;
            self.plugin_input.drawings.remove(&peer);
        }
    }

    /// A connection went away: its grabs, session and lines go with it.
    pub(crate) fn forget_plugin_input(&mut self, peer: u64) {
        self.plugin_input.grabs.retain(|grab| grab.peer != peer);
        if self
            .plugin_input
            .session
            .is_some_and(|s| s.grab.peer == peer)
        {
            self.end_session(false);
        }
        if self.plugin_input.drawings.remove(&peer).is_some() {
            self.needs_redraw = true;
        }
    }

    /// The lines of all plugins, for the renderer.
    pub fn line_elements<R>(
        &self,
        renderer: &mut R,
        origin: Point<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let mut elements = Vec::new();
        for drawing in self.plugin_input.drawings.values() {
            let alpha = match drawing.fading {
                Some((since, fade)) => 1.0 - since.elapsed().as_secs_f32() / fade.as_secs_f32(),
                None => 1.0,
            };
            if alpha <= 0.0 {
                continue;
            }
            for (&(tx, ty), buffer) in &drawing.tiles {
                let corner = Point::<i32, Logical>::from((tx * TILE, ty * TILE)) - origin;
                let location: Point<i32, Physical> = corner.to_physical_precise_round(scale);
                let element = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    buffer,
                    Some(alpha),
                    None,
                    None,
                    Kind::Unspecified,
                );
                match element {
                    Ok(element) => elements.push(element),
                    Err(err) => tracing::warn!(?err, "failed to upload plugin lines"),
                }
            }
        }
        elements
    }
}

impl Drawing {
    /// A round-capped line, its edge smoothed over a pixel.
    fn paint_line(
        &mut self,
        from: Point<f64, Logical>,
        to: Point<f64, Logical>,
        width: f64,
        color: [f32; 4],
    ) {
        let radius = width / 2.0;
        let (ax, ay, bx, by) = (from.x, from.y, to.x, to.y);
        let (dx, dy) = (bx - ax, by - ay);
        let length2 = dx * dx + dy * dy;
        let left = (ax.min(bx) - radius - 1.0).floor() as i32;
        let top = (ay.min(by) - radius - 1.0).floor() as i32;
        let right = (ax.max(bx) + radius + 1.0).ceil() as i32;
        let bottom = (ay.max(by) + radius + 1.0).ceil() as i32;
        for ty in top.div_euclid(TILE)..=(bottom - 1).div_euclid(TILE) {
            for tx in left.div_euclid(TILE)..=(right - 1).div_euclid(TILE) {
                let (x0, y0) = (tx * TILE, ty * TILE);
                let area = Rectangle::<i32, Buffer>::new(
                    (left.max(x0) - x0, top.max(y0) - y0).into(),
                    (right.min(x0 + TILE) - left.max(x0), bottom.min(y0 + TILE) - top.max(y0))
                        .into(),
                );
                if area.size.w <= 0 || area.size.h <= 0 {
                    continue;
                }
                let buffer = self.tiles.entry((tx, ty)).or_insert_with(|| {
                    MemoryRenderBuffer::new(
                        Fourcc::Argb8888,
                        (TILE, TILE),
                        1,
                        Transform::Normal,
                        None,
                    )
                });
                let _ = buffer.render().draw(|mem| {
                    for py in area.loc.y..area.loc.y + area.size.h {
                        for px in area.loc.x..area.loc.x + area.size.w {
                            // Distance of the pixel centre to the segment.
                            let (cx, cy) = (f64::from(x0 + px) + 0.5, f64::from(y0 + py) + 0.5);
                            let t = if length2 > 0.0 {
                                (((cx - ax) * dx + (cy - ay) * dy) / length2).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            let distance = (cx - ax - t * dx).hypot(cy - ay - t * dy);
                            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0) as f32;
                            if coverage <= 0.0 {
                                continue;
                            }
                            let index = ((py * TILE + px) * 4) as usize;
                            let new = (color[3] * coverage * 255.0).round() as u8;
                            if new <= mem[index + 3] {
                                continue;
                            }
                            // Little endian ARGB is B G R A in memory, premultiplied.
                            mem[index] = (color[2] * coverage * 255.0).round() as u8;
                            mem[index + 1] = (color[1] * coverage * 255.0).round() as u8;
                            mem[index + 2] = (color[0] * coverage * 255.0).round() as u8;
                            mem[index + 3] = new;
                        }
                    }
                    Ok::<_, ()>(vec![area])
                });
            }
        }
    }

    /// Everything painted so far in another colour, keeping its shape.
    fn recolor(&mut self, color: [f32; 4]) {
        let whole = Rectangle::<i32, Buffer>::from_size((TILE, TILE).into());
        for buffer in self.tiles.values_mut() {
            let _ = buffer.render().draw(|mem| {
                for pixel in mem.chunks_exact_mut(4) {
                    if pixel[3] == 0 {
                        continue;
                    }
                    // Line colours are opaque, so alpha is how much of the pixel the line covers.
                    let coverage = f32::from(pixel[3]) / 255.0;
                    pixel[0] = (color[2] * coverage * 255.0).round() as u8;
                    pixel[1] = (color[1] * coverage * 255.0).round() as u8;
                    pixel[2] = (color[0] * coverage * 255.0).round() as u8;
                    pixel[3] = (color[3] * coverage * 255.0).round() as u8;
                }
                Ok::<_, ()>(vec![whole])
            });
        }
    }
}
