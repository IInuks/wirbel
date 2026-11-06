//! The control socket behind `wirbel pos`, `wirbel move` and `wirbel mmsg`.
//!
//! The protocol is line based, one request per line:
//!
//! - `position` answers with one position line.
//! - `watch` answers with a position line now and again whenever the position changes.
//! - `move X Y` and `move-rel DX DY` answer with `ok` or `error MESSAGE`.
//!
//! A position line is `position X Y OUTPUT`, without `OUTPUT` if the pointer is on no output.
//! - `tags` and `monitors` answer with one line of JSON, in the format of Mango's `mmsg`.
//! - `watch-tags` and `watch-monitors` do so now and again whenever that line changes.
//! - `workspace N`, `quit` and `reload` act like the bind and answer with `ok`.
//!
//! Nothing here may block the compositor. A client that stops reading only misses lines: its next
//! one is the latest one, sent as soon as it reads again.

use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
};

use smithay::{
    backend::input::InputTime,
    reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
};

use crate::{
    Wirbel,
    config::{Action, WORKSPACES, WorkspaceTarget},
};

/// Longest request a client may send.
const MAX_REQUEST: usize = 4096;
/// Answers a client may leave unread before it is dropped.
const MAX_BACKLOG: usize = 64 * 1024;

/// The socket of the compositor with the given Wayland socket, which is what makes it unique.
pub fn socket_path(wayland_display: &OsStr) -> Option<PathBuf> {
    let name = Path::new(wayland_display).file_name()?.to_str()?;
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime_dir).join(format!("wirbel-{name}.sock")))
}

/// Where the pointer is, in global layout coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub output: Option<String>,
}

impl Position {
    /// `{:?}` is the shortest text that reads back as the same number, and always has a fraction.
    pub fn to_line(&self) -> String {
        let (x, y) = (self.x, self.y);
        match &self.output {
            Some(output) => format!("position {x:?} {y:?} {output}"),
            None => format!("position {x:?} {y:?}"),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.strip_prefix("position ")?.splitn(3, ' ');
        Some(Self {
            x: parts.next()?.parse().ok()?,
            y: parts.next()?.parse().ok()?,
            output: parts.next().map(str::to_string),
        })
    }

    pub fn to_json(&self) -> String {
        let (x, y) = (self.x, self.y);
        let output = self.output.as_deref().map_or("null".into(), json_string);
        format!(r#"{{"x":{x:?},"y":{y:?},"output":{output}}}"#)
    }
}

fn json_string(text: &str) -> String {
    let mut json = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            c if c < ' ' => json.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => json.push(c),
        }
    }
    json.push('"');
    json
}

/// What the bars of Mango want to know about an output.
#[derive(Debug, Clone, PartialEq)]
struct MonitorState {
    name: String,
    focused: bool,
    area: (i32, i32, i32, i32),
    scale: f64,
    active_workspace: usize,
    windows: [usize; WORKSPACES],
}

fn tags_json(monitor: &MonitorState) -> String {
    let tags: Vec<String> = monitor
        .windows
        .iter()
        .enumerate()
        .map(|(index, windows)| {
            let (number, active) = (index + 1, index == monitor.active_workspace);
            format!(
                r#"{{"index":{number},"is_active":{active},"is_urgent":false,"layout":"DW","client_count":{windows}}}"#
            )
        })
        .collect();
    tags.join(",")
}

