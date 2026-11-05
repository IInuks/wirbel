use std::env;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use wirbel::{Wirbel, backend};

const USAGE: &str = "usage: wirbel [-c COMMAND]...

  -c, --command COMMAND  run a shell command inside the compositor (repeatable)
  -h, --help             show this help";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("wirbel=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let mut commands = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--command" => commands.push(args.next().ok_or(USAGE)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }

    if env::var_os("WAYLAND_DISPLAY").is_none() && env::var_os("DISPLAY").is_none() {
        return Err("no Wayland or X11 session found, and the DRM backend does not exist yet".into());
    }

    let mut event_loop = EventLoop::try_new()?;
    let display = Display::new()?;
    let mut state = Wirbel::new(&mut event_loop, display)?;
    backend::winit::init(&mut event_loop, &mut state)?;

    tracing::info!(socket = ?state.socket_name, "compositor ready");
    for command in &commands {
        state.spawn(command);
    }

    event_loop.run(None, &mut state, Wirbel::after_dispatch)?;
    Ok(())
}
