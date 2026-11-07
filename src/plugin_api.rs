//! What plugins can ask for on the control socket, on top of what `ipc` offers everyone.
//!
//! - `windows` answers with one line of JSON: every window with its id, app id, title, output,
//!   workspace, whether it has the focus, floats or is on screen, and where it is if so.
//! - `subscribe` answers `ok`, then sends a line whenever something happens:
//!   `event open ID APP_ID`, `event close ID`, `event title ID TITLE`, `event focus ID|none`,
//!   `event workspace OUTPUT N`. An app id with no text is `-`.
//! - `offset WINDOW DX DY` draws a window that far from its place, `opacity WINDOW ALPHA` draws it
//!   see-through, from 0 to 1, `border WINDOW 0xRRGGBB[AA]|none` gives it a border colour of its
//!   own. `reset WINDOW|all` takes back what this connection set. WINDOW is an id or `focused`.
//!   These are only looks: the window stays where it is for the mouse and the layout. They last as
//!   long as the connection that set them, so a plugin that ends leaves nothing behind. Effects of
//!   several connections add up: offsets add, opacities multiply, the newest border colour wins.
//! - `dispatch ACTION [ARGUMENT]` does what a bind with that action does, `dispatch spawn kitty`.
//! - `plugins` lists the running plugins as `running PATH` lines, then `ok`.
//! - `grab`, `ungrab`, `line`, `clear`, `window_at`, `focus` and `close` are for gestures, see
//!   `crate::plugin_input`.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use smithay::desktop::Window;

use crate::{
    Wirbel,
    config::{Action, Modifiers, parse_action, parse_button, parse_color, parse_modifiers},
    handlers::window_names,
    ipc::json_string,
    plugin_input::Grab,
    workspace::Placement,
};

/// What a window looks like to the plugins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effect {
    pub offset: (f64, f64),
    pub opacity: f32,
    /// Premultiplied, like the config colours.
    pub border: Option<[f32; 4]>,
}

impl Default for Effect {
    fn default() -> Self {
        Self {
            offset: (0.0, 0.0),
            opacity: 1.0,
            border: None,
        }
    }
}

