//! The program around the compositor: options, the command line tools, the choice of backend,
//! the safety net, the restart. The `wirbel` binary is `app::main(Vec::new())`; a program that
//! builds wirbel with extensions (see `crate::extension`) calls it with them.

use std::{
    env,
    path::{Path, PathBuf},
    process,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::Duration,
};

use crate::{
    Wirbel, backend,
    config::Config,
    extension::{self, Extension},
    safety::{self, CHORD_NAME, EXIT_EMERGENCY, EXIT_RESTART, HEARTBEAT, SESSION_ENV, Watchdog},
};
use smithay::reexports::{
    calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    wayland_server::Display,
};

const USAGE: &str = "usage: wirbel [--config FILE] [-c COMMAND]... [--timeout SECONDS] [--log FILE]
                     [--headless [WxH]] [--test-input]
       wirbel check [CONFIG]
       wirbel pos [(now|MILLISECONDS|loop) [x|y|xy] [DECIMALS|max]] [--once] [--json]
       wirbel distance [SECONDS|loop] [DECIMALS|max]
       wirbel move [--rel] X Y [SECONDS]
       wirbel click [COUNT|loop] [SECONDS]
       wirbel ws N|next|prev|next_used|prev_used
       wirbel restart
       wirbel mmsg ARGUMENTS
       wirbel inject REQUEST | --script SCRIPT | --help
       wirbel plug [list | add PATH [NAME]... | remove PLUGIN... | PLUGIN... true|false | define PLUGIN [NAME]]

      --config FILE      read this file instead of ~/.config/wirbel/config
  -c, --command COMMAND  run a shell command inside the compositor (repeatable)
      --timeout SECONDS  stop cleanly after this time, or by force if that does not work
      --log FILE         log to this file instead of stderr. On a VT that is the default, with
                         ~/.local/state/wirbel/wirbel.log
      --headless [WxH]   run without a display, on a GPU render node, with one virtual output of
                         this size, 1280x720 by default. Nothing shows on the screen and no input
                         devices are read; grim captures it through screencopy. For tests.
                         It reads no config file unless --config gives one
      --test-input       accept `wirbel inject` on the control socket. --headless implies it
      --stats            log every few seconds where the time goes, see src/stats.rs
      --panic-after SECONDS  panic on purpose, to try out the safety net
      --hang-after SECONDS   block the main loop on purpose, to try out the safety net
  -h, --help             show this help

Inside a Wayland or X11 session wirbel runs in a window, unless --headless says otherwise. Without
one it needs a virtual terminal on stdin and takes over the screen. Ctrl+Alt+Shift+Backspace ends
it at once, even if it hangs or the session is locked.

check looks at session, GPUs, outputs and input without taking over anything. It is safe to run
at any time, also inside another compositor.

pos prints the pointer position in one line that follows the pointer, until Ctrl+C.
      now                print it once and exit, same as --once
      MILLISECONDS       wait this long, then print it once and exit
      loop               follow the pointer, same as without an argument
      ... x|y|xy         after one of these: only this axis or both, without the output
      ... DECIMALS       after one of these: round to this many decimal places, 0 to 17
      ... max            after one of these: every decimal place, the default
      --once             print it once and exit
      --json             print one JSON object per line

distance prints how far the pointer has moved, in pixels along its way. With SECONDS, such as 2 or
0.5, it counts for that long and prints the result. With `loop` or without an argument it keeps
one line up to date, until Ctrl+C.
      ... DECIMALS       after one of these: round to this many decimal places, 0 to 17
      ... max            after one of these: every decimal place, the default

move sets the pointer, clamped to the outputs. It works like moving the mouse: a window being
dragged follows it.
      --rel              X and Y are relative to the current position
      SECONDS            get there on a straight line in this time, such as 4 or 0.5

click clicks the left button where the pointer is. It works like the real button: the window under
the pointer gets the focus. If that is not the terminal, Ctrl+C there has to wait until you click it.
      COUNT              click this many times
      loop               click until stopped
      ... SECONDS        after one of these: the time between clicks, default 1, such as 2 or 0.5

ws switches to workspace N, 1 to 9, on the output with the pointer.
      next, prev         the one after or before, from 9 on to 1 and back
      next_used, prev_used  the nearest one with windows that way, stops at 1 and 9

restart ends the session and starts it again, as if you logged out and in: all programs close,
the autostart and the plugins start anew, under wirbel-session also D-Bus, PipeWire and the portals.
Windows do not survive it. The bind action for it is `restart`.

mmsg answers what the bars of Mango ask of its `mmsg`: get or watch all-tags and all-monitors, dispatch
view,N and quit and reload_config. Called as `mmsg` through a symbolic link it does the same.

