//! `wirbel vwm ...`: the commands for invisible windows. Starting, listing and closing go to the
//! visible compositor of `WAYLAND_DISPLAY`; everything that drives one window goes straight to
//! the control socket of its shadow.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use super::{
    DEFAULT_SIZE,
    manager::{Open, shell_quote},
    text::{Line, by_clipboard, matching, recognise},
    valid_name,
    wait::{Condition, DEFAULT_TIMEOUT, Status, WaitError, Watcher, color_name, glob},
};
use crate::{
    backend::headless::parse_size,
    cli::ask_lines,
    ipc::{json_string, socket_path},
};

pub const USAGE: &str = "usage: wirbel vwm open [--name NAME] [--size WxH] [--share-bus] [--sound] [--env KEY=VALUE]... [--] COMMAND...
       wirbel vwm list [--json]
       wirbel vwm info NAME
       wirbel vwm resize NAME WxH
       wirbel vwm close NAME...|all [--kill]
       wirbel vwm adopt WINDOW|focused [NAME]
       wirbel vwm release NAME
       wirbel vwm type NAME TEXT...
       wirbel vwm key NAME KEY|MODS+KEY...
       wirbel vwm click NAME X Y [left|right|middle] [COUNT]
       wirbel vwm move NAME X Y [SECONDS]
       wirbel vwm drag NAME X1 Y1 X2 Y2 [left|right|middle]
       wirbel vwm scroll NAME STEPS [X Y]
       wirbel vwm shot NAME [FILE.png] [--region X,Y,W,H] [--cursor]
       wirbel vwm clip NAME get | set TEXT...
       wirbel vwm paste NAME [--key MODS+KEY] TEXT...
       wirbel vwm wait NAME CONDITION... [--timeout SECONDS]
       wirbel vwm status NAME
       wirbel vwm events NAME
       wirbel vwm pixel NAME X Y
       wirbel vwm find NAME FILE.png [SIMILARITY]
       wirbel vwm text NAME [--via a11y|ocr|clipboard] [--find GLOB] [--where] [--lang LANGUAGES]
       wirbel vwm click-text NAME GLOB [left|right|middle]
       wirbel vwm ui NAME tree | press LABEL | set LABEL TEXT...
       wirbel vwm audio NAME listen on|off | level [SECONDS] | record FILE.wav [SECONDS]";

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
      adopt      park a window that is open on the desktop: it leaves its workspace and the
                 taskbar and keeps running out of sight. WINDOW is its id from `wirbel vwm
                 windows`, or `focused`. A parked window takes shot, release and close; typing
                 and clicking need a window started with open
      release    put a parked window on the current workspace again
      windows    the windows on the desktop with their ids, as JSON
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
      wait       wait until CONDITION holds, at most 30 seconds or what --timeout says, and
                 print what it answers. Conditions: ready, window [GLOB], title GLOB, closed,
                 exited, stable [MS], changed [X,Y,W,H], pixel X Y #RRGGBB [TOLERANCE],
                 image FILE.png [SIMILARITY], cursor NAME, clipboard, file PATH. A GLOB matches
                 the whole text with * and ?, without them it matches a part of the text
      status     how the window is right now
      events     print what happens to the window, until it is gone or Ctrl+C
      pixel      print the colour of a pixel as #rrggbb
      find       print where the picture FILE.png shows in the window, the middle of it
      text       print the text the window shows. --via ocr reads a picture of it with the
                 program tesseract (any program, with places, may misread; --lang eng+deu picks
                 languages), --via clipboard presses CTRL+a and CTRL+c in the window and reads
                 its clipboard (exact, but changes the selection). Without --via it is ocr if
                 tesseract is installed, else the clipboard. --find prints only the lines that
                 match, and fails if none does. --where puts the middle of each line before
                 it, as X Y (ocr only)
                 --via a11y asks the program itself through the accessibility interface:
                 exact and quick where the program offers it (GTK, Qt, browsers)
      ui         the controls of the window by the accessibility interface: tree prints them,
                 press does what a click on the one named LABEL does, set replaces the text of
                 the field named LABEL. LABEL is a GLOB on the name or the text
      audio      the sound of the window. A program started invisibly is silent: it plays into
                 a sink of its own (open --sound lets it play on the speakers instead).
                 listen on routes that sink to the speakers and off stops that, level prints
                 how loud it is right now from 0 to 1, record writes it to a WAV file for
                 SECONDS or until Ctrl+C. The wait conditions sound and silence [MS] use it
      click-text click the middle of the first line that matches GLOB (ocr)
