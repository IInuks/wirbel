//! Parked windows: windows that were already open on the desktop and are taken out of sight.
//!
//! A program cannot be moved into a shadow once it runs, so a parked window stays a window of
//! the visible compositor: off every workspace, out of the taskbar, still drawing. It can be
//! photographed, closed and put back. It has no keyboard or pointer of its own: input belongs
//! to the one seat of the desktop, so the commands that type and click work on windows that
//! were started invisibly, not on parked ones.
//!
//! Requests of the visible compositor:
//!
//! - `vwm adopt WINDOW|focused [NAME]`: parks a window, by its id from `windows`. Answers
//!   `ok NAME`.
//! - `vwm release NAME`: puts it on the active workspace again, as if it had just opened.
//! - `vwm shot` TAB NAME TAB PATH TAB X,Y,W,H|-: a picture of it, see `super::shot`.
//! - `vwm close NAME` asks it to close, `vwm list` shows it with the kind `parked`.

use std::time::Instant;

use smithay::{desktop::Window, utils::IsAlive};

use super::{shot::Shot, valid_name};
use crate::{
    Wirbel, handlers::window_names, layout::Rect, plugin_api::window_id, workspace::Placement,
};

pub struct Parked {
    pub name: String,
    pub window: Window,
    pub since: Instant,
}

impl Wirbel {
    /// Whether `window` is parked.
    pub fn is_parked(&self, window: &Window) -> bool {
        self.vwm
            .parked
            .iter()
            .any(|parked| parked.window == *window)
    }

    /// The parked windows that still exist.
    pub(crate) fn parked_windows(&self) -> Vec<Window> {
        let alive = self
            .vwm
            .parked
            .iter()
            .filter(|parked| parked.window.alive());
        alive.map(|parked| parked.window.clone()).collect()
    }

    fn free_parked_name(&self, base: &str) -> String {
        let base: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(24)
            .collect();
        let base = if base.is_empty() { "window" } else { &base };
        let taken = |name: &str| {
            self.vwm.find(name).is_some()
                || self.vwm.parked.iter().any(|parked| parked.name == name)
        };
        (1..)
            .map(|n| format!("{base}{n}"))
            .find(|name| !taken(name))
            .unwrap_or_default()
    }

    /// Takes a window of the layout out of sight. Returns its name.
    pub fn vwm_adopt(&mut self, window: &Window, name: Option<&str>) -> Result<String, String> {
        if self.is_parked(window) {
            return Err("that window is parked already".into());
        }
        if self.locate(window).is_none() {
            return Err("only a window on a workspace can be parked".into());
        }
        let name = match name {
            Some(name) if !valid_name(name) => {
                return Err(format!(
                    "`{name}` cannot be a name: letters, digits, - and _"
                ));
            }
            Some(name)
                if self.vwm.find(name).is_some()
                    || self.vwm.parked.iter().any(|p| p.name == name) =>
            {
                return Err(format!("there is an invisible window `{name}` already"));
            }
            Some(name) => name.to_string(),
            None => self.free_parked_name(&window_names(window).1),
        };
        // Out of the layout as if it had closed, but it stays known as a held window.
        let next = self.workspace_of_mut(window).and_then(|workspace| {
            workspace.take(window);
            workspace.focus_candidate()
        });
        self.space.unmap_elem(window);
        self.urgent.retain(|other| other != window);
        if self.focused.as_ref() == Some(window) {
            self.focus_window(next, true);
            self.update_keyboard_focus();
        }
        if !self.held_windows.contains(window) {
            self.held_windows.push(window.clone());
        }
        self.vwm.parked.push(Parked {
            name: name.clone(),
            window: window.clone(),
            since: Instant::now(),
        });
        self.retile();
        self.needs_redraw = true;
        Ok(name)
    }