inject plays keyboard and mouse input as if it came from the devices, for tests. Only a wirbel
started with --test-input or --headless takes it. `wirbel inject --help` lists the requests and
the input scripts, such as `wirbel inject --script type:hi,tap:Return`.

plug manages plugins: programs that start with wirbel and change it through its socket, see
docs/PLUGINS.md. PLUGIN is the path of one or the name it was given.
      list               show them: on or off, the name if there is one, the path, if it runs
      add PATH [NAME]... add an executable file, or a folder with an executable `run`, switched on;
                         several at once, a word without a slash that is no file is the name of
                         the path before it
      remove PLUGIN...   take them out of the config
      PLUGIN... true|false  switch them on or off
      define PLUGIN NAME give it a name to use instead of the path, without NAME take it away
The changes go to the config as `plugin = PATH[, NAME][, true|false]` lines, which can be written by
hand as well, and a running wirbel reloads it.

All of these talk to the compositor of WAYLAND_DISPLAY.";

/// `USAGE` with the lines of the extensions: their usage after the other commands, their help
/// before the last paragraph.
fn usage(extensions: &[Rc<dyn Extension>]) -> String {
    let (head, rest) = USAGE.split_once("\n\n").unwrap_or((USAGE, ""));
    let (body, last) = rest.rsplit_once("\n\n").unwrap_or((rest, ""));
    let mut text = head.to_string();
    for extension in extensions {
        text += &format!("\n       {}", extension.info().usage);
    }
    text += "\n\n";
    text += body;
    for extension in extensions {
        let help = extension.info().help;
        if !help.is_empty() {
            text += &format!("\n\n{help}");
        }
    }
    text += "\n\n";
    text += last;
    text
}

#[derive(Debug, Default)]
struct Options {
    commands: Vec<String>,
    config_path: Option<PathBuf>,
    log: Option<PathBuf>,
    timeout: Option<Duration>,
    panic_after: Option<Duration>,
    hang_after: Option<Duration>,
    stats: bool,
    /// The output size of `--headless`.
    headless: Option<(i32, i32)>,
    /// Whether `wirbel inject` may play input.
    test_input: bool,
}

fn seconds(value: Option<String>) -> Result<Duration, String> {
    let seconds = value.and_then(|value| value.parse().ok());
    seconds.map(Duration::from_secs).ok_or(USAGE.to_string())
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => {
                let size = args
                    .peek()
                    .and_then(|next| backend::headless::parse_size(next));
                if size.is_some() {
                    args.next();
                }
                options.headless = Some(size.unwrap_or(backend::headless::DEFAULT_SIZE));
            }
            "-c" | "--command" => options.commands.push(args.next().ok_or(USAGE)?),
            "--config" => options.config_path = Some(args.next().ok_or(USAGE)?.into()),
            "--log" => options.log = Some(args.next().ok_or(USAGE)?.into()),
            "--timeout" => options.timeout = Some(seconds(args.next())?),
            "--stats" => options.stats = true,
            "--test-input" => options.test_input = true,
            "--panic-after" => options.panic_after = Some(seconds(args.next())?),
            "--hang-after" => options.hang_after = Some(seconds(args.next())?),
            "-h" | "--help" => return Ok(None),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(Some(options))
}

/// The backend to run. The DRM backend takes over the screen, so it wants to be asked twice: no
/// session around, and a VT on stdin. Anything else, such as a test harness or a script in the
/// background of a running desktop, ends here.
fn choose_backend(options: &Options) -> Result<backend::Kind, String> {
    if options.headless.is_some() {
        return Ok(backend::Kind::Headless);
    }
    let set = |name| env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("WAYLAND_DISPLAY") || set("DISPLAY") {
        return Ok(backend::Kind::Nested);
    }
    if safety::is_virtual_terminal(0) {
        return Ok(backend::Kind::Drm);
    }
    Err("no Wayland or X11 session, and stdin is not a virtual terminal".into())
}

/// Whatever is wrong with the file, the compositor has to come up.
fn load_config(path: Option<&Path>) -> Config {
    let Some(path) = path else {
        return Config::default();
    };
    match Config::load(path) {
        Ok((config, errors)) => {
            for error in errors {
                tracing::error!(path = %path.display(), %error, "skipping configuration line");
            }
            config
        }
        Err(err) => {
            tracing::error!(path = %path.display(), %err, "using the default configuration");
            Config::default()
        }
    }
}

