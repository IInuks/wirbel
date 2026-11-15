//! Invisible windows: a compositor of the test harness plays the visible desktop and starts real
//! shadows, `wirbel --shadow`, which need a GPU render node like every headless wirbel.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use wayland_client::protocol::wl_keyboard;
use wirbel::{
    test_harness::*,
    vwm::{cli::ask_display, manager::Open},
};

/// Starting a shadow includes an X server.
const PATIENCE: Duration = Duration::from_secs(20);

struct Desk {
    compositor: Compositor,
    root: PathBuf,
}

impl Desk {
    fn start(test: &str) -> Self {
        let compositor = Compositor::start();
        let root = std::env::temp_dir().join(format!("wirbel-vwm-{}-{test}", std::process::id()));
        let state_root = root.clone();
        compositor.with(move |state| {
            state.vwm.binary = Some(env!("CARGO_BIN_EXE_wirbel").into());
            state.vwm.state_root = Some(state_root);
        });
        Self { compositor, root }
    }

    /// All lines of the answer to `request`: one, or a list that ends with `ok`.
    fn ask(&self, request: &str) -> Vec<String> {
        let mut stream = UnixStream::connect(self.compositor.ipc_path()).unwrap();
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let mut lines = Vec::new();
        for line in BufReader::new(stream).lines() {
            let line = line.expect("no answer in time");
            let last = !line.starts_with("entry\t");
            lines.push(line);
            if last {
                break;
            }
        }
        lines
    }

    fn open(&self, name: Option<&str>, command: &str) -> String {
        let open = Open {
            name: name.map(str::to_string),
            size: (640, 400),
            // The bus of whoever runs the tests is good enough, and may be none.
            share_bus: true,
            env: Vec::new(),
            command: command.to_string(),
        };
        self.ask(&open.to_line()).remove(0)
    }

    /// The Wayland display of a shadow.
    fn display(&self, name: &str) -> String {
        let answer = self.ask(&format!("vwm where {name}")).remove(0);
        answer.strip_prefix("ok ").expect(&answer).to_string()
    }

    fn names(&self) -> Vec<String> {
        let lines = self.ask("vwm list");
        assert_eq!(lines.last().map(String::as_str), Some("ok"), "{lines:?}");
        let name = |line: &String| line.split('\t').nth(1).map(str::to_string);
        lines.iter().filter_map(name).collect()
    }

    fn group(&self, name: &str) -> libc::pid_t {
        let lines = self.ask("vwm list");
        let line = lines
            .iter()
            .find(|line| line.split('\t').nth(1) == Some(name));
        line.unwrap().split('\t').nth(4).unwrap().parse().unwrap()
    }