    /// Puts a parked window on the active workspace again.
    pub fn vwm_release(&mut self, name: &str) -> Result<(), String> {
        let index = self
            .vwm
            .parked
            .iter()
            .position(|parked| parked.name == name);
        let index = index.ok_or_else(|| format!("no parked window `{name}`"))?;
        let window = self.vwm.parked.remove(index).window;
        self.held_windows.retain(|other| *other != window);
        if !window.alive() {
            return Ok(());
        }
        let monitor = self
            .monitors
            .get_mut(self.active_monitor)
            .ok_or("no output")?;
        let placement = match crate::state::wants_to_float_window(&window) {
            true => Placement::Floating(Rect::new(0, 0, 0, 0)),
            false => Placement::Tiled,
        };
        monitor.workspace_mut().put(window.clone(), placement);
        self.retile();
        if window.wl_surface().is_some() {
            self.floating_commit(&window);
        }
        // Whoever brings a window back wants to use it.
        self.focus_window(Some(window), true);
        self.update_keyboard_focus();
        self.needs_redraw = true;
        Ok(())
    }

    /// Forgets parked windows that are gone.
    pub(crate) fn reap_parked(&mut self) {
        self.vwm.parked.retain(|parked| parked.window.alive());
    }

    /// The lines of `vwm list` for the parked windows.
    pub(crate) fn parked_list(&self) -> String {
        let mut answer = String::new();
        for parked in self
            .vwm
            .parked
            .iter()
            .filter(|parked| parked.window.alive())
        {
            let size = parked.window.geometry().size;
            let (title, _) = window_names(&parked.window);
            answer += &format!(
                "entry\t{}\tparked\tready\t{}\t-\t{}x{}\t{}\t{}\n",
                parked.name,
                window_id(&parked.window),
                size.w,
                size.h,
                parked.since.elapsed().as_secs(),
                title.replace(['\t', '\n'], " "),
            );
        }
        answer
    }

    fn parked(&self, name: &str) -> Option<&Parked> {
        let found = self.vwm.parked.iter().find(|parked| parked.name == name);
        found.filter(|parked| parked.window.alive())
    }
}

use smithay::wayland::seat::WaylandFocus;

/// The `vwm` requests about parked windows. `None` if `rest` is none of them, or names no
/// parked window where a shadow may be meant.
pub(crate) fn request(
    state: &mut Wirbel,
    peer: u64,
    rest: &str,
) -> Option<Result<Option<String>, String>> {
    state.reap_parked();
    if let Some(shot) = rest.strip_prefix("shot\t") {
        let (name, shot) = shot.split_once('\t')?;
        let Some(parked) = state.parked(name) else {
            return Some(Err(format!("no parked window `{name}`")));
        };
        let window = parked.window.clone();
        return Some(Shot::parse(peer, shot).map(|shot| {
            state.vwm.parked_shots.push((shot, window));
            state.needs_redraw = true;
            (state.redraw)();
            None
        }));
    }
    let mut words = rest.split_ascii_whitespace();
    let answer = match (words.next()?, words.next(), words.next(), words.next()) {
        ("adopt", Some(target), name, None) => {
            let window = match target {
                "focused" => state
                    .focused
                    .clone()
                    .ok_or("no window has the focus".to_string()),
                id => {
                    let id: u64 = match id.parse() {
                        Ok(id) => id,
                        Err(_) => return Some(Err("expected a window id or `focused`".into())),
                    };
                    let mut windows = state.listed_windows().into_iter();
                    windows
                        .find(|window| window_id(window) == id)
                        .ok_or(format!("no window {id}"))
                }
            };
            window
                .and_then(|window| state.vwm_adopt(&window, name))
                .map(|name| format!("ok {name}"))
        }
        ("release", Some(name), None, None) => state.vwm_release(name).map(|()| "ok".into()),
        ("close", Some(name), _, None) if state.parked(name).is_some() => {
            let window = state.parked(name).map(|parked| parked.window.clone());
            if let Some(window) = window {
                state.close_window(&window);
            }
            Ok("ok".into())
        }
        ("where", Some(name), None, None) if state.parked(name).is_some() => Err(format!(
            "`{name}` is a parked window: it takes shot, release and close, typing and clicking need a window started with `wirbel vwm open`"
        )),
        _ => return None,
    };
    Some(answer.map(Some))
}
