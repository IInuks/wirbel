//! The shadow side: what `wirbel --shadow NAME [WxH] -c COMMAND` does differently from a plain
//! headless wirbel.
//!
//! - No binds, borders or gaps: every key goes to the program, and its window fills the output.
//!   Further windows (dialogs) float on top in the middle.
//! - It starts the commands itself and ends when they have ended and no window is left, or when
//!   the compositor that started it is gone.
//! - On stdout it says `shadow ready WAYLAND_DISPLAY X_DISPLAY|-` once clients can connect, and
//!   `shadow exit CODE` before it ends.
//!
//! Requests on its control socket: `shadow close` asks every window to close, `shadow resize WxH`
//! changes the size of the output and with it of the window, `shadow size` answers `WxH`.
//! `shadow status` answers its state in a line, `shadow bus` the address of its session bus.
//! `shadow shot` and `shadow clip` are in `super::shot` and `super::clip`.

use std::{
    env,
    io::Write,
    process::Child,
    time::{Duration, Instant},
};

use smithay::reexports::calloop::{
    LoopHandle,
    timer::{TimeoutAction, Timer},
};

use crate::{Wirbel, backend::headless::parse_size, config::Config, layout::Gaps};

/// Layout, variant and options of the keyboard, separated by tabs: a shadow types on the same
/// keyboard as the desktop that started it.
pub const KEYMAP_ENV: &str = "WIRBEL_SHADOW_KEYMAP";
/// The `render_gpu` of the desktop: a shadow renders on the same card.
pub const GPU_ENV: &str = "WIRBEL_SHADOW_GPU";
/// The process id of the compositor that started the shadow.
pub const PARENT_ENV: &str = "WIRBEL_SHADOW_PARENT";

const TICK: Duration = Duration::from_millis(250);
/// How long a shadow without program and window stays: a launcher may end before its program
/// shows a window.
const GRACE: Duration = Duration::from_secs(3);

pub struct Shadow {
    pub name: String,
    children: Vec<Child>,
    /// Since when there is neither a running command nor a window.
    empty_since: Option<Instant>,
    /// Whether `shadow close` was asked: no grace then.
    closing: bool,
    parent: Option<libc::pid_t>,
    /// How many frames changed the picture, and when the last one did.
    pub damage: u64,
    pub last_damage: Option<Instant>,
    /// How often the clipboard got a new content.
    pub clipboard: u64,
}

/// The configuration of a shadow.
pub fn config() -> Config {
    let mut config = Config {
        gaps: Gaps { outer: 0, inner: 0 },
        border_width: 0,
        sloppy_focus: false,
        warp_cursor: false,
        ..Config::default()
    };
    config.binds.clear();
    config.mouse_binds.clear();
    config.axis_binds.clear();
    config.exec_once.clear();
    config.plugins.clear();
    config.monitor_rules.clear();
    if let Ok(keymap) = env::var(KEYMAP_ENV) {
        let mut parts = keymap.split('\t');
        config.keyboard_layout = parts.next().unwrap_or_default().to_string();
        config.keyboard_variant = parts.next().unwrap_or_default().to_string();
        config.keyboard_options = parts.next().unwrap_or_default().to_string();
    }
    config.render_gpu = env::var(GPU_ENV).ok().filter(|gpu| !gpu.is_empty());
    config
}

/// The value of [`KEYMAP_ENV`] for this configuration.
pub fn keymap_of(config: &Config) -> String {
    format!(
        "{}\t{}\t{}",
        config.keyboard_layout, config.keyboard_variant, config.keyboard_options
    )
}

fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    // Whoever started us may be gone; the parent check ends the shadow then.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Makes the compositor a shadow: starts the commands, says that it is ready and watches for
/// its end. Called once clients can connect.
pub fn start(
    state: &mut Wirbel,
    handle: &LoopHandle<'static, Wirbel>,
    name: &str,
    commands: &[String],
) {
    // The pointer starts in the bottom right corner: in the top left it rests on the first tab
    // of a browser, which then shows its tooltip over the page.
    let corner = (1e6, 1e6).into();
    state.move_pointer(corner, smithay::backend::input::InputTime::now());
    let mut children = Vec::new();
    for command in commands {
        let mut child = state.client_command(command);
        // Toolkits build their accessibility tree only when told that someone listens.
        child
            .env("GNOME_ACCESSIBILITY", "1")
            .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
            .env("ACCESSIBILITY_ENABLED", "1");
        match child.spawn() {
            Ok(child) => children.push(child),
            Err(err) => tracing::error!(%err, command, "failed to start"),
        }
    }
    let parent = env::var(PARENT_ENV).ok().and_then(|pid| pid.parse().ok());
    state.shadow = Some(Shadow {
        name: name.to_string(),
        children,
        empty_since: None,
        closing: false,
        parent,
        damage: 0,
        last_damage: None,
        clipboard: 0,
    });
    let x = state.xwayland.display;
    let x = x.map_or("-".to_string(), |display| display.to_string());
    say(&format!(
        "shadow ready {} {x}",
        state.socket_name.to_string_lossy()
    ));
    let timer = handle.insert_source(Timer::from_duration(TICK), |_, _, state| {
        if finished(state) {
            let code = state.shadow.as_mut().and_then(Shadow::exit_code);
            say(&format!("shadow exit {}", code.unwrap_or(0)));
            state.loop_signal.stop();
            return TimeoutAction::Drop;
        }
        TimeoutAction::ToDuration(TICK)
    });
    if let Err(err) = timer {
        tracing::error!(%err, "the shadow cannot watch for its end");
    }
}

impl Shadow {
    /// A frame changed the picture.
    pub fn damaged(&mut self) {
        self.damage += 1;
        self.last_damage = Some(Instant::now());
    }

    /// How the first command ended, if it has.
    fn exit_code(&mut self) -> Option<i32> {
        let status = self.children.first_mut()?.try_wait().ok()??;
        status.code()
    }
}

/// Whether the shadow has nothing left to do.
fn finished(state: &mut Wirbel) -> bool {
    let windows = !state.listed_windows().is_empty()
        || !state.unconfigured.is_empty()
        || !state.awaiting_buffer.is_empty();
    let Some(shadow) = state.shadow.as_mut() else {
        return false;
    };
    // Safety: signal 0 only asks whether the process exists.
    if let Some(parent) = shadow.parent
        && unsafe { libc::kill(parent, 0) } != 0
    {
        tracing::info!("the compositor that started this shadow is gone");
        return true;
    }
    let running = shadow
        .children
        .iter_mut()
        .any(|child| matches!(child.try_wait(), Ok(None)));
    if running || windows {
        shadow.empty_since = None;
        return false;
    }
    let since = *shadow.empty_since.get_or_insert_with(Instant::now);
    shadow.closing || since.elapsed() >= GRACE
}

/// After the event loop: the commands that still run end with the shadow.
pub fn end(state: &mut Wirbel) {
    let Some(shadow) = state.shadow.as_mut() else {
        return;
    };
    for child in &mut shadow.children {
        if matches!(child.try_wait(), Ok(None)) {
            // Safety: the child is ours and was not reaped, so the id is still its own.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        }
    }
}

/// A `shadow ...` request. `Ok(None)` answers later.
pub(crate) fn request(state: &mut Wirbel, peer: u64, rest: &str) -> Result<Option<String>, String> {
    if state.shadow.is_none() {
        return Err("this wirbel is not a shadow".into());
    }
    if let Some(shot) = rest.strip_prefix("shot\t") {
        state.vwm.shots.push(super::shot::Shot::parse(peer, shot)?);
        state.needs_redraw = true;
        (state.redraw)();
        return Ok(None);
    }
    if let Some(clip) = rest.strip_prefix("clip") {
        return super::clip::request(state, peer, clip);
    }
    plain_request(state, rest).map(Some)
}

fn plain_request(state: &mut Wirbel, rest: &str) -> Result<String, String> {
    let mut words = rest.split_ascii_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("close"), None, None) => {
            for window in state.listed_windows() {
                state.close_window(&window);
            }
            if let Some(shadow) = state.shadow.as_mut() {
                shadow.closing = true;
            }
            Ok("ok".into())
        }
        (Some("resize"), Some(size), None) => {
            let size = parse_size(size).ok_or("expected WxH, such as 1280x800")?;
            state.resize_headless(size)?;
            Ok("ok".into())
        }
        (Some("size"), None, None) => {
            let size = state
                .monitors
                .first()
                .and_then(|monitor| monitor.output.current_mode())
                .map(|mode| mode.size)
                .ok_or("no output")?;
            Ok(format!("{}x{}", size.w, size.h))
        }
        (Some("status"), None, None) => Ok(status(state)),
        // Where the programs of this shadow find their session bus, for `super::a11y`.
        (Some("bus"), None, None) => env::var("DBUS_SESSION_BUS_ADDRESS")
            .map(|address| format!("ok {address}"))
            .map_err(|_| "the shadow has no session bus".into()),
        _ => Err("expected close, resize WxH, size, status, shot or clip".into()),
    }
}