/// The number of a window for the plugins. It is never used for another window.
#[derive(Debug)]
struct WindowId(u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn window_id(window: &Window) -> u64 {
    let id = window
        .user_data()
        .get_or_insert_threadsafe(|| WindowId(NEXT_ID.fetch_add(1, Ordering::Relaxed)));
    id.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Id(u64),
    Focused,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginRequest {
    Windows,
    Subscribe,
    Offset(Target, f64, f64),
    Opacity(Target, f32),
    Border(Target, Option<[f32; 4]>),
    /// `None` is all windows.
    Reset(Option<Target>),
    Dispatch(Action),
    Plugins,
    Grab(Modifiers, u32),
    Ungrab(Modifiers, u32),
    Line {
        from: (f64, f64),
        to: (f64, f64),
        width: i32,
        color: [f32; 4],
    },
    /// Fading out over that many milliseconds, in that colour.
    Clear(u64, Option<[f32; 4]>),
    WindowAt(f64, f64),
    Focus(Target),
    Close(Target),
}

fn parse_target(word: Option<&str>) -> Result<Target, String> {
    match word {
        Some("focused") => Ok(Target::Focused),
        Some(word) => word
            .parse()
            .map(Target::Id)
            .map_err(|_| format!("expected a window id or `focused`, got `{word}`")),
        None => Err("expected a window id or `focused`".into()),
    }
}

fn parse_number(word: Option<&str>) -> Result<f64, String> {
    let word = word.ok_or("expected a number")?;
    let value: f64 = word
        .parse()
        .map_err(|_| format!("expected a number, got `{word}`"))?;
    value
        .is_finite()
        .then_some(value)
        .ok_or("numbers must be finite".into())
}

impl PluginRequest {
    /// `None` if the line is none of these requests.
    pub fn parse(line: &str) -> Option<Result<Self, String>> {
        let line = line.trim();
        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let mut words = rest.split_ascii_whitespace();
        let simple = |request| match rest {
            "" => Ok(request),
            _ => Err("this request takes no arguments".to_string()),
        };
        let request = match command {
            "windows" => simple(Self::Windows),
            "subscribe" => simple(Self::Subscribe),
            "plugins" => simple(Self::Plugins),
            "offset" => (|| {
                let target = parse_target(words.next())?;
                let (dx, dy) = (parse_number(words.next())?, parse_number(words.next())?);
                Ok(Self::Offset(target, dx, dy))
            })(),
            "opacity" => (|| {
                let target = parse_target(words.next())?;
                let alpha = parse_number(words.next())?;
                if !(0.0..=1.0).contains(&alpha) {
                    return Err(format!("opacity goes from 0 to 1, got {alpha}"));
                }
                Ok(Self::Opacity(target, alpha as f32))
            })(),
            "border" => (|| {
                let target = parse_target(words.next())?;
                let color = match words.next() {
                    Some("none") => None,
                    Some(word) => Some(parse_color(word)?),
                    None => return Err("expected a colour like 0xRRGGBB or `none`".into()),
                };
                Ok(Self::Border(target, color))
            })(),
            "grab" | "ungrab" => (|| {
                let modifiers = parse_modifiers(words.next().ok_or("expected modifiers")?)?;
                if modifiers == Modifiers::default() {
                    return Err("a grab needs at least one modifier".into());
                }
                let button = parse_button(words.next().ok_or("expected a button")?)?;
                Ok(match command {
                    "grab" => Self::Grab(modifiers, button),
                    _ => Self::Ungrab(modifiers, button),
                })
            })(),
            "line" => (|| {
                let from = (parse_number(words.next())?, parse_number(words.next())?);
                let to = (parse_number(words.next())?, parse_number(words.next())?);
                let width = parse_number(words.next())?;
                if !(1.0..=64.0).contains(&width) {
                    return Err(format!("width goes from 1 to 64, got {width}"));
                }
                let color = parse_color(words.next().ok_or("expected a colour like 0xRRGGBB")?)?;
                Ok(Self::Line {
                    from,
                    to,
                    width: width.round() as i32,
                    color,
                })
            })(),
            "clear" => (|| {
                let fade = match words.next() {
                    None => 0.0,
                    word => parse_number(word)?,
                };
                if !(0.0..=10_000.0).contains(&fade) {
                    return Err(format!("fading takes 0 to 10000 milliseconds, got {fade}"));
                }
                let color = words.next().map(parse_color).transpose()?;
                Ok(Self::Clear(fade as u64, color))
            })(),
            "window_at" => (|| {
                let (x, y) = (parse_number(words.next())?, parse_number(words.next())?);
                Ok(Self::WindowAt(x, y))
            })(),
            "focus" => parse_target(words.next()).map(Self::Focus),
            "close" => parse_target(words.next()).map(Self::Close),
            "reset" => match words.next() {
                Some("all") => Ok(Self::Reset(None)),
                word => parse_target(word).map(|target| Self::Reset(Some(target))),
            },
            "dispatch" => {
                let (name, argument) = rest.split_once(' ').unwrap_or((rest, ""));
                match name {
                    "" => Err("expected an action, like `dispatch spawn kitty`".into()),
                    name => parse_action(name, argument.trim()).map(Self::Dispatch),
                }
            }
            _ => return None,
        };
        // What a request does not use is a mistake, not something to ignore.
        let complete = matches!(command, "dispatch" | "windows" | "subscribe" | "plugins")
            || words.next().is_none();
        Some(match request {
            Ok(_) if !complete => Err("too many arguments".into()),
            request => request,
        })
    }
}

/// What subscribers were told last.
#[derive(Debug, Clone, PartialEq, Default)]
struct Snapshot {
    /// Id, app id and title.
    windows: Vec<(u64, String, String)>,
    focused: Option<u64>,
    /// Output and one-based workspace.
    workspaces: Vec<(String, usize)>,
}

/// What the plugins did to the windows and know about them.
#[derive(Default)]
pub struct PluginState {
    /// By window and connection.
    effects: HashMap<(u64, u64), Effect>,
    /// Kept while anyone subscribes.
    seen: Option<Snapshot>,
}

impl PluginState {
    /// All that the connections did to `window`, `None` for nothing.
    pub fn effect(&self, window: &Window) -> Option<Effect> {
        if self.effects.is_empty() {
            return None;
        }
        let id = window_id(window);
        let mine = self.effects.iter().filter(|((window, _), _)| *window == id);
        combine(mine.map(|((_, peer), effect)| (*peer, *effect)).collect())
    }
}

/// The effects of several connections in one. `None` for none.
fn combine(mut effects: Vec<(u64, Effect)>) -> Option<Effect> {
    if effects.is_empty() {
        return None;
    }
    // Newer connections have higher numbers, their colour wins.
    effects.sort_by_key(|(peer, _)| *peer);
    Some(
        effects
            .iter()
            .fold(Effect::default(), |sum, (_, effect)| Effect {
                offset: (
                    sum.offset.0 + effect.offset.0,
                    sum.offset.1 + effect.offset.1,
                ),
                opacity: sum.opacity * effect.opacity,
                border: effect.border.or(sum.border),
            }),
    )
}

/// Lines of `event`, from what changed between two snapshots.
fn events(old: &Snapshot, new: &Snapshot) -> Vec<String> {
    let mut lines = Vec::new();
    let find = |snapshot: &Snapshot, id| {
        let windows = snapshot.windows.iter();
        windows
            .into_iter()
            .find(|(other, _, _)| *other == id)
            .cloned()
    };
    for (id, _, _) in &old.windows {
        if find(new, *id).is_none() {
            lines.push(format!("event close {id}"));
        }
    }
    for (id, app_id, title) in &new.windows {
        match find(old, *id) {
            None => {
                let app_id = if app_id.is_empty() { "-" } else { app_id };
                lines.push(format!("event open {id} {app_id}"));
                if !title.is_empty() {
                    lines.push(format!("event title {id} {title}"));
                }
            }
            Some((_, _, old_title)) if old_title != *title => {
                lines.push(format!("event title {id} {title}"));
            }
            Some(_) => {}
        }
    }
    if old.focused != new.focused {
        match new.focused {
            Some(id) => lines.push(format!("event focus {id}")),
            None => lines.push("event focus none".into()),
        }
    }
    for workspace in &new.workspaces {
        if !old.workspaces.contains(workspace) {
            let (output, number) = workspace;
            lines.push(format!("event workspace {output} {number}"));
        }
    }
    lines
}

impl Wirbel {
    fn snapshot(&self) -> Snapshot {
        let windows = self.listed_windows().into_iter().map(|window| {
            let (title, app_id) = window_names(&window);
            // A title is one line on the wire.
            let title = title.replace(['\n', '\r'], " ");
            (
                window_id(&window),
                app_id.replace(char::is_whitespace, "_"),
                title,
            )
        });
        let monitors = self.monitors.iter().filter(|monitor| monitor.connected);
        Snapshot {
            windows: windows.collect(),
            focused: self.focused.as_ref().map(window_id),
            workspaces: monitors
                .map(|monitor| (monitor.output.name(), monitor.active + 1))
                .collect(),
        }
    }

    /// After every loop iteration: tells subscribers what changed, and forgets the effects of
    /// windows that are gone.
    pub fn plugin_housekeeping(&mut self) {
        let subscribed = self.ipc.has_subscribers();
        if !subscribed {
            self.plugin_state.seen = None;
        }
        if !subscribed && self.plugin_state.effects.is_empty() {
            return;
        }
        let snapshot = self.snapshot();
        if !self.plugin_state.effects.is_empty() {
            let before = self.plugin_state.effects.len();
            let alive = |id: &u64| snapshot.windows.iter().any(|(other, _, _)| other == id);
            self.plugin_state
                .effects
                .retain(|(window, _), _| alive(window));
            if self.plugin_state.effects.len() != before {
                self.update_borders();
            }
        }
        if subscribed {
            if let Some(old) = &self.plugin_state.seen
                && *old != snapshot
            {
                for line in events(old, &snapshot) {
                    self.ipc.send_event(&line);
                }
            }
            self.plugin_state.seen = Some(snapshot);
        }
    }

    /// A connection went away, and with it what it did.
    pub fn forget_peer(&mut self, peer: u64) {
        self.forget_plugin_input(peer);
        let before = self.plugin_state.effects.len();
        self.plugin_state
            .effects
            .retain(|(_, owner), _| *owner != peer);
        if self.plugin_state.effects.len() != before {
            self.update_borders();
        }
    }

    fn target_window(&self, target: Target) -> Result<Window, String> {
        match target {
            Target::Focused => self.focused.clone().ok_or("no window has the focus".into()),
            Target::Id(id) => {
                let windows = self.listed_windows().into_iter();
                let mut windows = windows.filter(|window| window_id(window) == id);
                windows.next().ok_or(format!("no window {id}"))
            }
        }
    }

    pub fn handle_plugin_request(&mut self, peer: u64, request: PluginRequest) {
        let answer = match request {
            PluginRequest::Windows => Ok(self.windows_json()),
            PluginRequest::Subscribe => {
                if self.plugin_state.seen.is_none() {
                    self.plugin_state.seen = Some(self.snapshot());
                }
                self.ipc.subscribe(peer);
                Ok("ok".into())
            }
            PluginRequest::Plugins => {
                let running: Vec<String> = self
                    .plugins
                    .running()
                    .map(|path| path.display().to_string())
                    .collect();
                for path in running {
                    self.ipc.reply(peer, &format!("running {path}"));
                }
                Ok("ok".into())
            }
            PluginRequest::Dispatch(action) => {
                self.run_action(action);
                Ok("ok".into())
            }
            PluginRequest::Offset(target, dx, dy) => {
                self.change_effect(peer, target, |effect| effect.offset = (dx, dy))
            }
            PluginRequest::Opacity(target, alpha) => {
                self.change_effect(peer, target, |effect| effect.opacity = alpha)
            }
            PluginRequest::Border(target, color) => {
                let changed = self.change_effect(peer, target, |effect| effect.border = color);
                self.update_borders();
                changed
            }
            PluginRequest::Reset(None) => {
                self.forget_peer(peer);
                self.needs_redraw = true;
                Ok("ok".into())
            }
            PluginRequest::Grab(modifiers, button) => self
                .add_grab(Grab {
                    peer,
                    modifiers,
                    button,
                })
                .map(|()| "ok".into()),
            PluginRequest::Ungrab(modifiers, button) => self
                .remove_grab(Grab {
                    peer,
                    modifiers,
                    button,
                })
                .map(|()| "ok".into()),
            PluginRequest::Line {
                from,
                to,
                width,
                color,
            } => {
                self.add_line(peer, from.into(), to.into(), width, color);
                Ok("ok".into())
            }
            PluginRequest::Clear(fade, color) => {
                self.clear_lines(peer, Duration::from_millis(fade), color);
                Ok("ok".into())
            }
            PluginRequest::WindowAt(x, y) => Ok(self
                .window_at((x, y).into())
                .map_or("none".into(), |window| window_id(&window).to_string())),
            PluginRequest::Focus(target) => self.target_window(target).map(|window| {
                self.activate_window(&window);
                "ok".into()
            }),
            PluginRequest::Close(target) => self.target_window(target).map(|window| {
                self.close_window(&window);
                "ok".into()
            }),
            PluginRequest::Reset(Some(target)) => self.target_window(target).map(|window| {
                let id = window_id(&window);
                self.plugin_state.effects.remove(&(id, peer));
                self.update_borders();
                "ok".to_string()
            }),
        };
        match answer {
            Ok(line) => self.ipc.reply(peer, &line),
            Err(err) => self.ipc.reply(peer, &format!("error {err}")),
        }
    }

    fn change_effect(
        &mut self,
        peer: u64,
        target: Target,
        change: impl FnOnce(&mut Effect),
    ) -> Result<String, String> {
        let window = self.target_window(target)?;
        let effects = &mut self.plugin_state.effects;
        let effect = effects.entry((window_id(&window), peer)).or_default();
        change(effect);
        if *effect == Effect::default() {
            effects.remove(&(window_id(&window), peer));
        }
        self.needs_redraw = true;
        Ok("ok".into())
    }

    fn windows_json(&self) -> String {
        let windows: Vec<String> = self
            .listed_windows()
            .iter()
            .map(|window| {
                let (title, app_id) = window_names(window);
                let id = window_id(window);
                let located = self.locate(window);
                let output = located
                    .map(|(monitor, _)| json_string(&self.monitors[monitor].output.name()))
                    .unwrap_or("null".into());
                let workspace = located.map_or("null".into(), |(_, workspace)| (workspace + 1).to_string());
                let floating = located.is_some_and(|(monitor, workspace)| {
                    let workspace = &self.monitors[monitor].workspaces[workspace];
                    matches!(workspace.placement(window), Some(Placement::Floating(_)))
                });
                let focused = self.focused.as_ref() == Some(window);
                let frame = self.frames.iter().find(|frame| frame.window == *window);
                let visible = frame.is_some();
                let geometry = frame.map_or(r#""x":null,"y":null,"width":null,"height":null"#.into(), |frame| {
                    let r = frame.inner;
                    format!(r#""x":{},"y":{},"width":{},"height":{}"#, r.x, r.y, r.w, r.h)
                });
                format!(
                    r#"{{"id":{id},"app_id":{},"title":{},"output":{output},"workspace":{workspace},"focused":{focused},"floating":{floating},"visible":{visible},{geometry}}}"#,
                    json_string(&app_id),
                    json_string(&title),
                )
            })
            .collect();
        format!("[{}]", windows.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_check_their_arguments() {
        let parse = |line| PluginRequest::parse(line).unwrap();
        assert_eq!(parse("windows"), Ok(PluginRequest::Windows));
        assert_eq!(parse("subscribe\n"), Ok(PluginRequest::Subscribe));
        assert_eq!(
            parse("offset 3 -1.5 2"),
            Ok(PluginRequest::Offset(Target::Id(3), -1.5, 2.0))
        );
        assert_eq!(
            parse("opacity focused 0.5"),
            Ok(PluginRequest::Opacity(Target::Focused, 0.5))
        );
        assert_eq!(
            parse("border 1 none"),
            Ok(PluginRequest::Border(Target::Id(1), None))
        );
        assert_eq!(
            parse("border 1 0xff0000"),
            Ok(PluginRequest::Border(
                Target::Id(1),
                Some([1.0, 0.0, 0.0, 1.0])
            ))
        );
        assert_eq!(parse("reset all"), Ok(PluginRequest::Reset(None)));
        assert_eq!(
            parse("reset 4"),
            Ok(PluginRequest::Reset(Some(Target::Id(4))))
        );
        assert_eq!(
            parse("dispatch spawn kitty --hold"),
            Ok(PluginRequest::Dispatch(Action::Spawn(
                "kitty --hold".into()
            )))
        );
        assert_eq!(
            parse("dispatch close"),
            Ok(PluginRequest::Dispatch(Action::Close))
        );
        for bad in [
            "windows 1",
            "offset",
            "offset 1 2",
            "offset x 1 2",
            "offset 1 2 3 4",
            "offset 1 nan 2",
            "opacity 1 2",
            "opacity 1 -0.1",
            "border 1 red",
            "border 1",
            "reset",
            "dispatch",
            "dispatch dance",
            "dispatch close now",
        ] {
            assert!(
                PluginRequest::parse(bad).unwrap().is_err(),
                "accepted {bad:?}"
            );
        }
        assert_eq!(PluginRequest::parse("position"), None);
    }

    #[test]
    fn events_name_what_changed() {
        let old = Snapshot {
            windows: vec![
                (1, "kitty".into(), "a".into()),
                (2, "foot".into(), "b".into()),
            ],
            focused: Some(1),
            workspaces: vec![("DP-4".into(), 1)],
        };
        let new = Snapshot {
            windows: vec![
                (1, "kitty".into(), "c".into()),
                (3, String::new(), String::new()),
            ],
            focused: None,
            workspaces: vec![("DP-4".into(), 2)],
        };
        assert_eq!(
            events(&old, &new),
            [
                "event close 2",
                "event title 1 c",
                "event open 3 -",
                "event focus none",
                "event workspace DP-4 2"
            ]
        );
        assert_eq!(events(&new, &new), Vec::<String>::new());
    }

    #[test]
    fn effects_of_several_connections_add_up() {
        let red = Some([1.0, 0.0, 0.0, 1.0]);
        let blue = Some([0.0, 0.0, 1.0, 1.0]);
        let effect = |offset, opacity, border| Effect {
            offset,
            opacity,
            border,
        };
        let sum = combine(vec![
            (5, effect((10.0, 0.0), 0.5, blue)),
            (2, effect((1.0, 2.0), 0.5, red)),
            (9, effect((0.0, 0.0), 1.0, None)),
        ]);
        assert_eq!(sum, Some(effect((11.0, 2.0), 0.25, blue)));
        assert_eq!(combine(Vec::new()), None);
    }
}
