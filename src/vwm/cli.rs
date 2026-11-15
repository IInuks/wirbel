//! `wirbel vwm ...`: the commands for invisible windows. Starting, listing and closing go to the
//! visible compositor of `WAYLAND_DISPLAY`; everything that drives one window goes straight to
//! the control socket of its shadow.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use super::{DEFAULT_SIZE, manager::Open, manager::shell_quote, valid_name};
use crate::{
    backend::headless::parse_size,
    cli::ask_lines,
    ipc::{json_string, socket_path},
};

pub const USAGE: &str = "usage: wirbel vwm open [--name NAME] [--size WxH] [--share-bus] [--env KEY=VALUE]... [--] COMMAND...
       wirbel vwm list [--json]
       wirbel vwm info NAME
       wirbel vwm resize NAME WxH
       wirbel vwm close NAME...|all [--kill]
       wirbel vwm type NAME TEXT...
       wirbel vwm key NAME KEY|MODS+KEY...
       wirbel vwm click NAME X Y [left|right|middle] [COUNT]
       wirbel vwm move NAME X Y [SECONDS]
       wirbel vwm drag NAME X1 Y1 X2 Y2 [left|right|middle]
       wirbel vwm scroll NAME STEPS [X Y]
       wirbel vwm shot NAME [FILE.png] [--region X,Y,W,H] [--cursor]
       wirbel vwm clip NAME get | set TEXT...
       wirbel vwm paste NAME [--key MODS+KEY] TEXT...";

pub const HELP: &str =
    "vwm runs programs in invisible windows: they never show on the screen and never take the
focus, and are driven by commands. See docs/vwm/DESIGN.md.
      open       start COMMAND invisibly and print the name of its window. Without --name the
                 name is the program and a number. The window is as large as --size, 1280x800 if
                 not given. The program gets a D-Bus session bus of its own, so that it does not
                 hand the start to a visible instance of itself; --share-bus gives it the bus of
                 the desktop. Browsers get a profile of their own in
                 ~/.local/state/wirbel/vwm/NAME/. Options of the program itself go after `--`
      list       the invisible windows: name, state, process group, size, age, command
      info       one of them, with the windows its program has open
      resize     give the window another size
      close      ask the windows to close and end what does not; --kill ends it at once
      type       type TEXT on the keyboard of the window, a line end is Return. Only what the
                 keyboard layout has keys for; `paste` takes any text
      key        press keys one after another: a key name such as Return, Escape, Tab, F5, a, or
                 a combination such as CTRL+l or CTRL+SHIFT+t. ENTER, ESC, DEL and the like work
      click      move the pointer to X Y of the window and click, COUNT times (2 is a double click)
      move       move the pointer there, in SECONDS if given, at once if not
      drag       press at X1 Y1, move to X2 Y2, let go
      scroll     turn the wheel STEPS notches, down if positive, at X Y if given
      shot       write a picture of the window as PNG and print its path. Without FILE it is
                 NAME.png in the current folder. --cursor draws the pointer too
      clip       print the text in the clipboard of the window, or put TEXT there. It is the
                 window's own clipboard, not the one of the desktop
      paste      put TEXT in that clipboard and press CTRL+v, or what --key names (terminals
                 want CTRL+SHIFT+v)
`wirbel vwm invisible create|close|screenshot ...` mean the same as `wirbel vwm open|close|shot ...`.
Positions are pixels from the top left corner of the window.";

/// How long one answer of a compositor may take. Starting a shadow includes starting an X
/// server and a session bus.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// One row of `vwm list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub group: String,
    pub display: String,
    pub size: String,
    pub seconds: u64,
    pub command: String,
}

impl Row {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.strip_prefix("entry\t")?.splitn(8, '\t');
        let mut next = || fields.next().map(str::to_string);
        Some(Self {
            name: next()?,
            kind: next()?,
            state: next()?,
            group: next()?,
            display: next()?,
            size: next()?,
            seconds: next()?.parse().ok()?,
            command: next()?,
        })
    }

    fn json(&self) -> String {
        format!(
            "{{\"name\":{},\"kind\":{},\"state\":{},\"pid\":{},\"display\":{},\"size\":{},\"seconds\":{},\"command\":{}}}",
            json_string(&self.name),
            json_string(&self.kind),
            json_string(&self.state),
            self.group.parse::<i64>().unwrap_or(0),
            json_string(&self.display),
            json_string(&self.size),
            self.seconds,
            json_string(&self.command)
        )
    }
}

fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h{}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The rows as a table with a header.
fn table(rows: &[Row]) -> String {
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                row.name.clone(),
                row.state.clone(),
                row.group.clone(),
                row.size.clone(),
                age(row.seconds),
                row.command.clone(),
            ]
        })
        .collect();
    let header = ["NAME", "STATE", "PID", "SIZE", "AGE", "COMMAND"];
    let width = |column: usize| {
        let longest = cells.iter().map(|row| row[column].chars().count()).max();
        longest.unwrap_or(0).max(header[column].len())
    };
    let widths: Vec<usize> = (0..5).map(width).collect();
    let line = |row: &[&str]| {
        let mut text = String::new();
        for (column, cell) in row.iter().enumerate() {
            match widths.get(column) {
                Some(width) => text += &format!("{cell:<width$}  "),
                None => text += cell,
            }
        }
        text.trim_end().to_string()
    };
    let mut text = line(&header);
    for row in &cells {
        let row: Vec<&str> = row.iter().map(String::as_str).collect();
        text += "\n";
        text += &line(&row);
    }
    text
}

fn rows() -> Result<Vec<Row>, String> {
    let lines = ask_lines("vwm list").map_err(outdated)?;
    Ok(lines.iter().filter_map(|line| Row::parse(line)).collect())
}

fn outdated(err: String) -> String {
    if err == "unknown request" {
        return "the running wirbel is older than this command, log out and in again or run `wirbel restart`".into();
    }
    err
}

/// The single answer of the visible compositor to `request`, without its `ok `.
fn ask_main(request: &str) -> Result<String, String> {
    let display = std::env::var_os("WAYLAND_DISPLAY").ok_or("WAYLAND_DISPLAY is not set")?;
    answer_of(&ask_display(&display.to_string_lossy(), request)?).map_err(outdated)
}

fn answer_of(line: &str) -> Result<String, String> {
    match line {
        "ok" => Ok(String::new()),
        line if line.starts_with("ok ") => Ok(line[3..].to_string()),
        line if line.starts_with("error ") => Err(line[6..].to_string()),
        line => Ok(line.to_string()),
    }
}

/// One request to the wirbel of that Wayland display, and the line it answers.
pub fn ask_display(display: &str, request: &str) -> Result<String, String> {
    let path = socket_path(display.as_ref()).ok_or("cannot tell where the socket is")?;
    let mut stream = UnixStream::connect(&path)
        .map_err(|err| format!("no wirbel on {display}: {err} ({})", path.display()))?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|err| format!("failed to send: {err}"))?;
    let timeout = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
    timeout.map_err(|err| format!("failed to set a timeout: {err}"))?;
    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line);
    match read {
        Ok(0) => Err("the compositor went away".into()),
        Ok(_) => Ok(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(err) => Err(format!("no answer: {err}")),
    }
}

/// The Wayland display of the shadow behind `name`.
pub fn display_of(name: &str) -> Result<String, String> {
    ask_main(&format!("vwm where {name}"))
}

/// One request to the shadow behind `name`, and its answer without `ok `.
pub fn ask_shadow(name: &str, request: &str) -> Result<String, String> {
    answer_of(&ask_display(&display_of(name)?, request)?)
}

