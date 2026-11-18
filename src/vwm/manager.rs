//! The side of the visible compositor: it starts the shadows, knows them by name and ends them.
//!
//! Requests on the control socket (fields of `open` are separated by tabs, because a command
//! and its environment may hold spaces):
//!
//! - `vwm open` TAB NAME|- TAB WxH|- TAB share|private TAB K=V pairs separated by U+001F TAB
//!   COMMAND: starts a shadow with that program. Answers `ok NAME` once the shadow takes
//!   requests, or an error if it ended before.
//! - `vwm list`: a line `entry` TAB NAME TAB KIND TAB STATE TAB PID TAB DISPLAY TAB WxH TAB
//!   SECONDS TAB COMMAND for each, then `ok`.
//! - `vwm where NAME`: `ok DISPLAY`, the Wayland display of the shadow, whose control socket
//!   takes everything else (`shadow`, `inject`, `windows`, `subscribe`).
//! - `vwm resized NAME WxH`: notes the new size for `list`.
//! - `vwm close NAME|all [kill]`: asks the windows to close; a shadow that has not ended a few
//!   seconds later is ended. With `kill` at once.
//!
//! A shadow is a process group of its own with everything it started. It ends with the
//! compositor.

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use smithay::reexports::calloop::{
    channel::{self, Sender},
    timer::{TimeoutAction, Timer},
};

use super::{DEFAULT_SIZE, shadow, valid_name};
use crate::{Wirbel, backend::headless::parse_size, ipc::socket_path, safety};

/// How long a shadow may take to close its windows before it is ended.
const CLOSE_PATIENCE: Duration = Duration::from_secs(5);
/// How long it may take to end before it is killed.
const TERM_PATIENCE: Duration = Duration::from_secs(3);
/// More shadows than this at once is a runaway script.
pub const MAX_SHADOWS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Started, not taking requests yet.
    Starting,
    Ready,
    /// Asked to close.
    Closing,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Closing => "closing",
        }
    }
}

#[derive(Debug)]
pub struct Entry {
    pub name: String,
    pub command: String,
    pub size: (i32, i32),
    /// The process group of the shadow.
    pub group: libc::pid_t,
    /// The Wayland display of the shadow, once it is ready.
    pub display: Option<String>,
    pub phase: Phase,
    pub started: Instant,
    /// Connections that asked for it and wait for it to be ready.
    waiting: Vec<u64>,
}

#[derive(Debug)]
enum Event {
    Ready {
        group: libc::pid_t,
        display: String,
    },
    Gone {
        group: libc::pid_t,
        code: Option<i32>,
    },
}

#[derive(Default)]
pub struct Manager {
    pub entries: Vec<Entry>,
    /// Where the threads that read the shadows' output report. Set up with the first shadow.
    sender: Option<Sender<Event>>,
    /// The wirbel binary that runs the shadows, if not this one. For tests, whose own binary
    /// is no wirbel.
    pub binary: Option<PathBuf>,
    /// Where the folders of the invisible windows go, if not the state folder.
    pub state_root: Option<PathBuf>,
    /// In a shadow: the pictures that wait for the next frame, see `super::shot`.
    pub shots: Vec<super::shot::Shot>,
    /// Windows of the desktop taken out of sight, see `super::park`.
    pub parked: Vec<super::park::Parked>,
    /// Pictures of parked windows that wait for the next frame.
    pub parked_shots: Vec<(super::shot::Shot, smithay::desktop::Window)>,
    /// See `Wirbel::reply_sender`.
    pub(crate) replies: Option<Sender<(u64, String)>>,
}

/// What `vwm open` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub name: Option<String>,
    pub size: (i32, i32),
    /// Whether the program shares the D-Bus session bus of the desktop.
    pub share_bus: bool,
    pub env: Vec<(String, String)>,
    pub command: String,
}

