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
    let start = Instant::now();
    while hidden.state.closed.len() < 2 {
        assert!(
            start.elapsed() < PATIENCE,
            "the windows were never asked to close"
        );
        std::thread::sleep(Duration::from_millis(10));
        hidden.sync();
    }
}

/// A shadow with one green window of a test client that fills it.
fn hidden_window(desk: &Desk, name: &str) -> (String, Client, usize) {
    assert_eq!(desk.open(Some(name), "sleep 60"), format!("ok {name}"));
    let display = desk.display(name);
    let mut client = Client::connect_path(&socket_of(&display));
    let window = client.open_window(GREEN);
    client.sync();
    client.take_input();
    (display, client, window)
}

#[test]
fn the_pointer_goes_to_a_place_and_clicks_there() {
    let desk = Desk::start("pointer");
    let (display, mut hidden, _) = hidden_window(&desk, "hidden");
    assert_eq!(ask_display(&display, "inject at 100 50").unwrap(), "ok");
    assert_eq!(
        ask_display(&display, "inject button left down").unwrap(),
        "ok"
    );
    assert_eq!(
        ask_display(&display, "inject button left up").unwrap(),
        "ok"
    );
    assert_eq!(ask_display(&display, "inject at 9000 9000").unwrap(), "ok");
    hidden.sync();
    const BTN_LEFT: u32 = 0x110;
    use wayland_client::protocol::wl_pointer::ButtonState;
    assert_eq!(
        hidden.take_input(),
        [
            // The pointer was over the window already, which opened under it.
            Input::PointerMotion(100.0, 50.0),
            Input::Button(BTN_LEFT, ButtonState::Pressed),
            Input::Button(BTN_LEFT, ButtonState::Released),
            // Kept on the output.
            Input::PointerMotion(639.0, 399.0),
        ]
    );
    assert!(
        ask_display(&display, "inject at 1")
            .unwrap()
            .starts_with("error ")
    );
}

#[test]
fn a_picture_of_a_shadow_is_what_its_window_shows() {
    use wirbel::vwm::shot::read_png;
    let desk = Desk::start("shot");
    let (display, _hidden, _) = hidden_window(&desk, "hidden");
    let file = desk.root.join("shot.png");
    let request = format!("shadow shot\t{}", file.display());
    assert_eq!(ask_display(&display, &request).unwrap(), "ok 640x400");
    let (rgb, width, height) = read_png(&file).unwrap();
    assert_eq!((width, height), (640, 400));
    assert!(
        rgb.chunks_exact(3).all(|pixel| pixel == [0, 255, 0]),
        "not all green"
    );

    // A part of it, and with the pointer, which is not green.
    assert_eq!(ask_display(&display, "inject at 20 20").unwrap(), "ok");
    let request = format!("shadow shot\t{}\t10,10,40,30\tcursor", file.display());
    assert_eq!(ask_display(&display, &request).unwrap(), "ok 40x30");
    let (rgb, width, height) = read_png(&file).unwrap();
    assert_eq!((width, height), (40, 30));
    assert!(
        rgb.chunks_exact(3).any(|pixel| pixel != [0, 255, 0]),
        "no pointer"
    );
    let request = format!("shadow shot\t{}\t10,10,40,30", file.display());
    assert_eq!(ask_display(&display, &request).unwrap(), "ok 40x30");
    let (rgb, ..) = read_png(&file).unwrap();
    assert!(
        rgb.chunks_exact(3).all(|pixel| pixel == [0, 255, 0]),
        "a pointer"
    );

    let request = format!("shadow shot\t{}\t700,0,10,10", file.display());
    assert!(
        ask_display(&display, &request)
            .unwrap()
            .starts_with("error ")
    );
    assert!(
        ask_display(&display, "shadow shot\tshot.png")
            .unwrap()
            .starts_with("error ")
    );
    // The desktop is no shadow.
    let desktop = desk
        .compositor
        .wayland_display()
        .to_string_lossy()
        .into_owned();
    assert!(
        ask_display(&desktop, &request)
            .unwrap()
            .starts_with("error this wirbel")
    );
}