/// The words of `open`: options anywhere before a `--`, everything else is the command. One
/// word alone is taken as a shell command, several are quoted word by word.
pub fn parse_open(args: &[String]) -> Result<Open, String> {
    let mut open = Open {
        name: None,
        size: DEFAULT_SIZE,
        share_bus: false,
        env: Vec::new(),
        command: String::new(),
    };
    let mut words: Vec<&str> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or(format!("{arg} needs {what}"));
        match arg.as_str() {
            "--" => {
                words.extend(args.by_ref().map(String::as_str));
            }
            "--name" => {
                let name = value("a name")?;
                if !valid_name(name) {
                    return Err(format!(
                        "`{name}` cannot be a name: letters, digits, - and _, at most 32"
                    ));
                }
                open.name = Some(name.clone());
            }
            "--size" => {
                let size = value("WxH")?;
                open.size = parse_size(size).ok_or("--size needs WxH, such as 1280x800")?;
            }
            "--share-bus" => open.share_bus = true,
            "--env" => {
                let pair = value("KEY=VALUE")?;
                let (key, value) = pair.split_once('=').ok_or("--env needs KEY=VALUE")?;
                if key.is_empty() || value.contains(['\t', '\n', '\u{1f}']) {
                    return Err("--env needs KEY=VALUE without tabs".into());
                }
                open.env.push((key.to_string(), value.to_string()));
            }
            word => words.push(word),
        }
    }
    open.command = match words.as_slice() {
        [] => return Err(USAGE.into()),
        [command] => command.to_string(),
        words => {
            let quoted: Vec<String> = words.iter().map(|word| shell_quote(word)).collect();
            quoted.join(" ")
        }
    };
    if open.command.contains(['\t', '\n']) {
        return Err("the command must not hold tabs or line ends".into());
    }
    Ok(open)
}

fn open(args: &[String]) -> Result<(), String> {
    let open = parse_open(args)?;
    println!("{}", ask_main(&open.to_line())?);
    Ok(())
}

