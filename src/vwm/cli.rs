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
       wirbel vwm close NAME...|all [--kill]";

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
`wirbel vwm invisible create|close ...` mean the same as `wirbel vwm open|close ...`.";

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
    fn answers() {
        assert_eq!(answer_of("ok"), Ok(String::new()));
        assert_eq!(answer_of("ok wayland-2"), Ok("wayland-2".into()));
        assert_eq!(answer_of("error no such"), Err("no such".into()));
        assert_eq!(answer_of("1280x800"), Ok("1280x800".into()));
        assert_eq!(age(5), "5s");
        assert_eq!(age(3725), "1h2m");
    }
}