fn all_tags_json(monitors: &[MonitorState]) -> String {
    let monitors: Vec<String> = monitors
        .iter()
        .map(|monitor| {
            let (name, tags) = (json_string(&monitor.name), tags_json(monitor));
            format!(r#"{{"monitor":{name},"tags":[{tags}]}}"#)
        })
        .collect();
    format!(r#"{{"all_tags":[{}]}}"#, monitors.join(","))
}

fn all_monitors_json(monitors: &[MonitorState]) -> String {
    let monitors: Vec<String> = monitors
        .iter()
        .map(|monitor| {
            let (name, tags) = (json_string(&monitor.name), tags_json(monitor));
            let (active, scale) = (monitor.focused, monitor.scale);
            let (x, y, width, height) = monitor.area;
            format!(
                r#"{{"name":{name},"active":{active},"x":{x},"y":{y},"width":{width},"height":{height},"scale":{scale},"layout_symbol":"DW","tags":[{tags}]}}"#
            )
        })
        .collect();
    format!(r#"{{"monitors":[{}]}}"#, monitors.join(","))
}

/// The two views of the workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Tags,
    Monitors,
}

impl View {
    fn render(self, monitors: &[MonitorState]) -> String {
        match self {
            Self::Tags => all_tags_json(monitors),
            Self::Monitors => all_monitors_json(monitors),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Request {
    Position,
    Watch,
    Move {
        x: f64,
        y: f64,
    },
    MoveRelative {
        dx: f64,
        dy: f64,
    },
    Get(View),
    WatchView(View),
    /// One-based.
    Workspace(usize),
    Quit,
    Reload,
}

impl Request {
    pub fn to_line(self) -> String {
        match self {
            Self::Position => "position".into(),
            Self::Watch => "watch".into(),
            Self::Move { x, y } => format!("move {x:?} {y:?}"),
            Self::MoveRelative { dx, dy } => format!("move-rel {dx:?} {dy:?}"),
            Self::Get(View::Tags) => "tags".into(),
            Self::Get(View::Monitors) => "monitors".into(),
            Self::WatchView(View::Tags) => "watch-tags".into(),
            Self::WatchView(View::Monitors) => "watch-monitors".into(),
            Self::Workspace(number) => format!("workspace {number}"),
            Self::Quit => "quit".into(),
            Self::Reload => "reload".into(),
        }
    }

    pub fn parse(line: &str) -> Result<Self, &'static str> {
        let mut words = line.split_ascii_whitespace();
        let command = words.next().unwrap_or_default();
        let simple = |request| match words.clone().next() {
            None => Ok(request),
            Some(_) => Err("this request takes no arguments"),
        };
        match command {
            "position" => simple(Self::Position),
            "watch" => simple(Self::Watch),
            "tags" => simple(Self::Get(View::Tags)),
            "monitors" => simple(Self::Get(View::Monitors)),
            "watch-tags" => simple(Self::WatchView(View::Tags)),
            "watch-monitors" => simple(Self::WatchView(View::Monitors)),
            "quit" => simple(Self::Quit),
            "reload" => simple(Self::Reload),
            "workspace" => {
                let number = words.next().and_then(|word| word.parse().ok());
                match number {
                    Some(number @ 1..=WORKSPACES) if words.next().is_none() => {
                        Ok(Self::Workspace(number))
                    }
                    _ => Err("expected a workspace from 1 to 9"),
                }
            }
            "move" | "move-rel" => {
                let mut number = || {
                    let word = words.next().ok_or("expected two numbers")?;
                    let value: f64 = word.parse().map_err(|_| "expected two numbers")?;
                    value
                        .is_finite()
                        .then_some(value)
                        .ok_or("coordinates must be finite")
                };
                let (a, b) = (number()?, number()?);
                if words.next().is_some() {
                    return Err("expected two numbers");
                }
                Ok(match command {
                    "move" => Self::Move { x: a, y: b },
                    _ => Self::MoveRelative { dx: a, dy: b },
                })
            }
            _ => Err("unknown request"),
        }
    }
}

struct Peer {
    stream: UnixStream,
    /// Received bytes that do not make up a line yet.
    input: Vec<u8>,
    /// Bytes on their way out. They stay in order, so a line never arrives torn.
    output: Vec<u8>,
    /// The position to send once `output` is empty. Newer positions replace it.
    next_position: Option<Position>,
    /// Whether the client watches, and the last position it was given.
    watching: bool,
    last_position: Option<Position>,
    /// The view of the workspaces this client watches.
    view: Option<View>,
    /// The line of that view to send once `output` is empty. Newer ones replace it.
    next_view: Option<String>,
    /// Whether a source waits for the socket to take more data.
    waiting: bool,
}

impl Peer {
    /// Writes as much as the socket takes. Returns whether everything went out.
    fn write_out(&mut self) -> io::Result<bool> {
        loop {
            if self.output.is_empty() {
                match (self.next_position.take(), self.next_view.take()) {
                    (Some(position), view) => {
                        queue_line(&mut self.output, &position.to_line());
                        self.next_view = view;
                    }
                    (None, Some(view)) => queue_line(&mut self.output, &view),
                    (None, None) => return Ok(true),
                }
            }
            match (&self.stream).write(&self.output) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => drop(self.output.drain(..written)),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }

    /// Ends the connection. The read source sees that and forgets the peer.
    fn close(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn queue_line(output: &mut Vec<u8>, line: &str) {
    output.extend_from_slice(line.as_bytes());
    output.push(b'\n');
}

pub struct Ipc {
    handle: LoopHandle<'static, Wirbel>,
    /// The socket to remove on exit. `None` if it could not be set up.
    path: Option<PathBuf>,
    peers: HashMap<u64, Peer>,
    next_id: u64,
    /// What the watchers of the workspaces know.
    monitors: Vec<MonitorState>,
}

impl Ipc {
    /// Opens the socket. Failing to do so costs `wirbel pos`, which is no reason to stay down.
    pub fn new(handle: LoopHandle<'static, Wirbel>, wayland_display: &OsStr) -> Self {
        let mut ipc = Self {
            handle,
            path: None,
            peers: HashMap::new(),
            next_id: 0,
            monitors: Vec::new(),
        };
        match ipc.listen(wayland_display) {
            Ok(path) => {
                tracing::info!(path = %path.display(), "control socket ready");
                ipc.path = Some(path);
            }
            Err(err) => tracing::error!(%err, "no control socket"),
        }
        ipc
    }

    fn listen(&self, wayland_display: &OsStr) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = socket_path(wayland_display).ok_or("XDG_RUNTIME_DIR is not set")?;
        // Whoever holds the Wayland socket owns this name, so a leftover is from a dead process.
        match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let source = Generic::new(listener, Interest::READ, Mode::Level);
        let inserted = self.handle.insert_source(source, |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => state.ipc.add_peer(stream),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        tracing::warn!(%err, "failed to accept a control client");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        });
        if let Err(err) = inserted {
            let _ = fs::remove_file(&path);
            return Err(err.error.into());
        }
        Ok(path)
    }

    fn add_peer(&mut self, stream: UnixStream) {
        let source = stream
            .set_nonblocking(true)
            .and_then(|()| stream.try_clone());
        let source = match source {
            Ok(source) => source,
            Err(err) => {
                tracing::warn!(%err, "failed to set up a control client");
                return;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let source = Generic::new(source, Interest::READ, Mode::Level);
        let inserted = self
            .handle
            .insert_source(source, move |_, _, state| Ok(state.peer_readable(id)));
        if let Err(err) = inserted {
            tracing::warn!(error = %err.error, "failed to watch a control client");
            return;
        }
        self.peers.insert(
            id,
            Peer {
                stream,
                input: Vec::new(),
                output: Vec::new(),
                next_position: None,
                watching: false,
                last_position: None,
                view: None,
                next_view: None,
                waiting: false,
            },
        );
    }

    /// Tells everyone watching about `position`.
    pub fn publish(&mut self, position: &Position) {
        let ids: Vec<u64> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.watching && peer.last_position.as_ref() != Some(position))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(peer) = self.peers.get_mut(&id) {
                peer.last_position = Some(position.clone());
                peer.next_position = Some(position.clone());
            }
            self.flush(id);
        }
    }

    /// Tells everyone watching a view about a change.
    fn publish_view(&mut self) {
        let ids: Vec<u64> = self.peers.keys().copied().collect();
        for id in ids {
            let peer = self.peers.get_mut(&id);
            if let Some(peer) = peer
                && let Some(view) = peer.view
            {
                peer.next_view = Some(view.render(&self.monitors));
                self.flush(id);
            }
        }
    }

    fn reply(&mut self, id: u64, line: &str) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        queue_line(&mut peer.output, line);
        if peer.output.len() > MAX_BACKLOG {
            peer.close();
            return;
        }
        self.flush(id);
    }

    fn flush(&mut self, id: u64) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        match peer.write_out() {
            Ok(true) => {}
            Ok(false) if peer.waiting => {}
            Ok(false) => self.wait_for_writable(id),
            Err(_) => peer.close(),
        }
    }

    /// Finishes writing once the client reads again. Until then there is no reason to wake up.
    fn wait_for_writable(&mut self, id: u64) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        let source = match peer.stream.try_clone() {
            Ok(stream) => Generic::new(stream, Interest::WRITE, Mode::Level),
            Err(_) => return peer.close(),
        };
        let inserted = self.handle.insert_source(source, move |_, _, state| {
            let Some(peer) = state.ipc.peers.get_mut(&id) else {
                return Ok(PostAction::Remove);
            };
            Ok(match peer.write_out() {
                Ok(false) => PostAction::Continue,
                Ok(true) => {
                    peer.waiting = false;
                    PostAction::Remove
                }
                Err(_) => {
                    peer.close();
                    PostAction::Remove
                }
            })
        });
        match inserted {
            Ok(_) => peer.waiting = true,
            Err(_) => peer.close(),
        }
    }

    pub fn client_count(&self) -> usize {
        self.peers.len()
    }

    /// The next complete request of a client.
    fn next_line(&mut self, id: u64) -> Option<String> {
        let peer = self.peers.get_mut(&id)?;
        let end = peer.input.iter().position(|byte| *byte == b'\n')?;
        let line: Vec<u8> = peer.input.drain(..=end).collect();
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

impl Wirbel {
    fn peer_readable(&mut self, id: u64) -> PostAction {
        let Some(peer) = self.ipc.peers.get_mut(&id) else {
            return PostAction::Remove;
        };
        let mut buffer = [0; MAX_REQUEST];
        match (&peer.stream).read(&mut buffer) {
            Ok(0) => {
                self.ipc.peers.remove(&id);
                return PostAction::Remove;
            }
            Ok(count) => peer.input.extend_from_slice(&buffer[..count]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return PostAction::Continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => return PostAction::Continue,
            Err(_) => {
                self.ipc.peers.remove(&id);
                return PostAction::Remove;
            }
        }
        while let Some(line) = self.ipc.next_line(id) {
            match Request::parse(&line) {
                Ok(request) => self.handle_request(id, request),
                Err(err) => self.ipc.reply(id, &format!("error {err}")),
            }
        }
        if let Some(peer) = self.ipc.peers.get_mut(&id)
            && peer.input.len() > MAX_REQUEST
        {
            peer.close();
        }
        PostAction::Continue
    }

    fn handle_request(&mut self, id: u64, request: Request) {
        match request {
            Request::Position => {
                let line = self.pointer_position().to_line();
                self.ipc.reply(id, &line);
            }
            Request::Watch => {
                if let Some(peer) = self.ipc.peers.get_mut(&id) {
                    peer.watching = true;
                    peer.last_position = None;
                }
                let position = self.pointer_position();
                self.ipc.publish(&position);
            }
            Request::Move { x, y } => self.ipc_move(id, |_, _| (x, y)),
            Request::MoveRelative { dx, dy } => self.ipc_move(id, |x, y| (x + dx, y + dy)),
            Request::Get(view) => {
                let line = view.render(&self.monitor_states());
                self.ipc.reply(id, &line);
            }
            Request::WatchView(view) => {
                self.ipc.monitors = self.monitor_states();
                if let Some(peer) = self.ipc.peers.get_mut(&id) {
                    peer.view = Some(view);
                    peer.next_view = Some(view.render(&self.ipc.monitors));
                }
                self.ipc.flush(id);
            }
            Request::Workspace(number) => {
                let target = WorkspaceTarget::Index(number - 1);
                self.run_action(Action::Workspace(target));
                self.ipc.reply(id, "ok");
            }
            Request::Quit => {
                self.ipc.reply(id, "ok");
                self.run_action(Action::Quit);
            }
            Request::Reload => {
                self.run_action(Action::Reload);
                self.ipc.reply(id, "ok");
            }
        }
    }

    fn monitor_states(&self) -> Vec<MonitorState> {
        self.monitors
            .iter()
            .enumerate()
            .map(|(index, monitor)| {
                let area = self.monitor_area(index);
                MonitorState {
                    name: monitor.output.name(),
                    focused: index == self.active_monitor,
                    area: area.map_or((0, 0, 0, 0), |area| (area.x, area.y, area.w, area.h)),
                    scale: monitor.output.current_scale().fractional_scale(),
                    active_workspace: monitor.active,
                    windows: std::array::from_fn(|index| monitor.workspaces[index].windows().len()),
                }
            })
            .collect()
    }

    /// Runs after every loop iteration. Looks only if somebody wants to know.
    pub fn publish_workspaces(&mut self) {
        if !self.ipc.peers.values().any(|peer| peer.view.is_some()) {
            return;
        }
        let monitors = self.monitor_states();
        if monitors != self.ipc.monitors {
            self.ipc.monitors = monitors;
            self.ipc.publish_view();
        }
    }

    /// Takes the path of real pointer motion, so to everybody else this is the mouse moving,
    /// grabs included: a dragged window follows, and a client holding a button sees the motion.
    fn ipc_move(&mut self, id: u64, target: impl FnOnce(f64, f64) -> (f64, f64)) {
        let on_output = (0..self.monitors.len()).any(|index| self.monitor_area(index).is_some());
        if !on_output {
            self.ipc.reply(id, "error no output");
            return;
        }
        let position = self.pointer_position();
        let (x, y) = target(position.x, position.y);
        self.on_pointer_motion((x, y).into(), InputTime::now());
        self.ipc.reply(id, "ok");
    }
}

#[cfg(test)]
mod tests {
    use smithay::reexports::calloop::EventLoop;

    use super::*;

    #[test]
    fn requests_survive_the_wire() {
        for request in [
            Request::Position,
            Request::Watch,
            Request::Move { x: 1.5, y: -2.0 },
            Request::MoveRelative {
                dx: 0.1 + 0.2,
                dy: 1e-7,
            },
            Request::Get(View::Tags),
            Request::Get(View::Monitors),
            Request::WatchView(View::Tags),
            Request::WatchView(View::Monitors),
            Request::Workspace(9),
            Request::Quit,
            Request::Reload,
        ] {
            assert_eq!(Request::parse(&request.to_line()), Ok(request));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        for line in [
            "",
            "dance",
            "move",
            "move 1",
            "move 1 2 3",
            "move 1 x",
            "move NaN 0",
            "move 0 inf",
            "move 1e999 0",
            "move-rel 1",
            "watch now",
            "position 1",
            "workspace",
            "workspace 0",
            "workspace 10",
            "workspace 1 2",
            "quit now",
        ] {
            assert!(Request::parse(line).is_err(), "accepted {line:?}");
        }
    }

    #[test]
    fn requests_accept_a_trailing_line_end() {
        assert_eq!(Request::parse("watch\n"), Ok(Request::Watch));
        assert_eq!(Request::parse("position\r\n"), Ok(Request::Position));
    }

    #[test]
    fn views_have_the_format_of_mmsg() {
        let monitors = [MonitorState {
            name: "DP-4".into(),
            focused: true,
            area: (0, 0, 2560, 1440),
            scale: 1.0,
            active_workspace: 1,
            windows: [2, 0, 0, 0, 0, 0, 0, 0, 0],
        }];
        let tags = all_tags_json(&monitors);
        assert!(
            tags.starts_with(
                r#"{"all_tags":[{"monitor":"DP-4","tags":[{"index":1,"is_active":false,"is_urgent":false,"layout":"DW","client_count":2},{"index":2,"is_active":true,"#
            ),
            "{tags}"
        );
        assert!(tags.ends_with(
            r#""index":9,"is_active":false,"is_urgent":false,"layout":"DW","client_count":0}]}]}"#
        ));
        let all = all_monitors_json(&monitors);
        assert!(
            all.starts_with(
                r#"{"monitors":[{"name":"DP-4","active":true,"x":0,"y":0,"width":2560,"height":1440,"scale":1,"layout_symbol":"DW","tags":[{"index":1,"#
            ),
            "{all}"
        );
    }

    #[test]
    fn positions_survive_the_wire() {
        for position in [
            Position {
                x: 1.0,
                y: 0.1 + 0.2,
                output: Some("DP-4".into()),
            },
            Position {
                x: -3.25,
                y: 1920.0,
                output: Some("with a space".into()),
            },
            Position {
                x: 0.0,
                y: 0.0,
                output: None,
            },
        ] {
            assert_eq!(Position::parse(&position.to_line()), Some(position));
        }
        assert_eq!(Position::parse("ok"), None);
        assert_eq!(Position::parse("position 1"), None);
    }

    #[test]
    fn json_is_exact_and_escaped() {
        let mut position = Position {
            x: 12.0,
            y: 0.1 + 0.2,
            output: Some("a\"b\\c\n".into()),
        };
        assert_eq!(
            position.to_json(),
            r#"{"x":12.0,"y":0.30000000000000004,"output":"a\"b\\c\u000a"}"#
        );
        position.output = None;
        assert_eq!(
            position.to_json(),
            r#"{"x":12.0,"y":0.30000000000000004,"output":null}"#
        );
    }

    fn with_event_loop(test: impl FnOnce(&EventLoop<'static, Wirbel>, &OsStr, PathBuf)) {
        // The name is ours alone, unlike Wayland socket numbers, which get reused.
        let display = format!(
            "ipc-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let display = OsStr::new(&display);
        let Some(path) = socket_path(display) else {
            return;
        };
        test(&EventLoop::try_new().unwrap(), display, path);
    }

    #[test]
    fn the_socket_lives_as_long_as_the_compositor() {
        with_event_loop(|event_loop, display, path| {
            let ipc = Ipc::new(event_loop.handle(), display);
            assert!(UnixStream::connect(&path).is_ok());
            drop(ipc);
            assert!(!path.exists());
        });
    }

    #[test]
    fn a_leftover_socket_is_replaced() {
        with_event_loop(|event_loop, display, path| {
            drop(UnixListener::bind(&path).unwrap());
            assert!(UnixStream::connect(&path).is_err());
            let ipc = Ipc::new(event_loop.handle(), display);
            assert!(UnixStream::connect(&path).is_ok());
            drop(ipc);
        });
    }

    #[test]
    fn the_socket_name_follows_the_wayland_display() {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR");
        if let Some(runtime_dir) = runtime_dir {
            let expected = PathBuf::from(runtime_dir).join("wirbel-wayland-7.sock");
            let name = |name: &str| socket_path(OsStr::new(name));
            assert_eq!(name("wayland-7").as_ref(), Some(&expected));
            assert_eq!(name("/run/user/1/wayland-7").as_ref(), Some(&expected));
        }
    }
}