impl Open {
    pub fn to_line(&self) -> String {
        let env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!(
            "vwm open\t{}\t{}x{}\t{}\t{}\t{}",
            self.name.as_deref().unwrap_or("-"),
            self.size.0,
            self.size.1,
            if self.share_bus { "share" } else { "private" },
            env.join("\u{1f}"),
            self.command
        )
    }

    fn parse(rest: &str) -> Result<Self, String> {
        let mut fields = rest.splitn(5, '\t');
        let mut next = || fields.next().ok_or("incomplete open request");
        let name = match next()? {
            "-" => None,
            name if valid_name(name) => Some(name.to_string()),
            name => {
                return Err(format!(
                    "`{name}` cannot be a name: letters, digits, - and _"
                ));
            }
        };
        let size = match next()? {
            "-" => DEFAULT_SIZE,
            size => parse_size(size).ok_or("expected WxH, such as 1280x800")?,
        };
        let share_bus = match next()? {
            "share" => true,
            "private" => false,
            other => return Err(format!("expected share or private, got `{other}`")),
        };
        let mut env = Vec::new();
        for pair in next()?.split('\u{1f}').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or("expected KEY=VALUE")?;
            env.push((key.to_string(), value.to_string()));
        }
        let command = next()?.trim().to_string();
        if command.is_empty() {
            return Err("no command".into());
        }
        Ok(Self {
            name,
            size,
            share_bus,
            env,
            command,
        })
    }
}

/// The first word of a shell command without its folder: `firefox` for `/usr/bin/firefox -x`.
/// Leading `VAR=value` words are skipped.
pub fn program_of(command: &str) -> &str {
    let word = command
        .split_ascii_whitespace()
        .find(|word| !word.contains('=') || word.starts_with('/'))
        .unwrap_or("window");
    word.rsplit('/').next().unwrap_or(word)
}

/// Where the files of an invisible window live: its log, the profiles of its programs.
pub fn state_dir(name: &str) -> Option<PathBuf> {
    let log = safety::default_log_path()?;
    Some(log.parent()?.join("vwm").join(name))
}

impl Manager {
    fn dir_of(&self, name: &str) -> Option<PathBuf> {
        match &self.state_root {
            Some(root) => Some(root.join(name)),
            None => state_dir(name),
        }
    }
}

/// What a program needs to run as an instance of its own beside a visible one: browsers lock
/// their profile and hand a second start to the first. The command with those flags added.
pub fn with_recipe(command: &str, dir: &Path) -> String {
    let program = program_of(command);
    let profile = dir.join("profile");
    let quoted = shell_quote(&profile.to_string_lossy());
    let firefox = [
        "firefox",
        "firefox-esr",
        "librewolf",
        "waterfox",
        "zen-browser",
    ];
    let chromium = [
        "chromium",
        "chrome",
        "google-chrome",
        "brave",
        "vivaldi",
        "microsoft-edge",
        "opera",
    ];
    let has = |flag: &str| {
        command
            .split_ascii_whitespace()
            .any(|w| w.starts_with(flag))
    };
    if firefox.contains(&program) {
        if has("--profile") || has("-profile") || has("-P") {
            return command.to_string();
        }
        let _ = fs::create_dir_all(&profile);
        let prefs = profile.join("user.js");
        if !prefs.exists() {
            let _ = fs::write(&prefs, FIREFOX_QUIET_START);
        }
        return format!("{command} --new-instance --profile {quoted}");
    }
    if chromium.iter().any(|name| program.starts_with(name)) {
        if has("--user-data-dir") {
            return command.to_string();
        }
        let mut out = format!("{command} --user-data-dir={quoted}");
        for flag in ["--no-first-run", "--no-default-browser-check"] {
            if !has(flag) {
                out.push(' ');
                out.push_str(flag);
            }
        }
        return out;
    }
    command.to_string()
}