/// Starts the deliberate failures of `--panic-after` and `--hang-after`.
fn arm_failures(event_loop: &EventLoop<'static, Wirbel>, options: &Options) {
    let handle = event_loop.handle();
    let insert = |delay, failure: fn()| {
        let timer = handle.insert_source(Timer::from_duration(delay), move |_, _, _| {
            failure();
            TimeoutAction::Drop
        });
        if let Err(err) = timer {
            tracing::error!(%err, "failed to arm the test failure");
        }
    };
    if let Some(delay) = options.panic_after {
        insert(delay, || panic!("--panic-after"));
    }
    if let Some(delay) = options.hang_after {
        insert(delay, || {
            tracing::warn!("--hang-after: blocking the main loop");
            loop {
                thread::park();
            }
        });
    }
}

/// Runs the compositor. True means it ended for a restart.
fn run(
    options: Options,
    kind: backend::Kind,
    extensions: Vec<Rc<dyn Extension>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut event_loop = EventLoop::try_new()?;
    let display = Display::new()?;
    // A headless test must not start the autostart and plugins of the user's own session, which
    // would talk to their D-Bus. It reads a config only when given one.
    let config_path = match kind {
        backend::Kind::Headless => options.config_path.clone(),
        _ => options.config_path.clone().or_else(Config::default_path),
    };
    let mut state = Wirbel::new(
        &mut event_loop,
        display,
        load_config(config_path.as_deref()),
    )?;
    state.config_path = config_path;
    for extension in extensions {
        state.add_extension(extension);
    }
    state.init_extensions();
    // Scripted input is for tests; in a real session no program may type into the user's windows.
    state.test_input = options.test_input || kind == backend::Kind::Headless;
    if state.test_input {
        tracing::info!("input injection on, see `wirbel inject`");
    }
    if options.stats || state.config.stats {
        state.stats.enable();
    }
    state.emergency = Box::new(|| {
        safety::hard_exit(EXIT_EMERGENCY, &format!("emergency stop, {CHORD_NAME}"));
    });

    let watchdog = Watchdog::start(event_loop.get_signal(), options.timeout);
    let beat = event_loop
        .handle()
        .insert_source(Timer::immediate(), move |_, _, _| {
            watchdog.beat();
            TimeoutAction::ToDuration(HEARTBEAT)
        });
    beat.map_err(|err| err.error)?;
    arm_failures(&event_loop, &options);

    let active = Arc::new(AtomicBool::new(true));
    let devices = env::var_os("WIRBEL_EMERGENCY_DEVICES")
        .map(|paths| env::split_paths(&paths).collect::<Vec<PathBuf>>());
    if kind == backend::Kind::Drm || devices.is_some() {
        safety::watch_keyboards(devices, active.clone(), || {
            safety::hard_exit(EXIT_EMERGENCY, &format!("emergency stop, {CHORD_NAME}"));
        })?;
    }

    match kind {
        backend::Kind::Nested => backend::winit::init(&mut event_loop, &mut state)?,
        backend::Kind::Drm => backend::drm::init(&mut event_loop, &mut state, active)?,
        backend::Kind::Headless => {
            let size = options.headless.unwrap_or(backend::headless::DEFAULT_SIZE);
            backend::headless::init(&mut event_loop, &mut state, size)?;
        }
    }

    state.start_xwayland();
    tracing::info!(socket = ?state.socket_name, "compositor ready");
    for command in options.commands.iter().chain(&state.config.exec_once) {
        state.autostart(command);
    }
    state.sync_plugins();

    event_loop.run(None, &mut state, Wirbel::after_dispatch)?;
    safety::end_autostart();
    tracing::info!("stopped");
    Ok(state.restart)
}

/// Starts wirbel again in place of this process, after everything of this one is closed. The name
/// it was started by finds the newest binary, as a login would.
fn restart_in_place() -> ! {
    use std::os::unix::process::CommandExt;
    let mut args = env::args_os();
    let program = args.next().unwrap_or_else(|| "wirbel".into());
    safety::restore_stderr();
    let err = process::Command::new(&program).args(args).exec();
    safety::say(&format!("wirbel: restart failed: {err}\n"));
    process::exit(1);
}