fn list(args: &[String]) -> Result<(), String> {
    let rows = rows()?;
    match args {
        [] if rows.is_empty() => println!("no invisible windows"),
        [] => println!("{}", table(&rows)),
        [flag] if flag == "--json" => {
            let rows: Vec<String> = rows.iter().map(Row::json).collect();
            println!("[{}]", rows.join(","));
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn info(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err(USAGE.into());
    };
    let row = rows()?
        .into_iter()
        .find(|row| row.name == *name)
        .ok_or_else(|| format!("no invisible window `{name}`"))?;
    println!("name     {}", row.name);
    println!("state    {}", row.state);
    println!("pid      {}", row.group);
    println!("display  {}", row.display);
    println!("size     {}", row.size);
    println!("age      {}", age(row.seconds));
    println!("command  {}", row.command);
    if let Some(dir) = super::manager::state_dir(&row.name) {
        println!("log      {}", dir.join("shadow.log").display());
    }
    if row.display != "-" {
        println!("windows  {}", ask_display(&row.display, "windows")?);
    }
    Ok(())
}

fn resize(args: &[String]) -> Result<(), String> {
    let [name, size] = args else {
        return Err(USAGE.into());
    };
    let (w, h) = parse_size(size).ok_or("expected WxH, such as 1280x800")?;
    ask_shadow(name, &format!("shadow resize {w}x{h}"))?;
    ask_main(&format!("vwm resized {name} {w}x{h}"))?;
    Ok(())
}

fn close(args: &[String]) -> Result<(), String> {
    let kill = args.iter().any(|arg| arg == "--kill");
    let names: Vec<&String> = args.iter().filter(|arg| *arg != "--kill").collect();
    if names.is_empty() || names.iter().any(|name| name.starts_with('-')) {
        return Err(USAGE.into());
    }
    for name in names {
        let kill = if kill { " kill" } else { "" };
        ask_main(&format!("vwm close {name}{kill}"))?;
    }
    Ok(())
}

/// A connection to the control socket of a shadow, for several requests in a row.
pub struct Session {
    reader: BufReader<UnixStream>,
}

impl Session {
    /// To the shadow behind this name.
    pub fn of(name: &str) -> Result<Self, String> {
        Self::on(&display_of(name)?)
    }

    /// To the wirbel of this Wayland display.
    pub fn on(display: &str) -> Result<Self, String> {
        let path = socket_path(display.as_ref()).ok_or("cannot tell where the socket is")?;
        let stream = UnixStream::connect(&path)
            .map_err(|err| format!("no wirbel on {display}: {err} ({})", path.display()))?;
        let timeout = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
        timeout.map_err(|err| format!("failed to set a timeout: {err}"))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Sends one request and returns its answer without `ok `.
    pub fn ask(&mut self, request: &str) -> Result<String, String> {
        self.reader
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .map_err(|err| format!("failed to send: {err}"))?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("the invisible window went away".into()),
            Ok(_) => answer_of(line.trim_end_matches(['\n', '\r'])),
            Err(err) => Err(format!("no answer: {err}")),
        }
    }

    /// Where the pointer is.
    pub fn position(&mut self) -> Result<(f64, f64), String> {
        let answer = self.ask("position")?;
        let mut words = answer.split_ascii_whitespace().skip(1);
        let mut number = || words.next().and_then(|word| word.parse::<f64>().ok());
        match (number(), number()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!("unexpected answer `{answer}`")),
        }
    }

    /// Moves the pointer to a place, in steps over `time` if that is not zero.
    pub fn glide(&mut self, (x, y): (f64, f64), time: Duration) -> Result<(), String> {
        if !time.is_zero() {
            let (from_x, from_y) = self.position()?;
            let steps = (time.as_millis() / GLIDE_STEP.as_millis()).max(1) as u32;
            for step in 1..steps {
                let part = f64::from(step) / f64::from(steps);
                let (sx, sy) = (from_x + (x - from_x) * part, from_y + (y - from_y) * part);
                self.ask(&format!("inject at {sx:?} {sy:?}"))?;
                std::thread::sleep(GLIDE_STEP);
            }
        }
        self.ask(&format!("inject at {x:?} {y:?}")).map(drop)
    }

    /// Presses a button, holds it as long as a finger would, and lets go.
    pub fn press(&mut self, button: &str) -> Result<(), String> {
        self.ask(&format!("inject button {button} down"))?;
        std::thread::sleep(CLICK_HOLD);
        self.ask(&format!("inject button {button} up")).map(drop)
    }
}

/// How long a click holds the button: a program that looks once per frame has to see it down.
const CLICK_HOLD: Duration = Duration::from_millis(30);
/// Between the clicks of a double click.
const CLICK_GAP: Duration = Duration::from_millis(60);
/// How often a slow move sets the pointer.
const GLIDE_STEP: Duration = Duration::from_millis(10);
/// How long a drag takes, so that the program sees it as one.
const DRAG_TIME: Duration = Duration::from_millis(300);
/// The most text one request carries: a request line is at most 4096 bytes.
const TYPE_CHUNK: usize = 500;

/// The name of a key as the shadow takes it: names people type are turned into the keysym.
pub fn key_name(name: &str) -> String {
    let known = match name.to_ascii_uppercase().as_str() {
        "ENTER" | "RETURN" => "Return",
        "ESC" | "ESCAPE" => "Escape",
        "DEL" | "DELETE" => "Delete",
        "BACKSPACE" => "BackSpace",
        "TAB" => "Tab",
        "SPACE" => "space",
        "UP" => "Up",
        "DOWN" => "Down",
        "LEFT" => "Left",
        "RIGHT" => "Right",
        "HOME" => "Home",
        "END" => "End",
        "PGUP" | "PAGEUP" => "Page_Up",
        "PGDN" | "PAGEDOWN" => "Page_Down",
        "INS" | "INSERT" => "Insert",
        _ => return name.to_string(),
    };
    known.to_string()
}

/// The request that presses `key`: one key, or a combination like `CTRL+SHIFT+t`.
pub fn key_request(key: &str) -> String {
    match key.rsplit_once('+') {
        Some((modifiers, name)) if !modifiers.is_empty() && !name.is_empty() => {
            format!("inject chord {modifiers}+{}", key_name(name))
        }
        _ => format!("inject tap {}", key_name(key)),
    }
}

/// The requests that type `text`: pieces that fit a request, Return for line ends.
pub fn type_requests(text: &str) -> Vec<String> {
    let mut requests = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            requests.push("inject tap Return".to_string());
        }
        let chars: Vec<char> = line.chars().filter(|c| *c != '\r').collect();
        for piece in chars.chunks(TYPE_CHUNK) {
            requests.push(format!("inject type {}", piece.iter().collect::<String>()));
        }
    }
    requests
}

fn number(text: &str) -> Result<f64, String> {
    let value: f64 = text
        .parse()
        .map_err(|_| format!("expected a number, got `{text}`"))?;
    value
        .is_finite()
        .then_some(value)
        .ok_or(format!("not a number: {text}"))
}

fn button(name: Option<&String>) -> Result<&str, String> {
    match name.map(String::as_str) {
        None => Ok("left"),
        Some(name @ ("left" | "right" | "middle")) => Ok(name),
        Some(other) => Err(format!("expected left, right or middle, got `{other}`")),
    }
}