#[test]
fn the_clipboard_of_a_shadow_is_read_and_set_from_outside() {
    use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
    let desk = Desk::start("clip");
    let (display, mut hidden, _) = hidden_window(&desk, "hidden");
    let file = desk.root.join("clip.txt");
    let get = format!("shadow clip get\t{}", file.display());
    assert!(
        ask_display(&display, &get)
            .unwrap()
            .starts_with("error the clipboard is empty")
    );

    // The window copies something.
    let manager: WlDataDeviceManager = hidden.bind(1..=3);
    let seat = hidden.seat();
    let device = manager.get_data_device(&seat, &hidden.handle, ());
    let source = manager.create_data_source(&hidden.handle, ());
    source.offer("text/plain".into());
    assert_eq!(ask_display(&display, "inject tap a").unwrap(), "ok");
    hidden.sync();
    device.set_selection(Some(&source), hidden.state.key_serial);
    hidden.sync();

    // The answer comes once the window has handed its text over.
    let asking = {
        let (display, get) = (display.clone(), get.clone());
        std::thread::spawn(move || ask_display(&display, &get).unwrap())
    };
    while !asking.is_finished() {
        hidden.sync();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        asking.join().unwrap(),
        format!("ok {}", CLIPBOARD_TEXT.len())
    );
    assert_eq!(std::fs::read(&file).unwrap(), CLIPBOARD_TEXT);

    // Text from outside replaces it: the window loses its selection and can paste ours.
    std::fs::write(&file, "from outside, with ümlauts").unwrap();
    let set = format!("shadow clip set\t{}", file.display());
    assert_eq!(ask_display(&display, &set).unwrap(), "ok");
    hidden.sync();
    assert!(hidden.state.source_events.contains(&SourceEvent::Cancelled));
    std::fs::remove_file(&file).unwrap();
    assert_eq!(ask_display(&display, &get).unwrap(), "ok 27");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "from outside, with ümlauts"
    );

    let offer = hidden
        .state
        .selection
        .clone()
        .expect("no offer of our text");
    let (read, write) = std::io::pipe().unwrap();
    use std::os::fd::AsFd;
    offer.receive("text/plain;charset=utf-8".into(), write.as_fd());
    hidden.sync();
    drop(write);
    let mut text = String::new();
    use std::io::Read;
    { read }.read_to_string(&mut text).unwrap();
    assert_eq!(text, "from outside, with ümlauts");

    assert!(
        ask_display(&display, "shadow clip get\tclip.txt")
            .unwrap()
            .starts_with("error ")
    );
}

