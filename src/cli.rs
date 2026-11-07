//! The commands that talk to a running compositor: `wirbel pos`, `wirbel distance`, `wirbel move`,
//! `wirbel click`, `wirbel ws`, `wirbel mmsg`.

use std::{
    env,
    io::{self, BufRead, BufReader, IsTerminal, Write},
    os::unix::net::UnixStream,
    thread,
    time::{Duration, Instant},
};

use crate::{
    config::WORKSPACES,
    ipc::{Position, Request, View, socket_path},
};

/// How long a single answer may take. A compositor that long away is stuck.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

const POS_USAGE: &str =
    "usage: wirbel pos [(now|MILLISECONDS|loop) [x|y|xy] [DECIMALS|max]] [--once] [--json]";
const DISTANCE_USAGE: &str = "usage: wirbel distance [SECONDS|loop] [DECIMALS|max]";
const MOVE_USAGE: &str = "usage: wirbel move [--rel] X Y [SECONDS]";
/// How often a slow `wirbel move` sets the pointer, more often than any screen shows a frame.
const MOVE_STEP: Duration = Duration::from_millis(4);
const WS_USAGE: &str = "usage: wirbel ws N, with N from 1 to 9";
const CLICK_USAGE: &str = "usage: wirbel click [COUNT|loop] [SECONDS]";
/// Between the clicks of `wirbel click COUNT`, if nobody says.
const DEFAULT_CLICK_GAP: Duration = Duration::from_secs(1);
/// More places than a double holds for a position would only print noise.
const MAX_DECIMALS: usize = 17;
const MMSG_USAGE: &str =
    "usage: mmsg get|watch all-tags|all-monitors, or mmsg dispatch view,N|quit|reload_config";

fn connect() -> Result<UnixStream, String> {
    let display = env::var_os("WAYLAND_DISPLAY").ok_or("WAYLAND_DISPLAY is not set")?;
    let path = socket_path(&display).ok_or("cannot tell where the socket is")?;
    UnixStream::connect(&path).map_err(|err| {
        format!(
            "no wirbel on {}: {err} ({})",
            display.display(),
            path.display()
        )
    })
}

/// Connects and sends `request`. The reader is at the first line of the answer.
fn send(request: Request) -> Result<BufReader<UnixStream>, String> {
    let mut stream = connect()?;
    let line = request.to_line() + "\n";
    stream
        .write_all(line.as_bytes())
        .map_err(|err| format!("failed to send: {err}"))?;
    Ok(BufReader::new(stream))
}

/// The next line of the answer. `None` means the compositor went away.
fn read_line(reader: &mut BufReader<UnixStream>) -> Result<Option<String>, String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line.trim_end_matches('\n').to_string())),
        Err(err) => Err(format!("failed to read the answer: {err}")),
    }
}

/// What a line of `wirbel pos` holds.
#[derive(Clone, Copy)]
enum Fields {
    PositionAndOutput,
    X,
    Y,
    Xy,
}

#[derive(Clone, Copy)]
enum Decimals {
    /// All there are: the shortest text that reads back as the same number.
    Max,
    Exactly(usize),
}

fn format_number(value: f64, decimals: Decimals) -> String {
    let Decimals::Exactly(places) = decimals else {
        return format!("{value:?}");
    };
    let text = format!("{value:.places$}");
    // A small negative number rounds to nothing, and nothing has no sign.
    match text.strip_prefix('-') {
        Some(rest) if rest.chars().all(|c| matches!(c, '0' | '.')) => rest.to_string(),
        _ => text,
    }
}

fn parse_decimals(text: &str, usage: &str) -> Result<Decimals, String> {
    if text == "max" {
        return Ok(Decimals::Max);
    }
    let places = text.parse().ok().filter(|places| *places <= MAX_DECIMALS);
    places.map(Decimals::Exactly).ok_or(usage.to_string())
}

/// A length of time as a person writes it, `2` or `0.5`.
fn parse_seconds(text: &str, usage: &str) -> Result<Duration, String> {
    let seconds = text.parse().map_err(|_| usage.to_string())?;
    Duration::try_from_secs_f64(seconds).map_err(|_| usage.to_string())
}