/// Everything the `wirbel` binary does, with these extensions built in. Never returns on errors:
/// it exits with a status.
pub fn main(extensions: Vec<Rc<dyn Extension>>) {
    // Their names are words of the config, which is read before there is a compositor.
    for extension in &extensions {
        extension::register(extension.info());
    }
    let called_as_mmsg = env::args_os()
        .next()
        .is_some_and(|name| Path::new(&name).file_name() == Some("mmsg".as_ref()));
    if called_as_mmsg {
        let args: Vec<String> = env::args().skip(1).collect();
        if let Err(err) = crate::cli::mmsg(&args) {
            eprintln!("mmsg: {err}");
            process::exit(1);
        }
        return;
    }
    let mut args = env::args().skip(1).peekable();
    if let Some(
        command @ ("pos" | "distance" | "move" | "click" | "ws" | "mmsg" | "plug" | "restart"
        | "inject"),
    ) = args.peek().map(String::as_str)
    {
        let command = command.to_string();
        args.next();
        let args: Vec<String> = args.collect();
        let result = match command.as_str() {
            "pos" => crate::cli::pos(&args),
            "distance" => crate::cli::distance(&args),
            "click" => crate::cli::click(&args),
            "ws" => crate::cli::workspace(&args),
            "mmsg" => crate::cli::mmsg(&args),
            "plug" => crate::plugin::plug(&args),
            "restart" => crate::cli::restart(&args),
            "inject" => crate::cli::inject(&args),
            _ => crate::cli::move_pointer(&args),
        };
        if let Err(err) = result {
            eprintln!("wirbel {command}: {err}");
            process::exit(1);
        }
        return;
    }

    let named = |name: &str| {
        extensions
            .iter()
            .find(|extension| extension.info().name == name)
            .map(|extension| extension.info().name)
    };
    if let Some(name) = args.peek().and_then(|arg| named(arg)) {
        args.next();
        let args: Vec<String> = args.collect();
        if let Err(err) = crate::cli::extension(name, &args) {
            eprintln!("wirbel {name}: {err}");
            process::exit(1);
        }
        return;
    }

    if args.peek().is_some_and(|arg| arg == "check") {
        args.next();
        let config = args.next().map(PathBuf::from).or_else(Config::default_path);
        let filter = "wirbel=info,error";
        // SAFETY: nothing else runs yet.
        unsafe { env::set_var("RUST_LOG", env::var("RUST_LOG").unwrap_or(filter.into())) };
        let logged = safety::install_log(None);
        if logged.is_err() || !crate::check::run(config.as_deref()) {
            process::exit(1);
        }
        return;
    }

    let options = match parse_options(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{}", usage(&extensions));
            return;
        }
        Err(err) => {
            eprintln!("{err}");
            process::exit(2);
        }
    };
    let kind = choose_backend(&options).unwrap_or_else(|err| {
        eprintln!("wirbel: {err}");
        process::exit(1);
    });

    safety::remember_terminal();
    let default_log = (kind == backend::Kind::Drm)
        .then(safety::default_log_path)
        .flatten();
    let log = options.log.clone().or(default_log);
    if let Err(err) = safety::install_log(log.as_deref()) {
        eprintln!("wirbel: cannot write the log: {err}");
        process::exit(1);
    }
    safety::install_panic_hook();

    let result = run(options, kind, extensions);
    safety::restore_terminal();
    match result {
        Err(err) => {
            tracing::error!(%err, "wirbel failed");
            safety::say(&format!("wirbel: {err}\n"));
            process::exit(1);
        }
        // wirbel-session starts the whole session again, its bus and portals too.
        Ok(true) if env::var_os(SESSION_ENV).is_some() => process::exit(EXIT_RESTART),
        Ok(true) => restart_in_place(),
        Ok(false) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        let args = args.iter().map(|arg| arg.to_string());
        parse_options(args).unwrap().unwrap()
    }

    #[test]
    fn headless_takes_an_optional_size() {
        assert_eq!(parse(&["--headless"]).headless, Some((1280, 720)));
        assert_eq!(parse(&["--headless", "640x480"]).headless, Some((640, 480)));
        let options = parse(&["--headless", "-c", "foot"]);
        assert_eq!(options.headless, Some((1280, 720)));
        assert_eq!(options.commands, ["foot"]);
        assert_eq!(parse(&[]).headless, None);
    }

    #[test]
    fn test_input_is_off_unless_asked_for() {
        assert!(!parse(&[]).test_input);
        assert!(parse(&["--test-input"]).test_input);
    }

    struct Named;

    impl Extension for Named {
        fn info(&self) -> extension::ExtensionInfo {
            extension::ExtensionInfo {
                name: "named",
                check: |_| Ok(()),
                usage: "wirbel named on|off",
                help: "named does what its name says.",
            }
        }

        fn command(&self, _: &mut Wirbel, _: &str) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn the_usage_lists_the_extensions() {
        assert_eq!(usage(&[]), USAGE);
        let text = usage(&[Rc::new(Named)]);
        let plug = text.find("wirbel plug [list").unwrap();
        let line = text.find("\n       wirbel named on|off\n\n").unwrap();
        let help = text.find("\n\nnamed does what its name says.\n\n").unwrap();
        let last = text.find("All of these talk").unwrap();
        assert!(plug < line && line < help && help < last);
    }

    #[test]
    fn headless_wins_over_a_session() {
        let options = parse(&["--headless"]);
        assert_eq!(choose_backend(&options), Ok(backend::Kind::Headless));
    }
}