    fn wait_gone(&self, name: &str) {
        let start = Instant::now();
        while self.names().iter().any(|other| other == name) {
            assert!(start.elapsed() < PATIENCE, "`{name}` never ended");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.ask("vwm close all kill");
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn socket_of(display: &str) -> PathBuf {
    Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(display)
}

fn group_alive(group: libc::pid_t) -> bool {
    // Safety: signal 0 only asks whether the group exists.
    unsafe { libc::kill(-group, 0) == 0 }
}

#[test]
fn a_shadow_starts_is_listed_resized_and_closed() {
    let desk = Desk::start("life");
    assert_eq!(desk.names(), Vec::<String>::new());
    assert_eq!(desk.open(Some("one"), "sleep 60"), "ok one");
    assert_eq!(desk.names(), ["one"]);
    let list = desk.ask("vwm list");
    let fields: Vec<&str> = list[0].split('\t').collect();
    assert_eq!(fields[..4], ["entry", "one", "shadow", "ready"]);
    assert_eq!(fields[6], "640x400");
    assert_eq!(fields[8], "sleep 60");

    let display = desk.display("one");
    assert_eq!(ask_display(&display, "shadow size").unwrap(), "640x400");
    assert_eq!(
        ask_display(&display, "shadow resize 800x500").unwrap(),
        "ok"
    );
    assert_eq!(ask_display(&display, "shadow size").unwrap(), "800x500");
    assert_eq!(desk.ask("vwm resized one 800x500"), ["ok"]);
    assert!(desk.ask("vwm list")[0].contains("\t800x500\t"));
    assert!(
        ask_display(&display, "shadow resize 0x5")
            .unwrap()
            .starts_with("error ")
    );

    let group = desk.group("one");
    assert!(group_alive(group));
    assert_eq!(desk.ask("vwm close one"), ["ok"]);
    desk.wait_gone("one");
    let start = Instant::now();
    while group_alive(group) {
        assert!(
            start.elapsed() < PATIENCE,
            "the shadow left processes behind"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!socket_of(&display).exists(), "the shadow left its socket");
}

#[test]
fn names_are_unique_and_made_up_when_missing() {
    let desk = Desk::start("names");
    assert_eq!(desk.open(None, "sleep 60"), "ok sleep1");
    assert_eq!(desk.open(None, "/bin/sleep 60"), "ok sleep2");
    assert_eq!(desk.open(Some("sleep3"), "sleep 60"), "ok sleep3");
    let again = desk.open(Some("sleep3"), "sleep 60");
    assert!(
        again.starts_with("error there is an invisible window"),
        "{again}"
    );
    assert_eq!(desk.names(), ["sleep1", "sleep2", "sleep3"]);
    assert!(desk.ask("vwm where nobody")[0].starts_with("error no invisible window"));
    assert!(desk.ask("vwm close nobody")[0].starts_with("error no invisible window"));
    assert!(desk.ask("vwm dance")[0].starts_with("error "));
}

#[test]
fn a_shadow_ends_with_its_program_and_at_once_when_killed() {
    let desk = Desk::start("ends");
    assert_eq!(desk.open(Some("short"), "true"), "ok short");
    assert_eq!(desk.open(Some("long"), "sleep 60"), "ok long");
    let group = desk.group("long");
    desk.wait_gone("short");
    assert_eq!(desk.names(), ["long"]);
    assert_eq!(desk.ask("vwm close long kill"), ["ok"]);
    desk.wait_gone("long");
    assert!(!group_alive(group));
}

#[test]
fn windows_of_a_shadow_stay_off_the_desktop_and_get_its_input() {
    let desk = Desk::start("windows");
    let mut visible = Client::connect(&desk.compositor);
    let v = visible.open_window(RED);
    visible.take_input();
    let frames = desk.compositor.frames();

    assert_eq!(desk.open(Some("hidden"), "sleep 60"), "ok hidden");
    let display = desk.display("hidden");
    let mut hidden = Client::connect_path(&socket_of(&display));
    let a = hidden.open_window(GREEN);
    hidden.sync();
    // No border, no gaps: the window is the output.
    assert_eq!(hidden.size(a), (640, 400));
    assert!(hidden.activated(a));
    hidden.take_input();

    // The desktop knows nothing of it.
    assert_eq!(desk.compositor.frames(), frames);
    let windows = ask_display(
        &desk.compositor.wayland_display().to_string_lossy(),
        "windows",
    );
    assert_eq!(windows.unwrap().matches("\"id\"").count(), 1);
    assert!(visible.activated(v));

    // Typing into the shadow reaches its window and nobody else.
    assert_eq!(ask_display(&display, "inject tap a").unwrap(), "ok");
    hidden.sync();
    const KEY_A: u32 = 30;
    assert_eq!(
        hidden.take_input(),
        [
            Input::Key(KEY_A, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_A, wl_keyboard::KeyState::Released),
        ]
    );
    visible.sync();
    assert_eq!(visible.take_input(), []);
    assert!(visible.activated(v));

    // A second window floats in front, the first keeps the whole output.
    // It gets no size from the shadow and picks its own, like a dialog.
    let b = hidden.open_sized_window((200, 100), BLUE, |_| ());
    hidden.sync();
    assert_eq!(hidden.size(a), (640, 400));
    let windows = ask_display(&display, "windows").unwrap();
    assert_eq!(windows.matches("\"floating\":true").count(), 1, "{windows}");
    assert_eq!(
        windows.matches("\"floating\":false").count(),
        1,
        "{windows}"
    );
    let _ = b;

    // Closing asks the windows and ends the shadow once they are gone.
    assert_eq!(desk.ask("vwm close hidden"), ["ok"]);
    hidden.sync();
    assert_eq!(hidden.state.closed.len(), 2);
}