Exit status: 0 done, 1 failed, 2 wrong usage, 3 the time of a wait ran out.
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
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|row| {
            [
                row.name.clone(),
                row.kind.clone(),
                row.state.clone(),
                row.group.clone(),
                row.size.clone(),
                age(row.seconds),
                row.command.clone(),
            ]
        })
        .collect();
    // For a parked window the id is that of the window and the last column its title.
    let header = ["NAME", "KIND", "STATE", "ID", "SIZE", "AGE", "COMMAND"];
    let width = |column: usize| {
        let longest = cells.iter().map(|row| row[column].chars().count()).max();
        longest.unwrap_or(0).max(header[column].len())
    };
    let widths: Vec<usize> = (0..6).map(width).collect();
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
    let sound = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--sound");
    let args: Vec<String> = match args.iter().position(|arg| arg == "--sound") {
        Some(at) if sound => args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .map(|(_, a)| a.clone())
            .collect(),
        _ => args.to_vec(),
    };
    let mut open = parse_open(&args)?;
    let taken: Vec<String> = rows()
        .map(|rows| rows.into_iter().map(|row| row.name).collect())
        .unwrap_or_default();
    super::audio::sweep(&taken);
    // The sink needs the name before the window has one: make it up here as the compositor
    // would. Without a sound server the program plays as it would anyway.
    if !sound {
        let name = open.name.clone().unwrap_or_else(|| {
            let base: String = super::manager::program_of(&open.command)
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(24)
                .collect();
            let base = if base.is_empty() {
                "window".to_string()
            } else {
                base
            };
            (1..)
                .map(|n| format!("{base}{n}"))
                .find(|name| !taken.contains(name))
                .unwrap_or_default()
        });
        if !taken.contains(&name)
            && let Ok(sink) = super::audio::create_sink(&name)
        {
            open.name = Some(name);
            open.env.push(("PULSE_SINK".into(), sink.clone()));
            open.env.push(("PIPEWIRE_NODE".into(), sink));
        }
    }
    emit(ask_main(&open.to_line())?.to_string());
    Ok(())
}

