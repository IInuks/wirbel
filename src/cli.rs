//! The commands that talk to a running compositor: `wirbel pos` and `wirbel move`.

use std::{
    env,
    io::{self, BufRead, BufReader, IsTerminal, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use crate::ipc::{Position, Request, socket_path};

/// How long a single answer may take. A compositor that long away is stuck.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

const POS_USAGE: &str = "usage: wirbel pos [--once] [--json]";
const MOVE_USAGE: &str = "usage: wirbel move [--rel] X Y";

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

fn format_position(position: &Position, json: bool) -> String {
    if json {
        return position.to_json();
    }
    let output = position.output.as_deref().unwrap_or("-");
    format!("{:?} {:?} {output}", position.x, position.y)
}

/// Prints what `wirbel pos` prints. A reader that went away is the normal way to end a pipe.
fn print_all(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

pub fn pos(args: &[String]) -> Result<(), String> {
    let (mut once, mut json) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--once" => once = true,
            "--json" => json = true,
            _ => return Err(POS_USAGE.into()),
        }
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
    loop {
        let Some(line) = read_line(&mut reader)? else {
            if in_place {
                let _ = print_all("\n");
            }
            return Err("the compositor went away".into());
        };
        let position =
            Position::parse(&line).ok_or_else(|| format!("unexpected answer: {line}"))?;
        let text = format_position(&position, json);
        let text = if in_place {
            format!("\r{text}\x1b[K")
        } else {
            text + "\n"
        };
        match print_all(&text) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(err) => return Err(err.to_string()),
        }
        if once {
            return Ok(());
        }
    }
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
    let [x, y] = coordinates else {
        return Err(MOVE_USAGE.into());
    };
    let (x, y) = (parse_coordinate(x)?, parse_coordinate(y)?);
    let request = if relative {
        Request::MoveRelative { dx: x, dy: y }
    } else {
        Request::Move { x, y }
    };

    let mut reader = send(request)?;
    let timeout = reader.get_ref().set_read_timeout(Some(ANSWER_TIMEOUT));
    timeout.map_err(|err| err.to_string())?;
    match read_line(&mut reader)?.as_deref() {
        Some("ok") => Ok(()),
        Some(line) => Err(line.strip_prefix("error ").unwrap_or(line).to_string()),
        None => Err("the compositor went away".into()),
    }
}