#[test]
fn waiting_sees_windows_pictures_files_and_the_end() {
    use wirbel::vwm::{
        shot::write_png,
        wait::{Condition, WaitError, wait_on},
    };
    let desk = Desk::start("wait");
    assert_eq!(desk.open(Some("hidden"), "sleep 60"), "ok hidden");
    let display = desk.display("hidden");
    let soon = Duration::from_millis(200);
    let wait = |condition: Condition, timeout: Duration| {
        wait_on(&display, &condition, timeout, &mut || Ok(()))
    };
    let timed_out =
        |result: Result<String, WaitError>| matches!(result, Err(WaitError::Timeout(_)));

    // Nothing there yet.
    assert!(timed_out(wait(Condition::Ready, soon)));
    assert_eq!(wait(Condition::Closed, soon), Ok(String::new()));
    assert!(timed_out(wait(Condition::Exited, soon)));

    let mut hidden = Client::connect_path(&socket_of(&display));
    let window = hidden.open_unmapped_window_with(|toplevel| {
        toplevel.set_title("Report (loading)".into());
        toplevel.set_app_id("reports".into());
    });
    hidden.draw(window, GREEN);
    hidden.sync();
    assert_eq!(
        wait(Condition::Ready, PATIENCE),
        Ok("Report (loading)".into())
    );
    assert_eq!(
        wait(Condition::Window(Some("reports".into())), soon),
        Ok("Report (loading)".into())
    );
    assert!(timed_out(wait(
        Condition::Window(Some("other".into())),
        soon
    )));
    assert_eq!(
        wait(Condition::Title("Report*".into()), soon),
        Ok("Report (loading)".into())
    );
    assert!(timed_out(wait(Condition::Title("*done*".into()), soon)));
    assert!(timed_out(wait(Condition::Closed, soon)));
    assert_eq!(
        wait(Condition::Stable(Duration::from_millis(100)), PATIENCE),
        Ok(String::new())
    );

    // The picture: a pixel, a part that changes, a small picture found in it.
    let green = Condition::Pixel {
        x: 5,
        y: 5,
        color: [0, 255, 0],
        tolerance: 8,
    };
    let blue = Condition::Pixel {
        x: 5,
        y: 5,
        color: [0, 0, 255],
        tolerance: 8,
    };
    assert_eq!(wait(green, soon), Ok("#00ff00".into()));
    assert!(timed_out(wait(blue.clone(), soon)));
    let outside = Condition::Pixel {
        x: 5000,
        y: 5,
        color: [0, 0, 0],
        tolerance: 0,
    };
    assert!(matches!(wait(outside, soon), Err(WaitError::Failed(_))));
    assert!(timed_out(wait(Condition::Changed(None), soon)));
    assert!(timed_out(wait(
        Condition::Changed(Some((0, 0, 10, 10))),
        soon
    )));

    let small = desk.root.join("small.png");
    let pixels: Vec<u8> = [0, 0, 255, 255].repeat(8 * 6);
    write_png(&small, &pixels, 8, (0, 0, 8, 6)).unwrap();
    let image = Condition::Image {
        path: small.clone(),
        similarity: 0.95,
    };
    assert!(timed_out(wait(image.clone(), soon)));

    // The window turns blue while someone waits for that.
    let waiting = {
        let (display, blue) = (display.clone(), blue.clone());
        std::thread::spawn(move || wait_on(&display, &blue, PATIENCE, &mut || Ok(())))
    };
    std::thread::sleep(Duration::from_millis(100));
    hidden.draw(window, BLUE);
    hidden.sync();
    assert_eq!(waiting.join().unwrap(), Ok("#0000ff".into()));
    assert_eq!(wait(image, soon), Ok("4 3".into()));
    let missing = Condition::Image {
        path: desk.root.join("nothing.png"),
        similarity: 0.95,
    };
    assert!(matches!(wait(missing, soon), Err(WaitError::Failed(_))));

    // A file, the clipboard, the tick of whoever waits.
    let file = desk.root.join("made.txt");
    assert!(timed_out(wait(Condition::File(file.clone()), soon)));
    std::fs::write(&file, "x").unwrap();
    assert_eq!(
        wait(Condition::File(file.clone()), soon),
        Ok(file.display().to_string())
    );
    assert!(timed_out(wait(Condition::Clipboard, soon)));
    let mut ticks = 0;
    let mut tick = || {
        ticks += 1;
        if ticks == 2 {
            let set = format!("shadow clip set\t{}", file.display());
            assert_eq!(ask_display(&display, &set).unwrap(), "ok");
        }
        Ok(())
    };
    assert_eq!(
        wait_on(&display, &Condition::Clipboard, PATIENCE, &mut tick),
        Ok(String::new())
    );
    let stopped = wait_on(&display, &Condition::Exited, PATIENCE, &mut || {
        Err("stop".into())
    });
    assert_eq!(stopped, Err(WaitError::Failed("stop".into())));

    // The end: the window closes, then the shadow is gone, which also counts as closed.
    hidden.close(window);
    hidden.sync();
    assert_eq!(wait(Condition::Closed, PATIENCE), Ok(String::new()));
    assert_eq!(desk.ask("vwm close hidden kill"), ["ok"]);
    desk.wait_gone("hidden");
    assert_eq!(wait(Condition::Closed, soon), Ok(String::new()));
    assert_eq!(wait(Condition::Exited, soon), Ok(String::new()));
    assert!(matches!(
        wait(Condition::Ready, soon),
        Err(WaitError::Failed(_))
    ));
}

#[test]
fn text_is_read_through_the_clipboard_and_recognition_says_if_it_cannot() {
    use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
    use wirbel::vwm::{
        text::{NO_TESSERACT, by_clipboard},
        wait::{Condition, WaitError, wait_on},
    };
    let desk = Desk::start("text");
    let (display, mut hidden, _) = hidden_window(&desk, "hidden");
    let manager: WlDataDeviceManager = hidden.bind(1..=3);
    let seat = hidden.seat();
    let device = manager.get_data_device(&seat, &hidden.handle, ());
    let source = manager.create_data_source(&hidden.handle, ());
    source.offer("text/plain".into());

    // The window copies when it sees the key, as a program does on select all and copy.
    let reading = {
        let display = display.clone();
        std::thread::spawn(move || by_clipboard(&display, &["c"]))
    };
    let mut copied = false;
    while !reading.is_finished() {
        hidden.sync();
        if !copied
            && hidden
                .take_input()
                .iter()
                .any(|input| matches!(input, Input::Key(..)))
        {
            device.set_selection(Some(&source), hidden.state.key_serial);
            copied = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(reading.join().unwrap().unwrap().as_bytes(), CLIPBOARD_TEXT);

    // A program that copies nothing is told apart from an empty text.
    let nothing = by_clipboard(&display, &["x"]);
    assert_eq!(
        nothing,
        Err("the program copied nothing after select all and copy".into())
    );

    // The test window shows no text; without the helper program the wait says what is missing.
    let installed = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join("tesseract").is_file());
    let waited = wait_on(
        &display,
        &Condition::Text("anything".into()),
        Duration::from_millis(100),
        &mut || Ok(()),
    );
    match installed {
        true => assert!(matches!(waited, Err(WaitError::Timeout(_))), "{waited:?}"),
        false => assert_eq!(waited, Err(WaitError::Failed(NO_TESSERACT.into()))),
    }
}