fn format_position(position: &Position, fields: Fields, decimals: Decimals) -> String {
    let x = format_number(position.x, decimals);
    let y = format_number(position.y, decimals);
    match fields {
        Fields::PositionAndOutput => {
            let output = position.output.as_deref().unwrap_or("-");
            format!("{x} {y} {output}")
        }
        Fields::X => x,
        Fields::Y => y,
        Fields::Xy => format!("{x} {y}"),
    }
}

/// Prints what `wirbel pos` prints. A reader that went away is the normal way to end a pipe.
fn print_all(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Puts `text` on the line a running command keeps up to date, unless it is there already.
/// False means the reader is gone.
fn show(text: String, shown: &mut Option<String>, in_place: bool) -> Result<bool, String> {
    // A change can be too small or on the other axis to show. Then there is nothing new.
    if shown.as_ref() == Some(&text) {
        return Ok(true);
    }
    *shown = Some(text.clone());
    let text = if in_place {
        format!("\r{text}\x1b[K")
    } else {
        text + "\n"
    };
    match print_all(&text) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err.to_string()),
    }
}

pub fn pos(args: &[String]) -> Result<(), String> {
    let (mut once, mut json) = (false, false);
    let mut delay = None;
    let (mut looping, mut fields, mut decimals) = (false, None, None);
    for arg in args {
        // What to show can only follow how to show it: `pos 2` waits, `pos now 2` rounds.
        let mode_chosen = looping || delay.is_some();
        match arg.as_str() {
            "--once" => once = true,
            "--json" => json = true,
            "loop" if !looping && delay.is_none() => looping = true,
            "x" if mode_chosen && fields.is_none() && decimals.is_none() => {
                fields = Some(Fields::X);
            }
            "y" if mode_chosen && fields.is_none() && decimals.is_none() => {
                fields = Some(Fields::Y);
            }
            "xy" if mode_chosen && fields.is_none() && decimals.is_none() => {
                fields = Some(Fields::Xy);
            }
            text if mode_chosen && decimals.is_none() => {
                decimals = Some(parse_decimals(text, POS_USAGE)?);
            }
            _ if mode_chosen => return Err(POS_USAGE.into()),
            "now" => delay = Some(Duration::ZERO),
            text => {
                let millis = text.parse().map_err(|_| POS_USAGE.to_string())?;
                delay = Some(Duration::from_millis(millis));
            }
        }
    }

    if (looping && once) || (json && (fields.is_some() || decimals.is_some())) {
        return Err(POS_USAGE.into());
    }
    let fields = fields.unwrap_or(Fields::PositionAndOutput);
    let decimals = decimals.unwrap_or(Decimals::Max);

    // The position is asked for after the wait, so it is the one at the end of the wait.
    if let Some(delay) = delay {
        once = true;
        thread::sleep(delay);
    }

    let request = if once {
        Request::Position
    } else {
        Request::Watch
    };
    let mut reader = send(request)?;
    if once {
        let timeout = reader.get_ref().set_read_timeout(Some(ANSWER_TIMEOUT));
        timeout.map_err(|err| err.to_string())?;
    }

    // Rewriting a line only makes sense for someone looking at it.
    let in_place = !once && !json && io::stdout().is_terminal();
    let mut shown = None;
    loop {
        let Some(line) = read_line(&mut reader)? else {
            if in_place {
                let _ = print_all("\n");
            }
            return Err("the compositor went away".into());
        };
        let position =
            Position::parse(&line).ok_or_else(|| format!("unexpected answer: {line}"))?;
        let text = if json {
            position.to_json()
        } else {
            format_position(&position, fields, decimals)
        };
        if !show(text, &mut shown, in_place)? || once {
            return Ok(());
        }
    }
}