fn type_text(args: &[String]) -> Result<(), String> {
    let [name, words @ ..] = args else {
        return Err(USAGE.into());
    };
    if words.is_empty() {
        return Err(USAGE.into());
    }
    let mut session = Session::of(name)?;
    for request in type_requests(&words.join(" ")) {
        session.ask(&request)?;
    }
    Ok(())
}

fn keys(args: &[String]) -> Result<(), String> {
    let [name, keys @ ..] = args else {
        return Err(USAGE.into());
    };
    if keys.is_empty() {
        return Err(USAGE.into());
    }
    let mut session = Session::of(name)?;
    for key in keys {
        session.ask(&key_request(key))?;
    }
    Ok(())
}

fn click(args: &[String]) -> Result<(), String> {
    let (name, x, y, rest) = match args {
        [name, x, y, rest @ ..] if rest.len() <= 2 => (name, number(x)?, number(y)?, rest),
        _ => return Err(USAGE.into()),
    };
    // The button and the count may come in either order or not at all.
    let count = rest.iter().find_map(|word| word.parse::<u32>().ok());
    let named = rest.iter().find(|word| word.parse::<u32>().is_err());
    let button = button(named)?;
    let count = count.unwrap_or(1).clamp(1, 10);
    let mut session = Session::of(name)?;
    session.glide((x, y), Duration::ZERO)?;
    for index in 0..count {
        if index > 0 {
            std::thread::sleep(CLICK_GAP);
        }
        session.press(button)?;
    }
    Ok(())
}

fn move_pointer(args: &[String]) -> Result<(), String> {
    let (name, x, y, time) = match args {
        [name, x, y] => (name, number(x)?, number(y)?, 0.0),
        [name, x, y, time] => (name, number(x)?, number(y)?, number(time)?),
        _ => return Err(USAGE.into()),
    };
    let time = Duration::try_from_secs_f64(time).map_err(|_| "the time cannot be negative")?;
    Session::of(name)?.glide((x, y), time)
}

fn drag(args: &[String]) -> Result<(), String> {
    let (name, from, to, button) = match args {
        [name, x1, y1, x2, y2, rest @ ..] if rest.len() <= 1 => (
            name,
            (number(x1)?, number(y1)?),
            (number(x2)?, number(y2)?),
            button(rest.first())?,
        ),
        _ => return Err(USAGE.into()),
    };
    let mut session = Session::of(name)?;
    session.glide(from, Duration::ZERO)?;
    session.ask(&format!("inject button {button} down"))?;
    std::thread::sleep(CLICK_HOLD);
    let moved = session.glide(to, DRAG_TIME);
    std::thread::sleep(CLICK_HOLD);
    // Whatever went wrong on the way, the button comes up again.
    session.ask(&format!("inject button {button} up"))?;
    moved
}

fn scroll(args: &[String]) -> Result<(), String> {
    let (name, steps, place) = match args {
        [name, steps] => (name, number(steps)?, None),
        [name, steps, x, y] => (name, number(steps)?, Some((number(x)?, number(y)?))),
        _ => return Err(USAGE.into()),
    };
    let mut session = Session::of(name)?;
    if let Some(place) = place {
        session.glide(place, Duration::ZERO)?;
    }
    session.ask(&format!("inject wheel {steps:?}")).map(drop)
}

/// `path` from the root, so that the shadow, which runs in another folder, finds it.
fn absolute(path: &str) -> Result<String, String> {
    let path = std::path::absolute(path).map_err(|err| format!("{path}: {err}"))?;
    let text = path.to_str().ok_or("the path is no valid text")?;
    if text.contains(['\t', '\n']) {
        return Err("the path must not hold tabs or line ends".into());
    }
    Ok(text.to_string())
}

/// What `shot` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotArgs {
    pub name: String,
    pub file: Option<String>,
    pub region: Option<String>,
    pub cursor: bool,
}

pub fn parse_shot(args: &[String]) -> Result<ShotArgs, String> {
    let mut plain = Vec::new();
    let (mut region, mut cursor) = (None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cursor" => cursor = true,
            "--region" => {
                let text = args.next().ok_or("--region needs X,Y,W,H")?;
                super::shot::parse_region(text)?;
                region = Some(text.clone());
            }
            flag if flag.starts_with("--") => return Err(USAGE.into()),
            _ => plain.push(arg.clone()),
        }
    }
    let mut plain = plain.into_iter();
    let name = plain.next().ok_or(USAGE)?;
    let file = plain.next();
    if plain.next().is_some() {
        return Err(USAGE.into());
    }
    Ok(ShotArgs {
        name,
        file,
        region,
        cursor,
    })
}

