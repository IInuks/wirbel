//! Macros: small programs that drive invisible windows, `wirbel macro run FILE`. A macro is a
//! text file of steps, one per line; every command of `wirbel vwm` is a step, and around them
//! there are variables, conditions, loops, functions, error handling and handlers that react to
//! what happens in a window. `docs/vwm/MACROS.md` describes the language.
//!
//! A macro runs in the process of the command, not in the compositor.

pub mod interp;
pub mod syntax;

use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use interp::{Error, Host, Interp, Outcome, Value};

use crate::{
    config::Config,
    vwm::{
        cli::{COMMANDS, capture},
        wait::{Condition, WaitError, wait},
    },
};

pub const USAGE: &str = "usage: wirbel macro run FILE|NAME [PARAMETER=VALUE]... [--json]
       wirbel macro check FILE|NAME
       wirbel macro list
       wirbel macro create [NAME]";

pub const HELP: &str =
    "macro runs a macro: a file of steps that drive invisible windows, see docs/vwm/MACROS.md.
      run        run it. NAME is a file in ~/.config/wirbel/macros, with or without its .wm.
                 What the macro returns is printed, as JSON with --json. Exit status: 0 done,
                 1 failed, 3 the time of a wait ran out, or what its `exit` says
      check      read it and say what is wrong with it, without running anything
      list       the macros in ~/.config/wirbel/macros
      create     record a new macro by doing it, see `wirbel vwm create macro`";

/// The real world.
struct System;

impl Host for System {
    fn command(&mut self, args: &[String]) -> Result<String, String> {
        capture(args)
    }

    fn wait(
        &mut self,
        name: &str,
        condition: &Condition,
        timeout: Duration,
        tick: &mut dyn FnMut(&mut dyn Host) -> Result<(), String>,
    ) -> Result<String, WaitError> {
        wait(name, condition, timeout, &mut || tick(&mut System))
    }

    fn print(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, prompt: &str) -> Result<String, String> {
        eprint!("{prompt} ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        read.map_err(|err| format!("no answer: {err}"))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }

    fn shell(&mut self, command: &str) -> Result<String, String> {
        let output = Command::new("sh")
            .args(["-c", command])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| format!("failed to run the shell: {err}"))?;
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim_end().to_string();
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(format!(
                "`{command}` ended with {code}: {}",
                text(&output.stderr)
            ));
        }
        Ok(text(&output.stdout))
    }

    fn sleep(&mut self, time: Duration) {
        std::thread::sleep(time);
    }

    fn read(&mut self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
    }
}

/// `~/.config/wirbel/macros`.
pub fn library() -> Option<PathBuf> {
    Some(Config::default_path()?.parent()?.join("macros"))
}

/// The file behind a name: a path as it is, else a file in the library.
fn find(name: &str) -> Result<PathBuf, String> {
    let direct = PathBuf::from(name);
    if direct.is_file() {
        return Ok(direct);
    }
    let library = library().filter(|_| !name.contains('/'));
    let found = library
        .into_iter()
        .flat_map(|dir| [dir.join(name), dir.join(format!("{name}.wm"))]);
    let found = found.into_iter().find(|path| path.is_file());
    found.ok_or_else(|| format!("no macro `{name}`"))
}

/// Runs the macro in `source` with this host. For the command and the tests.
pub fn run_with(
    host: &mut dyn Host,
    source: &str,
    file: &Path,
    given: HashMap<String, String>,
) -> Result<Outcome, Error> {
    let mut interp = Interp::new(host, COMMANDS, given);
    interp.library = library();
    interp.run(source, file)
}

fn run(args: &[String]) -> Result<i32, String> {
    let json = args.iter().any(|arg| arg == "--json");
    let mut args = args.iter().filter(|arg| *arg != "--json");
    let file = find(args.next().ok_or(USAGE)?)?;
    let mut given = HashMap::new();
    for arg in args {
        let (name, value) = arg
            .split_once('=')
            .ok_or("parameters are given as NAME=VALUE")?;
        given.insert(name.to_string(), value.to_string());
    }
    let source =
        std::fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
    match run_with(&mut System, &source, &file, given) {
        Ok(Outcome::Exit(code)) => Ok(code),
        Ok(Outcome::Done(value)) => {
            if json {
                println!("{}", value.to_json());
            } else if value != Value::Nil {
                println!("{value}");
            }
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(if err.timeout { 3 } else { 1 })
        }
    }
}

fn check(args: &[String]) -> Result<i32, String> {
    let [name] = args else {
        return Err(USAGE.into());
    };
    let file = find(name)?;
    let source =
        std::fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
    match syntax::parse(&source, COMMANDS) {
        Ok(steps) => {
            println!(
                "{}: fine, {} step(s) at the top",
                file.display(),
                steps.len()
            );
            Ok(0)
        }
        Err(err) => {
            eprintln!("{}:{}: {}", file.display(), err.line, err.message);
            Ok(1)
        }
    }
}

fn list() -> Result<i32, String> {
    let dir = library().ok_or("cannot tell where the config folder is")?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".wm"))
        .collect();
    names.sort();
    if names.is_empty() {
        println!("no macros in {}", dir.display());
    }
    for name in names {
        println!("{}", name.trim_end_matches(".wm"));
    }
    Ok(0)
}

/// `wirbel macro ARGS`. Returns the exit status.
pub fn main(args: &[String]) -> i32 {
    let result = match args.split_first().map(|(word, rest)| (word.as_str(), rest)) {
        Some(("run", rest)) => run(rest),
        Some(("check", rest)) => check(rest),
        Some(("list", [])) => list(),
        Some(("create", rest)) => crate::vwm::create::wizard(rest).map(|()| 0),
        Some(("--help" | "-h" | "help", _)) => {
            println!("{USAGE}\n\n{HELP}");
            Ok(0)
        }
        _ => Err(USAGE.into()),
    };
    result.unwrap_or_else(|err| {
        eprintln!("wirbel macro: {err}");
        if err.starts_with("usage: ") { 2 } else { 1 }
    })
}