/// The way the pointer went, in layout pixels. Added up from its positions: Going back and forth
/// counts in full, and a pointer that is locked for a game does not move.
pub fn distance(args: &[String]) -> Result<(), String> {
    let mut args = args.iter().map(String::as_str);
    let time = match args.next() {
        None | Some("loop") => None,
        Some(text) => Some(parse_seconds(text, DISTANCE_USAGE)?),
    };
    let decimals = match args.next() {
        None => Decimals::Max,
        Some(text) => parse_decimals(text, DISTANCE_USAGE)?,
    };
    if args.next().is_some() {
        return Err(DISTANCE_USAGE.into());
    }

    let mut reader = send(Request::Watch)?;
    let timeout = reader.get_ref().set_read_timeout(Some(ANSWER_TIMEOUT));
    timeout.map_err(|err| err.to_string())?;
    let next_position = |reader: &mut _| {
        let line = read_line(reader)?.ok_or("the compositor went away")?;
        Position::parse(&line).ok_or_else(|| format!("unexpected answer: {line}"))
    };

    // The first line is where the pointer is now, and that is where the way starts.
    let mut last = next_position(&mut reader)?;
    let end = time.map(|time| Instant::now() + time);
    let in_place = end.is_none() && io::stdout().is_terminal();
    let mut total = 0.0;
    let mut shown = None;
    if end.is_none() && !show(format_number(total, decimals), &mut shown, in_place)? {
        return Ok(());
    }
    reader
        .get_ref()
        .set_read_timeout(None)
        .map_err(|err| err.to_string())?;

    loop {
        if let Some(end) = end {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let timeout = reader.get_ref().set_read_timeout(Some(left));
            timeout.map_err(|err| err.to_string())?;
        }
        let position = match next_position(&mut reader) {
            Ok(position) => position,
            // Only the end of the time gives up on a read, and then a torn line is no loss.
            Err(_) if end.is_some_and(|end| Instant::now() >= end) => break,
            Err(err) => {
                if in_place {
                    let _ = print_all("\n");
                }
                return Err(err);
            }
        };
        total += (position.x - last.x).hypot(position.y - last.y);
        last = position;
        if end.is_none() && !show(format_number(total, decimals), &mut shown, in_place)? {
            return Ok(());
        }
    }
    show(format_number(total, decimals), &mut shown, false).map(|_| ())
}

fn parse_coordinate(text: &str) -> Result<f64, String> {
    let value: f64 = text.parse().map_err(|_| MOVE_USAGE.to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("not a finite number: {text}"))
    }
}

pub fn move_pointer(args: &[String]) -> Result<(), String> {
    let (relative, coordinates) = match args {
        [flag, rest @ ..] if flag == "--rel" => (true, rest),
        rest => (false, rest),
    };
    let (x, y, time) = match coordinates {
        [x, y] => (x, y, Duration::ZERO),
        [x, y, seconds] => (x, y, parse_seconds(seconds, MOVE_USAGE)?),
        _ => return Err(MOVE_USAGE.into()),
    };
    let (x, y) = (parse_coordinate(x)?, parse_coordinate(y)?);
    if time.is_zero() {
        return command(if relative {
            Request::MoveRelative { dx: x, dy: y }
        } else {
            Request::Move { x, y }
        });
    }

    // On a straight line at an even pace, from where the pointer is now. Every step is absolute,
    // so the end is the target even if the mouse moved on the way.
    let line = ask(Request::Position)?;
    let start = Position::parse(&line).ok_or_else(|| format!("unexpected answer: {line}"))?;
    let (dx, dy) = if relative {
        (x, y)
    } else {
        (x - start.x, y - start.y)
    };
    let mut reader = send(Request::Position)?;
    let timeout = reader.get_ref().set_read_timeout(Some(ANSWER_TIMEOUT));
    timeout.map_err(|err| err.to_string())?;
    read_line(&mut reader)?.ok_or("the compositor went away")?;

    let began = Instant::now();
    loop {
        let progress = (began.elapsed().as_secs_f64() / time.as_secs_f64()).min(1.0);
        let request = Request::Move {
            x: start.x + dx * progress,
            y: start.y + dy * progress,
        };
        let line = request.to_line() + "\n";
        let written = reader.get_mut().write_all(line.as_bytes());
        written.map_err(|err| format!("failed to send: {err}"))?;
        check_answer(&read_line(&mut reader)?.ok_or("the compositor went away")?)?;
        if progress >= 1.0 {
            return Ok(());
        }
        thread::sleep(MOVE_STEP);
    }
}