/// Settings for a fresh Firefox profile: no welcome page, no privacy notice in a second tab,
/// no question about the default browser. Without them the first start opens pages of its own
/// that take the keyboard while a macro already types.
const FIREFOX_QUIET_START: &str = "\
user_pref(\"browser.aboutwelcome.enabled\", false);
user_pref(\"browser.startup.homepage_override.mstone\", \"ignore\");
user_pref(\"startup.homepage_welcome_url\", \"\");
user_pref(\"startup.homepage_welcome_url.additional\", \"\");
user_pref(\"datareporting.policy.dataSubmissionPolicyBypassNotification\", true);
user_pref(\"datareporting.policy.firstRunURL\", \"\");
user_pref(\"toolkit.telemetry.reportingpolicy.firstRun\", false);
user_pref(\"browser.shell.checkDefaultBrowser\", false);
user_pref(\"trailhead.firstrun.didSeeAboutWelcome\", true);
";

/// The card for shadows when the configuration names none: with several cards, one that is not
/// driven by `nvidia`. That is the integrated one, which is enough for windows nobody looks at
/// and leaves the other to the screen. It also keeps shadows off a card whose user space
/// driver may be missing where they run, which ends in a crash inside Mesa. `None` leaves the
/// choice to the headless backend.
pub fn quiet_card(cards: impl Iterator<Item = (String, String)>) -> Option<String> {
    let mut cards: Vec<(String, String)> = cards.collect();
    cards.sort();
    let other = cards.iter().find(|(_, driver)| driver != "nvidia")?;
    (cards.len() > 1).then(|| other.0.clone())
}

/// `text` as one word of a shell command.
pub fn shell_quote(text: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// The wirbel binary to start a shadow with: this one, or the one of that name if this file was
/// replaced by an update meanwhile.
fn own_binary() -> PathBuf {
    match env::current_exe() {
        Ok(path) if path.is_file() => path,
        _ => env::args_os()
            .next()
            .map_or_else(|| "wirbel".into(), PathBuf::from),
    }
}

impl Manager {
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// A free name made of `base` and a number.
    fn free_name(&self, base: &str) -> String {
        let base: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(24)
            .collect();
        let base = if base.is_empty() { "window" } else { &base };
        (1..)
            .map(|n| format!("{base}{n}"))
            .find(|name| self.find(name).is_none())
            .unwrap_or_default()
    }
}

impl Wirbel {
    fn vwm_sender(&mut self) -> Result<Sender<Event>, String> {
        if let Some(sender) = &self.vwm.sender {
            return Ok(sender.clone());
        }
        let (sender, receiver) = channel::channel();
        self.loop_handle
            .insert_source(receiver, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.vwm_event(event);
                }
            })
            .map_err(|err| format!("no channel for the shadows: {err}"))?;
        self.vwm.sender = Some(sender.clone());
        Ok(sender)
    }

    /// Starts a shadow. `peer` hears `ok NAME` once it is ready. Returns the name.
    pub fn vwm_open(&mut self, open: Open, peer: Option<u64>) -> Result<String, String> {
        if self.shadow.is_some() {
            return Err("a shadow does not start shadows".into());
        }
        if self.vwm.entries.len() >= MAX_SHADOWS {
            return Err(format!("{MAX_SHADOWS} invisible windows run already"));
        }
        let name = match open.name {
            Some(name)
                if self.vwm.find(&name).is_some()
                    || self.vwm.parked.iter().any(|parked| parked.name == name) =>
            {
                return Err(format!("there is an invisible window `{name}` already"));
            }
            Some(name) => name,
            None => self.vwm.free_name(program_of(&open.command)),
        };
        let dir = self
            .vwm
            .dir_of(&name)
            .ok_or("cannot tell where the state folder is")?;
        // The shadow runs in another folder, and the path goes into commands.
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let log = fs::File::create(dir.join("shadow.log"))
            .map_err(|err| format!("no log for the shadow: {err}"))?;
        let command = with_recipe(&open.command, &dir);

        let shadow_args = [
            "--shadow".to_string(),
            name.clone(),
            format!("{}x{}", open.size.0, open.size.1),
            "-c".to_string(),
            command,
        ];
        let bus = (!open.share_bus)
            .then(|| find_in_path("dbus-run-session"))
            .flatten();
        let binary = self.vwm.binary.clone().unwrap_or_else(own_binary);
        let mut child = match bus {
            Some(bus) => {
                let mut child = Command::new(bus);
                child.arg("--").arg(binary);
                child
            }
            None => Command::new(binary),
        };
        let gpu = self.config.vwm_gpu.clone();
        let gpu = gpu.or_else(|| self.config.render_gpu.clone()).or_else(|| {
            let cards = fs::read_dir("/sys/class/drm")
                .into_iter()
                .flatten()
                .flatten();
            let cards = cards.filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let driver = fs::read_link(entry.path().join("device/driver")).ok()?;
                let card = name
                    .strip_prefix("card")?
                    .chars()
                    .all(|c| c.is_ascii_digit());
                card.then(|| (name, driver.file_name().map(|d| d.to_os_string())))
            });
            quiet_card(cards.map(|(name, driver)| {
                (
                    name,
                    driver
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            }))
        });
        match gpu {
            Some(gpu) => child.env(shadow::GPU_ENV, gpu),
            None => child.env_remove(shadow::GPU_ENV),
        };
        child
            .args(shadow_args)
            .envs(open.env.iter().map(|(k, v)| (k, v)))
            .env(shadow::KEYMAP_ENV, shadow::keymap_of(&self.config))
            .env(shadow::PARENT_ENV, std::process::id().to_string())
            .env(
                "RUST_LOG",
                env::var("RUST_LOG").unwrap_or("wirbel=info,warn".into()),
            )
            .env_remove(safety::SESSION_ENV)
            .env_remove("WIRBEL_RESTARTED")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .process_group(0);
        if let Some(home) = env::var_os("HOME").filter(|home| Path::new(home).is_dir()) {
            child.current_dir(home);
        }
        let sender = self.vwm_sender()?;
        let mut child = child
            .spawn()
            .map_err(|err| format!("failed to start the shadow: {err}"))?;
        let group = child.id() as libc::pid_t;
        safety::end_at_exit(group);
        let stdout = child.stdout.take();
        std::thread::spawn(move || {
            let mut code = None;
            for line in stdout
                .into_iter()
                .flat_map(|out| BufReader::new(out).lines())
            {
                let Ok(line) = line else { break };
                let mut words = line.split_ascii_whitespace();
                match (words.next(), words.next(), words.next()) {
                    (Some("shadow"), Some("ready"), Some(display)) => {
                        let display = display.to_string();
                        let _ = sender.send(Event::Ready { group, display });
                    }
                    (Some("shadow"), Some("exit"), Some(value)) => code = value.parse().ok(),
                    _ => {}
                }
            }
            let status = child.wait().ok().and_then(|status| status.code());
            safety::group_left(group);
            let _ = sender.send(Event::Gone {
                group,
                code: code.or(status),
            });
        });
        self.vwm.entries.push(Entry {
            name: name.clone(),
            command: open.command,
            size: open.size,
            group,
            display: None,
            phase: Phase::Starting,
            started: Instant::now(),
            waiting: peer.into_iter().collect(),
        });
        tracing::info!(name, group, "shadow started");
        Ok(name)
    }

    fn vwm_event(&mut self, event: Event) {
        match event {
            Event::Ready { group, display } => {
                let Some(entry) = self.vwm.entries.iter_mut().find(|e| e.group == group) else {
                    return;
                };
                entry.display = Some(display);
                if entry.phase == Phase::Starting {
                    entry.phase = Phase::Ready;
                }
                let answer = format!("ok {}", entry.name);
                for peer in std::mem::take(&mut entry.waiting) {
                    self.ipc.reply(peer, &answer);
                }
            }
            Event::Gone { group, code } => {
                let Some(index) = self.vwm.entries.iter().position(|e| e.group == group) else {
                    return;
                };
                let entry = self.vwm.entries.remove(index);
                tracing::info!(name = entry.name, ?code, "shadow ended");
                // Whatever the shadow left behind in its group goes with it.
                // Safety: the group was ours; if it is empty the signal goes nowhere.
                unsafe { libc::kill(-entry.group, libc::SIGTERM) };
                let answer = format!(
                    "error the shadow `{}` ended before it was ready, see {}",
                    entry.name,
                    self.vwm
                        .dir_of(&entry.name)
                        .map(|dir| dir.join("shadow.log").display().to_string())
                        .unwrap_or_default()
                );
                for peer in entry.waiting {
                    self.ipc.reply(peer, &answer);
                }
            }
        }
    }

    /// Ends a shadow: politely, unless `kill`.
    pub fn vwm_close(&mut self, name: &str, kill: bool) -> Result<(), String> {
        let entry = self
            .vwm
            .entries
            .iter_mut()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("no invisible window `{name}`"))?;
        let group = entry.group;
        if kill {
            // Safety: the group is ours until its `Gone` event, which removes the entry.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            return Ok(());
        }
        entry.phase = Phase::Closing;
        let asked = entry
            .display
            .as_ref()
            .and_then(|display| socket_path(display.as_ref()))
            .and_then(|path| UnixStream::connect(path).ok())
            .is_some_and(|mut stream| stream.write_all(b"shadow close\n").is_ok());
        let patience = if asked {
            CLOSE_PATIENCE
        } else {
            Duration::ZERO
        };
        let mut signal = libc::SIGTERM;
        let timer = Timer::from_duration(patience);
        let inserted = self.loop_handle.insert_source(timer, move |_, _, state| {
            if !state.vwm.entries.iter().any(|entry| entry.group == group) {
                return TimeoutAction::Drop;
            }
            // Safety: as above, the entry is still there, so the group is still ours.
            unsafe { libc::kill(-group, signal) };
            if signal == libc::SIGKILL {
                return TimeoutAction::Drop;
            }
            signal = libc::SIGKILL;
            TimeoutAction::ToDuration(TERM_PATIENCE)
        });
        inserted.map_err(|err| format!("no timer: {err}"))?;
        Ok(())
    }

    fn vwm_list(&self) -> String {
        let mut answer = String::new();
        for entry in &self.vwm.entries {
            answer += &format!(
                "entry\t{}\tshadow\t{}\t{}\t{}\t{}x{}\t{}\t{}\n",
                entry.name,
                entry.phase.name(),
                entry.group,
                entry.display.as_deref().unwrap_or("-"),
                entry.size.0,
                entry.size.1,
                entry.started.elapsed().as_secs(),
                entry.command.replace(['\t', '\n'], " "),
            );
        }
        answer + &self.parked_list() + "ok"
    }
}

/// A `vwm ...` request. `Ok(None)` answers later.
pub(crate) fn request(state: &mut Wirbel, peer: u64, rest: &str) -> Result<Option<String>, String> {
    if let Some(answer) = super::park::request(state, peer, rest) {
        return answer;
    }
    if let Some(open) = rest.strip_prefix("open\t") {
        state.vwm_open(Open::parse(open)?, Some(peer))?;
        return Ok(None);
    }
    let mut words = rest.split_ascii_whitespace();
    let answer = match (words.next(), words.next(), words.next(), words.next()) {
        (Some("list"), None, None, None) => state.vwm_list(),
        (Some("where"), Some(name), None, None) => {
            let entry = state
                .vwm
                .find(name)
                .ok_or_else(|| format!("no invisible window `{name}`"))?;
            let display = entry
                .display
                .as_ref()
                .ok_or_else(|| format!("`{name}` is still starting"))?;
            format!("ok {display}")
        }
        (Some("resized"), Some(name), Some(size), None) => {
            let size = parse_size(size).ok_or("expected WxH")?;
            if let Some(entry) = state.vwm.entries.iter_mut().find(|e| e.name == name) {
                entry.size = size;
            }
            "ok".into()
        }
        (Some("close"), Some(name), kill @ (None | Some("kill")), None) => {
            if name == "all" {
                for window in state.parked_windows() {
                    state.close_window(&window);
                }
            }
            let names: Vec<String> = match name {
                "all" => state.vwm.entries.iter().map(|e| e.name.clone()).collect(),
                name => vec![name.to_string()],
            };
            for name in names {
                state.vwm_close(&name, kill.is_some())?;
            }
            "ok".into()
        }
        _ => return Err("unknown vwm request".into()),
    };
    Ok(Some(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_survives_the_wire() {
        let open = Open {
            name: Some("browser1".into()),
            size: (800, 600),
            share_bus: false,
            env: vec![("A".into(), "one two".into()), ("B".into(), "x=y".into())],
            command: "firefox --new-tab 'a b'".into(),
        };
        let line = open.to_line();
        let rest = line.strip_prefix("vwm open\t").unwrap();
        assert_eq!(Open::parse(rest), Ok(open));
        let plain = Open::parse("-\t-\tshare\t\tfoot").unwrap();
        assert_eq!(plain.name, None);
        assert_eq!(plain.size, DEFAULT_SIZE);
        assert!(plain.share_bus && plain.env.is_empty());
        assert!(Open::parse("two words\t-\tshare\t\tfoot").is_err());
        assert!(Open::parse("-\t-\tshare\t\t  ").is_err());
        assert!(Open::parse("-\t-\tshare").is_err());
    }

    #[test]
    fn programs_and_names() {
        assert_eq!(program_of("/usr/bin/firefox -x"), "firefox");
        assert_eq!(program_of("LANG=C foot"), "foot");
        assert_eq!(program_of(""), "window");
        let mut manager = Manager::default();
        assert_eq!(manager.free_name("firefox"), "firefox1");
        manager.entries.push(Entry {
            name: "firefox1".into(),
            command: String::new(),
            size: DEFAULT_SIZE,
            group: 1,
            display: None,
            phase: Phase::Starting,
            started: Instant::now(),
            waiting: Vec::new(),
        });
        assert_eq!(manager.free_name("firefox"), "firefox2");
        assert_eq!(manager.free_name("my.app"), "myapp1");
        assert_eq!(manager.free_name("..."), "window1");
    }

    #[test]
    fn recipes_isolate_browsers() {
        let dir = Path::new("/nonexistent/state");
        assert_eq!(with_recipe("foot", dir), "foot");
        assert_eq!(
            with_recipe("chromium --incognito", dir),
            "chromium --incognito --user-data-dir=/nonexistent/state/profile \
             --no-first-run --no-default-browser-check"
        );
        assert_eq!(
            with_recipe("chromium --user-data-dir=/x", dir),
            "chromium --user-data-dir=/x"
        );
        assert_eq!(
            with_recipe("firefox --profile /x", dir),
            "firefox --profile /x"
        );
    }

    #[test]
    fn a_fresh_firefox_profile_starts_quietly() {
        let dir = std::env::temp_dir().join(format!("wirbel-recipe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let command = with_recipe("firefox", &dir);
        assert!(command.ends_with(
            &("--new-instance --profile ".to_string() + &dir.join("profile").to_string_lossy())
        ));
        let prefs = fs::read_to_string(dir.join("profile/user.js")).unwrap();
        assert!(prefs.contains("browser.aboutwelcome.enabled\", false"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadows_keep_off_the_nvidia_card_if_there_is_another() {
        let cards = |list: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = list
                .iter()
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .collect();
            quiet_card(owned.into_iter())
        };
        assert_eq!(
            cards(&[("card1", "nvidia"), ("card0", "amdgpu")]),
            Some("card0".into())
        );
        assert_eq!(
            cards(&[("card0", "nvidia"), ("card1", "i915")]),
            Some("card1".into())
        );
        assert_eq!(cards(&[("card0", "nvidia")]), None);
        assert_eq!(cards(&[("card0", "amdgpu")]), None);
        assert_eq!(cards(&[("card0", "nvidia"), ("card1", "nvidia")]), None);
        assert_eq!(cards(&[]), None);
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("plain-word_1.txt"), "plain-word_1.txt");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