fn list(args: &[String]) -> Result<(), String> {
    let rows = rows()?;
    match args {
        [] if rows.is_empty() => emit("no invisible windows".to_string()),
        [] => emit(table(&rows).to_string()),
        [flag] if flag == "--json" => {
            let rows: Vec<String> = rows.iter().map(Row::json).collect();
            emit(format!("[{}]", rows.join(",")));
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
    emit(format!("name     {}", row.name));
    emit(format!("state    {}", row.state));
    emit(format!("pid      {}", row.group));
    emit(format!("display  {}", row.display));
    emit(format!("size     {}", row.size));
    emit(format!("age      {}", age(row.seconds)));
    emit(format!("command  {}", row.command));
    if let Some(dir) = super::manager::state_dir(&row.name) {
        emit(format!("log      {}", dir.join("shadow.log").display()));
    }
    if row.display != "-" {
        emit(format!(
            "windows  {}",
            ask_display(&row.display, "windows")?
        ));
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
    for name in &names {
        let kill = if kill { " kill" } else { "" };
        ask_main(&format!("vwm close {name}{kill}"))?;
    }
    // Back only once they are gone, so that the name is free for the next one.
    let start = std::time::Instant::now();
    loop {
        let left = rows()?;
        let all = names.iter().any(|name| *name == "all");
        let gone = match all {
            true => left.is_empty(),
            false => !left.iter().any(|row| names.contains(&&row.name)),
        };
        if gone {
            let closed: Vec<String> = left.iter().map(|row| row.name.clone()).collect();
            super::audio::sweep(&closed);
            return Ok(());
        }
        if start.elapsed() > CLOSE_PATIENCE {
            return Err("still closing, `--kill` ends it at once".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long `close` waits for the windows to go: longer than the compositor gives them.
const CLOSE_PATIENCE: Duration = Duration::from_secs(12);

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
    let region = region.unwrap_or("-");
    // A parked window is photographed by the visible compositor, a shadow by itself.
    let parked = ask_main(&format!("vwm shot\t{name}\t{file}\t{region}"));
    match parked {
        Err(err) if err.starts_with("no parked window") => {}
        answer => return answer,
    }
    let cursor = if cursor { "cursor" } else { "-" };
    ask_shadow(name, &format!("shadow shot\t{file}\t{region}\t{cursor}"))
}

fn shot(args: &[String]) -> Result<(), String> {
    let shot = parse_shot(args)?;
    let file = shot.file.unwrap_or_else(|| format!("{}.png", shot.name));
    let file = absolute(&file)?;
    take_shot(&shot.name, &file, shot.region.as_deref(), shot.cursor)?;
    emit(file.to_string());
    Ok(())
}

/// A file for passing text or a picture to or from a shadow, in the runtime folder of this
/// user and gone afterwards.
pub(crate) struct Scratch(std::path::PathBuf);

impl Scratch {
    pub(crate) fn new(kind: &str) -> Result<Self, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos());
        let name = format!("wirbel-vwm-{}-{nanos}-{kind}", std::process::id());
        Ok(Self(std::path::PathBuf::from(dir).join(name)))
    }

    pub(crate) fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    pub(crate) fn file(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The text in the clipboard of the shadow behind `name`.
pub fn clip_get(name: &str) -> Result<String, String> {
    clip_get_on(&display_of(name)?)
}

/// The text in the clipboard of the shadow on this display.
pub fn clip_get_on(display: &str) -> Result<String, String> {
    let scratch = Scratch::new("clip")?;
    let request = format!("shadow clip get\t{}", scratch.path());
    answer_of(&ask_display(display, &request)?)?;
    let bytes = std::fs::read(&scratch.0).map_err(|err| format!("no clipboard text: {err}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Puts `text` in the clipboard of the shadow behind `name`.
pub fn clip_set(name: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let scratch = Scratch::new("clip")?;
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
            emit(clip_get(name)?.trim_end_matches('\n').to_string());
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

/// The condition and the time limit of `wait NAME CONDITION... [--timeout SECONDS]`.
pub fn parse_wait(args: &[String]) -> Result<(Condition, Duration), String> {
    let mut timeout = DEFAULT_TIMEOUT;
    let mut words = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--timeout" {
            let seconds = args.next().ok_or("--timeout needs seconds")?;
            let seconds = number(seconds.trim_end_matches('s'))?;
            timeout = Duration::try_from_secs_f64(seconds)
                .map_err(|_| "the time limit cannot be negative")?;
        } else {
            words.push(arg.as_str());
        }
    }
    Ok((Condition::parse(&words)?, timeout))
}

fn wait(args: &[String]) -> Result<(), String> {
    let [name, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let (condition, timeout) = parse_wait(rest)?;
    let answer = super::wait::wait(name, &condition, timeout, &mut || Ok(()));
    let answer = answer.map_err(|err| err.message())?;
    if !answer.is_empty() {
        emit(answer.to_string());
    }
    Ok(())
}

fn status(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err(USAGE.into());
    };
    let status = Watcher::of(name)?.status()?;
    emit(format!("windows    {}", status.windows));
    emit(format!("title      {}", status.title));
    match status.exited {
        Some(code) => emit(format!("program    exited with {code}")),
        None => emit("program    running".to_string()),
    }
    emit(format!("cursor     {}", status.cursor));
    emit(format!("frames     {}", status.damage));
    emit(format!("quiet      {} ms", status.quiet.as_millis()));
    emit(format!("clipboard  {}", status.clipboard));
    Ok(())
}

/// How long the picture has to rest before `events` calls it quiet.
const QUIET: Duration = Duration::from_millis(300);

/// What changed between two looks at a window, as lines for `events`.
pub fn changes(old: &Status, new: &Status, was_busy: bool) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    if old.windows != new.windows {
        lines.push(format!("windows {}", new.windows));
    }
    if old.title != new.title {
        lines.push(format!("title {}", new.title));
    }
    if old.cursor != new.cursor {
        lines.push(format!("cursor {}", new.cursor));
    }
    if old.clipboard != new.clipboard {
        lines.push("clipboard".into());
    }
    if let (None, Some(code)) = (old.exited, new.exited) {
        lines.push(format!("exited {code}"));
    }
    let busy = match was_busy {
        false => old.damage != new.damage,
        true => new.quiet < QUIET,
    };
    if busy != was_busy {
        lines.push(if busy { "busy" } else { "quiet" }.into());
    }
    (lines, busy)
}

fn events(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err(USAGE.into());
    };
    let mut watcher = Watcher::of(name)?;
    let mut old = watcher.status()?;
    let mut busy = false;
    emit(format!("windows {}", old.windows));
    if !old.title.is_empty() {
        emit(format!("title {}", old.title));
    }
    loop {
        std::thread::sleep(Duration::from_millis(40));
        let Ok(new) = watcher.status() else {
            emit("gone".to_string());
            return Ok(());
        };
        let (lines, now_busy) = changes(&old, &new, busy);
        for line in lines {
            emit(line.to_string());
        }
        (old, busy) = (new, now_busy);
    }
}

fn pixel(args: &[String]) -> Result<(), String> {
    let [name, x, y] = args else {
        return Err(USAGE.into());
    };
    let (x, y) = (number(x)? as i32, number(y)? as i32);
    let mut watcher = Watcher::of(name)?;
    let status = watcher.status()?;
    let color = watcher.picture(&status)?.pixel(x, y);
    emit(color_name(color.ok_or("the pixel is outside the window")?).to_string());
    Ok(())
}

fn find(args: &[String]) -> Result<(), String> {
    let (name, words) = match args {
        [name, rest @ ..] if (1..=2).contains(&rest.len()) => (name, rest),
        _ => return Err(USAGE.into()),
    };
    let mut condition = vec!["image"];
    condition.extend(words.iter().map(String::as_str));
    let condition = Condition::parse(&condition)?;
    let found = super::wait::wait(name, &condition, Duration::ZERO, &mut || Ok(()));
    match found {
        Ok(place) => emit(place.to_string()),
        Err(WaitError::Timeout(_)) => return Err("the picture does not show in the window".into()),
        Err(err) => return Err(err.message()),
    }
    Ok(())
}

/// The lines of text in the window on this display, read from a picture of it.
pub fn recognise_on(display: &str, language: Option<&str>) -> Result<Vec<Line>, String> {
    let scratch = Scratch::new("ocr.png")?;
    answer_of(&ask_display(
        display,
        &format!("shadow shot\t{}", scratch.path()),
    )?)?;
    recognise(scratch.file(), language)
}

fn tesseract_installed() -> bool {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths).any(|dir| dir.join("tesseract").is_file())
}

fn text(args: &[String]) -> Result<(), String> {
    let (mut via, mut pattern, mut place, mut language) = (None, None, false, None);
    let mut name = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or(format!("{arg} needs {what}"));
        match arg.as_str() {
            "--via" => via = Some(value("ocr or clipboard")?.clone()),
            "--find" | "find" => pattern = Some(value("a pattern")?.clone()),
            "--lang" => language = Some(value("languages")?.clone()),
            "--where" => place = true,
            word if name.is_none() && !word.starts_with("--") => name = Some(word.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let display = display_of(&name.ok_or(USAGE)?)?;
    let via = via.unwrap_or_else(|| {
        let ocr = tesseract_installed() || place;
        if ocr { "ocr" } else { "clipboard" }.to_string()
    });
    let lines: Vec<String> = match via.as_str() {
        "ocr" => {
            let lines = recognise_on(&display, language.as_deref())?;
            let found = matching(&lines, pattern.as_deref());
            let show = |line: &&Line| match place {
                true => format!("{} {} {}", line.middle().0, line.middle().1, line.text),
                false => line.text.clone(),
            };
            found.iter().map(show).collect()
        }
        "a11y" if place => return Err("--where needs --via ocr".into()),
        "a11y" => {
            let lines = super::a11y::text_of(&super::a11y::Tree::of(&display)?.items()?);
            let fits = |line: &String| pattern.as_deref().is_none_or(|p| glob(p, line));
            lines.into_iter().filter(fits).collect()
        }
        "clipboard" if place => return Err("--where needs --via ocr".into()),
        "clipboard" => {
            let text = by_clipboard(&display, &["CTRL+a", "CTRL+c"])?;
            let fits = |line: &&str| pattern.as_deref().is_none_or(|p| glob(p, line));
            text.lines().filter(fits).map(str::to_string).collect()
        }
        other => {
            return Err(format!(
                "expected --via a11y, ocr or clipboard, got `{other}`"
            ));
        }
    };
    if lines.is_empty() && pattern.is_some() {
        return Err("no such text in the window".into());
    }
    emit(lines.join("\n"));
    Ok(())
}

fn ui(args: &[String]) -> Result<(), String> {
    use super::a11y::{Tree, find};
    let [name, command, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let tree = Tree::of(&display_of(name)?)?;
    let items = tree.items()?;
    match (command.as_str(), rest) {
        ("tree", []) => {
            let lines: Vec<String> = items.iter().map(super::a11y::Item::line).collect();
            emit(lines.join("\n"));
            Ok(())
        }
        ("press", [label]) => {
            let item = find(&items, label, |item| item.actions);
            tree.press(item.ok_or_else(|| format!("no control `{label}` to press"))?)
        }
        ("set", [label, words @ ..]) if !words.is_empty() => {
            let item = find(&items, label, |item| item.editable);
            let item = item.ok_or_else(|| format!("no field `{label}` to fill"))?;
            tree.set(item, &words.join(" "))
        }
        _ => Err(USAGE.into()),
    }
}

fn audio(args: &[String]) -> Result<(), String> {
    use super::audio::{Listener, listen, record};
    let seconds = |text: &String| {
        let time = Duration::try_from_secs_f64(number(text.trim_end_matches('s'))?);
        time.map_err(|_| "the time cannot be negative".to_string())
    };
    match args {
        [name, word, state] if word == "listen" && (state == "on" || state == "off") => {
            listen(name, state == "on")
        }
        [name, word] if word == "level" => {
            let level = Listener::of(name)?.level(Duration::from_millis(300))?;
            emit(format!("{level:.4}"));
            Ok(())
        }
        [name, word, time] if word == "level" => {
            let level = Listener::of(name)?.level(seconds(time)?)?;
            emit(format!("{level:.4}"));
            Ok(())
        }
        [name, word, file] if word == "record" => record(name, &absolute(file)?, None),
        [name, word, file, time] if word == "record" => {
            record(name, &absolute(file)?, Some(seconds(time)?))
        }
        _ => Err(USAGE.into()),
    }
}

fn click_text(args: &[String]) -> Result<(), String> {
    let (name, pattern, rest) = match args {
        [name, pattern, rest @ ..] if rest.len() <= 1 => (name, pattern, rest),
        _ => return Err(USAGE.into()),
    };
    let button = button(rest.first())?;
    let display = display_of(name)?;
    let lines = recognise_on(&display, None)?;
    let found = matching(&lines, Some(pattern));
    let line = found.first().ok_or("no such text in the window")?;
    let (x, y) = line.middle();
    let mut session = Session::on(&display)?;
    session.glide((f64::from(x), f64::from(y)), Duration::ZERO)?;
    session.press(button)
}

/// What `wirbel vwm` exits with for this error.
pub fn exit_code(error: &str) -> i32 {
    if error.starts_with("timeout: ") {
        3
    } else if error.starts_with("usage: ") {
        2
    } else {
        1
    }
}

thread_local! {
    /// What the commands printed while `capture` runs, instead of printing it.
    static CAPTURED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Prints a line of output, or keeps it for `capture`.
fn emit(line: String) {
    let kept = CAPTURED.with(|captured| match captured.borrow_mut().as_mut() {
        Some(text) => {
            text.push_str(&line);
            text.push('\n');
            true
        }
        None => false,
    });
    if !kept {
        println!("{line}");
    }
}

/// Runs `wirbel vwm ARGS` and returns what it would print, without the last line end. For
/// macros, whose steps are these commands.
pub fn capture(args: &[String]) -> Result<String, String> {
    CAPTURED.with(|captured| *captured.borrow_mut() = Some(String::new()));
    let result = run(args);
    let text = CAPTURED
        .with(|captured| captured.borrow_mut().take())
        .unwrap_or_default();
    result.map(|()| text.trim_end_matches('\n').to_string())
}

/// The commands of `wirbel vwm` that are steps of a macro too.
pub const COMMANDS: &[&str] = &[
    "open",
    "list",
    "info",
    "resize",
    "close",
    "type",
    "key",
    "click",
    "move",
    "drag",
    "scroll",
    "shot",
    "clip",
    "paste",
    "status",
    "pixel",
    "find",
    "text",
    "click-text",
    "ui",
    "audio",
    "adopt",
    "release",
    "windows",
];

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
        "adopt" => match rest {
            [target] => ask_main(&format!("vwm adopt {target}")).map(emit),
            [target, name] => ask_main(&format!("vwm adopt {target} {name}")).map(emit),
            _ => Err(USAGE.into()),
        },
        "release" => match rest {
            [name] => ask_main(&format!("vwm release {name}")).map(drop),
            _ => Err(USAGE.into()),
        },
        "windows" => {
            let display =
                std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set")?;
            ask_display(&display, "windows").map(emit)
        }
        "type" => type_text(rest),
        "key" => keys(rest),
        "click" => click(rest),
        "move" => move_pointer(rest),
        "drag" => drag(rest),
        "scroll" => scroll(rest),
        "shot" | "screenshot" => shot(rest),
        "clip" => clip(rest),
        "paste" => paste(rest),
        "wait" => wait(rest),
        "status" => status(rest),
        "events" => events(rest),
        "pixel" => pixel(rest),
        "find" => find(rest),
        "text" => text(rest),
        "click-text" => click_text(rest),
        "ui" => ui(rest),
        "audio" => audio(rest),
        "--help" | "-h" | "help" => {
            emit(format!("{USAGE}\n\n{HELP}"));
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
            Some("NAME  KIND    STATE  ID    SIZE      AGE  COMMAND")
        );
        assert_eq!(
            lines.next(),
            Some("b1    shadow  ready  4242  1280x800  1m   firefox -x\ty")
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
    fn waits_parse_and_errors_map_to_exit_codes() {
        let (condition, timeout) = parse_wait(&words("title *Done* --timeout 2.5")).unwrap();
        assert_eq!(condition, Condition::Title("*Done*".into()));
        assert_eq!(timeout, Duration::from_millis(2500));
        let (condition, timeout) = parse_wait(&words("--timeout 20s ready")).unwrap();
        assert_eq!(
            (condition, timeout),
            (Condition::Ready, Duration::from_secs(20))
        );
        assert_eq!(parse_wait(&words("ready")).unwrap().1, DEFAULT_TIMEOUT);
        assert!(parse_wait(&words("ready --timeout")).is_err());
        assert!(parse_wait(&words("ready --timeout -1")).is_err());
        assert!(parse_wait(&words("--timeout 1")).is_err());
        assert_eq!(exit_code("timeout: 1 window(s)"), 3);
        assert_eq!(exit_code(USAGE), 2);
        assert_eq!(exit_code("no invisible window `x`"), 1);
    }

    #[test]
    fn events_are_the_differences() {
        let old = Status::parse("status 5 900 1 running default 0 One").unwrap();
        let new = Status::parse("status 6 0 2 exited:3 text 1 Two").unwrap();
        let (lines, busy) = changes(&old, &new, false);
        assert_eq!(
            lines,
            [
                "windows 2",
                "title Two",
                "cursor text",
                "clipboard",
                "exited 3",
                "busy"
            ]
        );
        assert!(busy);
        let rest = Status::parse("status 6 100 2 exited:3 text 1 Two").unwrap();
        assert_eq!(changes(&new, &rest, true), (Vec::new(), true));
        let rest = Status::parse("status 6 400 2 exited:3 text 1 Two").unwrap();
        assert_eq!(
            changes(&new, &rest, true),
            (vec!["quiet".to_string()], false)
        );
        assert_eq!(changes(&rest, &rest, false), (Vec::new(), false));
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