/// Clicks `COUNT` times or until stopped. The times are fixed from the first click on, so a late
/// answer does not push back the clicks after it.
pub fn click(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (count, gap) = match args.as_slice() {
        [] => (Some(1), DEFAULT_CLICK_GAP),
        [count] => (parse_click_count(count)?, DEFAULT_CLICK_GAP),
        [count, seconds] => (
            parse_click_count(count)?,
            parse_seconds(seconds, CLICK_USAGE)?,
        ),
        _ => return Err(CLICK_USAGE.into()),
    };

    let start = Instant::now();
    let mut done = 0;
    loop {
        command(Request::Click)?;
        done += 1;
        if count == Some(done) {
            return Ok(());
        }
        let next = start + gap.saturating_mul(done);
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

/// Shows workspace N on the output with the pointer, like the bind.
pub fn workspace(args: &[String]) -> Result<(), String> {
    let [number] = args else {
        return Err(WS_USAGE.into());
    };
    match number.parse() {
        Ok(number @ 1..=WORKSPACES) => command(Request::Workspace(number)),
        _ => Err(WS_USAGE.into()),
    }
}

/// `None` is until stopped.
fn parse_click_count(text: &str) -> Result<Option<u32>, String> {
    if text == "loop" {
        return Ok(None);
    }
    match text.parse() {
        Ok(count @ 1..) => Ok(Some(count)),
        _ => Err(CLICK_USAGE.into()),
    }
}

/// Asks for something that has no answer but whether it worked.
fn command(request: Request) -> Result<(), String> {
    check_answer(&ask(request)?)
}

fn check_answer(line: &str) -> Result<(), String> {
    match line {
        "ok" => Ok(()),
        "error unknown request" => Err(
            "the running wirbel is older than this command, log out and in again to update it"
                .into(),
        ),
        line => Err(line.strip_prefix("error ").unwrap_or(line).to_string()),
    }
}

/// The single line that answers `request`.
fn ask(request: Request) -> Result<String, String> {
    let mut reader = send(request)?;
    let timeout = reader.get_ref().set_read_timeout(Some(ANSWER_TIMEOUT));
    timeout.map_err(|err| format!("failed to set a timeout: {err}"))?;
    read_line(&mut reader)?.ok_or("the compositor went away".to_string())
}

/// Stands in for the `mmsg` of Mango, for the bars written against it. What they do not ask for
/// is refused, a caller sees that like a Mango that does not know the command.
pub fn mmsg(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let view = |name| match name {
        "all-tags" => Ok(View::Tags),
        "all-monitors" => Ok(View::Monitors),
        _ => Err(MMSG_USAGE.to_string()),
    };
    match args.as_slice() {
        ["get", "version"] => println!(r#"{{"version":"wirbel"}}"#),
        ["get", name] => {
            let line = ask(Request::Get(view(name)?))?;
            print_all(&format!("{line}\n")).map_err(|err| err.to_string())?;
        }
        ["watch", name] => {
            let mut reader = send(Request::WatchView(view(name)?))?;
            while let Some(line) = read_line(&mut reader)? {
                if print_all(&format!("{line}\n")).is_err() {
                    break;
                }
            }
        }
        ["dispatch", command] => {
            let mut fields = command.split(',');
            let request = match (fields.next(), fields.next()) {
                (Some("quit"), None) => Request::Quit,
                (Some("reload_config"), None) => Request::Reload,
                (Some("view"), Some(number)) => {
                    Request::Workspace(number.parse().map_err(|_| MMSG_USAGE)?)
                }
                _ => return Err(format!("{command} does not exist in wirbel")),
            };
            let answer = ask(request)?;
            if answer != "ok" {
                return Err(answer);
            }
        }
        _ => return Err(MMSG_USAGE.into()),
    }
    Ok(())
}