/// The state of a shadow in one line, for whoever waits for something:
/// `status DAMAGE QUIET_MS WINDOWS running|exited:CODE CURSOR CLIPBOARD TITLE`. DAMAGE counts
/// the frames that changed the picture, QUIET_MS is the time since the last of them, WINDOWS
/// the windows that have drawn, CURSOR the name of the pointer shape (`custom` for a picture of
/// the program, `hidden`), CLIPBOARD counts its contents, TITLE is the title of the window
/// with the keyboard and may hold spaces.
fn status(state: &mut Wirbel) -> String {
    use smithay::input::pointer::CursorImageStatus;
    let windows = state.listed_windows();
    let title = state
        .focused
        .as_ref()
        .or(windows.first())
        .map(|window| crate::handlers::window_names(window).0)
        .unwrap_or_default();
    let cursor = match &state.cursor_status {
        CursorImageStatus::Hidden => "hidden".to_string(),
        CursorImageStatus::Surface(_) => "custom".to_string(),
        CursorImageStatus::Named(icon) => icon.name().to_string(),
    };
    let Some(shadow) = state.shadow.as_mut() else {
        return "error this wirbel is not a shadow".into();
    };
    let command = match shadow.children.first_mut().map(Child::try_wait) {
        Some(Ok(Some(status))) => format!("exited:{}", status.code().unwrap_or(-1)),
        Some(_) => "running".to_string(),
        None => "exited:0".to_string(),
    };
    let quiet = shadow
        .last_damage
        .map_or(0, |last| last.elapsed().as_millis());
    format!(
        "status {} {quiet} {} {command} {cursor} {} {}",
        shadow.damage,
        windows.len(),
        shadow.clipboard,
        title.replace(['\n', '\r'], " ")
    )
}

impl Wirbel {
    /// Gives the headless output another size.
    pub fn resize_headless(&mut self, size: (i32, i32)) -> Result<(), String> {
        let mut resize = self
            .resize_output
            .take()
            .ok_or("this backend cannot resize its output")?;
        let result = resize(size);
        self.resize_output = Some(resize);
        let output = result?;
        self.output_resized(&output);
        self.needs_redraw = true;
        (self.redraw)();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shadow_has_no_binds_and_no_frame() {
        let config = config();
        assert!(config.binds.is_empty());
        assert!(config.mouse_binds.is_empty());
        assert!(config.axis_binds.is_empty());
        assert!(config.exec_once.is_empty());
        assert_eq!(config.border_width, 0);
        assert_eq!((config.gaps.outer, config.gaps.inner), (0, 0));
    }

    #[test]
    fn the_keymap_survives_the_environment() {
        let config = Config {
            keyboard_layout: "de".into(),
            keyboard_variant: "nodeadkeys".into(),
            keyboard_options: String::new(),
            ..Config::default()
        };
        assert_eq!(keymap_of(&config), "de\tnodeadkeys\t");
    }
}