/// Takes a picture of the shadow behind `name` into `file` and returns its size, `WxH`.
pub fn take_shot(
    name: &str,
    file: &str,
    region: Option<&str>,
    cursor: bool,
) -> Result<String, String> {
    let request = format!(
        "shadow shot\t{file}\t{}\t{}",
        region.unwrap_or("-"),
        if cursor { "cursor" } else { "-" }
    );
    ask_shadow(name, &request)
}

fn shot(args: &[String]) -> Result<(), String> {
    let shot = parse_shot(args)?;
    let file = shot.file.unwrap_or_else(|| format!("{}.png", shot.name));
    let file = absolute(&file)?;
    take_shot(&shot.name, &file, shot.region.as_deref(), shot.cursor)?;
    println!("{file}");
    Ok(())
}

/// A file for passing text to or from a shadow, private to this user and gone afterwards.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos());
        let name = format!("wirbel-vwm-clip-{}-{nanos}", std::process::id());
        Ok(Self(std::path::PathBuf::from(dir).join(name)))
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The text in the clipboard of the shadow behind `name`.
pub fn clip_get(name: &str) -> Result<String, String> {
    let scratch = Scratch::new()?;
    ask_shadow(name, &format!("shadow clip get\t{}", scratch.path()))?;
    let bytes = std::fs::read(&scratch.0).map_err(|err| format!("no clipboard text: {err}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Puts `text` in the clipboard of the shadow behind `name`.
pub fn clip_set(name: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let scratch = Scratch::new()?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&scratch.0)
        .map_err(|err| format!("no scratch file: {err}"))?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("no scratch file: {err}"))?;
    drop(file);
    ask_shadow(name, &format!("shadow clip set\t{}", scratch.path())).map(drop)
}

fn clip(args: &[String]) -> Result<(), String> {
    match args {
        [name, get] if get == "get" => {
            let text = clip_get(name)?;
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        [name, set, words @ ..] if set == "set" && !words.is_empty() => {
            clip_set(name, &words.join(" "))
        }
        _ => Err(USAGE.into()),
    }
}

fn paste(args: &[String]) -> Result<(), String> {
    let (name, key, words) = match args {
        [name, flag, key, words @ ..] if flag == "--key" => (name, key.as_str(), words),
        [name, words @ ..] => (name, "CTRL+v", words),
        _ => return Err(USAGE.into()),
    };
    if words.is_empty() {
        return Err(USAGE.into());
    }
    clip_set(name, &words.join(" "))?;
    ask_shadow(name, &key_request(key)).map(drop)
}

/// `wirbel vwm ARGS`.
pub fn run(args: &[String]) -> Result<(), String> {
    // `invisible` is an optional word, and two commands have a second name.
    let args = match args {
        [word, rest @ ..] if word == "invisible" => rest,
        args => args,
    };
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    match command.as_str() {
        "open" | "create" => open(rest),
        "list" => list(rest),
        "info" => info(rest),
        "resize" => resize(rest),
        "close" => close(rest),
        "type" => type_text(rest),
        "key" => keys(rest),
        "click" => click(rest),
        "move" => move_pointer(rest),
        "drag" => drag(rest),
        "scroll" => scroll(rest),
        "shot" | "screenshot" => shot(rest),
        "clip" => clip(rest),
        "paste" => paste(rest),
        "--help" | "-h" | "help" => {
            println!("{USAGE}\n\n{HELP}");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn open_takes_options_anywhere_before_the_dashes() {
        let open = parse_open(&words("firefox --name browser1")).unwrap();
        assert_eq!(open.name.as_deref(), Some("browser1"));
        assert_eq!(open.command, "firefox");
        assert_eq!(open.size, DEFAULT_SIZE);
        assert!(!open.share_bus);

        let open = parse_open(&words(
            "--size 800x600 --share-bus --env A=b -- kitty --name x",
        ));
        let open = open.unwrap();
        assert_eq!(open.size, (800, 600));
        assert!(open.share_bus);
        assert_eq!(open.env, vec![("A".to_string(), "b".to_string())]);
        assert_eq!(open.command, "kitty --name x");
        assert_eq!(open.name, None);
    }

    #[test]
    fn open_quotes_several_words_and_keeps_one() {
        let args = vec!["sh".to_string(), "-c".to_string(), "echo a b".to_string()];
        assert_eq!(parse_open(&args).unwrap().command, "sh -c 'echo a b'");
        let args = vec!["foot -e 'top -d 1'".to_string()];
        assert_eq!(parse_open(&args).unwrap().command, "foot -e 'top -d 1'");
    }

    #[test]
    fn open_refuses_nonsense() {
        assert!(parse_open(&[]).is_err());
        assert!(parse_open(&words("--name")).is_err());
        assert!(parse_open(&words("--name a.b foot")).is_err());
        assert!(parse_open(&words("--size big foot")).is_err());
        assert!(parse_open(&words("--env nothing foot")).is_err());
        assert!(parse_open(&words("--name x")).is_err());
    }

    #[test]
    fn rows_and_tables() {
        let line = "entry\tb1\tshadow\tready\t4242\twayland-3\t1280x800\t75\tfirefox -x\ty";
        let row = Row::parse(line).unwrap();
        assert_eq!(row.name, "b1");
        assert_eq!(row.command, "firefox -x\ty");
        assert_eq!(row.seconds, 75);
        assert!(Row::parse("entry\tb1").is_none());
        let table = table(std::slice::from_ref(&row));
        let mut lines = table.lines();
        assert_eq!(
            lines.next(),
            Some("NAME  STATE  PID   SIZE      AGE  COMMAND")
        );
        assert_eq!(
            lines.next(),
            Some("b1    ready  4242  1280x800  1m   firefox -x\ty")
        );
        assert!(
            row.json()
                .starts_with("{\"name\":\"b1\",\"kind\":\"shadow\"")
        );
        assert!(row.json().contains("\"pid\":4242"));
    }

    #[test]
    fn keys_by_the_names_people_type() {
        assert_eq!(key_request("ENTER"), "inject tap Return");
        assert_eq!(key_request("esc"), "inject tap Escape");
        assert_eq!(key_request("a"), "inject tap a");
        assert_eq!(key_request("F5"), "inject tap F5");
        assert_eq!(key_request("CTRL+l"), "inject chord CTRL+l");
        assert_eq!(key_request("CTRL+SHIFT+tab"), "inject chord CTRL+SHIFT+Tab");
        // A lone plus is the plus key, not a combination without a key.
        assert_eq!(key_request("+"), "inject tap +");
    }

    #[test]
    fn text_is_typed_in_pieces_with_return_between_lines() {
        assert_eq!(type_requests("hello"), ["inject type hello"]);
        assert_eq!(
            type_requests("a b\r\nc\n"),
            [
                "inject type a b",
                "inject tap Return",
                "inject type c",
                "inject tap Return"
            ]
        );
        let long = "x".repeat(TYPE_CHUNK + 1);
        let requests = type_requests(&long);
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.len() < 4096));
    }

    #[test]
    fn shots_parse() {
        let shot = parse_shot(&words("b1 out.png --region 1,2,3,4 --cursor")).unwrap();
        assert_eq!(shot.name, "b1");
        assert_eq!(shot.file.as_deref(), Some("out.png"));
        assert_eq!(shot.region.as_deref(), Some("1,2,3,4"));
        assert!(shot.cursor);
        let shot = parse_shot(&words("--cursor b1")).unwrap();
        assert!(shot.file.is_none() && shot.region.is_none());
        assert!(parse_shot(&words("b1 a.png b.png")).is_err());
        assert!(parse_shot(&words("b1 --region 1,2")).is_err());
        assert!(parse_shot(&words("b1 --what")).is_err());
        assert!(parse_shot(&[]).is_err());
    }

    #[test]
    fn answers() {
        assert_eq!(answer_of("ok"), Ok(String::new()));
        assert_eq!(answer_of("ok wayland-2"), Ok("wayland-2".into()));
        assert_eq!(answer_of("error no such"), Err("no such".into()));
        assert_eq!(answer_of("1280x800"), Ok("1280x800".into()));
        assert_eq!(age(5), "5s");
        assert_eq!(age(3725), "1h2m");
    }
}
